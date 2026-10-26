//! Sans-IO EV communication controller core.
//!
//! Nothing here touches a socket or a clock. A host adapter feeds events in and carries
//! out the returned actions, including arming and cancelling the machine's timers, so the
//! machines never need to be polled.
#![no_std]

#[cfg(test)]
extern crate std;

#[cfg(feature = "din")]
pub mod din;
#[cfg(feature = "iso2")]
pub mod iso2;
pub mod ev;
pub mod log;
pub mod machine;
pub mod sdp;
#[cfg(feature = "slac")]
pub mod slac;
pub mod session;

#[cfg(all(feature = "strict", not(any(feature = "iso20", feature = "iso2", feature = "din"))))]
compile_error!("evcc-core: enable a protocol: `iso20`, `iso2`, `din` (or `acdp`)");
#[cfg(all(feature = "strict", not(any(feature = "ac", feature = "dc"))))]
compile_error!("evcc-core: enable `ac` and/or `dc`: a connector delivers energy");
#[cfg(all(feature = "strict", feature = "inverted-pantograph", feature = "ac"))]
compile_error!("evcc-core: the inverted pantograph (`acdp`) has no AC: do not enable `ac` with it");
#[cfg(all(feature = "strict", feature = "inverted-pantograph", feature = "plug"))]
compile_error!("evcc-core: a pantograph and a plug are different connectors, and a machine drives one connector: build one machine per connector");
#[cfg(all(feature = "strict", feature = "iso20", not(feature = "plug"), not(feature = "inverted-pantograph")))]
compile_error!("evcc-core: `iso20` needs a connection: `plug` or `acdp`");

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
    /// `V2G_EVCC_CommunicationSetup_Timer`: from the connection until `SessionSetupRes`.
    Setup,
    /// `V2G_EVCC_Sequence_Timer`: how long the EVCC may take before its next request while it
    /// waits for the vehicle (contactors, ramp-down).
    Sequence,
    /// The side stream of a schedule renegotiation (V2G20-1041: its timing is independent of
    /// the main stream's): the response to the request in flight, or the pause before a repeat.
    Side,
    /// `V2G_EVCC_ReadyToCharge_Timer` (DIN SPEC 70121): from the start of the session until the
    /// first `PowerDeliveryRes`.
    ReadyToCharge,
    /// The wait of the current SLAC step: response, batch interval, link-up or retry pause.
    Slac,
    /// `TT_EV_atten_results`: how long the EV collects attenuation reports.
    SlacResults,
    /// `TT_matching_repetition`: how long a failed SLAC run is repeated.
    SlacOverall,
    /// ACDP: the interval of the `ACDP_SystemStatusReq` stream.
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerCmd {
    /// (Re)arm `timer` to fire in `ms` milliseconds, replacing any pending expiry.
    Arm { timer: Timer, ms: u32 },
    Cancel(Timer),
}

/// The timer commands of one step (at most three, e.g. arm the response timer and the
/// ongoing timer while cancelling the sequence timer). Timers not mentioned keep running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timers {
    cmds: [Option<TimerCmd>; 3],
}

impl Timers {
    pub const NONE: Timers = Timers { cmds: [None; 3] };

    pub const fn one(cmd: TimerCmd) -> Self {
        Self { cmds: [Some(cmd), None, None] }
    }

    /// Adds a command. A step never needs more than three; a third would be a bug in the
    /// machine, so it panics in debug builds and is dropped in release builds.
    pub fn with(mut self, cmd: TimerCmd) -> Self {
        match self.cmds.iter_mut().find(|c| c.is_none()) {
            Some(slot) => *slot = Some(cmd),
            None => debug_assert!(false, "more than three timer commands in one step"),
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
