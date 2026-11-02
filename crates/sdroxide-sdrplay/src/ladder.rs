//! The LNA ladder as the hardware reports it, and the split of one total gain
//! reduction between the LNA and the IF.
//!
//! An RSP has two knobs: the LNA, a step index whose size in dB depends on the
//! model and the band, and the IF, a reduction in 1 dB steps. Nothing on this
//! side knows what a given LNA state is worth, so the ladder is *learned*: the
//! GainChange event publishes the LNA's reduction for the state now in force,
//! and each state seen is remembered. States not yet seen are estimated from
//! their neighbours, and the estimate is replaced by the measurement the first
//! time the state is used, after which the caller re-splits.
//!
//! Pure data and arithmetic, so it can be tested without a radio.

use std::collections::BTreeMap;

/// What one LNA state is guessed to be worth when nothing has been measured
/// at all. The ladders in the API tables run 3–7 dB a state.
pub const DEFAULT_STEP_DB: f64 = 6.0;

#[derive(Debug, Clone, Default)]
pub struct LnaLadder {
    /// State → the LNA's gain reduction in dB, as measured. State 0 is always
    /// zero by definition and never stored.
    measured: BTreeMap<u8, i32>,
}

impl LnaLadder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything: the table changes with the band, the port and the
    /// HDR switch.
    pub fn clear(&mut self) {
        self.measured.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.measured.is_empty()
    }

    /// Record a measurement. Returns whether it changed what the ladder
    /// answers (so the caller knows to split again). A reading that would
    /// make the ladder run backwards is a stale event, not a fact, and is
    /// refused.
    pub fn learn(&mut self, state: u8, gr_db: i32) -> bool {
        if state == 0 {
            return false;
        }
        if gr_db < 0 {
            return false;
        }
        let below_ok = self.measured.range(..state).next_back().is_none_or(|(_, &g)| gr_db >= g);
        let above_ok = self.measured.range(state + 1..).next().is_none_or(|(_, &g)| gr_db <= g);
        if !(below_ok && above_ok) {
            return false;
        }
        self.measured.insert(state, gr_db) != Some(gr_db)
    }

    /// The LNA's reduction at `state`, measured if known, estimated if not.
    pub fn gr_at(&self, state: u8) -> i32 {
        if state == 0 {
            return 0;
        }
        if let Some(&g) = self.measured.get(&state) {
            return g;
        }
        let lower =
            self.measured.range(..state).next_back().map(|(&s, &g)| (s, g)).unwrap_or((0, 0));
        match self.measured.range(state + 1..).next().map(|(&s, &g)| (s, g)) {
            Some((hs, hg)) => {
                let (ls, lg) = lower;
                let t = f64::from(state - ls) / f64::from(hs - ls);
                (f64::from(lg) + t * f64::from(hg - lg)).round() as i32
            }
            None => {
                let (ls, lg) = lower;
                let slope = if ls > 0 { f64::from(lg) / f64::from(ls) } else { DEFAULT_STEP_DB };
                (f64::from(lg) + slope * f64::from(state - ls)).round() as i32
            }
        }
    }

    /// The largest total reduction the ladder and the IF can reach together.
    pub fn max_reduction(&self, max_state: u8, if_max: i32) -> i32 {
        self.gr_at(max_state) + if_max
    }

    /// Split a total reduction into `(LNA state, IF reduction)`.
    ///
    /// The LNA stays at the highest gain (lowest state) the IF can make up
    /// for: the LNA sets the noise figure, so it is the last thing given up.
    /// A request out of reach is clamped to the nearest end.
    pub fn split(&self, total: i32, max_state: u8, if_min: i32, if_max: i32) -> (u8, i32) {
        for s in 0..=max_state {
            let r = total - self.gr_at(s);
            if r <= if_max {
                return (s, r.max(if_min));
            }
        }
        (max_state, if_max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ladder() -> LnaLadder {
        // RSP1A-like: 0, 6, 12, 18, 24, 30, 36, 42 dB.
        let mut l = LnaLadder::new();
        for s in 1..=7u8 {
            l.learn(s, i32::from(s) * 6);
        }
        l
    }

    #[test]
    fn state_zero_is_free_and_never_stored() {
        let mut l = LnaLadder::new();
        assert_eq!(l.gr_at(0), 0);
        assert!(!l.learn(0, 5));
        assert!(l.is_empty());
    }

    #[test]
    fn a_measured_state_is_returned_as_measured() {
        let l = ladder();
        assert_eq!(l.gr_at(3), 18);
        assert_eq!(l.gr_at(7), 42);
    }

    #[test]
    fn nothing_measured_uses_the_default_step() {
        let l = LnaLadder::new();
        assert_eq!(l.gr_at(2), 12);
    }

    #[test]
    fn unmeasured_states_are_interpolated_then_extrapolated() {
        let mut l = LnaLadder::new();
        l.learn(2, 8);
        l.learn(6, 24);
        assert_eq!(l.gr_at(4), 16, "between two measurements");
        assert_eq!(l.gr_at(1), 4, "below the first: from state 0");
        assert_eq!(l.gr_at(8), 32, "above the last: the mean slope so far");
    }

    #[test]
    fn learning_reports_change_and_refuses_a_backwards_reading() {
        let mut l = LnaLadder::new();
        assert!(l.learn(3, 18));
        assert!(!l.learn(3, 18), "same reading, nothing to redo");
        assert!(l.learn(3, 17), "a different reading moves the ladder");
        assert!(!l.learn(4, 10), "state 4 cannot be worth less than state 3");
        assert!(!l.learn(2, 30), "state 2 cannot be worth more than state 3");
        assert!(!l.learn(5, -1));
    }

    #[test]
    fn clear_forgets_the_band() {
        let mut l = ladder();
        l.clear();
        assert!(l.is_empty());
    }

    #[test]
    fn the_lna_is_the_last_thing_given_up() {
        let l = ladder();
        // 20..59 IF: anything up to 59 dB is the IF's alone.
        assert_eq!(l.split(20, 7, 20, 59), (0, 20));
        assert_eq!(l.split(43, 7, 20, 59), (0, 43));
        assert_eq!(l.split(59, 7, 20, 59), (0, 59));
        // One dB more and the IF is out of range: the first LNA state that
        // brings it back.
        assert_eq!(l.split(60, 7, 20, 59), (1, 54));
        assert_eq!(l.split(100, 7, 20, 59), (7, 58));
    }

    #[test]
    fn every_total_is_reachable_to_the_decibel() {
        let l = ladder();
        for total in 20..=l.max_reduction(7, 59) {
            let (s, if_gr) = l.split(total, 7, 20, 59);
            assert!((20..=59).contains(&if_gr), "IF {if_gr} out of range at {total}");
            assert_eq!(l.gr_at(s) + if_gr, total, "split of {total} does not add up");
        }
    }

    #[test]
    fn the_split_is_monotonic_in_the_lna() {
        let l = ladder();
        let mut last = 0;
        for total in 20..=101 {
            let (s, _) = l.split(total, 7, 20, 59);
            assert!(s >= last, "the LNA went back at {total}");
            last = s;
        }
    }

    #[test]
    fn out_of_reach_requests_are_clamped() {
        let l = ladder();
        assert_eq!(l.split(0, 7, 20, 59), (0, 20), "more gain than there is");
        assert_eq!(l.split(500, 7, 20, 59), (7, 59), "less gain than there is");
        // The extended range opens the IF down to 0.
        assert_eq!(l.split(0, 7, 0, 59), (0, 0));
    }

    #[test]
    fn the_ladder_length_follows_the_band() {
        let l = ladder();
        assert_eq!(l.split(80, 3, 20, 59), (3, 59), "a short ladder stops at its last state");
    }
}
