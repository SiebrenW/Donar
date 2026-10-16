//! Public types of the session machine: buffers, states, events, notices and steps.

use crate::{Timer, Timers};
use exi::DecodeError;
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
    /// Shutting down: the host stops drawing current and reports
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
    ChargeParametersDiscovered(dc::DcChargeParameterDiscoveryRes),
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
    /// EVSE asked for the stop, `within_s` is its `NotificationMaxDelay` (V2G20-1416).
    StopEnergyTransfer { within_s: Option<u16> },
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
