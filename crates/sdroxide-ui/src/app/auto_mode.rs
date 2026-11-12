//! Auto mode — the app-side half of the unattended FT8/FT4/FT2 sequencer.
//!
//! The policy (which station to answer, whether the mode and band allow it) is
//! pure and lives in [`sdroxide_types::auto`]. This is the loop that runs it
//! once a frame: it keeps the operator's input clock, honours the transmit
//! watchdog as a pause rather than a stop, and puts the chosen action on the
//! command channel to the engine.
//!
//! It runs app-wide rather than inside the FT8 panel for a reason: an
//! unattended run must not depend on which pane or tab is on screen. The engine
//! does the sequencing; this only ever starts the next contact.

use crate::i18n::tr;
use sdroxide_types::auto;
use sdroxide_types::{Command, QsoStep};

use super::SdroxideApp;

impl SdroxideApp {
    /// Whether this station may key the band the dial is on: the one rule of
    /// the operator's transmit band list (ticked and validated), the same the
    /// engine's key-down gate applies.
    ///
    /// The engine still has the final say at key-down — the device's own range,
    /// the offset against the band plan — but this is the question auto mode
    /// has to answer before it offers to arm at all.
    pub(in crate::app) fn auto_tx_permitted(&self) -> bool {
        self.radio_cfg.as_ref().is_some_and(|c| c.tx_dial_allowed(self.state.rx_freq_hz()))
    }

    /// The inactivity stop for this radio, in seconds, from its own setting,
    /// or `None` when it is switched off (a PlutoSDR set to DISABLE).
    fn auto_idle_stop_s(&self) -> Option<f64> {
        let (min, long) = self
            .radio_cfg
            .as_ref()
            .map_or((0, false), |c| (c.auto_idle_stop_min, auto::has_long_idle_stop(c.backend)));
        auto::auto_idle_stop_s(min, long)
    }

    /// Run auto mode for this frame. Cheap and a no-op unless it is armed.
    pub(in crate::app) fn tick_auto_mode(
        &mut self,
        ctx: &eframe::egui::Context,
        now: f64,
        cmds: &mut Vec<Command>,
    ) {
        // Any input event at all counts as the operator being present. A
        // minimized or unfocused window produces none, which is exactly what
        // the inactivity stop is for.
        if !ctx.input(|i| i.events.is_empty()) {
            self.auto_last_activity = now;
        }
        let Some(role) = self.auto_role else {
            return;
        };
        // The engine's own view of the mode, band and watchdog, not the UI's
        // editable copies: this is what will actually key.
        let Some((mode, step, watchdog, tx_watchdog, status_tx_next, my_call, my_grid)) =
            self.digi_status.as_ref().map(|s| {
                (
                    s.mode,
                    s.step,
                    s.config.tx_watchdog_min,
                    s.tx_watchdog,
                    s.tx_next,
                    s.config.my_call.clone(),
                    s.config.my_grid.clone(),
                )
            })
        else {
            return;
        };
        // **Count the CQ calls of an Auto CQ run from what actually went out.**
        // The sequencer repeats the CQ by itself, so one command is many
        // transmissions; the exchange log is the record of them. Observed every
        // frame, because the run has to be counted whether or not the loop is
        // free to act on it this second.
        let cq_sent = if let Some(s) = self.digi_status.as_ref() {
            self.auto_cq.observe(s.step, &s.transcript)
        } else {
            0
        };
        let tx_permitted = self.auto_tx_permitted();
        if let Some(reason) = auto::auto_block_reason(mode, watchdog, tx_permitted) {
            self.stop_auto(tr!("auto-stopped-reason", reason = reason), cmds);
            return;
        }
        if let Some(stop_s) = self.auto_idle_stop_s()
            && now - self.auto_last_activity >= stop_s
        {
            let mins = (stop_s / 60.0).round() as u32;
            self.stop_auto(tr!("auto-stopped-idle", mins = mins), cmds);
            return;
        }
        // Only start a contact from a state that is genuinely free. `Idle` is
        // the obvious one; `Confirming` is the engine holding a *logged*
        // contact for a few minutes in case the far end repeats its final
        // message, and waiting out that hold would idle an unattended run for
        // five minutes a contact. It is free once no re-send is owed — while
        // one is, the engine is about to transmit and must be left to it.
        let free = step == QsoStep::Idle || (step == QsoStep::Confirming && !status_tx_next);
        if !free {
            self.auto_resume_at = None;
            return;
        }
        if now < self.auto_cooldown_until {
            return;
        }
        // The transmit watchdog is the pacing, not the end: after it trips,
        // wait one watchdog span and try again.
        if tx_watchdog {
            let span = f64::from(watchdog) * 60.0;
            let resume = *self.auto_resume_at.get_or_insert(now + span);
            if now < resume {
                return;
            }
        }
        self.auto_resume_at = None;

        // **The decision is a pure function of the facts** (`auto::plan`), so
        // what happens next can be read, and tested, without an engine in the
        // room: a caller first, then — in the roles that answer them — a station
        // calling CQ, and CQ ourselves only when neither exists.
        let caller_target = self.auto_caller(&my_call);
        let cq_target = self.auto_target(&my_call, &my_grid);
        let facts = auto::AutoFacts {
            caller: caller_target.is_some(),
            cq_candidate: cq_target.is_some(),
            cq_calls: cq_sent,
            repeats: self.digi_cfg_edit.auto_cq_repeats,
            then_listen: self.digi_cfg_edit.auto_cq_then_listen,
            session_min: self.digi_cfg_edit.auto_session_min,
            session_s: now - self.auto_armed_at,
        };
        let start = |me: &mut Self, t: auto::AutoTarget, cmds: &mut Vec<Command>| {
            me.auto_tried.insert(t.call.to_ascii_uppercase());
            // **A call is counted when it is made**, and the quota sets a delay
            // rather than a permanent exclusion (`auto::after_call`), so a station
            // that was silent earlier is a candidate again later.
            let now_unix = crate::time::now_unix() as f64;
            let key = t.call.to_ascii_uppercase();
            let rec = me.auto_attempts.get(&key).copied().unwrap_or((0, 0.0));
            me.auto_attempts
                .insert(key, auto::after_call(rec, me.digi_cfg_edit.auto_call_retries, now_unix));
            let label = me.auto_role.map_or("auto", |r| r.label());
            me.auto_note = tr!("auto-answering", role = label, call = &t.call);
            cmds.push(Command::DigiStartQso {
                from: t.call,
                grid: t.grid,
                snr: t.snr_db,
                audio_hz: t.audio_hz,
                wait_for_cq: false,
            });
        };
        match auto::plan(role, &facts) {
            auto::AutoStep::AnswerCaller => match caller_target {
                Some(t) => start(self, t, cmds),
                None => self.auto_note = tr!("auto-caller-went-away"),
            },
            auto::AutoStep::AnswerCq => match cq_target {
                Some(t) => start(self, t, cmds),
                None => self.auto_note = tr!("auto-nobody-left"),
            },
            auto::AutoStep::CallCq => {
                self.auto_note = tr!("auto-calling-cq", role = role.label(), n = cq_sent + 1);
                cmds.push(Command::DigiCallCq);
            }
            auto::AutoStep::Listen => {
                // The run has made the calls the operator allowed. In Auto CQ
                // that hands over to Auto Rx rather than going quiet — the
                // station is still watching, and a CQ that finally gets answered
                // is picked up by the listening half. In the other roles it is
                // simply "nothing to do": answer what turns up, key nothing.
                if role == auto::AutoRole::Cq {
                    let limit = self.digi_cfg_edit.auto_cq_repeats;
                    self.auto_role = Some(auto::AutoRole::Rx);
                    self.auto_cq = auto::CqCounter::default();
                    self.auto_note = tr!("auto-cq-no-answer", limit = limit);
                } else {
                    self.auto_note = tr!("auto-listening", role = role.label());
                }
            }
            auto::AutoStep::Stop(reason) => {
                let limit = match reason {
                    auto::StopReason::CqLimit => u32::from(self.digi_cfg_edit.auto_cq_repeats),
                    auto::StopReason::Session => u32::from(self.digi_cfg_edit.auto_session_min),
                };
                let sentence = reason.sentence(limit);
                self.stop_auto(
                    tr!("auto-role-stopped", role = role.label(), sentence = sentence),
                    cmds,
                );
                return;
            }
        }
        self.auto_cooldown_until = now + auto::AUTO_COOLDOWN_S;
    }

    /// Keep the log index current and run the pure selection against it.
    ///
    /// The index is borrowed directly rather than cloned: it is the whole log
    /// as sets, and cloning it every frame would be the one expensive thing in
    /// this path.
    ///
    /// The ignore list is asked again here even though the ingress filter
    /// already drops new decodes, because a row that was in the list *before*
    /// the operator muted that station is still in it. Muting is an
    /// instruction not to answer somebody, and auto mode answers on its own
    /// initiative — so it honours the list for a decode it would otherwise
    /// pick out of the ones already held.
    ///
    /// The list is borrowed, not copied: the rule is handed to the picker as a
    /// predicate so it can drop those rows in place. An earlier cut built a
    /// filtered `Vec` of the whole decode list instead, which meant cloning up
    /// to 200 rows every frame for the sake of the few that were muted.
    /// Take a station that is calling **us**, if there is one.
    ///
    /// This is the pending-caller queue issue #236 asks for: IU5HIU calling
    /// while we are finishing with SP3JHZ used to be a line in the panel saying
    /// "also calling" and nothing more. Here it is the next contact, so the run
    /// goes finish the QSO → work whoever called → and only then call CQ again.
    ///
    /// Nobody is worked twice a session and the mute list is honoured, the same
    /// two rules [`Self::auto_target`] keeps. A station that never comes back is
    /// dropped by [`QsoMachine`]'s own give-up count, so this needs no separate
    /// retry budget: it is asked again only while it is still calling.
    ///
    /// [`QsoMachine`]: sdroxide_digi::qso::QsoMachine
    fn auto_caller(&mut self, my_call: &str) -> Option<auto::AutoTarget> {
        let len = self.qso_log.len();
        if self.log_index_cache.as_ref().map(|(l, _)| *l) != Some(len) {
            self.log_index_cache = Some((len, sdroxide_types::LogIndex::build(&self.qso_log)));
        }
        let ix = &self.log_index_cache.as_ref().expect("just filled").1;
        // **The two rules that make a queue a queue rather than a loop**: a
        // station already called its budget of times is passed over, and one
        // whose latest traffic is addressed to somebody else is left to finish
        // — it becomes eligible again by itself once that traffic is old, which
        // is the deferral, with `BUSY_DEFER_S` as the timeout.
        let (budget, attempts, decodes) =
            (self.digi_cfg_edit.auto_call_retries, self.auto_attempts.clone(), &self.digi_decodes);
        let now = crate::time::now_unix();
        let mine = my_call.to_string();
        let now_f = now as f64;
        let skip = |d: &sdroxide_types::Decode| -> bool {
            if let Some(from) = d.from.as_deref()
                && let Some(rec) = attempts.get(&from.to_ascii_uppercase())
                && auto::retry_holds(*rec, budget, now_f)
            {
                return true;
            }
            if let Some(from) = d.from.as_deref()
                && auto::looks_busy(decodes, from, &mine, now, auto::BUSY_DEFER_S as i64)
            {
                return true;
            }
            crate::app::ignore::is_ignored(&self.session_ignored, d.from.as_deref())
        };
        auto::pick_caller(&self.digi_decodes, my_call, ix, &self.auto_tried, skip)
    }

    fn auto_target(&mut self, my_call: &str, my_grid: &str) -> Option<auto::AutoTarget> {
        let len = self.qso_log.len();
        if self.log_index_cache.as_ref().map(|(l, _)| *l) != Some(len) {
            self.log_index_cache = Some((len, sdroxide_types::LogIndex::build(&self.qso_log)));
        }
        let ix = &self.log_index_cache.as_ref().expect("just filled").1;
        let budget = self.digi_cfg_edit.auto_call_retries;
        let attempts = self.auto_attempts.clone();
        let mine = my_call.to_string();
        let now = crate::time::now_unix();
        let decodes = &self.digi_decodes;
        let now_f = now as f64;
        let busy_or_exhausted = |d: &sdroxide_types::Decode| -> bool {
            if let Some(from) = d.from.as_deref()
                && let Some(rec) = attempts.get(&from.to_ascii_uppercase())
                && auto::retry_holds(*rec, budget, now_f)
            {
                return true;
            }
            d.from.as_deref().is_some_and(|from| {
                auto::looks_busy(decodes, from, &mine, now, auto::BUSY_DEFER_S as i64)
            })
        };
        // The same two rules as a caller: a station already called its budget of
        // times, or one whose latest traffic is with somebody else, is passed
        // over — and becomes eligible again when that traffic goes stale.
        let ignored = &self.session_ignored;
        auto::pick_cq(&self.digi_decodes, ix, my_call, my_grid, &self.auto_tried, |d| {
            busy_or_exhausted(d) || crate::app::ignore::is_ignored(ignored, d.from.as_deref())
        })
    }

    /// Pending callers for the queue display: stations calling us that are
    /// eligible to be worked next, in priority order (best SNR first).
    pub(in crate::app) fn pending_callers(&self, my_call: &str) -> Vec<String> {
        if my_call.trim().is_empty() || self.digi_decodes.is_empty() {
            return Vec::new();
        }
        let mine = my_call.to_ascii_uppercase();
        let budget = self.digi_cfg_edit.auto_call_retries;
        let attempts = &self.auto_attempts;
        let now = crate::time::now_unix();
        let now_f = now as f64;
        let decodes = &self.digi_decodes;
        let mut callers: Vec<(String, i16, i64)> = Vec::new();
        for d in decodes {
            let Some(to_field) = d.to.as_deref() else { continue };
            if !to_field.eq_ignore_ascii_case(&mine) {
                continue;
            }
            let Some(from) = d.from.as_deref().filter(|c| !c.is_empty()) else { continue };
            if from.eq_ignore_ascii_case(&mine) {
                continue;
            }
            let key = from.to_ascii_uppercase();
            if self.auto_tried.contains(&key) {
                continue;
            }
            if let Some(rec) = attempts.get(&key)
                && auto::retry_holds(*rec, budget, now_f)
            {
                continue;
            }
            if auto::looks_busy(decodes, from, &mine, now, auto::BUSY_DEFER_S as i64) {
                continue;
            }
            callers.push((from.to_string(), d.snr_db, d.slot_utc));
        }
        // Sort by SNR descending, then by slot_utc descending (newest first)
        callers.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));
        // Deduplicate by callsign (keep best SNR)
        let mut seen = std::collections::HashSet::new();
        callers.retain(|c| seen.insert(c.0.to_ascii_uppercase()));
        callers.into_iter().map(|c| c.0).collect()
    }

    /// Disarm auto mode, leaving `note` as the reason shown on the toggle.
    pub(in crate::app) fn stop_auto(&mut self, note: String, cmds: &mut Vec<Command>) {
        if self.auto_role.is_some() {
            cmds.push(Command::DigiStopQso);
            cmds.push(Command::DigiAbortTx);
        }
        self.disarm_auto(note);
    }

    /// Clear auto mode without sending anything, for the case where the stop
    /// command is already on its way (STOP QSO, STOP TX, a bound Abort TX).
    pub(in crate::app) fn disarm_auto(&mut self, note: String) {
        if self.auto_role.is_some() {
            self.auto_role = None;
            self.auto_cq = auto::CqCounter::default();
            self.auto_resume_at = None;
            self.auto_note = note;
        }
    }

    /// Arm one of the two roles now. Returns `false` (doing nothing) when it may
    /// not run.
    pub(in crate::app) fn arm_auto(&mut self, now: f64, role: auto::AutoRole) -> bool {
        let Some(s) = self.digi_status.as_ref() else { return false };
        let tx_permitted = self.auto_tx_permitted();
        if auto::auto_block_reason(s.mode, s.config.tx_watchdog_min, tx_permitted).is_some() {
            return false;
        }
        self.begin_auto_session(now, role);
        true
    }

    /// **A fresh auto session**, from a clean slate.
    ///
    /// What the last run tried says nothing about this one. A station that did
    /// not answer — and one whose contact broke off before the log — is left
    /// alone for the rest of *that* run and is a candidate again in the next:
    /// propagation does not turn in a few minutes, so calling it again minutes
    /// later is chatter on the band and no contact, while the run that follows
    /// may well hear it. The log is still what "worked" means, so a station in
    /// it is never answered however clean the slate is.
    ///
    /// Apart from the gate above so this half can be read — and tested — without
    /// an engine, a mode and a transmit permission in the room.
    fn begin_auto_session(&mut self, now: f64, role: auto::AutoRole) {
        // The blotter: stations this run has already called. It survived the
        // end of the last run, which is why a station that did not answer was
        // never tried again for the life of the program.
        self.auto_tried.clear();
        self.auto_attempts.clear();
        self.auto_role = Some(role);
        self.auto_cq = auto::CqCounter::default();
        self.auto_last_activity = now;
        self.auto_cooldown_until = 0.0;
        self.auto_resume_at = None;
        self.auto_armed_at = now;
        self.auto_note = match role {
            auto::AutoRole::Rx => tr!("auto-rx-listening"),
            auto::AutoRole::Cq => tr!("auto-cq-calling"),
            auto::AutoRole::Continuous => tr!("auto-cont-running"),
        };
    }
}

#[cfg(test)]
mod auto_session_tests {
    //! What an auto session remembers and what the next one forgets.

    use std::cell::RefCell;
    use std::rc::Rc;

    use eframe::egui;
    use sdroxide_types::{Command, Decode, RadioController, RadioEvent, auto};

    use super::*;

    /// Keeps every command the app sends, and answers nothing: these tests read
    /// the decision, not the engine.
    struct Sent(Rc<RefCell<Vec<Command>>>);

    impl RadioController for Sent {
        fn send(&mut self, cmd: Command) {
            self.0.borrow_mut().push(cmd);
        }
        fn poll_event(&mut self) -> Option<RadioEvent> {
            None
        }
    }

    fn app() -> SdroxideApp {
        let dir =
            std::env::temp_dir().join(format!("sdroxide-auto-session-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("SDROXIDE_CONFIG_DIR", &dir) };
        let ctx = egui::Context::default();
        SdroxideApp::new_tab(
            &ctx,
            None,
            None,
            Box::new(Sent(Rc::new(RefCell::new(Vec::new())))),
            0,
            true,
        )
    }

    fn cq(from: &str) -> Decode {
        Decode {
            slot_utc: 1,
            snr_db: -7,
            dt: 0.0,
            audio_hz: 1200.0,
            message: format!("CQ {from} FN42"),
            to: None,
            from: Some(from.to_string()),
            grid: Some("FN42".to_string()),
            is_cq: true,
            cq_to: None,
            free_text: false,
            rr73_to: None,
        }
    }

    /// The reported fault: a station auto called once — and did not answer, or
    /// broke off before the log — was left alone for the life of the program,
    /// because the blotter of "tried this session" was never emptied.
    #[test]
    fn a_station_tried_in_the_last_session_is_a_candidate_in_the_next() {
        let _guard = crate::multi::frame_test_lock();
        let mut app = app();
        app.digi_decodes.push(cq("K1ABC"));
        app.auto_tried.insert("K1ABC".to_string());

        assert!(
            app.auto_target("MM0ABC", "JO22").is_none(),
            "the run that tried it leaves it alone: it did not answer"
        );

        app.begin_auto_session(0.0, auto::AutoRole::Rx);
        let picked = app.auto_target("MM0ABC", "JO22").map(|t| t.call);
        assert_eq!(picked.as_deref(), Some("K1ABC"), "a new session picks it up again");
    }

    /// And what a new session forgets is the blotter and the call budget, not
    /// the log: a station already worked stays out, which is what "worked"
    /// means.
    #[test]
    fn a_new_session_still_never_answers_a_station_already_in_the_log() {
        let _guard = crate::multi::frame_test_lock();
        let mut app = app();
        app.digi_decodes.push(cq("K1ABC"));
        app.auto_tried.insert("K1ABC".to_string());
        app.auto_attempts.insert("K1ABC".to_string(), (2, crate::time::now_unix() as f64 + 600.0));
        app.begin_auto_session(0.0, auto::AutoRole::Rx);

        assert!(app.auto_tried.is_empty(), "the blotter is clean");
        assert!(app.auto_attempts.is_empty(), "and so is the call budget");
        assert_eq!(
            app.auto_target("MM0ABC", "JO22").map(|t| t.call).as_deref(),
            Some("K1ABC"),
            "nothing in the log, so it is a candidate again"
        );

        // Log it: now it is worked, and no session start changes that.
        app.qso_log.push(sdroxide_types::QsoRecord {
            call: "K1ABC".to_string(),
            mode: "FT8".to_string(),
            ..Default::default()
        });
        app.begin_auto_session(1.0, auto::AutoRole::Rx);
        assert!(
            app.auto_target("MM0ABC", "JO22").is_none(),
            "worked is worked: the log keeps it out"
        );
    }
}
