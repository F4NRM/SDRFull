//! **The SondeHub live feed**: one MQTT-over-websocket subscription, kept by the
//! station for all of its clients.
//!
//! SondeHub's own guidance ([MQTT Websockets]) is explicit: use the socket, not
//! the API, and run a mirror rather than letting each client connect. Asking
//! `/sondes` once a minute per tenth-degree cell is the thing they asked us to
//! stop doing (issue #745, 2026-11-15), so the station keeps **one** connection,
//! subscribes to the `batch` topic, and holds the newest frame of every sonde in
//! memory. `/sonde-online` is answered from that map; the REST question stays as
//! a cold-start fallback only, and its answer is kept far longer than before.
//!
//! What goes out: one `GET` of SondeHub's websocket address when the feed is
//! first wanted, then one websocket connection with a client id of this
//! station's own (`sdroxide-<random>`), an MQTT CONNECT and one SUBSCRIBE. No
//! place is sent at all — the filtering is ours — so nothing about the operator
//! leaves the machine. Nothing is written to disk, and nothing is asked until a
//! client's operator has switched the layer on; ten quiet minutes close it again.
//!
//! [MQTT Websockets]: https://github.com/projecthorus/sondehub-infra/wiki/MQTT-Websockets

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sdroxide_types::sonde_online::{OnlineSonde, RADIUS_M, parse_frames};

/// Where SondeHub says its websocket is.
const ADDRESS: &str = "https://api.v2.sondehub.org/sondes/websocket";
/// How long the address is believed before it is asked again.
const ADDRESS_HOLD: Duration = Duration::from_secs(6 * 60 * 60);
/// The topic that carries every sonde at once — what SondeHub asks clients to
/// prefer over the per-serial ones.
const TOPIC: &str = "batch";
/// How long a frame counts as "live". A sonde is heard every second or so; one
/// nobody has heard for this long has landed, been switched off, or gone out of
/// range, and is dropped rather than drawn for ever.
const KEEP: Duration = Duration::from_secs(30 * 60);
/// How long an answer is served again without rebuilding it.
const ANSWER_HOLD: Duration = Duration::from_secs(10);
/// The most sondes held at once. The feed carries the whole world; a station's
/// clients only ever ask about the few hundred near them, and this is a bound
/// on a bad day rather than on a normal one.
const MAX_SONDES: usize = 20_000;
/// The feed is wanted for this long after the last client asked for it.
const IDLE: Duration = Duration::from_secs(10 * 60);
/// The MQTT keepalive; the socket is pinged at half of it.
const KEEPALIVE_S: u16 = 60;
/// A read that returns nothing this long means the socket is dead, however
/// healthy it looks.
const SILENCE: Duration = Duration::from_secs(90);
/// How long to wait before connecting again, and the most that grows to.
const BACKOFF_MIN: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// One frame that has arrived, with the moment it did.
struct Held {
    sonde: OnlineSonde,
    at: Instant,
}

/// The feed: every live sonde the station has heard of, and the last time a
/// client asked for them.
#[derive(Default)]
pub struct Live {
    held: Mutex<BTreeMap<String, Held>>,
    /// When a client last asked. `None` before the layer was ever switched on.
    wanted: Mutex<Option<Instant>>,
    /// One task however many clients ask.
    started: AtomicBool,
    /// The last answer built, and when: rebuilding one costs a serialisation
    /// of a few hundred rows and every client asks for the same cell.
    answer: Mutex<Option<(String, Instant, i32, i32)>>,
}

impl Live {
    /// A client has switched the layer on (or is looking again): keep the feed
    /// running for the next [`IDLE`].
    pub fn touch(&self) {
        if let Ok(mut w) = self.wanted.lock() {
            *w = Some(Instant::now());
        }
    }

    /// Whether a client has asked recently enough for the feed to run.
    fn wanted_now(&self, now: Instant) -> bool {
        self.wanted
            .lock()
            .ok()
            .and_then(|w| *w)
            .is_some_and(|at| now.duration_since(at) < IDLE)
    }

    /// The sondes within [`RADIUS_M`] of a place, as the JSON array the clients
    /// read, or `None` while the feed has nothing to say (no socket yet, or
    /// nothing near enough).
    pub fn answer(&self, lat: f64, lon: f64, now: Instant) -> Option<String> {
        let key = (sdroxide_types::sonde_online::tenths(lat), sdroxide_types::sonde_online::tenths(lon));
        if let Ok(a) = self.answer.lock()
            && let Some((body, at, klat, klon)) = a.as_ref()
            && (*klat, *klon) == key
            && now.duration_since(*at) < ANSWER_HOLD
        {
            return Some(body.clone());
        }
        let held = self.held.lock().ok()?;
        let near: Vec<&OnlineSonde> = held
            .values()
            .map(|h| &h.sonde)
            .filter(|s| within(lat, lon, s.lat, s.lon))
            .collect();
        if near.is_empty() {
            return None;
        }
        let body = serde_json::to_string(&near).ok()?;
        if let Ok(mut a) = self.answer.lock() {
            *a = Some((body.clone(), now, key.0, key.1));
        }
        Some(body)
    }

    /// One batch of frames from the socket: parse, keep the newest of each
    /// serial, and let the dead ones go.
    pub fn ingest(&self, json: &str, now: Instant) {
        let Ok(mut held) = self.held.lock() else { return };
        for sonde in parse_frames(json) {
            // A frame older than the one held for that serial (a redelivery, or
            // two stations reporting out of order) does not replace it.
            if let Some(have) = held.get(&sonde.serial)
                && have.sonde.unix > sonde.unix
            {
                continue;
            }
            held.insert(sonde.serial.clone(), Held { sonde, at: now });
        }
        held.retain(|_, h| now.duration_since(h.at) < KEEP);
        // Newest first, so the cap drops the oldest rather than the ones on the
        // air right now.
        if held.len() > MAX_SONDES {
            let mut all: Vec<(String, Instant)> = held.iter().map(|(k, h)| (k.clone(), h.at)).collect();
            all.sort_by_key(|(_, at)| *at);
            for (serial, _) in all.into_iter().take(held.len() - MAX_SONDES) {
                held.remove(&serial);
            }
        }
        if let Ok(mut a) = self.answer.lock() {
            // A fresh frame can change what a client would be told.
            *a = None;
        }
    }

    /// Drop everything: the feed has been idle and nothing should stay cached.
    pub fn clear(&self) {
        if let Ok(mut held) = self.held.lock() {
            held.clear();
        }
        if let Ok(mut a) = self.answer.lock() {
            *a = None;
        }
    }

    /// How many sondes are held, for the probe's line of output.
    pub fn held_len(&self) -> usize {
        self.held.lock().map(|h| h.len()).unwrap_or(0)
    }

    /// Whether the feed has anything to answer with.
    pub fn has_data(&self) -> bool {
        self.held.lock().map(|h| !h.is_empty()).unwrap_or(false)
    }

    /// Start the connection task, once, if a runtime is there to hold it.
    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(handle) = tokio::runtime::Handle::try_current().ok() else { return };
        let live = self.clone();
        handle.spawn(async move { run(live).await });
    }
}

/// How far apart two places are, roughly: the feed's own filter is a circle of
/// [`RADIUS_M`] round the station, and a flat-earth approximation is exact
/// enough at that size and costs no trigonometry per row.
fn within(lat: f64, lon: f64, alat: f64, alon: f64) -> bool {
    let km_per_deg = 111.32_f64;
    let dlat = (alat - lat) * km_per_deg;
    let dlon = (alon - lon) * km_per_deg * lat.to_radians().cos().abs().max(0.01);
    (dlat * dlat + dlon * dlon).sqrt() * 1000.0 <= f64::from(RADIUS_M)
}

/// Ask SondeHub where its websocket is. The reply is the address as plain text
/// (`wss://ws-reader.v2.sondehub.org/`), and anything that is not one is
/// refused rather than handed to the socket layer.
fn address() -> Result<String, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_global(Some(Duration::from_secs(20)))
        .http_status_as_error(false)
        .build()
        .into();
    let mut resp = agent
        .get(ADDRESS)
        .header("User-Agent", "sdroxide")
        .call()
        .map_err(|e| e.to_string())?;
    if resp.status().as_u16() != 200 {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let text = resp.body_mut().with_config().limit(1024).read_to_string().map_err(|e| e.to_string())?;
    let url = text.trim();
    if url.starts_with("wss://") {
        Ok(url.to_string())
    } else {
        Err(format!("not a websocket address: {url:?}"))
    }
}

/// The whole task: connect, subscribe, read, and reconnect while anybody wants
/// the feed.
async fn run(live: Arc<Live>) {
    let mut cached: Option<(String, Instant)> = None;
    let mut backoff = BACKOFF_MIN;
    let mut failure_logged = false;
    loop {
        let now = Instant::now();
        if !live.wanted_now(now) {
            if live.has_data() {
                live.clear();
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
            continue;
        }
        let url = match &cached {
            Some((url, at)) if now.duration_since(*at) < ADDRESS_HOLD => url.clone(),
            _ => match tokio::task::spawn_blocking(address).await {
                Ok(Ok(url)) => {
                    cached = Some((url.clone(), Instant::now()));
                    url
                }
                _ => {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX);
                    continue;
                }
            },
        };
        match session(&live, &url).await {
            Ok(()) => {
                backoff = BACKOFF_MIN;
            }
            Err(e) => {
                // Said once per outage rather than once per attempt: an
                // evening without SondeHub is a line, not a wall.
                if !failure_logged {
                    tracing::warn!("sondehub live feed: {e}");
                    failure_logged = true;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
        }
    }
}

/// One connection: connect, CONNECT, SUBSCRIBE, then read until it ends.
async fn session(live: &Arc<Live>, url: &str) -> Result<(), String> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::Message;

    let mut request = url.into_client_request().map_err(|e| e.to_string())?;
    request
        .headers_mut()
        .insert("User-Agent", "sdroxide".parse().map_err(|_| "bad header".to_string())?);
    // The broker only speaks MQTT to a socket that asked for it: paho (which
    // SondeHub's own example uses) sends this on every websocket connection, and
    // without it the CONNECT is never answered at all.
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        "mqtt".parse().map_err(|_| "bad header".to_string())?,
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.map_err(|e| e.to_string())?;
    let client_id = client_id();
    socket
        .send(Message::Binary(connect_packet(&client_id).into()))
        .await
        .map_err(|e| e.to_string())?;
    socket
        .send(Message::Binary(subscribe_packet(TOPIC, 1).into()))
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!("sondehub live feed: subscribed to {TOPIC} as {client_id}");

    // The first ping goes out at once, which is harmless; the silence timer must
    // *not* — an interval's first tick is immediate, which ended the session the
    // moment it began.
    let mut ping = tokio::time::interval(Duration::from_secs(u64::from(KEEPALIVE_S) / 2));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let silence = tokio::time::sleep(SILENCE);
    tokio::pin!(silence);
    loop {
        tokio::select! {
            _ = ping.tick() => {
                socket.send(Message::Binary(pingreq().into())).await.map_err(|e| e.to_string())?;
            }
            _ = &mut silence => {
                return Err("nothing arrived for 90 s".to_string());
            }
            msg = socket.next() => {
                let Some(msg) = msg else { return Err("the socket closed".to_string()) };
                let msg = msg.map_err(|e| e.to_string())?;
                match msg {
                    Message::Binary(bytes) => {
                        if let Some((topic, payload)) = publish(&bytes)
                            && topic.ends_with(TOPIC)
                            && let Ok(text) = std::str::from_utf8(payload)
                        {
                            live.ingest(text, Instant::now());
                            silence.as_mut().reset(tokio::time::Instant::now() + SILENCE);
                        }
                    }
                    Message::Text(text) => {
                        // Some brokers send text frames carrying the same MQTT bytes.
                        if let Some((topic, payload)) = publish(text.as_bytes())
                            && (topic.ends_with(TOPIC) || std::env::var_os("SONDE_LIVE_TOPIC").is_some())
                            && let Ok(text) = std::str::from_utf8(payload)
                        {
                            live.ingest(text, Instant::now());
                            silence.as_mut().reset(tokio::time::Instant::now() + SILENCE);
                        }
                    }
                    Message::Ping(p) => {
                        socket.send(Message::Pong(p)).await.map_err(|e| e.to_string())?;
                        silence.as_mut().reset(tokio::time::Instant::now() + SILENCE);
                    }
                    Message::Close(_) => return Err("the far end closed".to_string()),
                    _ => {}
                }
            }
        }
        if !live.wanted_now(Instant::now()) {
            return Ok(());
        }
    }
}

/// A client id that is this station's own: SondeHub asks for one per connection,
/// prefixed with something unique to the program.
fn client_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    format!("sdroxide-{:08x}", h.finish() as u32)
}

/// MQTT: the bytes of one control packet, with its fixed header.
fn packet(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 5);
    out.push(kind);
    // Remaining length: seven bits a byte, the top bit meaning "more follows".
    let mut len = body.len();
    loop {
        let mut byte = (len % 128) as u8;
        len /= 128;
        if len > 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if len == 0 {
            break;
        }
    }
    out.extend_from_slice(body);
    out
}

fn with_len(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u16).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}

/// MQTT CONNECT: protocol "MQTT", level 4, clean session, a keepalive, and the
/// client id. No will, no user name, no password — the feed is public and
/// read-only.
fn connect_packet(client_id: &str) -> Vec<u8> {
    let mut body = Vec::new();
    with_len(&mut body, "MQTT");
    body.push(4); // protocol level 3.1.1
    body.push(0x02); // clean session
    body.extend_from_slice(&KEEPALIVE_S.to_be_bytes());
    with_len(&mut body, client_id);
    packet(0x10, &body)
}

/// MQTT SUBSCRIBE at QoS 0, for one topic filter.
fn subscribe_packet(topic: &str, packet_id: u16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&packet_id.to_be_bytes());
    with_len(&mut body, topic);
    body.push(0); // QoS 0
    packet(0x82, &body)
}

/// MQTT PINGREQ, which is a fixed header and nothing else.
fn pingreq() -> Vec<u8> {
    vec![0xC0, 0x00]
}

/// A PUBLISH packet's topic and payload, for the QoS 0 the feed uses.
fn publish(bytes: &[u8]) -> Option<(&str, &[u8])> {
    let (&kind, rest) = bytes.split_first()?;
    if kind >> 4 != 3 {
        return None;
    }
    // Remaining length, up to four bytes; anything longer is not this protocol.
    let mut len = 0usize;
    let mut shift = 0;
    let mut at = 0usize;
    loop {
        let byte = *rest.get(at)?;
        at += 1;
        len |= usize::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 21 {
            return None;
        }
    }
    let body = rest.get(at..at + len)?;
    let qos = (kind >> 1) & 0x03;
    // A QoS 1 or 2 packet carries a packet id before the topic.
    let skip = if qos > 0 { 2 } else { 0 };
    let tl = u16::from_be_bytes([*body.get(skip)?, *body.get(skip + 1)?]) as usize;
    let topic = std::str::from_utf8(body.get(skip + 2..skip + 2 + tl)?).ok()?;
    let payload = body.get(skip + 2 + tl..)?;
    Some((topic, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A batch of one frame, at a time SondeHub's parser reads back.
    fn frame(serial: &str, lat: f64, lon: f64, at: &str) -> String {
        format!("{{\"serial\":\"{serial}\",\"datetime\":\"2026-11-15T{at}Z\",\"lat\":{lat},\"lon\":{lon}}}")
    }

    #[test]
    fn the_connect_packet_is_mqtt_3_1_1_with_our_own_client_id() {
        let p = connect_packet("sdroxide-1234abcd");
        assert_eq!(p[0], 0x10, "CONNECT");
        assert_eq!(p[1] as usize, p.len() - 2, "remaining length covers the body");
        let body = &p[2..];
        assert_eq!(&body[..2], &[0, 4], "protocol name length");
        assert_eq!(&body[2..6], b"MQTT");
        assert_eq!(body[6], 4, "level 3.1.1");
        assert_eq!(body[7] & 0x02, 0x02, "clean session");
        let id_len = u16::from_be_bytes([body[10], body[11]]) as usize;
        assert_eq!(&body[12..12 + id_len], b"sdroxide-1234abcd");
    }

    #[test]
    fn the_client_id_is_ours_and_different_every_time() {
        let a = client_id();
        assert!(a.starts_with("sdroxide-"), "{a}");
        assert_ne!(a, client_id());
    }

    #[test]
    fn the_subscribe_packet_names_the_batch_topic_at_qos_zero() {
        let p = subscribe_packet("batch", 1);
        assert_eq!(p[0], 0x82, "SUBSCRIBE");
        assert_eq!(u16::from_be_bytes([p[2], p[3]]), 1, "packet id");
        assert_eq!(p[4..6], [0, 5], "topic length");
        assert_eq!(&p[6..11], b"batch");
        assert_eq!(p[11], 0, "QoS 0");
        assert_eq!(pingreq(), vec![0xC0, 0x00]);
    }

    #[test]
    fn a_publish_is_read_back_with_its_topic_and_payload() {
        let mut body = Vec::new();
        body.extend_from_slice(&5u16.to_be_bytes());
        body.extend_from_slice(b"batch");
        body.extend_from_slice(b"[{}]");
        let p = packet(0x30, &body);
        let (topic, payload) = publish(&p).expect("a publish");
        assert_eq!(topic, "batch");
        assert_eq!(payload, b"[{}]");
        // Not a publish, and a truncated one, are refused rather than guessed at.
        assert!(publish(&[0x20, 0x02, 0, 0]).is_none());
        assert!(publish(&p[..p.len() - 1]).is_none());
    }

    #[test]
    fn frames_from_the_socket_are_held_per_serial_newest_wins() {
        let live = Live::default();
        let now = Instant::now();
        live.ingest(&format!("[{}]", frame("S1", 52.0, 4.0, "10:00:00")), now);
        live.ingest(&format!("[{}]", frame("S1", 52.5, 4.5, "10:00:30")), now);
        live.ingest(&format!("[{}]", frame("S2", 60.0, 4.0, "10:00:10")), now);
        let held = live.held.lock().unwrap();
        assert_eq!(held.len(), 2);
        assert_eq!(held["S1"].sonde.lat, 52.5, "the newer frame of that serial won");
    }

    #[test]
    fn an_answer_carries_only_what_is_within_the_radius() {
        let live = Live::default();
        let now = Instant::now();
        // S1 is at the place asked about; S2 is ~890 km north of it.
        live.ingest(&format!("[{}]", frame("S1", 52.0, 4.0, "10:00:00")), now);
        live.ingest(&format!("[{}]", frame("S2", 60.0, 4.0, "10:00:00")), now);
        let near = live.answer(52.0, 4.0, now).expect("an answer");
        assert!(near.contains("S1"));
        assert!(!near.contains("S2"), "890 km is outside the 400 km circle");
        // And an empty feed answers nothing, so the route falls back.
        let empty = Live::default();
        assert!(empty.answer(52.0, 4.0, now).is_none());
    }

    #[test]
    fn a_sonde_nobody_has_heard_for_half_an_hour_is_dropped() {
        let live = Live::default();
        let now = Instant::now();
        live.ingest(&format!("[{}]", frame("S1", 52.0, 4.0, "10:00:00")), now);
        live.ingest(
            &format!("[{}]", frame("S2", 52.0, 4.0, "10:01:00")),
            now + KEEP + Duration::from_secs(1),
        );
        let held = live.held.lock().unwrap();
        assert_eq!(held.len(), 1, "only the fresh one is left");
        assert!(held.contains_key("S2"));
    }

    #[test]
    fn only_what_the_operator_is_looking_at_is_close_enough() {
        // ~1 degree of latitude is 111 km; 400 km is 3.6 degrees.
        assert!(within(52.0, 4.0, 52.0, 4.0));
        assert!(within(52.0, 4.0, 54.0, 4.0), "222 km north");
        assert!(!within(52.0, 4.0, 56.0, 4.0), "445 km north");
        // Longitude shrinks with latitude.
        assert!(within(60.0, 4.0, 60.0, 6.0), "111 km east at 60 degrees");
        assert!(!within(60.0, 4.0, 60.0, 12.0));
    }

    #[test]
    fn the_feed_is_only_wanted_for_a_while_after_it_is_asked_for() {
        let live = Live::default();
        assert!(!live.wanted_now(Instant::now()), "nothing has asked yet");
        live.touch();
        assert!(live.wanted_now(Instant::now()));
        assert!(!live.wanted_now(Instant::now() + IDLE + Duration::from_secs(1)));
    }
}
