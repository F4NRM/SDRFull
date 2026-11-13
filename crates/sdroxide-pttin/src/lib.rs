//! The desk microphone and the pedal that key the transmitter (issue #636).
//!
//! Two slots, each read by its own thread: a serial port's handshake lines, a
//! USB HID report, or a MIDI note. The thread debounces what it reads and
//! publishes one fact, "this slot says transmit". The engine that owns the
//! hub asks for the change once a tick and keys the radio through the same
//! `Command::SetPtt` path every other source uses, so every guard applies.
//!
//! # One owner per hub
//!
//! A hub hands its changes to one owner, the id it was last configured for. The
//! station keeps one process-wide hub: the devices belong to the station, not to
//! a radio, so whichever engine is told takes them over, like the T/R switch.
//! The operator's own computer (native client) keeps another, owned by the
//! interface, so a desk microphone there keys whichever station is in front.
//!
//! # What it never does
//!
//! A switch that is already closed when its port opens does not key; the first
//! reading only primes the filter. A port that vanishes while the switch is
//! down releases the transmitter. Nothing here decides *whether* the station
//! may transmit; that is the engine's, as for the on-screen button.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sdroxide_types::{
    Debounce, LineDrive, PttHidDevice, PttInputConfig, PttInputSource, PttInputState,
    PttInputsConfig, PttInputsStatus, PttMidiPort, PttSlot, SenseLine,
};
use tracing::{info, warn};

/// How often a serial port's lines are read and a MIDI queue drained.
const TICK: Duration = Duration::from_millis(10);
/// How long a HID read waits for a report before the filter is fed again.
const HID_WAIT: Duration = Duration::from_millis(20);
/// How long to wait before trying a device that would not open once more.
const RETRY: Duration = Duration::from_secs(2);

/// What a slot's thread tells the rest of the program.
#[derive(Default)]
struct Shared {
    state: PttInputState,
    /// The slot says transmit (after debounce, polarity and hold/toggle).
    keyed: bool,
    /// Called when `keyed` changes, so an interface that sleeps between frames
    /// hears a pedal at once.
    wake: Option<Wake>,
}

/// What a hub calls when a slot's verdict changes.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

struct Worker {
    stop: Arc<AtomicBool>,
    shared: Arc<Mutex<Shared>>,
    thread: Option<JoinHandle<()>>,
    cfg: PttInputConfig,
}

impl Worker {
    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Default)]
struct Lists {
    serial: Vec<String>,
    hid: Vec<PttHidDevice>,
    midi: Vec<PttMidiPort>,
}

#[derive(Default)]
struct Inner {
    wake: Option<Wake>,
    /// The engine that owns the devices and keys its radio from them.
    owner: Option<u32>,
    slots: [Option<Worker>; 2],
    lists: Lists,
    /// The keyed state last handed to the owner, so it hears changes only.
    last_given: bool,
}

/// The station's two PTT inputs. See the module docs.
pub struct PttInputsHub {
    inner: Mutex<Inner>,
}

impl Default for PttInputsHub {
    fn default() -> Self {
        Self::new()
    }
}

fn index(slot: PttSlot) -> usize {
    match slot {
        PttSlot::DeskMic => 0,
        PttSlot::Pedal => 1,
    }
}

/// What the station can offer to choose from.
fn scan() -> Lists {
    let serial = serialport::available_ports()
        .map(|p| p.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default();
    let hid = sdroxide_relay::hid::enumerate(&[])
        .into_iter()
        .map(|e| PttHidDevice { key: e.key, name: e.name, vendor: e.vendor, product: e.product })
        .collect();
    let midi = sdroxide_midi::input_ports()
        .into_iter()
        .map(|p| PttMidiPort { id: p.id, name: p.name })
        .collect();
    Lists { serial, hid, midi }
}

impl PttInputsHub {
    /// An empty hub: nothing open, no owner.
    pub fn new() -> Self {
        PttInputsHub { inner: Mutex::new(Inner::default()) }
    }

    /// A hub that calls `wake` whenever a slot starts or stops saying transmit.
    pub fn with_wake(wake: Wake) -> Self {
        PttInputsHub { inner: Mutex::new(Inner { wake: Some(wake), ..Inner::default() }) }
    }

    /// Take the devices over for `engine` and open what the setup asks for. A
    /// slot whose setup did not change is left running, so pressing APPLY with
    /// the switch down does not drop the transmitter.
    pub fn configure(&self, engine: u32, cfg: &PttInputsConfig) {
        let mut g = self.inner.lock().unwrap();
        g.owner = Some(engine);
        for slot in PttSlot::ALL {
            let want = cfg.slot(slot).clone();
            let i = index(slot);
            let unchanged = g.slots[i].as_ref().is_some_and(|w| w.cfg == want);
            if unchanged && want.enabled {
                continue;
            }
            // Dropping stops and joins the old thread, which lets go of the
            // port before the new one opens it.
            g.slots[i] = None;
            if want.enabled {
                g.slots[i] = Some(spawn(want, g.wake.clone()));
            }
        }
        if g.lists.serial.is_empty() && g.lists.hid.is_empty() && g.lists.midi.is_empty() {
            g.lists = scan();
        }
    }

    /// Look again for ports and devices.
    pub fn rescan(&self) {
        // Outside the lock: a HID enumeration can take a moment.
        let lists = scan();
        self.inner.lock().unwrap().lists = lists;
    }

    /// Whether `engine` owns the devices.
    pub fn owned_by(&self, engine: u32) -> bool {
        self.inner.lock().unwrap().owner == Some(engine)
    }

    /// The change in "a slot says transmit", for the owner only, once.
    pub fn take_change(&self, engine: u32) -> Option<bool> {
        let mut g = self.inner.lock().unwrap();
        if g.owner != Some(engine) {
            return None;
        }
        let now = g.slots.iter().flatten().any(|w| w.shared.lock().unwrap().keyed);
        if now == g.last_given {
            return None;
        }
        g.last_given = now;
        Some(now)
    }

    /// Both slots' condition and the lists to choose from.
    pub fn status(&self) -> PttInputsStatus {
        let g = self.inner.lock().unwrap();
        let state = |i: usize| {
            g.slots[i].as_ref().map(|w| w.shared.lock().unwrap().state.clone()).unwrap_or_default()
        };
        PttInputsStatus {
            desk_mic: state(0),
            pedal: state(1),
            serial_ports: g.lists.serial.clone(),
            hid_devices: g.lists.hid.clone(),
            midi_ports: g.lists.midi.clone(),
        }
    }
}

fn spawn(cfg: PttInputConfig, wake: Option<Wake>) -> Worker {
    let stop = Arc::new(AtomicBool::new(false));
    let shared = Arc::new(Mutex::new(Shared { wake, ..Shared::default() }));
    let (s, sh, c) = (stop.clone(), shared.clone(), cfg.clone());
    let thread = std::thread::Builder::new()
        .name("ptt-input".into())
        .spawn(move || match c.source {
            PttInputSource::Serial => run_serial(&c, &s, &sh),
            PttInputSource::UsbHid => run_hid(&c, &s, &sh),
            PttInputSource::UsbMidi => run_midi(&c, &s, &sh),
        })
        .ok();
    Worker { stop, shared, thread, cfg }
}

/// Publish a change of condition. `keyed` is touched only when `Some`.
fn publish(shared: &Mutex<Shared>, keyed: Option<bool>, f: impl FnOnce(&mut PttInputState)) {
    let mut g = shared.lock().unwrap();
    f(&mut g.state);
    let mut wake = None;
    if let Some(k) = keyed {
        if g.keyed != k {
            wake = g.wake.clone();
        }
        g.keyed = k;
    }
    drop(g);
    if let Some(w) = wake {
        w();
    }
}

fn fail(shared: &Mutex<Shared>, what: &str, e: impl std::fmt::Display) {
    let text = format!("{what}: {e}");
    publish(shared, Some(false), |s| {
        s.open = false;
        s.pressed = false;
        s.error = text;
    });
}

fn run_serial(cfg: &PttInputConfig, stop: &AtomicBool, shared: &Mutex<Shared>) {
    let t0 = Instant::now();
    let mut port: Option<Box<dyn serialport::SerialPort>> = None;
    let mut deb = Debounce::default();
    let mut retry_at = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if port.is_none() {
            if cfg.port.trim().is_empty() {
                publish(shared, Some(false), |s| {
                    s.open = false;
                    s.error = "no serial port chosen".into();
                });
            } else if Instant::now() >= retry_at {
                match serialport::new(cfg.port.trim(), 9600).timeout(TICK).open() {
                    Ok(mut p) => {
                        let drive = |level: LineDrive| match level {
                            LineDrive::Leave => None,
                            LineDrive::On => Some(true),
                            LineDrive::Off => Some(false),
                        };
                        let mut ok = true;
                        if let Some(on) = drive(cfg.rts) {
                            ok &= p.write_request_to_send(on).is_ok();
                        }
                        if let Some(on) = drive(cfg.dtr) {
                            ok &= p.write_data_terminal_ready(on).is_ok();
                        }
                        if !ok {
                            fail(shared, &cfg.port, "cannot set RTS/DTR on this port");
                            retry_at = Instant::now() + RETRY;
                        } else {
                            info!("PTT input: {} open", cfg.port);
                            deb.reset();
                            publish(shared, None, |s| {
                                s.open = true;
                                s.error.clear();
                            });
                            port = Some(p);
                        }
                    }
                    Err(e) => {
                        fail(shared, &cfg.port, e);
                        retry_at = Instant::now() + RETRY;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let p = port.as_mut().unwrap();
        let read = (|| {
            Ok::<_, serialport::Error>((
                p.read_clear_to_send()?,
                p.read_data_set_ready()?,
                p.read_carrier_detect()?,
            ))
        })();
        match read {
            Err(e) => {
                warn!("PTT input: {} lost: {e}", cfg.port);
                fail(shared, &cfg.port, e);
                port = None;
                deb.reset();
                retry_at = Instant::now() + RETRY;
            }
            Ok((cts, dsr, dcd)) => {
                let level = match cfg.line {
                    SenseLine::Cts => Some(cts),
                    SenseLine::Dsr => Some(dsr),
                    SenseLine::Dcd => Some(dcd),
                    SenseLine::Off => None,
                };
                let now_ms = t0.elapsed().as_millis() as u64;
                let edge = level
                    .and_then(|l| deb.feed(cfg, sdroxide_types::level_pressed(cfg, l), now_ms));
                let pressed = level.is_some() && deb.pressed();
                publish(shared, edge, |s| {
                    s.cts = cts;
                    s.dsr = dsr;
                    s.dcd = dcd;
                    s.pressed = pressed;
                });
                std::thread::sleep(TICK);
            }
        }
    }
    // Back to receive on the way out; the port closes and the driver puts RTS
    // and DTR where it likes.
    publish(shared, Some(false), |s| {
        s.open = false;
        s.pressed = false;
    });
}

fn run_hid(cfg: &PttInputConfig, stop: &AtomicBool, shared: &Mutex<Shared>) {
    let t0 = Instant::now();
    let mut dev: Option<Box<dyn sdroxide_relay::hid::HidDev>> = None;
    let mut deb = Debounce::default();
    let mut retry_at = Instant::now();
    let mut raw: Option<bool> = None;
    while !stop.load(Ordering::Relaxed) {
        if dev.is_none() {
            if cfg.hid_key.trim().is_empty() {
                publish(shared, Some(false), |s| {
                    s.open = false;
                    s.error = "no USB HID device chosen".into();
                });
            } else if Instant::now() >= retry_at {
                match sdroxide_relay::hid::open_input(cfg.hid_key.trim()) {
                    Ok(d) => {
                        info!("PTT input: HID {} open", cfg.hid_name);
                        deb.reset();
                        raw = None;
                        publish(shared, None, |s| {
                            s.open = true;
                            s.error.clear();
                        });
                        dev = Some(d);
                    }
                    Err(e) => {
                        fail(shared, &cfg.hid_name, e);
                        retry_at = Instant::now() + RETRY;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        match dev.as_mut().unwrap().read_input(HID_WAIT) {
            Err(e) => {
                warn!("PTT input: HID {} lost: {e}", cfg.hid_name);
                fail(shared, &cfg.hid_name, e);
                dev = None;
                retry_at = Instant::now() + RETRY;
                continue;
            }
            Ok(Some(report)) => {
                raw = sdroxide_types::hid_pressed(cfg, &report);
                publish(shared, None, |s| s.hid_report = report);
            }
            Ok(None) => {}
        }
        let now_ms = t0.elapsed().as_millis() as u64;
        let edge = raw.and_then(|r| deb.feed(cfg, r, now_ms));
        let pressed = deb.pressed();
        publish(shared, edge, |s| s.pressed = pressed);
    }
    publish(shared, Some(false), |s| {
        s.open = false;
        s.pressed = false;
    });
}

fn run_midi(cfg: &PttInputConfig, stop: &AtomicBool, shared: &Mutex<Shared>) {
    let t0 = Instant::now();
    let midi_cfg = sdroxide_midi::MidiConfig {
        enabled: true,
        in_port_id: cfg.midi_port_id.clone(),
        in_port_name: cfg.midi_port_name.clone(),
        ..Default::default()
    };
    let mut handle = sdroxide_midi::spawn(midi_cfg, || {});
    let mut deb = Debounce::default();
    let mut raw: Option<bool> = None;
    let mut was_open = false;
    while !stop.load(Ordering::Relaxed) {
        for ev in handle.poll() {
            match ev {
                sdroxide_midi::MidiEvent::Control(c) => {
                    publish(shared, None, |s| {
                        s.last_midi = Some(c.msg);
                        s.midi_count = s.midi_count.wrapping_add(1);
                    });
                    if let Some(p) = sdroxide_types::midi_pressed(cfg, &c.msg, c.value, c.off) {
                        raw = Some(p);
                    }
                }
                sdroxide_midi::MidiEvent::Connected(_) => {}
                sdroxide_midi::MidiEvent::Disconnected => {
                    // A port that goes while the switch is down releases it.
                    raw = None;
                    deb.reset();
                    publish(shared, Some(false), |s| s.pressed = false);
                }
            }
        }
        let st = handle.status();
        if st.connected != was_open || st.error.is_some() {
            was_open = st.connected;
            publish(shared, None, |s| {
                s.open = st.connected;
                s.error = st.error.clone().unwrap_or_default();
            });
        }
        let now_ms = t0.elapsed().as_millis() as u64;
        let edge = raw.and_then(|r| deb.feed(cfg, r, now_ms));
        let pressed = deb.pressed();
        publish(shared, edge, |s| s.pressed = pressed);
        std::thread::sleep(TICK);
    }
    publish(shared, Some(false), |s| {
        s.open = false;
        s.pressed = false;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_slot_reads_nothing_and_keys_nothing() {
        let hub = PttInputsHub { inner: Mutex::new(Inner::default()) };
        hub.configure(0, &PttInputsConfig::default());
        assert_eq!(hub.take_change(0), None);
        let st = hub.status();
        assert!(!st.desk_mic.open && !st.pedal.open);
    }

    #[test]
    fn only_the_engine_that_was_told_hears_the_change() {
        let hub = PttInputsHub { inner: Mutex::new(Inner::default()) };
        hub.configure(1, &PttInputsConfig::default());
        // Make slot 0 say transmit without a device.
        let shared = Arc::new(Mutex::new(Shared { keyed: true, ..Shared::default() }));
        hub.inner.lock().unwrap().slots[0] = Some(Worker {
            stop: Arc::new(AtomicBool::new(false)),
            shared,
            thread: None,
            cfg: PttInputConfig::default(),
        });
        assert_eq!(hub.take_change(0), None, "engine 0 does not own the devices");
        assert_eq!(hub.take_change(1), Some(true));
        assert_eq!(hub.take_change(1), None, "told once");
        hub.inner.lock().unwrap().slots[0].as_ref().unwrap().shared.lock().unwrap().keyed = false;
        assert_eq!(hub.take_change(1), Some(false));
    }

    #[test]
    fn either_slot_keys_and_both_must_release() {
        let hub = PttInputsHub { inner: Mutex::new(Inner::default()) };
        hub.configure(0, &PttInputsConfig::default());
        let mk = |keyed| Worker {
            stop: Arc::new(AtomicBool::new(false)),
            shared: Arc::new(Mutex::new(Shared { keyed, ..Shared::default() })),
            thread: None,
            cfg: PttInputConfig::default(),
        };
        {
            let mut g = hub.inner.lock().unwrap();
            g.slots[0] = Some(mk(true));
            g.slots[1] = Some(mk(true));
        }
        assert_eq!(hub.take_change(0), Some(true));
        hub.inner.lock().unwrap().slots[0].as_ref().unwrap().shared.lock().unwrap().keyed = false;
        assert_eq!(hub.take_change(0), None, "the pedal still holds it");
        hub.inner.lock().unwrap().slots[1].as_ref().unwrap().shared.lock().unwrap().keyed = false;
        assert_eq!(hub.take_change(0), Some(false));
    }

    #[test]
    fn a_port_that_will_not_open_says_why_and_never_keys() {
        let shared = Mutex::new(Shared::default());
        let stop = AtomicBool::new(false);
        let cfg = PttInputConfig {
            enabled: true,
            port: "definitely-not-a-port".into(),
            ..PttInputConfig::default()
        };
        std::thread::scope(|s| {
            s.spawn(|| run_serial(&cfg, &stop, &shared));
            std::thread::sleep(Duration::from_millis(300));
            stop.store(true, Ordering::Relaxed);
        });
        let g = shared.lock().unwrap();
        assert!(!g.keyed);
        assert!(!g.state.open);
    }

    #[test]
    fn the_wake_is_called_when_a_slot_starts_or_stops_saying_transmit() {
        use std::sync::atomic::AtomicUsize;
        let n = Arc::new(AtomicUsize::new(0));
        let c = n.clone();
        let wake: Wake = Arc::new(move || {
            c.fetch_add(1, Ordering::SeqCst);
        });
        let shared = Mutex::new(Shared { wake: Some(wake), ..Shared::default() });
        publish(&shared, Some(true), |_| {});
        publish(&shared, Some(true), |_| {});
        publish(&shared, None, |_| {});
        assert_eq!(n.load(Ordering::SeqCst), 1, "a repeat of the same verdict is not news");
        publish(&shared, Some(false), |_| {});
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }
}
