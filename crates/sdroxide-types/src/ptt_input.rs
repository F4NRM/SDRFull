//! A desk microphone or a footswitch that keys the transmitter (issue #636).
//!
//! # What this is for
//!
//! A table microphone's PTT bar, a footswitch, a push button on a cable: none of
//! them is a radio. What they have in common is a contact that closes while the
//! operator wants to talk. The oldest way to read one on a PC is the serial
//! port's own handshake lines: the program raises RTS (or DTR), the switch
//! connects it to CTS, DSR or DCD, and the level that comes back is "the
//! operator is pressing". A USB-RS232 adapter (a Prolific or FTDI chip) does the
//! same through its own driver. The same contact can also arrive as a USB HID
//! report or as a MIDI note.
//!
//! There are two identical slots, so that a station with a microphone on one
//! USB port and a pedal on another has a place for each. Either one keys the
//! transmitter; the transmitter drops when both are released.
//!
//! # Why the vocabulary lives here
//!
//! The settings panel is compiled to wasm for the browser client, and the
//! device is on the machine the radio is attached to, not on the browser's. The
//! hardware half is native-only and lives in `sdroxide-radio`; nothing here
//! touches a port.

use serde::{Deserialize, Serialize};

use crate::{MidiMsg, SenseLine};

/// Where the contact comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PttInputSource {
    /// A serial port (RS-232, or a USB-serial adapter): CTS, DSR or DCD.
    #[default]
    Serial,
    /// A USB HID device: one bit of its input report.
    UsbHid,
    /// A USB MIDI device: one note or controller.
    UsbMidi,
}

impl PttInputSource {
    pub const ALL: [PttInputSource; 3] =
        [PttInputSource::Serial, PttInputSource::UsbHid, PttInputSource::UsbMidi];
    pub fn label(self) -> &'static str {
        match self {
            PttInputSource::Serial => "Serial",
            PttInputSource::UsbHid => "USB HID",
            PttInputSource::UsbMidi => "USB MIDI",
        }
    }
}

/// What the program does with one of its own output lines (RTS, DTR).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LineDrive {
    /// Not touched: whatever the driver left it at.
    #[default]
    Leave,
    /// Held at a positive voltage. The usual supply for a switch wired from RTS
    /// to DSR.
    On,
    /// Held at a negative voltage.
    Off,
}

impl LineDrive {
    pub const ALL: [LineDrive; 3] = [LineDrive::Leave, LineDrive::On, LineDrive::Off];
    pub fn label(self) -> &'static str {
        match self {
            LineDrive::Leave => "Leave",
            LineDrive::On => "On",
            LineDrive::Off => "Off",
        }
    }
}

/// How a press becomes a transmission.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum PttHold {
    /// Transmitting while the contact is closed, the way a microphone bar works.
    #[default]
    Hold,
    /// Each press flips between transmit and receive, the way a footswitch used
    /// for a long over is often wired.
    Toggle,
}

impl PttHold {
    pub const ALL: [PttHold; 2] = [PttHold::Hold, PttHold::Toggle];
    pub fn label(self) -> &'static str {
        match self {
            PttHold::Hold => "Hold",
            PttHold::Toggle => "Toggle",
        }
    }
}

/// One slot's setup. `Default` is off and harmless.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PttInputConfig {
    pub enabled: bool,
    pub source: PttInputSource,
    /// Serial: the port name (`COM3`, `/dev/ttyUSB0`).
    pub port: String,
    /// Serial: which input line is the switch. `Off` reads nothing.
    pub line: SenseLine,
    /// Serial: what RTS does while the port is open.
    pub rts: LineDrive,
    /// Serial: what DTR does while the port is open.
    pub dtr: LineDrive,
    /// Whether a high level (serial), a set bit (HID) or a note-on (MIDI) means
    /// "pressed". An opto-isolated switch usually pulls the line down.
    pub active_high: bool,
    /// Contact bounce, in milliseconds: a change is believed only when it has
    /// stood this long.
    pub debounce_ms: u16,
    pub hold: PttHold,
    /// HID: the device, by the key the station's list gave (a path, opaque).
    pub hid_key: String,
    /// HID: the device's name as it was when it was chosen, shown when it is not
    /// plugged in.
    pub hid_name: String,
    /// HID: which byte of the input report (report id excluded) holds the
    /// switch.
    pub hid_byte: u8,
    /// HID: which bits of that byte. Pressed when `byte & mask != 0` (flipped by
    /// `active_high`).
    pub hid_mask: u8,
    /// MIDI: the port's stable id and name, as `sdroxide-midi` lists them.
    pub midi_port_id: String,
    pub midi_port_name: String,
    /// MIDI: the note or controller that is the switch. A note-on is pressed, a
    /// note-off released; a controller is pressed from value 64 up.
    pub midi_msg: MidiMsg,
}

impl Default for PttInputConfig {
    fn default() -> Self {
        PttInputConfig {
            enabled: false,
            source: PttInputSource::Serial,
            port: String::new(),
            line: SenseLine::Dsr,
            rts: LineDrive::On,
            dtr: LineDrive::Leave,
            active_high: true,
            debounce_ms: 30,
            hold: PttHold::Hold,
            hid_key: String::new(),
            hid_name: String::new(),
            hid_byte: 0,
            hid_mask: 0x01,
            midi_port_id: String::new(),
            midi_port_name: String::new(),
            midi_msg: MidiMsg::default(),
        }
    }
}

/// The station's two slots: the desk microphone, and the pedal or switch.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PttInputsConfig {
    /// "Desk or external microphone".
    pub desk_mic: PttInputConfig,
    /// "Pedal or microphone switch".
    pub pedal: PttInputConfig,
}

impl PttInputsConfig {
    pub fn slot(&self, slot: PttSlot) -> &PttInputConfig {
        match slot {
            PttSlot::DeskMic => &self.desk_mic,
            PttSlot::Pedal => &self.pedal,
        }
    }
    pub fn slot_mut(&mut self, slot: PttSlot) -> &mut PttInputConfig {
        match slot {
            PttSlot::DeskMic => &mut self.desk_mic,
            PttSlot::Pedal => &mut self.pedal,
        }
    }
}

/// Which of the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PttSlot {
    DeskMic,
    Pedal,
}

impl PttSlot {
    pub const ALL: [PttSlot; 2] = [PttSlot::DeskMic, PttSlot::Pedal];
}

/// A HID device the station can see.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PttHidDevice {
    /// What goes in [`PttInputConfig::hid_key`].
    pub key: String,
    pub name: String,
    pub vendor: u16,
    pub product: u16,
}

/// A MIDI input port the station can see.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PttMidiPort {
    pub id: String,
    pub name: String,
}

/// One slot's condition.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PttInputState {
    /// The device is open (serial), or connected (MIDI), or read (HID).
    pub open: bool,
    /// Why it is not, in words for the operator. Empty when nothing is wrong.
    pub error: String,
    /// The serial input lines as last read, for the live indicators.
    pub cts: bool,
    pub dsr: bool,
    pub dcd: bool,
    /// The most recent HID input report, report id excluded.
    pub hid_report: Vec<u8>,
    /// The most recent MIDI message the port sent, for the learn button.
    pub last_midi: Option<MidiMsg>,
    /// How many MIDI controls the port has sent since it opened, so the learn
    /// button sees a second press of the control it saw last.
    pub midi_count: u32,
    /// The operator is pressing right now (after debounce and polarity).
    pub pressed: bool,
}

/// What the station reports: both slots, and what it can see to offer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PttInputsStatus {
    pub desk_mic: PttInputState,
    pub pedal: PttInputState,
    pub serial_ports: Vec<String>,
    pub hid_devices: Vec<PttHidDevice>,
    pub midi_ports: Vec<PttMidiPort>,
}

impl PttInputsStatus {
    pub fn slot(&self, slot: PttSlot) -> &PttInputState {
        match slot {
            PttSlot::DeskMic => &self.desk_mic,
            PttSlot::Pedal => &self.pedal,
        }
    }
}

/// The learn button for HID: which byte and bit moved between two reports.
///
/// Returns the first byte that differs and the bits in it that changed, or
/// `None` when the two are the same. `before` is the report when the operator
/// pressed "learn", `now` the one after they pressed the switch.
pub fn hid_learn(before: &[u8], now: &[u8]) -> Option<(u8, u8)> {
    for (i, (a, b)) in before.iter().zip(now.iter()).enumerate() {
        if a != b {
            return Some((u8::try_from(i).ok()?, a ^ b));
        }
    }
    None
}

/// Whether a HID report says "pressed" under this setup.
pub fn hid_pressed(cfg: &PttInputConfig, report: &[u8]) -> Option<bool> {
    let byte = *report.get(usize::from(cfg.hid_byte))?;
    let set = byte & cfg.hid_mask != 0;
    Some(set == cfg.active_high)
}

/// Whether a serial line level says "pressed" under this setup.
pub fn level_pressed(cfg: &PttInputConfig, level: bool) -> bool {
    level == cfg.active_high
}

/// Debounce one input: a change is believed only after it has stood for the
/// configured time, and a `Toggle` slot flips on each believed press.
///
/// Pure and clocked by the caller, so it is tested without a device.
#[derive(Debug, Clone, Default)]
pub struct Debounce {
    /// What is believed.
    stable: bool,
    /// What was last seen, and when it was first seen.
    seen: bool,
    since_ms: u64,
    /// The latched state of a `Toggle` slot.
    latched: bool,
    primed: bool,
}

impl Debounce {
    /// Feed one raw reading at `now_ms`. Returns the keyed state of the slot
    /// when it changed, `None` otherwise.
    ///
    /// The first reading after opening only primes the filter: a switch that is
    /// already closed when the program starts does not key the transmitter.
    pub fn feed(&mut self, cfg: &PttInputConfig, raw: bool, now_ms: u64) -> Option<bool> {
        if !self.primed {
            self.primed = true;
            self.stable = raw;
            self.seen = raw;
            self.since_ms = now_ms;
            return None;
        }
        if raw != self.seen {
            self.seen = raw;
            self.since_ms = now_ms;
        }
        if self.seen == self.stable {
            return None;
        }
        if now_ms.saturating_sub(self.since_ms) < u64::from(cfg.debounce_ms) {
            return None;
        }
        self.stable = self.seen;
        match cfg.hold {
            PttHold::Hold => Some(self.stable),
            PttHold::Toggle => {
                if self.stable {
                    self.latched = !self.latched;
                    Some(self.latched)
                } else {
                    None
                }
            }
        }
    }

    /// What the operator is pressing, after debounce.
    pub fn pressed(&self) -> bool {
        self.stable
    }

    /// Forget everything: a port was closed or the setup changed.
    pub fn reset(&mut self) {
        *self = Debounce::default();
    }
}

/// How a MIDI message moves the switch, if it is the one configured.
///
/// `Some(true)` pressed, `Some(false)` released, `None` not this control.
pub fn midi_pressed(cfg: &PttInputConfig, msg: &MidiMsg, value: u8, off: bool) -> Option<bool> {
    let want = &cfg.midi_msg;
    if msg.kind != want.kind || msg.data1 != want.data1 {
        return None;
    }
    if want.channel != MidiMsg::ANY_CHANNEL && want.channel != msg.channel {
        return None;
    }
    let on = match msg.kind {
        crate::MidiMsgKind::Note => !off && value > 0,
        _ => value >= 64,
    };
    Some(on == cfg.active_high)
}

/// A raw MIDI message as a browser's Web MIDI delivers it (status byte first),
/// in this crate's terms: the identity, the value, and whether it is a
/// note-off. `None` for anything that is not a note or a controller.
pub fn midi_from_bytes(bytes: &[u8]) -> Option<(MidiMsg, u8, bool)> {
    let status = *bytes.first()?;
    let channel = status & 0x0F;
    let d1 = bytes.get(1).copied().unwrap_or(0);
    let d2 = bytes.get(2).copied().unwrap_or(0);
    let msg = |kind| MidiMsg { kind, channel, data1: d1 };
    match status & 0xF0 {
        0x90 => Some((msg(crate::MidiMsgKind::Note), d2, false)),
        0x80 => Some((msg(crate::MidiMsgKind::Note), d2, true)),
        0xB0 => Some((msg(crate::MidiMsgKind::Cc), d2, false)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(debounce_ms: u16, hold: PttHold) -> PttInputConfig {
        PttInputConfig { debounce_ms, hold, ..PttInputConfig::default() }
    }

    #[test]
    fn a_switch_closed_at_start_does_not_key() {
        let c = cfg(30, PttHold::Hold);
        let mut d = Debounce::default();
        assert_eq!(d.feed(&c, true, 0), None);
        assert_eq!(d.feed(&c, true, 100), None);
        assert_eq!(d.feed(&c, false, 200), None);
        assert_eq!(d.feed(&c, false, 240), Some(false));
    }

    #[test]
    fn bounce_shorter_than_the_debounce_is_ignored() {
        let c = cfg(30, PttHold::Hold);
        let mut d = Debounce::default();
        d.feed(&c, false, 0);
        // 10 ms blip.
        assert_eq!(d.feed(&c, true, 100), None);
        assert_eq!(d.feed(&c, false, 110), None);
        assert_eq!(d.feed(&c, false, 200), None);
        assert!(!d.pressed());
        // A real press.
        assert_eq!(d.feed(&c, true, 300), None);
        assert_eq!(d.feed(&c, true, 331), Some(true));
        assert!(d.pressed());
        assert_eq!(d.feed(&c, false, 400), None);
        assert_eq!(d.feed(&c, false, 431), Some(false));
    }

    #[test]
    fn toggle_flips_on_each_press_and_ignores_the_release() {
        let c = cfg(0, PttHold::Toggle);
        let mut d = Debounce::default();
        d.feed(&c, false, 0);
        assert_eq!(d.feed(&c, true, 10), Some(true));
        assert_eq!(d.feed(&c, false, 20), None);
        assert_eq!(d.feed(&c, true, 30), Some(false));
        assert_eq!(d.feed(&c, false, 40), None);
        assert_eq!(d.feed(&c, true, 50), Some(true));
    }

    #[test]
    fn polarity_decides_what_pressed_means() {
        let high = PttInputConfig { active_high: true, ..PttInputConfig::default() };
        let low = PttInputConfig { active_high: false, ..PttInputConfig::default() };
        assert!(level_pressed(&high, true));
        assert!(!level_pressed(&high, false));
        assert!(level_pressed(&low, false));
        assert!(!level_pressed(&low, true));
    }

    #[test]
    fn a_hid_report_is_read_by_byte_and_mask() {
        let c = PttInputConfig {
            hid_byte: 1,
            hid_mask: 0b0000_0100,
            active_high: true,
            ..PttInputConfig::default()
        };
        assert_eq!(hid_pressed(&c, &[0, 0b100]), Some(true));
        assert_eq!(hid_pressed(&c, &[0xFF, 0b011]), Some(false));
        // A report too short to hold the byte says nothing.
        assert_eq!(hid_pressed(&c, &[0]), None);
    }

    #[test]
    fn the_learn_button_finds_the_byte_and_the_bit_that_moved() {
        assert_eq!(hid_learn(&[0, 0, 0], &[0, 0b10, 0]), Some((1, 0b10)));
        assert_eq!(hid_learn(&[0, 5], &[0, 5]), None);
        assert_eq!(hid_learn(&[1, 0], &[0, 4]), Some((0, 1)));
    }

    #[test]
    fn a_midi_note_presses_and_releases_only_the_chosen_control() {
        let mut c = PttInputConfig::default();
        c.midi_msg = MidiMsg { kind: crate::MidiMsgKind::Note, channel: 2, data1: 60 };
        let on = MidiMsg { kind: crate::MidiMsgKind::Note, channel: 2, data1: 60 };
        assert_eq!(midi_pressed(&c, &on, 100, false), Some(true));
        assert_eq!(midi_pressed(&c, &on, 0, true), Some(false));
        let other = MidiMsg { kind: crate::MidiMsgKind::Note, channel: 2, data1: 61 };
        assert_eq!(midi_pressed(&c, &other, 100, false), None);
        let wrong_channel = MidiMsg { kind: crate::MidiMsgKind::Note, channel: 3, data1: 60 };
        assert_eq!(midi_pressed(&c, &wrong_channel, 100, false), None);
        // A controller presses from 64 up.
        c.midi_msg =
            MidiMsg { kind: crate::MidiMsgKind::Cc, channel: MidiMsg::ANY_CHANNEL, data1: 64 };
        let cc = MidiMsg { kind: crate::MidiMsgKind::Cc, channel: 9, data1: 64 };
        assert_eq!(midi_pressed(&c, &cc, 127, false), Some(true));
        assert_eq!(midi_pressed(&c, &cc, 0, false), Some(false));
    }

    #[test]
    fn the_default_is_off_and_keys_nothing() {
        let c = PttInputsConfig::default();
        assert!(!c.desk_mic.enabled && !c.pedal.enabled);
    }

    #[test]
    fn raw_midi_bytes_become_a_note_a_release_or_a_controller() {
        let (m, v, off) = midi_from_bytes(&[0x93, 60, 0]).unwrap();
        assert_eq!(
            (m.kind, m.channel, m.data1, v, off),
            (crate::MidiMsgKind::Note, 3, 60, 0, false)
        );
        assert!(midi_from_bytes(&[0x80, 60, 64]).unwrap().2);
        assert_eq!(midi_from_bytes(&[0xB0, 7, 100]).unwrap().0.kind, crate::MidiMsgKind::Cc);
        assert!(midi_from_bytes(&[0xF8]).is_none(), "a clock tick is not a control");
        assert!(midi_from_bytes(&[]).is_none());
    }
}
