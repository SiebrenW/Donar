//! Sans-IO EV communication controller core.
//!
//! Nothing here touches a socket or a clock. A host adapter feeds events in and carries
//! out the returned actions, including arming and cancelling the single timer a machine
//! uses, so the machines never need to be polled.
#![no_std]

#[cfg(test)]
extern crate std;

pub mod sdp;

/// What the host should do with the machine's one timer after a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimerCmd {
    /// Leave the timer as it is.
    #[default]
    Keep,
    /// (Re)arm the timer to fire in `ms` milliseconds, replacing any pending one.
    Arm { ms: u32 },
    Cancel,
}
