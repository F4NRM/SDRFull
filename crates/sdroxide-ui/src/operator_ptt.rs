//! The desk microphone and pedal of the computer the operator sits at.
//!
//! The switch is on the operator's desk, so this computer reads it, and the
//! over goes to whichever station is on screen: local or a far one reached
//! over `--connect`, through the same `Command::SetPtt` every other source uses
//! (issue #636). The station keeps its own, separate pair for a microphone that
//! is wired to it (Settings → T/R switch).
//!
//! Native only: a browser reaches a serial port, a HID device or a MIDI port
//! through Web Serial, WebHID and Web MIDI, which are not wired up yet. The web
//! build gets the same API as a stand-in that never keys.

use sdroxide_types::{Command, PttInputsConfig, PttInputsStatus, RadioState};

/// Whether this build can read the operator's switch at all.
pub const AVAILABLE: bool = cfg!(not(target_arch = "wasm32"));

#[cfg(not(target_arch = "wasm32"))]
pub use native::OperatorPtt;
#[cfg(target_arch = "wasm32")]
pub use stub::OperatorPtt;

/// What the hub's single owner is called; there is only the interface.
#[cfg(not(target_arch = "wasm32"))]
const OWNER: u32 = 0;

/// A slot that cannot hold the transmitter for ever by itself: how long after
/// we asked to key a state that still says "receiving" means the station
/// refused, so the release has nothing of ours to undo.
#[cfg(not(target_arch = "wasm32"))]
const REFUSED_AFTER: std::time::Duration = std::time::Duration::from_secs(1);

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use sdroxide_pttin::PttInputsHub;
    use std::sync::{Arc, OnceLock};
    use std::time::Instant;

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

    fn hub(ctx: &eframe::egui::Context) -> &'static PttInputsHub {
        HUB.get_or_init(|| {
            let ctx = ctx.clone();
            let hub = PttInputsHub::with_wake(Arc::new(move || crate::repaint::animate(&ctx)));
            hub.configure(OWNER, &load());
            hub
        })
    }

    pub struct OperatorPtt {
        hub: &'static PttInputsHub,
        /// What the file holds; the dialog edits its own copy.
        pub cfg: PttInputsConfig,
        /// When this tab keyed its station, while it holds it keyed.
        keyed_at: Option<Instant>,
        /// The station was seen transmitting since `keyed_at`.
        seen_on: bool,
    }

    impl OperatorPtt {
        pub fn new(ctx: &eframe::egui::Context) -> Self {
            OperatorPtt { hub: hub(ctx), cfg: load(), keyed_at: None, seen_on: false }
        }

        /// Save the setup and open (or close) the devices.
        pub fn apply(&mut self, cfg: &PttInputsConfig) {
            if let Err(e) = sdroxide_config::save_operator_ptt_config(cfg) {
                tracing::warn!("operator_ptt.json: {e}");
            }
            self.cfg = cfg.clone();
            self.hub.configure(OWNER, cfg);
        }

        pub fn rescan(&self) {
            self.hub.rescan();
        }

        pub fn status(&self) -> PttInputsStatus {
            self.hub.status()
        }

        /// Once a frame: key or release the station this tab shows.
        ///
        /// The guards are the engine's own for a station-side switch: a press
        /// while something else is transmitting or tuning is ignored, and a
        /// release only unkeys what this switch keyed. A tab that stops being
        /// the focused one lets go of what it keyed, so a station left behind
        /// is not left transmitting.
        pub fn poll(&mut self, focused: bool, state: &RadioState, cmds: &mut Vec<Command>) {
            if !focused {
                if self.keyed_at.take().is_some() {
                    cmds.push(Command::SetPtt(false));
                }
                return;
            }
            if let Some(on) = self.hub.take_change(OWNER) {
                if on {
                    if !(state.tx.ptt || state.tx.tune) {
                        cmds.push(Command::SetPtt(true));
                        self.keyed_at = Some(Instant::now());
                        self.seen_on = false;
                    }
                } else if self.keyed_at.take().is_some() {
                    cmds.push(Command::SetPtt(false));
                }
            }
            if let Some(at) = self.keyed_at {
                // Whatever ended the over (a refusal, the operator, a timeout),
                // the next release has nothing of ours to undo.
                if state.tx.ptt {
                    self.seen_on = true;
                } else if self.seen_on || at.elapsed() > REFUSED_AFTER {
                    self.keyed_at = None;
                }
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod stub {
    use super::*;

    #[derive(Default)]
    pub struct OperatorPtt {
        pub cfg: PttInputsConfig,
    }

    impl OperatorPtt {
        pub fn new(_ctx: &eframe::egui::Context) -> Self {
            Self::default()
        }
        pub fn apply(&mut self, cfg: &PttInputsConfig) {
            self.cfg = cfg.clone();
        }
        pub fn rescan(&self) {}
        pub fn status(&self) -> PttInputsStatus {
            PttInputsStatus::default()
        }
        pub fn poll(&mut self, _focused: bool, _state: &RadioState, _cmds: &mut Vec<Command>) {}
    }
}
