//! Sans-IO EVSE (SECC) side of ISO 15118-20.
//!
//! The counterpart of `evcc-core`: nothing here touches a socket, a clock or a thread. A host
//! adapter feeds events in ([`Event`]) and carries out what comes back ([`Step`]); the charger itself
//! (its limits, its meter, its authorization, its contactors) is the host's implementation of
//! [`Secc`]. The session machine ([`session::Session`]) does what the standard fixes: the message
//! sequence, the response codes of Tables 224 and 226, the multiplexed side streams, V2G20-459 and
//! V2G20-460 for what comes out of turn.
//!
//! Host contract:
//! The buffers ([`Buffers`]) are yours and the same for every call, so the session and its buffers can sit
//! in one struct.
//!
//! 1. Accept a connection, then send [`Event::Connected`].
//! 2. Read from the socket into [`session::Session::rx_space`] of your `rx` buffer, then send
//!    [`Event::Received`] with the byte count.
//! 3. After every call write `tx[..send]` (your `tx` buffer) to the stream, apply the timer command, and close the
//!    connection when [`Step::close`] is set (after writing).
//! 4. When the armed timer fires, send [`Event::TimerExpired`].
#![no_std]

pub mod secc;
pub mod session;

pub use secc::{Action, DcLoopAnswer, AcLoopAnswer, Progress, Secc};
pub use session::{Buffers, Session};

/// The one timer of the machine: `V2G_SECC_Sequence_Timeout` (Table 215), the time the charger waits
/// for the next request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timer {
    Sequence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerCmd {
    /// (Re)arm the timer to fire in `ms` milliseconds, replacing a pending expiry.
    Arm { timer: Timer, ms: u32 },
    Cancel(Timer),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// A connection was accepted. `secured` is true for TLS.
    Connected { secured: bool },
    /// `n` bytes were written into [`Session::rx_space`].
    Received(usize),
    TimerExpired(Timer),
}

/// What the host does after a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Step {
    /// Length of the frames at the start of the TX buffer to write to the stream.
    pub send: usize,
    pub timer: Option<TimerCmd>,
    /// Close the connection (after writing `send`).
    pub close: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    AwaitSupportedAppProtocolReq,
    AwaitSessionSetupReq,
    AwaitAuthorizationSetupReq,
    AwaitAuthorizationReq,
    AwaitServiceDiscoveryReq,
    AwaitServiceDetailReq,
    AwaitServiceSelectionReq,
    AwaitChargeParameterDiscoveryReq,
    AwaitScheduleExchangeReq,
    AwaitCableCheckReq,
    AwaitPreChargeReq,
    AwaitPowerDeliveryReq,
    /// Energy flows; charge loop requests and the stop are accepted.
    ChargeLoop,
    /// Welding detection after the stop (V2G20-1448); ends with a `Finished` request.
    AwaitWeldingReq,
    AwaitSessionStopReq,
    /// The session was stopped properly.
    Stopped,
    /// The EV paused the session (`SessionStopReq` Pause); it can be resumed on a new connection.
    Paused,
    Closed,
}
