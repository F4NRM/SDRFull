//! The desk microphone and the pedal that key the transmitter (issue #636):
//! two identical blocks under the MIDI controller in Settings > Controls.
//!
//! The devices are on the station's machine, so a remote screen edits a copy
//! and APPLY sends it; the station saves it and opens the hardware. Nothing
//! here touches a port.

use crate::i18n::{Tr, tr};
use eframe::egui::{self, Color32, ComboBox, RichText};
use sdroxide_types::{
    LineDrive, PttHold, PttInputConfig, PttInputSource, PttInputState, PttInputsConfig,
    PttInputsStatus, PttSlot, SenseLine,
};

use crate::chrome::StyledCombo;

const GOOD: Color32 = Color32::from_rgb(90, 200, 110);
const BAD: Color32 = Color32::from_rgb(230, 90, 80);

/// What the learn buttons are waiting for: the HID report as it was when the
/// button was pressed, and the MIDI message count at that moment.
#[derive(Default, Clone)]
pub(in crate::app) struct PttLearn {
    hid: [Option<Vec<u8>>; 2],
    midi: [Option<u32>; 2],
}

/// What the Controls tab hands the section.
pub(in crate::app) struct PttView<'a> {
    pub edit: &'a mut PttInputsConfig,
    pub seeded: bool,
    pub status: &'a PttInputsStatus,
    pub learn: &'a mut PttLearn,
    pub apply: &'a mut bool,
    pub rescan: &'a mut bool,
}

pub(in crate::app) fn settings_ptt_inputs_section(ui: &mut egui::Ui, v: PttView<'_>) {
    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);
    if !v.seeded {
        ui.label(
            RichText::new(tr!("ptt-desk-title")).size(14.0).strong().color(crate::theme::CYAN()),
        );
        super::note(ui, &tr!("ptt-waiting"));
        return;
    }
    super::note(ui, &tr!("ptt-intro"));
    for (i, slot) in PttSlot::ALL.into_iter().enumerate() {
        if i > 0 {
            ui.add_space(10.0);
            ui.separator();
        }
        ui.add_space(6.0);
        let title = match slot {
            PttSlot::DeskMic => tr!("ptt-desk-title"),
            PttSlot::Pedal => tr!("ptt-pedal-title"),
        };
        ui.label(RichText::new(title).size(14.0).strong().color(crate::theme::CYAN()));
        ui.add_space(4.0);
        slot_block(ui, slot, i, v.edit.slot_mut(slot), v.status, v.learn, v.rescan);
    }
    ui.add_space(8.0);
    if crate::chrome::chip_accent(
        ui,
        false,
        RichText::new(tr!("settings-relay-apply")).strong(),
        crate::theme::GREEN(),
        crate::theme::INK_ON_CYAN(),
    )
    .on_hover_text(tr!("ptt-apply-hint"))
    .clicked()
    {
        *v.apply = true;
    }
}

fn slot_block(
    ui: &mut egui::Ui,
    slot: PttSlot,
    i: usize,
    cfg: &mut PttInputConfig,
    status: &PttInputsStatus,
    learn: &mut PttLearn,
    rescan: &mut bool,
) {
    let st = status.slot(slot);
    crate::chrome::checkbox(ui, &mut cfg.enabled, tr!("settings-controls-enable"));
    ui.add_enabled_ui(cfg.enabled, |ui| {
        egui::Grid::new(("ptt-grid", i))
            .num_columns(2)
            .spacing([12.0, 6.0])
            .min_row_height(ui.spacing().interact_size.y)
            .show(ui, |ui| {
                ui.label(tr!("ptt-source"));
                ComboBox::from_id_salt(("ptt-source", i))
                    .width(200.0)
                    .selected_text(cfg.source.tr())
                    .show_styled(ui, |ui| {
                        for s in PttInputSource::ALL {
                            if ui.selectable_label(cfg.source == s, s.tr()).clicked() {
                                cfg.source = s;
                            }
                        }
                    });
                ui.end_row();

                match cfg.source {
                    PttInputSource::Serial => serial_rows(ui, i, cfg, status),
                    PttInputSource::UsbHid => hid_rows(ui, i, cfg, status, st, learn),
                    PttInputSource::UsbMidi => midi_rows(ui, i, cfg, status, st, learn),
                }

                ui.label(tr!("ptt-active-high"));
                ui.checkbox(&mut cfg.active_high, tr!("ptt-active-high-label"))
                    .on_hover_text(tr!("ptt-active-high-hint"));
                ui.end_row();

                ui.label(tr!("ptt-debounce"));
                ui.add(egui::DragValue::new(&mut cfg.debounce_ms).range(0..=500).suffix(" ms"));
                ui.end_row();

                ui.label(tr!("ptt-hold"));
                ComboBox::from_id_salt(("ptt-hold", i))
                    .width(200.0)
                    .selected_text(cfg.hold.tr())
                    .show_styled(ui, |ui| {
                        for h in PttHold::ALL {
                            if ui.selectable_label(cfg.hold == h, h.tr()).clicked() {
                                cfg.hold = h;
                            }
                        }
                    });
                ui.end_row();
            });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if crate::chrome::chip(ui, false, tr!("settings-controls-rescan-ports")).clicked() {
                *rescan = true;
            }
            state_line(ui, st);
        });
    });
}

fn serial_rows(ui: &mut egui::Ui, i: usize, cfg: &mut PttInputConfig, status: &PttInputsStatus) {
    ui.label(tr!("settings-relay-port"));
    let shown = if cfg.port.is_empty() { tr!("settings-controls-none") } else { cfg.port.clone() };
    ComboBox::from_id_salt(("ptt-port", i)).width(260.0).selected_text(shown).show_styled(
        ui,
        |ui| {
            if ui.selectable_label(cfg.port.is_empty(), tr!("settings-controls-none")).clicked() {
                cfg.port.clear();
            }
            // A saved port that is not plugged in stays choosable by name.
            let mut ports = status.serial_ports.clone();
            if !cfg.port.is_empty() && !ports.contains(&cfg.port) {
                ports.push(cfg.port.clone());
            }
            for p in ports {
                if ui.selectable_label(cfg.port == p, &p).clicked() {
                    cfg.port = p;
                }
            }
        },
    );
    ui.end_row();

    ui.label(tr!("ptt-input-line"));
    ComboBox::from_id_salt(("ptt-line", i)).width(260.0).selected_text(cfg.line.tr()).show_styled(
        ui,
        |ui| {
            for l in [SenseLine::Cts, SenseLine::Dsr, SenseLine::Dcd, SenseLine::Off] {
                if ui.selectable_label(cfg.line == l, l.tr()).clicked() {
                    cfg.line = l;
                }
            }
        },
    );
    ui.end_row();

    for (label, id, value) in
        [(tr!("ptt-rts"), "rts", &mut cfg.rts), (tr!("ptt-dtr"), "dtr", &mut cfg.dtr)]
    {
        ui.label(label);
        ComboBox::from_id_salt(("ptt-drive", id, i))
            .width(200.0)
            .selected_text(value.tr())
            .show_styled(ui, |ui| {
                for d in LineDrive::ALL {
                    if ui.selectable_label(*value == d, d.tr()).clicked() {
                        *value = d;
                    }
                }
            });
        ui.end_row();
    }
}

fn hid_rows(
    ui: &mut egui::Ui,
    i: usize,
    cfg: &mut PttInputConfig,
    status: &PttInputsStatus,
    st: &PttInputState,
    learn: &mut PttLearn,
) {
    ui.label(tr!("ptt-hid-device"));
    let shown = if cfg.hid_key.is_empty() {
        tr!("settings-controls-none")
    } else if cfg.hid_name.is_empty() {
        cfg.hid_key.clone()
    } else {
        cfg.hid_name.clone()
    };
    ComboBox::from_id_salt(("ptt-hid", i)).width(320.0).selected_text(shown).show_styled(
        ui,
        |ui| {
            if ui.selectable_label(cfg.hid_key.is_empty(), tr!("settings-controls-none")).clicked()
            {
                cfg.hid_key.clear();
                cfg.hid_name.clear();
            }
            for d in &status.hid_devices {
                let text = format!("{} ({:04x}:{:04x})", d.name, d.vendor, d.product);
                if ui.selectable_label(cfg.hid_key == d.key, text).clicked() {
                    cfg.hid_key = d.key.clone();
                    cfg.hid_name = d.name.clone();
                    learn.hid[i] = None;
                }
            }
        },
    );
    ui.end_row();

    ui.label(tr!("ptt-hid-switch"));
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(tr!("ptt-hid-byte-bits", byte = cfg.hid_byte, mask = cfg.hid_mask));
            let learning = learn.hid[i].is_some();
            let text = if learning { tr!("ptt-learning") } else { tr!("ptt-learn") };
            if crate::chrome::chip(ui, learning, text).clicked() {
                learn.hid[i] = if learning { None } else { Some(st.hid_report.clone()) };
            }
        });
        // The learn button: the first byte and bits that moved since it was
        // pressed are the switch.
        if let Some(before) = &learn.hid[i] {
            if let Some((byte, mask)) = sdroxide_types::hid_learn(before, &st.hid_report) {
                cfg.hid_byte = byte;
                cfg.hid_mask = mask;
                learn.hid[i] = None;
            }
        }
        let report = if st.hid_report.is_empty() {
            tr!("ptt-hid-no-report")
        } else {
            let hex: Vec<String> = st.hid_report.iter().map(|b| format!("{b:02x}")).collect();
            tr!("ptt-hid-report", bytes = hex.join(" "))
        };
        ui.label(RichText::new(report).small().weak());
    });
    ui.end_row();
}

fn midi_rows(
    ui: &mut egui::Ui,
    i: usize,
    cfg: &mut PttInputConfig,
    status: &PttInputsStatus,
    st: &PttInputState,
    learn: &mut PttLearn,
) {
    ui.label(tr!("ptt-midi-port"));
    let shown = if cfg.midi_port_name.is_empty() {
        tr!("settings-controls-none")
    } else {
        cfg.midi_port_name.clone()
    };
    ComboBox::from_id_salt(("ptt-midi", i)).width(320.0).selected_text(shown).show_styled(
        ui,
        |ui| {
            if ui
                .selectable_label(cfg.midi_port_name.is_empty(), tr!("settings-controls-none"))
                .clicked()
            {
                cfg.midi_port_id.clear();
                cfg.midi_port_name.clear();
            }
            for p in &status.midi_ports {
                if ui.selectable_label(cfg.midi_port_name == p.name, &p.name).clicked() {
                    cfg.midi_port_id = p.id.clone();
                    cfg.midi_port_name = p.name.clone();
                }
            }
        },
    );
    ui.end_row();

    ui.label(tr!("ptt-midi-control"));
    ui.horizontal(|ui| {
        ui.label(super::controls::midi_msg_label(&cfg.midi_msg));
        let learning = learn.midi[i].is_some();
        let text = if learning { tr!("ptt-learning") } else { tr!("ptt-learn") };
        if crate::chrome::chip(ui, learning, text).clicked() {
            learn.midi[i] = if learning { None } else { Some(st.midi_count) };
        }
    });
    if let (Some(base), Some(msg)) = (learn.midi[i], st.last_midi) {
        if st.midi_count != base {
            cfg.midi_msg = msg;
            learn.midi[i] = None;
        }
    }
    ui.end_row();
}

/// The live line under a block: open or not, the lines as read, pressed or not.
fn state_line(ui: &mut egui::Ui, st: &PttInputState) {
    if st.open {
        ui.label(RichText::new(format!("● {}", tr!("ptt-open"))).color(GOOD));
        for (line, on) in
            [(SenseLine::Cts, st.cts), (SenseLine::Dsr, st.dsr), (SenseLine::Dcd, st.dcd)]
        {
            ui.label(
                RichText::new(format!("{} {}", line_name(line), if on { "1" } else { "0" }))
                    .small(),
            );
        }
        let (text, color) = if st.pressed {
            (tr!("ptt-pressed"), BAD)
        } else {
            (tr!("ptt-released"), ui.visuals().weak_text_color())
        };
        ui.label(RichText::new(text).strong().color(color));
    } else if !st.error.is_empty() {
        ui.label(RichText::new(&st.error).color(BAD));
    } else {
        ui.label(RichText::new(tr!("ptt-not-open")).weak());
    }
}

/// CTS, DSR, DCD: the names of the lines, the same in every language.
fn line_name(line: SenseLine) -> &'static str {
    match line {
        SenseLine::Cts => "CTS",
        SenseLine::Dsr => "DSR",
        SenseLine::Dcd => "DCD",
        SenseLine::Off => "",
    }
}
