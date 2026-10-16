//! Public types of the session machine: buffers, states, events, notices and steps.

use crate::{Timer, Timers};
use exi::DecodeError;
use iso15118_20::common::SelectedService;
use iso15118_20::common_types::ResponseCode;
use iso15118_20::dc;

/// Host-provided memory. Nothing is allocated by the core.
pub struct Buffers<'a> {
    /// Receives V2GTP frames; must hold the largest expected frame (header + payload).
    pub rx: &'a mut [u8],
    /// Holds the one outgoing frame.
    pub tx: &'a mut [u8],
    /// Scratch for decoded messages, reset for every received frame.
    pub arena: &'a mut [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    AwaitSupportedAppProtocolRes,
    AwaitSessionSetupRes,
    AwaitAuthorizationSetupRes,
    AwaitAuthorizationRes,
    AwaitServiceDiscoveryRes,
    AwaitServiceDetailRes,
    AwaitServiceSelectionRes,
    AwaitChargeParameterDiscoveryRes,
    AwaitScheduleExchangeRes,
    AwaitCableCheckRes,
    AwaitPreChargeRes,
    /// Pre-charge is finished; the host closes the contactors and reports
    /// [`Event::ContactorsClosed`].
    AwaitContactors,
    /// `PowerDeliveryReq(Start)` is out.
    AwaitPowerDeliveryRes,
    /// Energy is flowing: `DC_ChargeLoopReq`s go out in a cycle.
    AwaitChargeLoopRes,
    /// `PowerDeliveryReq(Standby)` is out.
    AwaitPowerDeliveryStandbyRes,
    /// Shutting down (or entering standby): the host stops drawing current and reports
    /// [`Event::EnergyTransferStopped`].
    AwaitCurrentStopped,
    /// `PowerDeliveryReq(Stop)` is out.
    AwaitPowerDeliveryStopRes,
    /// The host opens the contactors and reports [`Event::ContactorsOpened`].
    AwaitContactorsOpened,
    /// `DC_WeldingDetectionReq` is out.
    AwaitWeldingDetectionRes,
    AwaitSessionStopRes,
    Ended,
}

/// What it takes to resume a paused session (V2G20-1541): its id and, since authorization,
/// service selection and the charger's offer carry over (V2G20-1844), those facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resume {
    pub session_id: [u8; 8],
    pub selected: SelectedService,
    /// `ServiceRenegotiationSupported` of the first `ServiceDiscoveryRes` of the session.
    pub renegotiation_supported: Option<bool>,
}

/// How the shutdown that is under way ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum StopKind {
    /// `SessionStopReq(Terminate)`, the session is over.
    #[default]
    Terminate,
    /// `SessionStopReq(Pause)`: the session can be resumed later.
    Pause,
    /// `SessionStopReq(ServiceRenegotiation)`: service discovery follows.
    ServiceRenegotiation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The flow implemented so far finished normally.
    Completed,
    /// The EVSE rejected or did not accept our protocol offer.
    NoCommonProtocol,
    /// The EVSE offers no authorization service this EV can use (only EIM is implemented).
    NoCompatibleAuthorization,
    /// The EVSE does not offer the energy transfer service this EV wants.
    NoCompatibleService,
    /// None of the EVSE's parameter sets for the service matches `Config::parameter_filter`.
    NoCompatibleParameterSet,
    /// The EVSE answered with a failing response code.
    Rejected(ResponseCode),
    Timeout,
    /// Bad V2GTP framing, or a frame larger than the RX buffer.
    Framing,
    Malformed(DecodeError),
    /// A valid response whose content this EV cannot work with (e.g. a Scheduled control mode
    /// answer to a Dynamic request).
    UnexpectedMessage,
    /// The session was paused (`SessionStopReq` Pause, V2G20-1540): close the TLS connection
    /// (V2G20-2071) and, when the vehicle wants to go on, make a new one and start a session
    /// with this in `Config::resume` (V2G20-2073).
    Paused(Resume),
    /// The connection is not secured and `Config::allow_unsecured` is off.
    TlsRequired,
    /// The TX buffer cannot hold the outgoing message.
    TxBufferTooSmall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The transport is connected. `secured` is true for TLS.
    Start { secured: bool },
    /// `n` bytes were written into [`Session::rx_space`].
    Received(usize),
    TimerExpired(Timer),
    /// The host closed the vehicle's contactors after [`Notice::CloseContactors`].
    ContactorsClosed,
    /// The vehicle wants to stop charging (user, full battery, fault). Only acted on while the
    /// charge loop runs.
    StopRequested,
    /// The host opened the vehicle's contactors after [`Notice::OpenContactors`].
    ContactorsOpened,
    /// The vehicle wants to pause the session (V2G20-1540): an orderly stop of the charge, then
    /// `SessionStopReq(Pause)`; the session ends with [`EndReason::Paused`]. Only acted on
    /// while the charge loop runs.
    PauseRequested,
    /// The vehicle wants new charging targets and a new power profile (V2G20-1046): a schedule
    /// renegotiation runs as a side stream next to the charge loop. Ignored outside the charge
    /// loop, before the first `ChargeLoopReq` and while one is running.
    RenegotiateSchedule,
    /// The vehicle wants to change its service selection (V2G20-1661): the charge ends in an
    /// orderly way, the session is not stopped, and service discovery starts over. Ignored
    /// unless the EVSE offered service renegotiation in its first `ServiceDiscoveryRes`.
    RenegotiateServices,
    /// The host reduced the current to zero after [`Notice::StopEnergyTransfer`]
    /// (V2G20-1310 asks for this before `PowerDeliveryReq(Stop)`).
    EnergyTransferStopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// The EVSE accepted the session; `session_id` is the id it assigned.
    SessionSetUp { session_id: [u8; 8] },
    /// Authorization finished successfully.
    Authorized,
    /// The EVSE accepted the service selection.
    ServiceSelected { service_id: u16, parameter_set_id: u16 },
    /// The EVSE's DC limits, from `DC_ChargeParameterDiscoveryRes`.
    ChargeParametersDiscovered(dc::DcCpdResEnergyTransferModeChoice),
    /// The EVSE finished the schedule exchange.
    ScheduleExchanged,
    /// The cable check finished successfully.
    CableChecked,
    /// Pre-charge is finished: close the EV's contactors, then send
    /// [`Event::ContactorsClosed`] (IEC 61851-23 t9).
    CloseContactors,
    /// The EVSE enabled the energy transfer: the vehicle may draw current.
    EnergyTransferStarted,
    /// What the EVSE reported in the latest `DC_ChargeLoopRes`.
    ChargeLoop(ChargeLoopStatus),
    /// Stop drawing current (ramp down), then report [`Event::EnergyTransferStopped`]. When the
    /// EVSE asked for the stop, `within_s` is its `NotificationMaxDelay` (V2G20-1416). Also the
    /// first step into a standby period, which the EVSE asked for with `Pause` (V2G20-1194).
    StopEnergyTransfer { within_s: Option<u16> },
    /// The EVSE accepted the standby period: no current flows, the contactors stay closed and
    /// the charge loop goes on. [`Notice::EnergyTransferStarted`] follows when it ends (the
    /// EVSE sent `ExitStandby`) or when the EVSE refused the standby period.
    StandbyEntered,
    /// The session was resumed instead of set up anew (`OK_OldSessionJoined`, V2G20-2540):
    /// authorization and service selection carry over, charge parameter discovery follows.
    SessionResumed { session_id: [u8; 8] },
    /// A schedule renegotiation finished: the EVSE accepted the new power profile.
    ScheduleRenegotiated,
    /// The EVSE agreed to a service renegotiation: the session goes on with a new service
    /// discovery, after which the usual notices follow again (pre-charge, close contactors).
    ServicesRenegotiating,
    /// The EVSE acknowledged the stop: open the contactors, then send
    /// [`Event::ContactorsOpened`]; welding detection follows.
    OpenContactors,
}

/// The EVSE's side of one charge loop exchange (Dynamic control mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChargeLoopStatus {
    pub present_current: iso15118_20::common_types::RationalNumber,
    pub present_voltage: iso15118_20::common_types::RationalNumber,
    pub power_limit_achieved: bool,
    pub current_limit_achieved: bool,
    pub voltage_limit_achieved: bool,
    pub maximum_charge_power: iso15118_20::common_types::RationalNumber,
    pub maximum_charge_current: iso15118_20::common_types::RationalNumber,
    pub maximum_voltage: iso15118_20::common_types::RationalNumber,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Step {
    /// Length of the framed message at the start of the TX buffer to write to the stream.
    pub send: Option<usize>,
    pub timers: Timers,
    pub notice: Option<Notice>,
    /// Set when the session is over; the host closes the connection.
    pub end: Option<EndReason>,
}
