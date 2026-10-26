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

#[cfg(feature = "din")]
pub mod din;
#[cfg(feature = "iso2")]
pub mod iso2;
pub mod secc;
#[cfg(feature = "iso20")]
pub mod session;
#[cfg(feature = "slac")]
pub mod slac;

#[cfg(feature = "iso20")]
pub use secc::{Action, DcLoopAnswer, AcLoopAnswer, MobilityNeeds, Progress, Secc};
#[cfg(feature = "iso20")]
pub use session::{Buffers, Session};

#[cfg(all(feature = "strict", not(any(feature = "iso20", feature = "iso2", feature = "din"))))]
compile_error!("evse-core: enable a protocol: `iso20`, `iso2`, `din` (or `acdp`)");
#[cfg(all(feature = "strict", not(any(feature = "ac", feature = "dc"))))]
compile_error!("evse-core: enable `ac` and/or `dc`: a connector delivers energy");
#[cfg(all(feature = "strict", feature = "inverted-pantograph", feature = "ac"))]
compile_error!("evse-core: the inverted pantograph (`acdp`) has no AC: do not enable `ac` with it");
#[cfg(all(feature = "strict", feature = "inverted-pantograph", feature = "plug"))]
compile_error!("evse-core: a pantograph and a plug are different connectors, and a machine drives one connector: build one machine per connector");
#[cfg(all(feature = "strict", feature = "iso20", not(feature = "plug"), not(feature = "inverted-pantograph")))]
compile_error!("evse-core: `iso20` needs a connection: `plug` or `acdp`");

/// The timeout of `V2G_SECC_Sequence_Timeout` (Table 215), for every protocol.
pub const SEQUENCE_TIMEOUT_MS: u32 = 60_000;

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
    /// The host measured a change of the control pilot. Report every change at once, at any time: the
    /// standards give the charger 10 ms for an emergency shutdown (IEC 61851-23 CC.4.3). A change away
    /// from C/D that nothing explains (the power delivery stop, `SessionStopReq`, the charger's own
    /// shutdown) ends the session with [`secc::Ended::Emergency`], and the state E, F or A with
    /// [`secc::Ended::PilotFault`]; nothing is sent and the connection is closed. The host does the rest
    /// (oscillator off, current down, contactors) without waiting for the machine.
    Pilot(control_pilot::Reading),
}

/// What the host does after a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Step {
    /// Length of the frames at the start of the TX buffer to write to the stream.
    pub send: usize,
    pub timer: Option<TimerCmd>,
    /// Close the connection (after writing `send`).
    pub close: bool,
    /// The control pilot guard changed: arm the host's own monitor with it (see [`control_pilot::Guard`]). Published
    /// when the connection is accepted, whenever what is expected of the pilot changes (the vehicle was seen in C or D,
    /// the stop begins) and, disarmed, when the session is over.
    pub pilot: Option<control_pilot::Guard>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    AwaitSupportedAppProtocolReq,
    AwaitSessionSetupReq,
    /// ACDP: the vehicle is positioned with `ACDP_VehiclePositioningReq` before it is authorized.
    AwaitVehiclePositioningReq,
    AwaitAuthorizationSetupReq,
    AwaitAuthorizationReq,
    AwaitServiceDiscoveryReq,
    AwaitServiceDetailReq,
    AwaitServiceSelectionReq,
    AwaitChargeParameterDiscoveryReq,
    AwaitScheduleExchangeReq,
    /// ACDP: the pantograph is connected before the cable check (`ACDP_ConnectReq`, Table 80).
    AwaitConnectReq,
    AwaitCableCheckReq,
    AwaitPreChargeReq,
    AwaitPowerDeliveryReq,
    /// Energy flows; charge loop requests and the stop are accepted.
    ChargeLoop,
    /// ACDP: the pantograph is disconnected after the power delivery has stopped (`ACDP_DisconnectReq`), in place of the
    /// welding detection.
    AwaitDisconnectReq,
    /// Welding detection after the stop (V2G20-1448); ends with a `Finished` request.
    AwaitWeldingReq,
    AwaitSessionStopReq,
    /// The session was stopped properly.
    Stopped,
    /// The EV paused the session (`SessionStopReq` Pause); it can be resumed on a new connection.
    Paused,
    Closed,
}
