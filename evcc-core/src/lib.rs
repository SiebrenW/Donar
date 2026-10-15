//! Sans-IO EV communication controller core.
//!
//! Nothing here touches a socket or a clock. A host adapter feeds events in and carries
//! out the returned actions, including arming and cancelling the machine's timers, so the
//! machines never need to be polled.
#![no_std]

#[cfg(test)]
extern crate std;

pub mod ev;
pub mod log;
pub mod sdp;
pub mod session;

/// The timers the machines use. Each is independent; the host keeps at most one pending
/// expiry per timer and reports it back as an event naming the timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timer {
    /// Interval between SDP requests.
    Sdp,
    /// How long to wait for the response to the message just sent.
    Response,
    /// Upper bound on the time spent in a repeated `Ongoing` exchange (e.g. authorization).
    Ongoing,
    /// Pause before repeating a request while the EVSE answers `Ongoing`.
    Poll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerCmd {
    /// (Re)arm `timer` to fire in `ms` milliseconds, replacing any pending expiry.
    Arm { timer: Timer, ms: u32 },
    Cancel(Timer),
}

/// The timer commands of one step (at most two: e.g. arm the response timer and the
/// ongoing timer, or cancel both). Timers not mentioned keep running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timers {
    cmds: [Option<TimerCmd>; 2],
}

impl Timers {
    pub const NONE: Timers = Timers { cmds: [None, None] };

    pub const fn one(cmd: TimerCmd) -> Self {
        Self { cmds: [Some(cmd), None] }
    }

    /// Adds a command. A step never needs more than two; a third would be a bug in the
    /// machine, so it panics in debug builds and is dropped in release builds.
    pub fn with(mut self, cmd: TimerCmd) -> Self {
        match self.cmds.iter_mut().find(|c| c.is_none()) {
            Some(slot) => *slot = Some(cmd),
            None => debug_assert!(false, "more than two timer commands in one step"),
        }
        self
    }

    pub fn iter(&self) -> impl Iterator<Item = TimerCmd> + '_ {
        self.cmds.iter().flatten().copied()
    }

    pub fn is_empty(&self) -> bool {
        self.cmds[0].is_none()
    }
}
