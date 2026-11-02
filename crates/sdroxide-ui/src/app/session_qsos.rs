//! SESSION QSOs: the QSOs logged during this session, in a sortable table.
//!
//! A detachable tool window (see `TW_SESSION_QSOS`). It lists the session's
//! successful QSOs (73 / RR73 / RRR, or any manually logged contact) and
//! lets the operator sort by any column and export ADIF / TXT / CSV, with or
//! without the CW contacts.

use crate::i18n::tr;
use eframe::egui::{self, RichText};
use sdroxide_types::{Command, QsoRecord};

use crate::app::SdroxideApp;
use crate::chrome;
use crate::download;



/// Sort keys for the SESSION QSOs table. The integer is persisted in
/// `UiSettings.session_qso_sort`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::app) enum SessionQsoSort {
    Time = 0,
    Call = 1,
    Grid = 2,
    Band = 3,
    Mode = 4,
    RstSent = 5,
    RstRcvd = 6,
    Country = 7,
    CqZone = 8,
    Distance = 9,
    Final = 10,
}

impl SessionQsoSort {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Call,
            2 => Self::Grid,
            3 => Self::Band,
            4 => Self::Mode,
            5 => Self::RstSent,
            6 => Self::RstRcvd,
            7 => Self::Country,
            8 => Self::CqZone,
            9 => Self::Distance,
            10 => Self::Final,
            _ => Self::Time,
        }
    }

    pub fn label(self) -> String {
        match self {
            Self::Time => tr!("sessionqso-time"),
            Self::Call => tr!("sessionqso-call"),
            Self::Grid => tr!("sessionqso-grid"),
            Self::Band => tr!("sessionqso-band"),
            Self::Mode => tr!("sessionqso-mode"),
            Self::RstSent => tr!("sessionqso-rst-s"),
            Self::RstRcvd => tr!("sessionqso-rst-r"),
            Self::Country => tr!("sessionqso-country"),
            Self::CqZone => "CQ".to_string(),
            Self::Distance => tr!("sessionqso-dist"),
            Self::Final => tr!("sessionqso-final"),
        }
    }
}

impl SdroxideApp {
    pub(in crate::app) fn session_qsos_window(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        let open = self.tool_window(
            ctx,
            "session-qsos",
            &tr!("settings-tool-window-session-qsos"),
            [960.0, 620.0],
            self.show_session_qsos,
            |me, ui| me.session_qsos_body(ctx, ui, cmds),
        );
        self.show_session_qsos = open;
        if open {
            self.ui_settings.tool_windows_open |= crate::app::frame::TW_SESSION_QSOS;
        } else {
            self.ui_settings.tool_windows_open &= !crate::app::frame::TW_SESSION_QSOS;
        }
        crate::app::persist::persist_ui_settings(&self.ui_settings);
    }

    fn session_qsos_body(&mut self, _ctx: &egui::Context, ui: &mut egui::Ui, _cmds: &mut Vec<Command>) {
        crate::chrome::window_body_bg(ui);

        // Filter: session + successful.
        let mut rows: Vec<QsoRecord> = self
            .qso_log
            .iter()
            .filter(|q| self.session_qso_ids.contains(&q.id))
            .filter(|q| {
                let f = q.final_msg.to_lowercase();
                f.is_empty() || f.contains("73") || f.contains("rrr")
            })
            .cloned()
            .collect();

        // CW toggle for export.
        let mut include_cw = true;
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr!("sessionqso-count", n = rows.len())).size(11.0).color(crate::theme::gray(150)));
            ui.separator();
            chrome::checkbox(ui, &mut include_cw, &tr!("sessionqso-include-cw"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let have = !rows.is_empty();
                ui.add_enabled_ui(have, |ui| {
                    if chrome::chip(ui, false, "ADIF").clicked() {
                        let filtered: Vec<QsoRecord> = rows
                            .iter()
                            .filter(|q| include_cw || q.mode.to_uppercase() != "CW")
                            .cloned()
                            .collect();
                        let adif = sdroxide_types::qso_log_to_adif(&filtered);
                        download::save("session-qsos.adi", adif.as_bytes());
                    }
                    if chrome::chip(ui, false, "TXT").clicked() {
                        let filtered: Vec<QsoRecord> = rows
                            .iter()
                            .filter(|q| include_cw || q.mode.to_uppercase() != "CW")
                            .cloned()
                            .collect();
                        let txt = sdroxide_types::qso_log_to_text(&filtered);
                        download::save("session-qsos.txt", txt.as_bytes());
                    }
                    if chrome::chip(ui, false, "CSV").clicked() {
                        let filtered: Vec<QsoRecord> = rows
                            .iter()
                            .filter(|q| include_cw || q.mode.to_uppercase() != "CW")
                            .cloned()
                            .collect();
                        let csv = sdroxide_types::qso_log_to_csv(&filtered);
                        download::save("session-qsos.csv", csv.as_bytes());
                    }
                });
            });
        });
        ui.separator();

        // Sort.
        let sort = SessionQsoSort::from_u8(self.ui_settings.session_qso_sort);
        let desc = self.ui_settings.session_qso_sort_desc;
        let my_grid = self.my_grid();
        rows.sort_by(|a, b| {
            let cmp = match sort {
                SessionQsoSort::Time => a.start_utc.cmp(&b.start_utc),
                SessionQsoSort::Call => a.call.to_lowercase().cmp(&b.call.to_lowercase()),
                SessionQsoSort::Grid => a.grid.clone().unwrap_or_default().to_lowercase()
                    .cmp(&b.grid.clone().unwrap_or_default().to_lowercase()),
                SessionQsoSort::Band => a.band.to_lowercase().cmp(&b.band.to_lowercase()),
                SessionQsoSort::Mode => a.mode.to_lowercase().cmp(&b.mode.to_lowercase()),
                SessionQsoSort::RstSent => a.rst_sent.unwrap_or(0).cmp(&b.rst_sent.unwrap_or(0)),
                SessionQsoSort::RstRcvd => a.rst_rcvd.unwrap_or(0).cmp(&b.rst_rcvd.unwrap_or(0)),
                SessionQsoSort::Country => a.country.to_lowercase().cmp(&b.country.to_lowercase()),
                SessionQsoSort::CqZone => a.cq_zone.unwrap_or(0).cmp(&b.cq_zone.unwrap_or(0)),
                SessionQsoSort::Distance => {
                    let da = sdroxide_types::grid_distance_km(&my_grid, a.grid.as_deref().unwrap_or(""))
                        .unwrap_or(0.0);
                    let db = sdroxide_types::grid_distance_km(&my_grid, b.grid.as_deref().unwrap_or(""))
                        .unwrap_or(0.0);
                    da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                }
                SessionQsoSort::Final => a.final_msg.to_lowercase().cmp(&b.final_msg.to_lowercase()),
            };
            if desc { cmp.reverse() } else { cmp }
        });

        // Header row.
        ui.horizontal(|ui| {
            for (i, key) in [
                SessionQsoSort::Time,
                SessionQsoSort::Call,
                SessionQsoSort::Grid,
                SessionQsoSort::Band,
                SessionQsoSort::Mode,
                SessionQsoSort::RstSent,
                SessionQsoSort::RstRcvd,
                SessionQsoSort::Country,
                SessionQsoSort::CqZone,
                SessionQsoSort::Distance,
                SessionQsoSort::Final,
            ]
            .iter()
            .enumerate()
            {
                let active = sort == *key;
                let arrow = if active {
                    if desc { " v" } else { " ^" }
                } else {
                    ""
                };
                let label = format!("{}{}", key.label(), arrow);
                if ui.button(RichText::new(label).size(11.0).strong()).clicked() {
                    if active {
                        self.ui_settings.session_qso_sort_desc = !self.ui_settings.session_qso_sort_desc;
                    } else {
                        self.ui_settings.session_qso_sort = *key as u8;
                        self.ui_settings.session_qso_sort_desc = true;
                    }
                    crate::app::persist::persist_ui_settings(&self.ui_settings);
                }
                if i < 10 {
                    ui.add_space(4.0);
                }
            }
        });
        ui.separator();

        // Rows.
        let row_h = 18.0f32;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                for r in &rows[range] {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format_time(r.start_utc)).size(11.0));
                        ui.label(RichText::new(&r.call).size(11.0));
                        ui.label(RichText::new(r.grid.clone().unwrap_or_default()).size(11.0));
                        ui.label(RichText::new(&r.band).size(11.0));
                        ui.label(RichText::new(&r.mode).size(11.0));
                        ui.label(RichText::new(r.rst_sent.map(|v| v.to_string()).unwrap_or_default()).size(11.0));
                        ui.label(RichText::new(r.rst_rcvd.map(|v| v.to_string()).unwrap_or_default()).size(11.0));
                        ui.label(RichText::new(&r.country).size(11.0));
                        ui.label(RichText::new(r.cq_zone.map(|v| v.to_string()).unwrap_or_default()).size(11.0));
                        let dist = sdroxide_types::grid_distance_km(&my_grid, r.grid.as_deref().unwrap_or(""))
                            .map(|v| format!("{:.0}", v))
                            .unwrap_or_default();
                        ui.label(RichText::new(dist).size(11.0));
                        ui.label(RichText::new(if r.final_msg.is_empty() { "—".into() } else { r.final_msg.clone() }).size(11.0));
                    });
                }
            });

    }
}

fn format_time(unix: i64) -> String {
    let (y, mo, d, h, mi, _) = sdroxide_types::utc_ymd_hms(unix);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}")
}