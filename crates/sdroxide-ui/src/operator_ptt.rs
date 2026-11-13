//! The desk microphone and pedal of the computer the operator sits at.
//!
//! The switch is on the operator's desk, so this computer reads it, and the
//! over goes to whichever station is on screen: local or a far one reached
//! over `--connect` or from a browser, through the same `Command::SetPtt` every
//! other source uses (issue #636). The station keeps its own, separate pair for
//! a microphone that is wired to it (Settings → T/R switch).
//!
//! Two ways to read the switch, one set of rules. The native client opens the
//! device through the operating system (`sdroxide-pttin`) and keeps the choices
//! in `operator_ptt.json`; the browser reads it through Web Serial, WebHID and
//! Web MIDI (`operator_ptt_web`) and the server keeps the choices, per login.
//! What a press does once it is read is the same code for both, below.

use sdroxide_types::{Command, PttInputSource, PttInputsConfig, PttInputsStatus, RadioState};

/// How long after we asked to key a state that still says "receiving" means the
/// station refused, so the release has nothing of ours to undo.
const REFUSED_AFTER_S: f64 = 1.0;

/// Whether the person has to allow each device to the page (a browser).
pub const NEEDS_GRANT: bool = cfg!(target_arch = "wasm32");

#[cfg(not(target_arch = "wasm32"))]
mod backend {
    use super::*;
    use sdroxide_pttin::PttInputsHub;
    use std::sync::{Arc, OnceLock};

    /// What the hub's single owner is called; there is only the interface.
    const OWNER: u32 = 0;

    /// One hub for the window: every radio tab is a whole app, and two hubs
    /// would fight over the same serial port. Only the focused tab asks it for
    /// changes, so the press goes to the station on screen.
    static HUB: OnceLock<PttInputsHub> = OnceLock::new();

    /// A unit-test build must not read the operator's configuration or open
    /// their devices.
    fn load() -> PttInputsConfig {
        if cfg!(test) {
            PttInputsConfig::default()
        } else {
            sdroxide_config::load_operator_ptt_config()
        }
    }

    #[derive(Clone)]
    pub struct Backend(&'static PttInputsHub);

    impl Backend {
        pub fn new(ctx: &eframe::egui::Context) -> Self {
            Backend(HUB.get_or_init(|| {
                let ctx = ctx.clone();
                let hub = PttInputsHub::with_wake(Arc::new(move || crate::repaint::animate(&ctx)));
                hub.configure(OWNER, &load());
                hub
            }))
        }

        /// What the setup was when the program started.
        pub fn initial(&self) -> PttInputsConfig {
            load()
        }

        /// Keep the choices where this kind of client keeps them.
        pub fn save(&self, cfg: &PttInputsConfig) {
            if let Err(e) = sdroxide_config::save_operator_ptt_config(cfg) {
                tracing::warn!("operator_ptt.json: {e}");
            }
        }

        pub fn configure(&self, cfg: &PttInputsConfig) {
            self.0.configure(OWNER, cfg);
        }
        pub fn rescan(&self) {
            self.0.rescan();
        }
        pub fn status(&self) -> PttInputsStatus {
            self.0.status()
        }
        pub fn take_change(&self) -> Option<bool> {
            self.0.take_change(OWNER)
        }
        /// A native client needs no permission from anybody.
        pub fn grant(&self, _kind: PttInputSource) {}
    }
}

#[cfg(target_arch = "wasm32")]
mod backend {
    use super::*;
    use crate::operator_ptt_web::{WebHub, hub};
    use std::rc::Rc;

    #[derive(Clone)]
    pub struct Backend(WebHub);

    impl Backend {
        pub fn new(ctx: &eframe::egui::Context) -> Self {
            let ctx = ctx.clone();
            Backend(hub(Rc::new(move || crate::repaint::animate(&ctx))))
        }

        /// Nothing until the server says what it keeps for this login.
        pub fn initial(&self) -> PttInputsConfig {
            PttInputsConfig::default()
        }

        /// The server keeps it (`send_operator_ptt`), not the browser.
        pub fn save(&self, _cfg: &PttInputsConfig) {}

        pub fn configure(&self, cfg: &PttInputsConfig) {
            self.0.configure(cfg);
        }
        pub fn rescan(&self) {
            self.0.rescan();
        }
        pub fn status(&self) -> PttInputsStatus {
            self.0.status()
        }
        pub fn take_change(&self) -> Option<bool> {
            self.0.take_change()
        }
        pub fn grant(&self, kind: PttInputSource) {
            self.0.grant(kind);
        }
    }
}

pub struct OperatorPtt {
    backend: backend::Backend,
    /// What was last applied; the dialog edits its own copy.
    pub cfg: PttInputsConfig,
    /// When this tab keyed its station (the frame clock, in seconds: a browser
    /// has no `std::time::Instant`), while it holds it keyed.
    keyed_at: Option<f64>,
    /// The station was seen transmitting since `keyed_at`.
    seen_on: bool,
}

impl OperatorPtt {
    pub fn new(ctx: &eframe::egui::Context) -> Self {
        let backend = backend::Backend::new(ctx);
        let cfg = backend.initial();
        OperatorPtt { backend, cfg, keyed_at: None, seen_on: false }
    }

    /// The operator pressed APPLY: keep the setup and open (or close) the
    /// devices.
    pub fn apply(&mut self, cfg: &PttInputsConfig) {
        self.backend.save(cfg);
        self.cfg = cfg.clone();
        self.backend.configure(cfg);
    }

    /// The server told a browser what it keeps for this login: open that. A
    /// native client keeps its own file and ignores it.
    pub fn adopt(&mut self, cfg: &PttInputsConfig) {
        if NEEDS_GRANT {
            self.cfg = cfg.clone();
            self.backend.configure(cfg);
        }
    }

    pub fn rescan(&self) {
        self.backend.rescan();
    }

    /// Ask the browser to allow one more device. Does nothing natively.
    pub fn grant(&self, kind: PttInputSource) {
        self.backend.grant(kind);
    }

    pub fn status(&self) -> PttInputsStatus {
        self.backend.status()
    }

    /// Once a frame: key or release the station this tab shows.
    ///
    /// The guards are the engine's own for a station-side switch: a press while
    /// something else is transmitting or tuning is ignored, and a release only
    /// unkeys what this switch keyed. A tab that stops being the focused one
    /// lets go of what it keyed, so a station left behind is not left
    /// transmitting.
    pub fn poll(&mut self, focused: bool, now: f64, state: &RadioState, cmds: &mut Vec<Command>) {
        if !focused {
            if self.keyed_at.take().is_some() {
                cmds.push(Command::SetPtt(false));
            }
            return;
        }
        if let Some(on) = self.backend.take_change() {
            if on {
                if !(state.tx.ptt || state.tx.tune) {
                    cmds.push(Command::SetPtt(true));
                    self.keyed_at = Some(now);
                    self.seen_on = false;
                }
            } else if self.keyed_at.take().is_some() {
                cmds.push(Command::SetPtt(false));
            }
        }
        if let Some(at) = self.keyed_at {
            // Whatever ended the over (a refusal, the operator, a timeout), the
            // next release has nothing of ours to undo.
            if state.tx.ptt {
                self.seen_on = true;
            } else if self.seen_on || now - at > REFUSED_AFTER_S {
                self.keyed_at = None;
            }
        }
    }
}
