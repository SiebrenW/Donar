//! The charger, as the session machine sees it: what to offer, what to answer, what to do.
//!
//! Every method has a default that is a charger which accepts and does nothing, so a host
//! implements what it has. Methods that take time (authorization, the cable check, pre-charge,
//! welding detection, the schedule exchange) answer with [`Progress`]: `Ongoing` makes the machine
//! send `EVSEProcessing = Ongoing` and ask again when the vehicle repeats its request (that is the
//! standard's polling, so the host never blocks).
//!
//! The types are those of the codec (`iso15118_20`): a host that offers a parameter set or answers a
//! charge loop writes what goes on the wire, and the machine adds the headers, the sequence and the
//! checks.

use iso15118_20::ResponseMessage;
use iso15118_20::ac;
use iso15118_20::common::{
    self, Authorization, ChargingSession, MeteringConfirmationReq, ParameterSet, PnCAReqAuthorizationMode, PowerDeliveryReq, SelectedService,
    Service, ServiceSelectionReq,
};
use iso15118_20::common_types::{EvseStatus, MessageHeader, MeterInfo, RationalNumber, Receipt, ResponseCode};
use iso15118_20::dc;
use v2gtp::PayloadType;

/// The answer of a step that may take several rounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress<T> {
    /// Not yet: the machine answers `Ongoing` and the vehicle asks again.
    Ongoing,
    Done(T),
}

/// What the machine does with a request before looking at it (for faults and tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Action {
    #[default]
    Pass,
    /// Do not answer.
    Silent,
    /// Close the connection without answering.
    Close,
}

/// What an `AuthorizationReq` asks to be checked.
pub enum Authorize<'a, 'r> {
    /// External identification means: the host knows whether the user has authorized.
    Eim,
    /// Plug & Charge: `mode` is the signed element, `fragment` the stream the signature in the header
    /// covers (V2G20-1063; the host verifies it with the contract certificate in `mode`, and checks
    /// the chain), `challenge` the one of this session that `mode` has to echo (V2G20-2565).
    PnC { header: &'a MessageHeader<'r>, mode: &'a PnCAReqAuthorizationMode<'r>, fragment: &'a [u8], challenge: [u8; 16] },
}

/// A signed element the host verifies: the stream the signature covers and the signature of the
/// header.
pub struct Signed<'a, 'r> {
    pub header: &'a MessageHeader<'r>,
    pub fragment: &'a [u8],
}

/// What the answer to a DC charge loop request is made of; the machine adds the header and, per
/// the standard, checks the control mode against the service.
pub struct DcLoopAnswer {
    pub response_code: ResponseCode,
    pub present_current: RationalNumber,
    pub present_voltage: RationalNumber,
    pub power_limit_achieved: bool,
    pub current_limit_achieved: bool,
    pub voltage_limit_achieved: bool,
    /// `DynamicDc` (or `BptDynamicDc` for a bidirectional service) with the limits for the vehicle.
    pub control_mode: dc::ClResControlModeChoice,
    /// A notification for the vehicle (`Terminate`, `Pause`, ...) with the seconds it has.
    pub status: Option<EvseStatus>,
}

/// What the charger names of the vehicle's mobility needs (`MobilityNeedsMode` 2): in the answer to the
/// schedule exchange, and in the control mode of a charge loop answer. A field is left out when it is
/// `None`. `departure_time` is seconds from the answer's time stamp.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MobilityNeeds {
    pub departure_time: Option<u32>,
    pub minimum_soc: Option<i8>,
    pub target_soc: Option<i8>,
}

/// The same for an AC charge loop request.
pub struct AcLoopAnswer {
    pub response_code: ResponseCode,
    pub target_frequency: Option<RationalNumber>,
    pub control_mode: ac::ClResControlModeChoice,
    pub status: Option<EvseStatus>,
}

/// How a session ended its service flow, for [`Secc::session_ended`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    Stopped(ChargingSession),
    /// The connection went away or the sequence timer ran out.
    Lost,
}

pub trait Secc {
    // ---- identity and time

    /// `EVSEID` of `SessionSetupRes`.
    fn evse_id(&self) -> &str;
    /// The charger's clock in microseconds (V2G20-1534: the time stamps of its messages).
    fn now_micros(&self) -> u64;
    /// A fresh session id (V2G20-1243: not zero, unique among the sessions this charger knows).
    fn new_session_id(&mut self) -> [u8; 8];
    /// 128 random bits for `AuthorizationSetupRes` (V2G20-697).
    fn gen_challenge(&mut self) -> [u8; 16];

    // ---- what the charger offers

    /// The protocols the charger speaks: whether it takes this namespace and major version of a
    /// `SupportedAppProtocolReq` entry. Of those it takes, the lowest priority number wins.
    fn supports_protocol(&self, namespace: &str, major: u32) -> bool {
        (namespace == "urn:iso:std:iso:15118:-20:DC" || namespace == "urn:iso:std:iso:15118:-20:AC") && major == 1
    }
    fn authorization_services(&self) -> &[Authorization] {
        &[Authorization::Eim]
    }
    /// Offer Plug & Charge's certificate installation service (`AuthorizationSetupRes`).
    fn certificate_installation_service(&self) -> bool {
        false
    }
    /// The energy transfer services of `ServiceDiscoveryRes`: service ids of Table 204 (1 `AC`, 2 `DC`,
    /// 5 `AC_BPT`, 6 `DC_BPT`).
    fn energy_services(&self) -> &[Service] {
        &[]
    }
    /// The parameter sets of a service (`ServiceDetailRes`); empty when the service is not offered.
    fn parameter_sets(&self, _service_id: u16) -> &[ParameterSet<'_>] {
        &[]
    }
    /// Offer service renegotiation in `ServiceDiscoveryRes` (V2G20-1480).
    fn service_renegotiation_supported(&self) -> bool {
        false
    }
    /// Agree to a `SessionStopReq(ServiceRenegotiation)` (V2G20-1643, V2G20-1955); by default when it was offered.
    fn accept_service_renegotiation(&mut self) -> bool {
        self.service_renegotiation_supported()
    }

    // ---- the service flow

    /// The service selection of a session paused with this id, if it can be resumed (V2G20-2623:
    /// authorization and the selection carry over, the next request is the charge parameter discovery).
    fn resume(&mut self, _session_id: [u8; 8]) -> Option<SelectedService> {
        None
    }
    /// Authorization (and the polling of it: `Ongoing` until it is decided). The code is `OK*` to
    /// accept, a `WARNING_*` to leave the vehicle another try (V2G20-1583), `FAILED_*` to end.
    fn authorize(&mut self, request: Authorize<'_, '_>) -> Progress<ResponseCode> {
        match request {
            Authorize::Eim => Progress::Done(ResponseCode::Ok),
            Authorize::PnC { .. } => Progress::Done(ResponseCode::WarningAuthorizationSelectionInvalid),
        }
    }
    /// The vehicle selected a service and a parameter set (checked against the offer already).
    fn service_selected(&mut self, _selection: &ServiceSelectionReq<'_>) {}

    /// `DC_ChargeParameterDiscoveryRes` for the request: the charger's limits, or the code that
    /// refuses (`FAILED_WrongChargeParameter`). The mode has to be the one of the selected service.
    fn dc_charge_parameters(&mut self, _bpt: bool, _request: &dc::DcChargeParameterDiscoveryReq<'_>) -> Result<dc::DcCpdResEnergyTransferModeChoice, ResponseCode> {
        Err(ResponseCode::FailedWrongChargeParameter)
    }
    fn ac_charge_parameters(&mut self, _bpt: bool, _request: &ac::AcChargeParameterDiscoveryReq<'_>) -> Result<ac::AcCpdResEnergyTransferModeChoice, ResponseCode> {
        Err(ResponseCode::FailedWrongChargeParameter)
    }

    /// Whether the schedule is ready (`Ongoing` while it is being worked out); `renegotiation` on the side
    /// stream of a schedule renegotiation.
    fn schedule_ready(&mut self, _renegotiation: bool) -> Progress<()> {
        Progress::Done(())
    }
    /// The vehicle's energy request has been answered as `Finished`: its `ScheduleExchangeReq` (in Scheduled
    /// mode with the `EVEnergyOffer`, if there is one). For the Dynamic control mode the return is what the
    /// charger names of the mobility needs in the answer; with `MobilityNeedsMode` 2 it provides or
    /// changes them (V2G20-1291), otherwise leave it at the default.
    fn schedule_requested(&mut self, _request: &common::ScheduleExchangeReq<'_>) -> MobilityNeeds {
        MobilityNeeds::default()
    }
    /// Scheduled control mode: the schedule tuples to offer (ids 1 to 255, V2G20-773), at most three.
    /// Empty in Dynamic mode.
    fn schedule_tuples(&self) -> &[common::ScheduleTuple<'_>] {
        &[]
    }

    // ---- DC only

    /// The cable check (`Ongoing` while the insulation is tested); `Done(code)`.
    fn cable_check(&mut self) -> Progress<ResponseCode> {
        Progress::Done(ResponseCode::Ok)
    }
    /// One round of pre-charge: the voltage at the charger's output, given what the vehicle
    /// wants (`target`) and its present voltage.
    fn pre_charge(&mut self, target: RationalNumber, _vehicle: RationalNumber) -> RationalNumber {
        target
    }
    /// One round of welding detection: the voltage seen at the (disabled) output.
    fn welding_detection(&mut self) -> RationalNumber {
        RationalNumber { exponent: 0, value: 0 }
    }

    // ---- energy transfer

    /// `PowerDeliveryReq`: close the contactors on `Start`, open them on `Stop`, enter or leave a standby
    /// period. The machine has checked the profile and the schedule selection. The code is the
    /// answer (`OK`, `FAILED_PowerDeliveryNotApplied`, `WARNING_StandbyNotAllowed`, ...).
    fn power_delivery(&mut self, _request: &PowerDeliveryReq<'_>) -> ResponseCode {
        ResponseCode::Ok
    }
    /// A DC charge loop request: what the charger delivers and what it tells the vehicle.
    fn dc_loop(&mut self, _request: &dc::DcChargeLoopReq<'_>, _bpt: bool) -> Option<DcLoopAnswer> {
        None
    }
    fn ac_loop(&mut self, _request: &ac::AcChargeLoopReq<'_>, _bpt: bool) -> Option<AcLoopAnswer> {
        None
    }
    /// The charger's meter for the answer just made: the energy of the service session so far
    /// (V2G20-902). `wanted` when the vehicle asked for it (V2G20-1081) or the answer notifies a
    /// metering confirmation (V2G20-1083); a charger with a meter may send it always (V2G20-1833).
    fn meter_info(&self, _wanted: bool) -> Option<MeterInfo<'_>> {
        None
    }
    /// A receipt for the answer just made, if the charger has one.
    fn receipt(&self) -> Option<Receipt<'_>> {
        None
    }
    /// The vehicle renegotiates its schedule on the side stream: the charge parameters and the
    /// schedule exchange were answered; this is the `PowerDeliveryReq(ScheduleRenegotiation)` that ends it.
    fn schedule_renegotiated(&mut self, _request: &PowerDeliveryReq<'_>) -> ResponseCode {
        ResponseCode::Ok
    }
    /// A `MeteringConfirmationReq` (V2G20-904: the host may verify the signature with the contract
    /// certificate of the authorization and compare the reading with what it sent).
    fn confirm_metering(&mut self, _request: &MeteringConfirmationReq<'_>, _signed: Signed<'_, '_>) -> ResponseCode {
        ResponseCode::Ok
    }
    /// The vehicle stops, pauses or renegotiates the services.
    fn session_ended(&mut self, _how: Ended) {}

    // ---- the machine's own decisions, for the host to bend

    /// Called with every request before it is looked at.
    fn intercept(&mut self, _payload_type: PayloadType, _request: &Request<'_>) -> Action {
        Action::Pass
    }
    /// The response code the machine is about to send, which the host may change (a fault to inject,
    /// a refusal for a reason only the host knows). Also what the machine acts on.
    fn response_code(&mut self, _message: ResponseMessage, code: ResponseCode) -> ResponseCode {
        code
    }
    /// A step of the flow that was taken or refused, for a log: `(what happened)`.
    fn note(&mut self, _text: core::fmt::Arguments<'_>) {}
}

/// A decoded request of either message set.
pub enum Request<'a> {
    Common(common::Message<'a>),
    Dc(dc::Message<'a>),
    Ac(ac::Message<'a>),
}
