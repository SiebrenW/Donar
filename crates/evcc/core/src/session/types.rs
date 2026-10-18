//! Public types of the session machine: buffers, states, events, notices and steps.

use crate::{Timer, Timers};
use exi::DecodeError;
use iso15118_20::common::SelectedService;
use iso15118_20::common_types::ResponseCode;
use iso15118_20::ac;
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
    /// DIN SPEC 70121: `ServicePaymentSelectionReq` is out.
    AwaitServicePaymentSelectionRes,
    /// DIN SPEC 70121: `ContractAuthenticationReq` is out.
    AwaitContractAuthenticationRes,
    /// DIN SPEC 70121: the host changes the control pilot to state C (before the cable check) and
    /// reports [`Event::ControlPilotChanged`].
    AwaitControlPilotC,
    /// ISO 15118-2: `PaymentDetailsReq` is out (Plug & Charge).
    AwaitPaymentDetailsRes,
    /// ISO 15118-2 AC: `ChargingStatusReq` is out (the charge loop).
    AwaitChargingStatusRes,
    /// ISO 15118-2 AC: `MeteringReceiptReq` is out.
    AwaitMeteringReceiptRes,
    /// DIN SPEC 70121: `CurrentDemandReq` is out (the charge loop).
    AwaitCurrentDemandRes,
    /// DIN SPEC 70121: the host changes the control pilot to state B (after the energy transfer
    /// has stopped) and reports [`Event::ControlPilotChanged`].
    AwaitControlPilotB,
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
    /// Welding detection never saw the charger's voltage fall below the limit: the vehicle's
    /// contactors did not open (or the charger kept its voltage up). Safety relevant: the host
    /// should not release the connector lock and should report a fault. The vehicle has told
    /// the charger with `SessionStopReq` (termination code `ContactorWelded`) by then.
    ContactorWelded,
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
    /// DIN SPEC 70121: the EVSE answered with a failing response code.
    RejectedDin(din70121::types::ResponseCode),
    /// ISO 15118-2: the EVSE answered with a failing response code.
    RejectedIso2(iso15118_2::types::ResponseCode),
    /// The control pilot said emergency (IEC 61851-23 CC.3.4.3, CC.4.3): it left state C/D without the vehicle's
    /// own change, or the charger turned its oscillator off or changed the duty cycle (V2G-DC-656). Nothing
    /// was sent; the host opens the contactors, sets the pilot to B and closes the connection.
    Emergency(control_pilot::Reading),
    /// The control pilot is in state E or F (or A): the connection is lost or the charger signals a fault
    /// (V2G-DC-655, V2G2-737). Close the connection.
    PilotFault(control_pilot::Reading),
}

impl EndReason {
    /// The `EVTerminationCode` of the `SessionStopReq` the vehicle sends when it ends the session for this
    /// non-critical reason of its own (V2G20-2644, V2G-DC-648).
    pub(crate) fn termination_code(self) -> &'static str {
        match self {
            EndReason::NoCompatibleAuthorization => "NoCompatibleAuthorization",
            EndReason::NoCompatibleService => "NoCompatibleService",
            EndReason::NoCompatibleParameterSet => "NoCompatibleParameterSet",
            EndReason::ContactorWelded => "ContactorWelded",
            _ => "Terminated",
        }
    }
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
    /// The host changed the control pilot after [`Notice::ControlPilot`].
    ControlPilotChanged,
    /// The host measured a change of the control pilot (state or PWM). Report every change at once, at any
    /// time, also before [`Event::Start`]: an emergency has to reach the session within the milliseconds
    /// the standards give (IEC 61851-23 CC.4.3: 50 ms for the vehicle). The session ends with
    /// [`EndReason::Emergency`] or [`EndReason::PilotFault`] and sends nothing more; the host opens the
    /// contactors and closes the connection (it does not wait for this event to do the first).
    Pilot(control_pilot::Reading),
}

/// A control pilot state (IEC 61851-1) the vehicle changes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPilot {
    /// Connected, not ready: the state before the session and after the energy transfer.
    B,
    /// Ready to charge: before the cable check.
    C,
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
    /// The EVSE's AC limits, from `AC_ChargeParameterDiscoveryRes`.
    AcChargeParametersDiscovered(ac::AcCpdResEnergyTransferModeChoice),
    /// What the EVSE reported in the latest `AC_ChargeLoopRes`: the power the on-board charger
    /// is to draw (or feed back, negative).
    AcChargeLoop(AcChargeLoopStatus),
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
    /// DIN SPEC 70121: change the control pilot to this state, then send
    /// [`Event::ControlPilotChanged`].
    ControlPilot(ControlPilot),
}

/// What the charger's meter said in a charge loop answer (`MeterInfo`, V2G20-902: the energy of the
/// service session so far), when the vehicle asked for it ([`super::Config::request_meter_info`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeterReading {
    pub charged_wh: u64,
    /// Energy given back (BPT), if the meter counts it.
    pub discharged_wh: Option<u64>,
    /// The meter's time stamp, if it has one.
    pub timestamp: Option<u64>,
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
    /// The meter reading of this answer, if it had one.
    pub meter: Option<MeterReading>,
    /// The mobility needs this answer named, with `MobilityNeedsMode` 2 (and only what changed).
    pub mobility_needs: Option<crate::ev::MobilityNeeds>,
}

/// The EVSE's side of one AC charge loop exchange (Dynamic control mode). A negative target
/// active power is a discharge of the vehicle (BPT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcChargeLoopStatus {
    pub target_active_power: crate::ev::Phases,
    /// Per line, `None` where the EVSE sent none.
    pub target_reactive_power: [Option<iso15118_20::common_types::RationalNumber>; 3],
    /// What the EVSE measures (`EVSEPresentActivePower`), per line.
    pub present_active_power: [Option<iso15118_20::common_types::RationalNumber>; 3],
    pub target_frequency: Option<iso15118_20::common_types::RationalNumber>,
    /// The target comes from the vehicle's own schedule (Scheduled mode, the charger named none): changes of it
    /// are held to the charger's power ramp limit (V2G20-1831); a target the charger names overrides the limit
    /// (V2G20-1832).
    pub target_from_schedule: bool,
    /// The meter reading of this answer, if it had one.
    pub meter: Option<MeterReading>,
    /// The mobility needs this answer named, with `MobilityNeedsMode` 2 (and only what changed).
    pub mobility_needs: Option<crate::ev::MobilityNeeds>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Step {
    /// Length of the framed message at the start of the TX buffer to write to the stream.
    pub send: Option<usize>,
    pub timers: Timers,
    pub notice: Option<Notice>,
    /// Set when the session is over; the host closes the connection.
    pub end: Option<EndReason>,
    /// The control pilot guard changed: arm the host's own monitor with it (see [`control_pilot::Guard`]). Published
    /// when the session starts, whenever what is expected of the pilot changes (the vehicle holds C, the shutdown
    /// begins, the oscillator is meant to be 5 %) and, disarmed, when the session ends.
    pub pilot: Option<control_pilot::Guard>,
}
