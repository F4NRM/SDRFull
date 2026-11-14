//! The browser's half of the operator's own microphone and pedal (issue #636):
//! the same two slots as the native client, read through Web Serial, WebHID and
//! Web MIDI instead of the operating system.
//!
//! The browser decides what a page may touch. A serial port, a HID device or a
//! MIDI input has to be **allowed** once by the person (a prompt the browser
//! draws), after which the page finds it again by what it is: `usb:VVVV:PPPP`
//! for a serial port, `hid:VVVV:PPPP` for a HID device, the id the browser
//! gives a MIDI input. Only those choices are kept, on the server, per login
//! (`ClientMsg::SetOperatorPtt`); the permission itself stays in the browser.
//! Chrome and Edge have all three; Firefox and Safari have no Serial and no
//! HID, and say so.
//!
//! The debounce, the polarity and the hold/toggle rules are the types crate's,
//! so a switch behaves here as it does on the station. One task per slot reads
//! what the browser reports and publishes one fact: "this slot says transmit".

use crate::i18n::tr;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use sdroxide_types::{
    Debounce, LineDrive, PttHidDevice, PttInputConfig, PttInputSource, PttInputState,
    PttInputsConfig, PttInputsStatus, PttMidiPort, PttSlot, SenseLine,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};

#[wasm_bindgen(inline_js = r#"
const S = { serial: [null, null], hid: [null, null], midi: [null, null], access: null };
const hex = (n) => (n || 0).toString(16).padStart(4, '0');
const dedupe = (items, base) => {
  const seen = {};
  return items.map((x) => {
    const b = base(x);
    seen[b] = (seen[b] || 0) + 1;
    return [seen[b] > 1 ? b + '#' + seen[b] : b, x];
  });
};
const serialAll = async () =>
  dedupe(await navigator.serial.getPorts(), (p) => {
    const i = p.getInfo();
    return 'usb:' + hex(i.usbVendorId) + ':' + hex(i.usbProductId);
  });
const hidAll = async () =>
  dedupe(await navigator.hid.getDevices(), (d) => 'hid:' + hex(d.vendorId) + ':' + hex(d.productId));
// Only the explicit Allow button may ask for MIDI permission. A restored
// setup or a device-list refresh must never raise a prompt during startup.
const midiAccess = () => S.access;
const msg = (e) => String((e && (e.message || e.name)) || e);

export function pttSupport() {
  return ((typeof navigator.serial?.getPorts === 'function') ? 1 : 0) |
    ((typeof navigator.hid?.getDevices === 'function') ? 2 : 0) |
    ((typeof navigator.requestMIDIAccess === 'function') ? 4 : 0);
}
export function pttSleep(ms) { return new Promise((r) => setTimeout(r, ms)); }

export async function pttSerialList() { return (await serialAll()).map((x) => x[0]); }
export async function pttSerialRequest() {
  try { await navigator.serial.requestPort(); } catch (e) { return e && e.name === 'NotFoundError' ? '' : msg(e); }
  return '';
}
export async function pttSerialClose(slot, gen) {
  const s = S.serial[slot];
  if (!s || s.gen !== gen) return;
  S.serial[slot] = null;
  try { await s.port.close(); } catch (e) {}
}
export async function pttSerialOpen(slot, gen, id, rts, dtr) {
  const e = (await serialAll()).find((x) => x[0] === id);
  if (!e) return '@not-authorized';
  const port = e[1];
  try {
    await port.open({ baudRate: 9600 });
    const sig = {};
    if (rts) sig.requestToSend = rts === 1;
    if (dtr) sig.dataTerminalReady = dtr === 1;
    if (rts || dtr) await port.setSignals(sig);
  } catch (err) {
    try { await port.close(); } catch (_) {}
    return msg(err);
  }
  S.serial[slot] = { port, gen };
  return '';
}
export async function pttSerialRead(slot, gen) {
  const s = S.serial[slot];
  if (!s || s.gen !== gen) return -1;
  try {
    const g = await s.port.getSignals();
    return (g.clearToSend ? 1 : 0) | (g.dataSetReady ? 2 : 0) | (g.dataCarrierDetect ? 4 : 0);
  } catch (e) { return -1; }
}

export async function pttHidList() {
  return (await hidAll()).map((x) => [x[0], x[1].productName || '', x[1].vendorId, x[1].productId]);
}
export async function pttHidRequest() {
  try { await navigator.hid.requestDevice({ filters: [] }); } catch (e) { return msg(e); }
  return '';
}
export async function pttHidClose(slot, gen) {
  const s = S.hid[slot];
  if (!s || s.gen !== gen) return;
  S.hid[slot] = null;
  s.dev.removeEventListener('inputreport', s.fn);
  try { await s.dev.close(); } catch (e) {}
}
export async function pttHidOpen(slot, gen, key) {
  const e = (await hidAll()).find((x) => x[0] === key);
  if (!e) return '@not-authorized';
  const dev = e[1];
  try { if (!dev.opened) await dev.open(); } catch (err) { return msg(err); }
  const st = { dev, gen, q: [] };
  st.fn = (ev) => {
    const d = ev.data;
    if (st.q.length < 64) st.q.push(new Uint8Array(d.buffer.slice(d.byteOffset, d.byteOffset + d.byteLength)));
  };
  dev.addEventListener('inputreport', st.fn);
  S.hid[slot] = st;
  return '';
}
export function pttHidTake(slot, gen) {
  const s = S.hid[slot];
  if (!s || s.gen !== gen) return null;
  const q = s.q;
  s.q = [];
  return q;
}
export function pttHidAlive(slot, gen) {
  const s = S.hid[slot];
  return !!s && s.gen === gen && s.dev.opened;
}

export async function pttMidiRequest() {
  try { S.access = await navigator.requestMIDIAccess(); return ''; }
  catch (e) { return msg(e); }
}
export async function pttMidiList() {
  if (!S.access) return [];
  try { return [...midiAccess().inputs.values()].map((i) => [i.id, i.name || i.id]); }
  catch (e) { return []; }
}
export async function pttMidiClose(slot, gen) {
  const s = S.midi[slot];
  if (!s || s.gen !== gen) return;
  S.midi[slot] = null;
  s.inp.removeEventListener('midimessage', s.fn);
}
export async function pttMidiOpen(slot, gen, id) {
  let a;
  a = midiAccess();
  if (!a) return 'NotAllowedError';
  const inp = a.inputs.get(id);
  if (!inp) return '@lost';
  const st = { inp, gen, q: [] };
  st.fn = (ev) => { if (st.q.length < 256) st.q.push(Array.from(ev.data)); };
  inp.addEventListener('midimessage', st.fn);
  S.midi[slot] = st;
  return '';
}
export function pttMidiTake(slot, gen) {
  const s = S.midi[slot];
  if (!s || s.gen !== gen) return null;
  const q = s.q;
  s.q = [];
  return q;
}
export function pttMidiAlive(slot, gen) {
  const s = S.midi[slot];
  return !!s && s.gen === gen && s.inp.state === 'connected';
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = pttSupport)]
    fn js_support() -> u32;
    #[wasm_bindgen(js_name = pttSleep)]
    fn js_sleep(ms: u32) -> js_sys::Promise;

    #[wasm_bindgen(js_name = pttSerialList)]
    fn js_serial_list() -> js_sys::Promise;
    #[wasm_bindgen(js_name = pttSerialRequest)]
    fn js_serial_request() -> js_sys::Promise;
    #[wasm_bindgen(js_name = pttSerialOpen)]
    fn js_serial_open(slot: u32, generation: u32, id: &str, rts: u32, dtr: u32) -> js_sys::Promise;
    #[wasm_bindgen(js_name = pttSerialRead)]
    fn js_serial_read(slot: u32, generation: u32) -> js_sys::Promise;
    #[wasm_bindgen(js_name = pttSerialClose)]
    fn js_serial_close(slot: u32, generation: u32) -> js_sys::Promise;

    #[wasm_bindgen(js_name = pttHidList)]
    fn js_hid_list() -> js_sys::Promise;
    #[wasm_bindgen(js_name = pttHidRequest)]
    fn js_hid_request() -> js_sys::Promise;
    #[wasm_bindgen(js_name = pttHidOpen)]
    fn js_hid_open(slot: u32, generation: u32, key: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_name = pttHidTake)]
    fn js_hid_take(slot: u32, generation: u32) -> JsValue;
    #[wasm_bindgen(js_name = pttHidAlive)]
    fn js_hid_alive(slot: u32, generation: u32) -> bool;
    #[wasm_bindgen(js_name = pttHidClose)]
    fn js_hid_close(slot: u32, generation: u32) -> js_sys::Promise;

    #[wasm_bindgen(js_name = pttMidiRequest)]
    fn js_midi_request() -> js_sys::Promise;
    #[wasm_bindgen(js_name = pttMidiList)]
    fn js_midi_list() -> js_sys::Promise;
    #[wasm_bindgen(js_name = pttMidiOpen)]
    fn js_midi_open(slot: u32, generation: u32, id: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_name = pttMidiTake)]
    fn js_midi_take(slot: u32, generation: u32) -> JsValue;
    #[wasm_bindgen(js_name = pttMidiAlive)]
    fn js_midi_alive(slot: u32, generation: u32) -> bool;
    #[wasm_bindgen(js_name = pttMidiClose)]
    fn js_midi_close(slot: u32, generation: u32) -> js_sys::Promise;
}

/// What the browser can do at all, as the three bits `pttSupport` returns.
#[derive(Clone, Copy, Default)]
pub struct Support {
    pub serial: bool,
    pub hid: bool,
    pub midi: bool,
}

pub fn support() -> Support {
    let b = js_support();
    Support { serial: b & 1 != 0, hid: b & 2 != 0, midi: b & 4 != 0 }
}

/// A call that returns nothing but an error text (empty is success).
async fn text(p: js_sys::Promise) -> String {
    match JsFuture::from(p).await {
        Ok(v) => v.as_string().unwrap_or_default(),
        Err(e) => e.as_string().unwrap_or_else(|| format!("{e:?}")),
    }
}

async fn sleep(ms: u32) {
    let _ = JsFuture::from(js_sleep(ms)).await;
}

fn now_ms() -> u64 {
    js_sys::Date::now() as u64
}

#[derive(Default)]
struct Shared {
    state: PttInputState,
    keyed: bool,
}

type Wake = Rc<dyn Fn()>;

/// Publish a change of condition; `keyed` is touched only when `Some`, and the
/// interface is woken when it changes.
fn publish(
    shared: &RefCell<Shared>,
    wake: &Wake,
    keyed: Option<bool>,
    f: impl FnOnce(&mut PttInputState),
) {
    let mut changed = false;
    {
        let mut g = shared.borrow_mut();
        f(&mut g.state);
        if let Some(k) = keyed {
            changed = g.keyed != k;
            g.keyed = k;
        }
    }
    if changed {
        wake();
    }
}

fn fail(shared: &RefCell<Shared>, wake: &Wake, what: &str, e: &str) {
    let translated = match e {
        "@no-device" => tr!("ptt-web-no-device"),
        "@lost" => tr!("ptt-web-lost"),
        "@not-authorized" => tr!("ptt-web-not-authorized"),
        "NotAllowedError" => tr!("ptt-web-permission"),
        "@serial" => tr!("ptt-web-no-api", api = "Web Serial"),
        "@hid" => tr!("ptt-web-no-api", api = "WebHID"),
        "@midi" => tr!("ptt-web-no-api", api = "Web MIDI"),
        _ => tr!("ptt-web-error", detail = e),
    };
    let text = if what.is_empty() { translated } else { format!("{what}: {translated}") };
    publish(shared, wake, Some(false), |s| {
        s.open = false;
        s.pressed = false;
        s.error = text;
    });
}

struct Slot {
    cancel: Rc<Cell<bool>>,
    shared: Rc<RefCell<Shared>>,
    cfg: PttInputConfig,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.cancel.set(true);
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
    slots: [Option<Slot>; 2],
    lists: Lists,
    last_given: bool,
    /// Tells one task's port from the next one's on the same slot.
    generation: u32,
}

/// The two slots, read through the browser. Cheap to clone: every radio tab of
/// the window holds the same one.
#[derive(Clone)]
pub struct WebHub {
    inner: Rc<RefCell<Inner>>,
    wake: Wake,
}

thread_local! {
    static HUB: RefCell<Option<WebHub>> = const { RefCell::new(None) };
}

/// The window's one hub: two would fight over the same port.
pub fn hub(wake: Wake) -> WebHub {
    let existing = HUB.with(|h| h.borrow().clone());
    if let Some(h) = existing {
        return h;
    }
    let hub = WebHub { inner: Rc::new(RefCell::new(Inner::default())), wake };
    HUB.with(|h| *h.borrow_mut() = Some(hub.clone()));
    hub
}

fn index(slot: PttSlot) -> usize {
    match slot {
        PttSlot::DeskMic => 0,
        PttSlot::Pedal => 1,
    }
}

impl WebHub {
    /// Open what the setup asks for. A slot whose setup did not change is left
    /// running.
    pub fn configure(&self, cfg: &PttInputsConfig) {
        let mut g = self.inner.borrow_mut();
        for slot in PttSlot::ALL {
            let want = cfg.slot(slot).clone();
            let i = index(slot);
            let unchanged = g.slots[i].as_ref().is_some_and(|w| w.cfg == want);
            if unchanged && want.enabled {
                continue;
            }
            g.slots[i] = None;
            if want.enabled {
                g.generation = g.generation.wrapping_add(1);
                let generation = g.generation;
                let cancel = Rc::new(Cell::new(false));
                let shared = Rc::new(RefCell::new(Shared::default()));
                let (c, sh, w, cf) =
                    (cancel.clone(), shared.clone(), self.wake.clone(), want.clone());
                let n = i as u32;
                spawn_local(async move {
                    match cf.source {
                        PttInputSource::Serial => serial_task(n, generation, cf, c, sh, w).await,
                        PttInputSource::UsbHid => hid_task(n, generation, cf, c, sh, w).await,
                        PttInputSource::UsbMidi => midi_task(n, generation, cf, c, sh, w).await,
                    }
                });
                g.slots[i] = Some(Slot { cancel, shared, cfg: want });
            }
        }
    }

    /// Ask the browser what it has been allowed.
    pub fn rescan(&self) {
        let inner = self.inner.clone();
        spawn_local(async move {
            let sup = support();
            let mut lists = Lists::default();
            if sup.serial {
                lists.serial = array_strings(JsFuture::from(js_serial_list()).await.ok());
            }
            if sup.hid
                && let Ok(v) = JsFuture::from(js_hid_list()).await
            {
                for row in js_sys::Array::from(&v).iter() {
                    let r = js_sys::Array::from(&row);
                    lists.hid.push(PttHidDevice {
                        key: r.get(0).as_string().unwrap_or_default(),
                        name: r
                            .get(1)
                            .as_string()
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| tr!("ptt-source-hid")),
                        vendor: r.get(2).as_f64().unwrap_or(0.0) as u16,
                        product: r.get(3).as_f64().unwrap_or(0.0) as u16,
                    });
                }
            }
            if sup.midi
                && let Ok(v) = JsFuture::from(js_midi_list()).await
            {
                for row in js_sys::Array::from(&v).iter() {
                    let r = js_sys::Array::from(&row);
                    lists.midi.push(PttMidiPort {
                        id: r.get(0).as_string().unwrap_or_default(),
                        name: r.get(1).as_string().unwrap_or_default(),
                    });
                }
            }
            inner.borrow_mut().lists = lists;
        });
    }

    /// Show the browser's own prompt for one more device, then list again.
    /// Must be called from a click: the browser refuses it otherwise.
    pub fn grant(&self, kind: PttInputSource) {
        let this = self.clone();
        let sup = support();
        // Started now, in the click, not after an await: the browser wants the
        // gesture to be fresh.
        let request = match kind {
            PttInputSource::Serial if sup.serial => Some(js_serial_request()),
            PttInputSource::UsbHid if sup.hid => Some(js_hid_request()),
            PttInputSource::UsbMidi if sup.midi => Some(js_midi_request()),
            _ => None,
        };
        spawn_local(async move {
            if let Some(p) = request {
                let _ = text(p).await;
            }
            this.rescan();
        });
    }

    /// The change in "a slot says transmit", once.
    pub fn take_change(&self) -> Option<bool> {
        let mut g = self.inner.borrow_mut();
        let now = g.slots.iter().flatten().any(|w| w.shared.borrow().keyed);
        if now == g.last_given {
            return None;
        }
        g.last_given = now;
        Some(now)
    }

    pub fn status(&self) -> PttInputsStatus {
        let g = self.inner.borrow();
        let state = |i: usize| {
            g.slots[i].as_ref().map(|w| w.shared.borrow().state.clone()).unwrap_or_default()
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

fn array_strings(v: Option<JsValue>) -> Vec<String> {
    v.map(|v| js_sys::Array::from(&v).iter().filter_map(|x| x.as_string()).collect())
        .unwrap_or_default()
}

/// The words for a browser that cannot do what the slot asks.
fn unsupported(source: PttInputSource) -> Option<&'static str> {
    let s = support();
    match source {
        PttInputSource::Serial if !s.serial => Some("@serial"),
        PttInputSource::UsbHid if !s.hid => Some("@hid"),
        PttInputSource::UsbMidi if !s.midi => Some("@midi"),
        _ => None,
    }
}

const RETRY_MS: u32 = 2000;
const TICK_MS: u32 = 20;

async fn serial_task(
    slot: u32,
    generation: u32,
    cfg: PttInputConfig,
    cancel: Rc<Cell<bool>>,
    shared: Rc<RefCell<Shared>>,
    wake: Wake,
) {
    let t0 = now_ms();
    let mut deb = Debounce::default();
    let mut open = false;
    let drive = |l: LineDrive| match l {
        LineDrive::Leave => 0,
        LineDrive::On => 1,
        LineDrive::Off => 2,
    };
    while !cancel.get() {
        if let Some(why) = unsupported(PttInputSource::Serial) {
            fail(&shared, &wake, "", why);
            sleep(RETRY_MS).await;
            continue;
        }
        if !open {
            if cfg.port.trim().is_empty() {
                fail(&shared, &wake, "", "@no-device");
                sleep(200).await;
                continue;
            }
            let err = text(js_serial_open(
                slot,
                generation,
                cfg.port.trim(),
                drive(cfg.rts),
                drive(cfg.dtr),
            ))
            .await;
            if err.is_empty() {
                open = true;
                deb.reset();
                publish(&shared, &wake, None, |s| {
                    s.open = true;
                    s.error.clear();
                });
            } else {
                fail(&shared, &wake, &cfg.port, &err);
                sleep(RETRY_MS).await;
            }
            continue;
        }
        let v = JsFuture::from(js_serial_read(slot, generation))
            .await
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(-1.0);
        if v < 0.0 {
            fail(&shared, &wake, &cfg.port, "@lost");
            let _ = JsFuture::from(js_serial_close(slot, generation)).await;
            open = false;
            deb.reset();
            sleep(RETRY_MS).await;
            continue;
        }
        let bits = v as u32;
        let (cts, dsr, dcd) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
        let level = match cfg.line {
            SenseLine::Cts => Some(cts),
            SenseLine::Dsr => Some(dsr),
            SenseLine::Dcd => Some(dcd),
            SenseLine::Off => None,
        };
        let edge = level
            .and_then(|l| deb.feed(&cfg, sdroxide_types::level_pressed(&cfg, l), now_ms() - t0));
        let pressed = level.is_some() && deb.pressed();
        publish(&shared, &wake, edge, |s| {
            s.cts = cts;
            s.dsr = dsr;
            s.dcd = dcd;
            s.pressed = pressed;
        });
        sleep(TICK_MS).await;
    }
    let _ = JsFuture::from(js_serial_close(slot, generation)).await;
    publish(&shared, &wake, Some(false), |s| {
        s.open = false;
        s.pressed = false;
    });
}

async fn hid_task(
    slot: u32,
    generation: u32,
    cfg: PttInputConfig,
    cancel: Rc<Cell<bool>>,
    shared: Rc<RefCell<Shared>>,
    wake: Wake,
) {
    let t0 = now_ms();
    let mut deb = Debounce::default();
    let mut open = false;
    let mut raw: Option<bool> = None;
    while !cancel.get() {
        if let Some(why) = unsupported(PttInputSource::UsbHid) {
            fail(&shared, &wake, "", why);
            sleep(RETRY_MS).await;
            continue;
        }
        if !open {
            if cfg.hid_key.trim().is_empty() {
                fail(&shared, &wake, "", "@no-device");
                sleep(200).await;
                continue;
            }
            let err = text(js_hid_open(slot, generation, cfg.hid_key.trim())).await;
            if err.is_empty() {
                open = true;
                deb.reset();
                raw = None;
                publish(&shared, &wake, None, |s| {
                    s.open = true;
                    s.error.clear();
                });
            } else {
                fail(&shared, &wake, &cfg.hid_name, &err);
                sleep(RETRY_MS).await;
            }
            continue;
        }
        if !js_hid_alive(slot, generation) {
            fail(&shared, &wake, &cfg.hid_name, "@lost");
            let _ = JsFuture::from(js_hid_close(slot, generation)).await;
            open = false;
            sleep(RETRY_MS).await;
            continue;
        }
        let taken = js_hid_take(slot, generation);
        if !taken.is_null() && !taken.is_undefined() {
            for item in js_sys::Array::from(&taken).iter() {
                let report = js_sys::Uint8Array::new(&item).to_vec();
                raw = sdroxide_types::hid_pressed(&cfg, &report);
                publish(&shared, &wake, None, |s| s.hid_report = report);
            }
        }
        // The filter is fed on every tick, so a contact that held still for the
        // debounce time is believed without waiting for another report.
        let edge = raw.and_then(|r| deb.feed(&cfg, r, now_ms() - t0));
        let pressed = deb.pressed();
        publish(&shared, &wake, edge, |s| s.pressed = pressed);
        sleep(TICK_MS).await;
    }
    let _ = JsFuture::from(js_hid_close(slot, generation)).await;
    publish(&shared, &wake, Some(false), |s| {
        s.open = false;
        s.pressed = false;
    });
}

async fn midi_task(
    slot: u32,
    generation: u32,
    cfg: PttInputConfig,
    cancel: Rc<Cell<bool>>,
    shared: Rc<RefCell<Shared>>,
    wake: Wake,
) {
    let t0 = now_ms();
    let mut deb = Debounce::default();
    let mut open = false;
    let mut raw: Option<bool> = None;
    while !cancel.get() {
        if let Some(why) = unsupported(PttInputSource::UsbMidi) {
            fail(&shared, &wake, "", why);
            sleep(RETRY_MS).await;
            continue;
        }
        if !open {
            if cfg.midi_port_id.trim().is_empty() {
                fail(&shared, &wake, "", "@no-device");
                sleep(200).await;
                continue;
            }
            let err = text(js_midi_open(slot, generation, cfg.midi_port_id.trim())).await;
            if err.is_empty() {
                open = true;
                deb.reset();
                raw = None;
                publish(&shared, &wake, None, |s| {
                    s.open = true;
                    s.error.clear();
                });
            } else {
                fail(&shared, &wake, &cfg.midi_port_name, &err);
                sleep(RETRY_MS).await;
            }
            continue;
        }
        if !js_midi_alive(slot, generation) {
            // A port that goes while the switch is down releases it.
            raw = None;
            deb.reset();
            fail(&shared, &wake, &cfg.midi_port_name, "@lost");
            let _ = JsFuture::from(js_midi_close(slot, generation)).await;
            open = false;
            sleep(RETRY_MS).await;
            continue;
        }
        let taken = js_midi_take(slot, generation);
        if !taken.is_null() && !taken.is_undefined() {
            for item in js_sys::Array::from(&taken).iter() {
                let bytes: Vec<u8> = js_sys::Array::from(&item)
                    .iter()
                    .filter_map(|b| b.as_f64())
                    .map(|b| b as u8)
                    .collect();
                if let Some((msg, value, off)) = sdroxide_types::midi_from_bytes(&bytes) {
                    publish(&shared, &wake, None, |s| {
                        s.last_midi = Some(msg);
                        s.midi_count = s.midi_count.wrapping_add(1);
                    });
                    if let Some(p) = sdroxide_types::midi_pressed(&cfg, &msg, value, off) {
                        raw = Some(p);
                    }
                }
            }
        }
        let edge = raw.and_then(|r| deb.feed(&cfg, r, now_ms() - t0));
        let pressed = deb.pressed();
        publish(&shared, &wake, edge, |s| s.pressed = pressed);
        sleep(TICK_MS).await;
    }
    let _ = JsFuture::from(js_midi_close(slot, generation)).await;
    publish(&shared, &wake, Some(false), |s| {
        s.open = false;
        s.pressed = false;
    });
}
