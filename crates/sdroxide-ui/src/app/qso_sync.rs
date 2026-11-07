//! The logbook on the station, as a browser client sees it.
//!
//! A browser has no filesystem, and what it keeps in the page's storage is
//! capped at a few megabytes, differs from one browser to the next and goes with
//! the browser's data. So the station keeps the logbook (`qso_log.json` on the
//! machine the server runs on) and the browser works on a copy of it: it asks
//! for the log when it connects, and sends back each change as a delta.
//!
//! A native client, local or remote, keeps its own `qso_log.json` and does none
//! of this: [`SdroxideApp::sync_qso_log`] returns at once off the web.
//!
//! The first time a browser connects after this arrived, the log it still holds
//! in its own storage is merged into the station's (the import's own duplicate
//! rule: same call within two minutes is the same contact) and the browser's copy
//! is emptied once it has been sent.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use sdroxide_types::QsoRecord;

use crate::app::SdroxideApp;
use crate::app::net::new_import_records;

/// How many records go in one message. A busy station's log is tens of
/// thousands of contacts, and a first upload may be all of it.
const CHUNK: usize = 500;

/// Set by every change to the logbook (`persist_qso_log`), cleared when the
/// changes have been worked out. A flag rather than a comparison, because a
/// comparison of a full logbook every frame is what the logbook's caches exist
/// to avoid.
pub(in crate::app) static QSO_LOG_DIRTY: AtomicBool = AtomicBool::new(false);

/// What the browser knows the station holds.
#[derive(Default)]
pub(in crate::app) struct QsoSync {
    /// The log as last exchanged with the station, by id.
    mirror: HashMap<u64, QsoRecord>,
    /// The log has been asked for.
    requested: bool,
    /// The station's log has arrived. Until then nothing is sent: a delta
    /// worked out against an empty mirror would be the whole log.
    pub(in crate::app) synced: bool,
}

impl QsoSync {
    /// The station's log arrived. `local` is what the browser held before (its
    /// own storage, the first time): contacts of it the station does not have
    /// are added, and left for [`Self::changes`] to send.
    pub(in crate::app) fn adopt(
        &mut self,
        station: Vec<QsoRecord>,
        local: Vec<QsoRecord>,
    ) -> Vec<QsoRecord> {
        let (fresh, _) = new_import_records(&station, local);
        self.mirror = station.iter().map(|q| (q.id, q.clone())).collect();
        let mut next = station.iter().map(|q| q.id).max().unwrap_or(0) + 1;
        let mut log = station;
        for mut r in fresh {
            r.id = next;
            next += 1;
            log.push(r);
        }
        self.synced = true;
        log
    }

    /// What differs from the station's copy: records to store and ids to
    /// delete. The mirror is brought up to date, the messages being assumed
    /// delivered (the socket is reliable, and a failed save is reported by the
    /// station).
    pub(in crate::app) fn changes(&mut self, log: &[QsoRecord]) -> (Vec<QsoRecord>, Vec<u64>) {
        let mut upsert = Vec::new();
        let mut here = std::collections::HashSet::with_capacity(log.len());
        for q in log {
            here.insert(q.id);
            if self.mirror.get(&q.id) != Some(q) {
                self.mirror.insert(q.id, q.clone());
                upsert.push(q.clone());
            }
        }
        let delete: Vec<u64> = self.mirror.keys().copied().filter(|id| !here.contains(id)).collect();
        for id in &delete {
            self.mirror.remove(id);
        }
        (upsert, delete)
    }
}

impl SdroxideApp {
    /// Once a frame: ask the station for its logbook the first time, then send
    /// whatever changed since. Does nothing outside the browser.
    pub(in crate::app) fn sync_qso_log(&mut self) {
        if !cfg!(target_arch = "wasm32") {
            return;
        }
        if !self.qso_sync.requested {
            self.qso_sync.requested = true;
            self.ctrl.request_qso_log();
        }
        if !self.qso_sync.synced || !QSO_LOG_DIRTY.swap(false, Ordering::Relaxed) {
            return;
        }
        let (upsert, delete) = self.qso_sync.changes(&self.qso_log);
        if upsert.is_empty() && delete.is_empty() {
            return;
        }
        // The deletes go with the first chunk, so a record edited and its
        // neighbour deleted arrive in the order they were made.
        let mut delete = Some(delete);
        for chunk in upsert.chunks(CHUNK) {
            self.ctrl.send_qso_log_apply(chunk.to_vec(), delete.take().unwrap_or_default());
        }
        if let Some(delete) = delete {
            self.ctrl.send_qso_log_apply(Vec::new(), delete);
        }
    }

    /// The station's logbook arrived: show it, with what this browser held.
    pub(in crate::app) fn adopt_station_qso_log(&mut self, station: Vec<QsoRecord>) {
        let local = std::mem::take(&mut self.qso_log);
        self.qso_log = self.qso_sync.adopt(station, local);
        self.log_content_changed();
        // Whatever the station did not have is now in the log and not in the
        // mirror: let the next frame send it.
        QSO_LOG_DIRTY.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(id: u64, call: &str, start: i64) -> QsoRecord {
        QsoRecord { id, call: call.into(), start_utc: start, ..Default::default() }
    }

    #[test]
    fn a_log_that_has_just_arrived_has_nothing_to_send() {
        let mut s = QsoSync::default();
        let log = s.adopt(vec![q(1, "W1AW", 100), q(2, "EA1AB", 200)], Vec::new());
        assert!(s.synced);
        assert_eq!(s.changes(&log), (Vec::new(), Vec::new()));
    }

    #[test]
    fn a_change_is_sent_once() {
        let mut s = QsoSync::default();
        let mut log = s.adopt(vec![q(1, "W1AW", 100), q(2, "EA1AB", 200)], Vec::new());
        log[1].comment = "edited".into();
        log.push(q(3, "F4ABC", 300));
        log.remove(0);
        let (up, del) = s.changes(&log);
        assert_eq!(up.iter().map(|r| r.id).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(del, [1]);
        assert_eq!(s.changes(&log), (Vec::new(), Vec::new()), "and not again");
    }

    #[test]
    fn what_the_browser_kept_is_merged_and_then_sent() {
        let mut s = QsoSync::default();
        // The browser's own copy has the station's contact (renumbered, as two
        // copies of one log are) and one the station never heard of.
        let local = vec![q(1, "W1AW", 130), q(2, "F4ABC", 900)];
        let log = s.adopt(vec![q(1, "W1AW", 100)], local);
        assert_eq!(log.len(), 2, "the duplicate within two minutes is not added twice");
        let (up, del) = s.changes(&log);
        assert_eq!(up.len(), 1);
        assert_eq!(up[0].call, "F4ABC");
        assert_eq!(up[0].id, 2, "numbered after the station's own");
        assert!(del.is_empty());
    }
}
