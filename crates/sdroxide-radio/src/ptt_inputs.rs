//! The station's desk microphone and pedal (issue #636). The readers and the
//! hub live in `sdroxide-pttin`, which the operator's own computer shares; this
//! file is the station's one process-wide instance.
//!
//! A client is attached to one radio's session, so its `SetPttInputs` reaches
//! that radio and no other. The devices belong to the station, not to a radio:
//! whichever engine is told takes them over, like the T/R switch, and the other
//! engines leave them alone.

use std::sync::OnceLock;

pub use sdroxide_pttin::PttInputsHub;

static HUB: OnceLock<PttInputsHub> = OnceLock::new();

/// The one hub of the process.
pub fn hub() -> &'static PttInputsHub {
    HUB.get_or_init(PttInputsHub::new)
}
