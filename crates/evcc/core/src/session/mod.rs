//! What every machine reports to its host (the events, the steps, the notices, the states, the reasons a session ends) and, with
//! the `iso20` feature, the ISO 15118-20 session machine.

mod types;
#[cfg(feature = "iso20")]
pub use types::{AcdpConnection, AcdpSystemStatus};
pub use types::{AcChargeLoopStatus, AcdpPosition, Buffers, ChargeLoopStatus, MeterReading, ControlPilot, EndReason, Event, Notice, Resume, State, Step};

#[cfg(feature = "iso20")]
mod machine;
#[cfg(feature = "iso20")]
pub use machine::*;
