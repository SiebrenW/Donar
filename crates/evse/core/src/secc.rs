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
use iso15118_20::acdp;
use iso15118_20::common::{
    self, Authorization, MeteringConfirmationReq, ParameterSet, PnCAReqAuthorizationMode, PowerDeliveryReq, SelectedService,
    Service, ServiceSelectionReq,
};
use iso15118_20::common_types::{EvseStatus, MessageHeader, MeterInfo, RationalNumber, Receipt, ResponseCode};
use iso15118_20::dc;
use v2gtp::PayloadType;

pub use crate::common::{Action, Ended, Progress, Stop};

/// What an `ACDP_VehiclePositioningReq` says (Table 81).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcdpPositioningRequest {
    /// `EVMobilityStatus`: the vehicle is positioned and immobilized and asks for the pantograph.
    pub immobilized: bool,
    /// `EVPositioningSupport`: the vehicle wants to be guided by the charger.
    pub positioning_support: bool,
}

/// What the charger's pairing and positioning device (PPD) sees of the vehicle, for the answer to
/// `ACDP_VehiclePositioningReq` (Table 82). The deviations and the windows are in millimetres and
/// mean something only with `positioning_support` (the basic RFID device only knows `in_charge_position`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcdpPositioning {
    /// The EVID the vehicle was paired by is still the one the device detects. False: the vehicle left the zone or another
    /// one took its place, the answer is `FAILED_AssociationError` and the session ends (8.3.4.7.5.1, 8.6.4.6.2.2).
    pub association: bool,
    pub positioning_support: bool,
    pub x_deviation_mm: i16,
    pub y_deviation_mm: i16,
    pub window_x_mm: i16,
    pub window_y_mm: i16,
    pub in_charge_position: bool,
}

impl AcdpPositioning {
    /// A charger without a positioning device that can tell: associated, no guidance, in position.
    pub const UNGUIDED: AcdpPositioning =
        AcdpPositioning { association: true, positioning_support: false, x_deviation_mm: 0, y_deviation_mm: 0, window_x_mm: 0, window_y_mm: 0, in_charge_position: true };
}

/// The charger's side of `ACDP_SystemStatusRes` (Table 88): how the charging point is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcdpSystemStatus {
    pub mechanical: acdp::MechanicalChargingDeviceStatus,
    /// All sub-systems of the charger are ready to charge.
    pub ready_to_charge: bool,
    pub isolation: acdp::IsolationStatus,
    /// Disabled by the operator or for maintenance.
    pub disabled: bool,
    pub utility_interrupt: bool,
    /// An emergency shutdown, or the E-stop pressed at the charger.
    pub emergency_shutdown: bool,
    /// A non-recoverable fault (a breaker failure, say).
    pub malfunction: bool,
    /// The PPD sees the vehicle within the contact window, and the vehicle is still associated (only meaningful with a PPD).
    pub in_charge_position: bool,
    pub association: bool,
}

impl AcdpSystemStatus {
    /// A charging point without a fault: home, ready, isolation safe, nothing wrong.
    pub const HEALTHY: AcdpSystemStatus = AcdpSystemStatus {
        mechanical: acdp::MechanicalChargingDeviceStatus::Home,
        ready_to_charge: true,
        isolation: acdp::IsolationStatus::Safe,
        disabled: false,
        utility_interrupt: false,
        emergency_shutdown: false,
        malfunction: false,
        in_charge_position: true,
        association: true,
    };
}

/// The charger's answer to `ACDP_ConnectReq` and `ACDP_DisconnectReq` (Tables 84, 86): the pantograph, and the control pilot state the
/// charger sees through its contacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcdpConnection {
    /// The movement is over: the pantograph is at its end position (connect) or home (disconnect). `Ongoing` otherwise.
    pub finished: bool,
    pub electrical: acdp::ElectricalChargingDeviceStatus,
    pub mechanical: acdp::MechanicalChargingDeviceStatus,
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

/// What every ISO 15118-20 charger has, whatever it delivers: identity, clock, what it offers, authorization, the schedule, the power
/// delivery and the hooks for the host to bend the machine. The rest is split by capability, so that a host implements only what its
/// hardware does: [`DcSecc`], [`AcSecc`] and [`InvertedPantographSecc`]. The machine asks for the combination it needs ([`Iso20Evse`]).
pub trait Secc {

    /// `EVSEID` of `SessionSetupRes`.
    fn evse_id(&self) -> &str;
    /// The charger's clock in microseconds (V2G20-1534: the time stamps of its messages).
    fn now_micros(&self) -> u64;
    /// A fresh session id (V2G20-1243: not zero, unique among the sessions this charger knows).
    fn new_session_id(&mut self) -> [u8; 8];
    /// 128 random bits for `AuthorizationSetupRes` (V2G20-697).
    fn gen_challenge(&mut self) -> [u8; 16];

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
    /// The charger has no power (V2G20-2121): `GoToPause` is set in the answer to the schedule exchange, and the vehicle
    /// may go straight to the power delivery (V2G20-3304, 3305; with Power 0 in the first schedule entry, V2G20-3299). Asked when the answer is `Finished`.
    fn no_power(&self) -> bool {
        false
    }
    /// Scheduled control mode: the schedule tuples to offer (ids 1 to 255, V2G20-773), at most three.
    /// Empty in Dynamic mode.
    fn schedule_tuples(&self) -> &[common::ScheduleTuple<'_>] {
        &[]
    }

    /// `PowerDeliveryReq`: close the contactors on `Start`, open them on `Stop`, enter or leave a standby
    /// period. The machine has checked the profile and the schedule selection. The code is the
    /// answer (`OK`, `FAILED_PowerDeliveryNotApplied`, `WARNING_StandbyNotAllowed`, ...).
    fn power_delivery(&mut self, _request: &PowerDeliveryReq<'_>) -> ResponseCode {
        ResponseCode::Ok
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

/// A charger that delivers DC (a plug or a pantograph): the charge parameters, the cable check, the pre-charge, the welding detection and the DC charge loop.
pub trait DcSecc: Secc {
    /// `DC_ChargeParameterDiscoveryRes` for the request: the charger's limits, or the code that
    /// refuses (`FAILED_WrongChargeParameter`). The mode has to be the one of the selected service.
    fn dc_charge_parameters(&mut self, _bpt: bool, _request: &dc::DcChargeParameterDiscoveryReq<'_>) -> Result<dc::DcCpdResEnergyTransferModeChoice, ResponseCode> {
        Err(ResponseCode::FailedWrongChargeParameter)
    }

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
    /// A DC charge loop request: what the charger delivers and what it tells the vehicle.
    fn dc_loop(&mut self, _request: &dc::DcChargeLoopReq<'_>, _bpt: bool) -> Option<DcLoopAnswer> {
        None
    }
}

/// A charger that delivers AC: the charge parameters and the AC charge loop.
pub trait AcSecc: Secc {
    fn ac_charge_parameters(&mut self, _bpt: bool, _request: &ac::AcChargeParameterDiscoveryReq<'_>) -> Result<ac::AcCpdResEnergyTransferModeChoice, ResponseCode> {
        Err(ResponseCode::FailedWrongChargeParameter)
    }
    fn ac_loop(&mut self, _request: &ac::AcChargeLoopReq<'_>, _bpt: bool) -> Option<AcLoopAnswer> {
        None
    }
}

/// A charger with the inverted pantograph (ACDP of ISO 15118-20, OppCharge): the pairing and positioning device, the pantograph itself and the status of the charging point. DC only.
pub trait InvertedPantographSecc: Secc {

    /// What the pairing and positioning device sees of the vehicle now. The machine answers `Ongoing` to a
    /// request that says the vehicle is still mobilized and `Finished` to one that says it is immobilized (V2G20-4068, 4069);
    /// the answer decides only what goes with it, and whether the association holds ([`AcdpPositioning::association`]).
    fn acdp_positioning(&mut self, _request: AcdpPositioningRequest) -> AcdpPositioning {
        AcdpPositioning::UNGUIDED
    }
    /// The vehicle asks for the pantograph (`ACDP_ConnectReq`, `electrical` is the pilot state the vehicle sees). Lower it; answer with
    /// `finished` false while it moves and true when it is down. The default is a pantograph that is down at once.
    fn acdp_connect(&mut self, _electrical: acdp::ElectricalChargingDeviceStatus) -> AcdpConnection {
        AcdpConnection { finished: true, electrical: acdp::ElectricalChargingDeviceStatus::StateB, mechanical: acdp::MechanicalChargingDeviceStatus::EndPosition }
    }
    /// The vehicle asks for the pantograph to go home (`ACDP_DisconnectReq`). Raise it; `finished` when it is home. The default is
    /// a pantograph that is home at once.
    fn acdp_disconnect(&mut self, _electrical: acdp::ElectricalChargingDeviceStatus) -> AcdpConnection {
        AcdpConnection { finished: true, electrical: acdp::ElectricalChargingDeviceStatus::StateA, mechanical: acdp::MechanicalChargingDeviceStatus::Home }
    }
    /// The vehicle's status (`ACDP_SystemStatusReq`, once a second or so from the session setup to the stop): how the charging point is
    /// (V2G20-4017 to 4019). The vehicle's `EVReadyToCharge` false is answered like any other (4019).
    fn acdp_system_status(&mut self, _vehicle: &acdp::EvTechnicalStatus) -> AcdpSystemStatus {
        AcdpSystemStatus::HEALTHY
    }
    /// ACDP: an emergency shutdown (the vehicle's CP went from C/D to B, or the charger has one of its own): ramp down the current, open
    /// the DC contactors and raise the pantograph (NOTE 2, 3 of 8.6.4.5.2.1). The disconnect and the session stop follow on the
    /// open connection.
    fn acdp_emergency(&mut self) {}
}

#[cfg(feature = "dc")]
pub trait DcSeccPart: DcSecc {}
#[cfg(feature = "dc")]
impl<T: DcSecc> DcSeccPart for T {}
#[cfg(not(feature = "dc"))]
pub trait DcSeccPart {}
#[cfg(not(feature = "dc"))]
impl<T> DcSeccPart for T {}

#[cfg(feature = "ac")]
pub trait AcSeccPart: AcSecc {}
#[cfg(feature = "ac")]
impl<T: AcSecc> AcSeccPart for T {}
#[cfg(not(feature = "ac"))]
pub trait AcSeccPart {}
#[cfg(not(feature = "ac"))]
impl<T> AcSeccPart for T {}

#[cfg(feature = "inverted-pantograph")]
pub trait PantographSeccPart: InvertedPantographSecc {}
#[cfg(feature = "inverted-pantograph")]
impl<T: InvertedPantographSecc> PantographSeccPart for T {}
#[cfg(not(feature = "inverted-pantograph"))]
pub trait PantographSeccPart {}
#[cfg(not(feature = "inverted-pantograph"))]
impl<T> PantographSeccPart for T {}

/// What the ISO 15118-20 machine needs of a charger: the capabilities compiled in (DC, AC, the inverted pantograph).
pub trait Iso20Evse: Secc + DcSeccPart + AcSeccPart + PantographSeccPart {}
impl<T: Secc + DcSeccPart + AcSeccPart + PantographSeccPart> Iso20Evse for T {}

/// The charger as the machine sees it: the methods of [`Secc`] through `Deref`, the capability methods as methods of its own.
/// They call the charger when the capability is compiled in and answer with neutral values (a refusal, nothing to deliver) when it is not.
pub struct SeccCaps<S>(pub S);

impl<S> core::ops::Deref for SeccCaps<S> {
    type Target = S;
    fn deref(&self) -> &S {
        &self.0
    }
}

impl<S> core::ops::DerefMut for SeccCaps<S> {
    fn deref_mut(&mut self) -> &mut S {
        &mut self.0
    }
}

#[cfg(feature = "dc")]
impl<S: DcSecc> SeccCaps<S> {
    pub fn dc_charge_parameters(&mut self, bpt: bool, request: &dc::DcChargeParameterDiscoveryReq<'_>) -> Result<dc::DcCpdResEnergyTransferModeChoice, ResponseCode> {
        self.0.dc_charge_parameters(bpt, request)
    }
    pub fn cable_check(&mut self) -> Progress<ResponseCode> {
        self.0.cable_check()
    }
    pub fn pre_charge(&mut self, target: RationalNumber, vehicle: RationalNumber) -> RationalNumber {
        self.0.pre_charge(target, vehicle)
    }
    pub fn welding_detection(&mut self) -> RationalNumber {
        self.0.welding_detection()
    }
    pub fn dc_loop(&mut self, request: &dc::DcChargeLoopReq<'_>, bpt: bool) -> Option<DcLoopAnswer> {
        self.0.dc_loop(request, bpt)
    }
}

#[cfg(not(feature = "dc"))]
impl<S> SeccCaps<S> {
    pub fn dc_charge_parameters(&mut self, _bpt: bool, _request: &dc::DcChargeParameterDiscoveryReq<'_>) -> Result<dc::DcCpdResEnergyTransferModeChoice, ResponseCode> {
        Err(ResponseCode::FailedWrongChargeParameter)
    }
    pub fn cable_check(&mut self) -> Progress<ResponseCode> {
        Progress::Done(ResponseCode::Failed)
    }
    pub fn pre_charge(&mut self, target: RationalNumber, _vehicle: RationalNumber) -> RationalNumber {
        target
    }
    pub fn welding_detection(&mut self) -> RationalNumber {
        RationalNumber { exponent: 0, value: 0 }
    }
    pub fn dc_loop(&mut self, _request: &dc::DcChargeLoopReq<'_>, _bpt: bool) -> Option<DcLoopAnswer> {
        None
    }
}

#[cfg(feature = "ac")]
impl<S: AcSecc> SeccCaps<S> {
    pub fn ac_charge_parameters(&mut self, bpt: bool, request: &ac::AcChargeParameterDiscoveryReq<'_>) -> Result<ac::AcCpdResEnergyTransferModeChoice, ResponseCode> {
        self.0.ac_charge_parameters(bpt, request)
    }
    pub fn ac_loop(&mut self, request: &ac::AcChargeLoopReq<'_>, bpt: bool) -> Option<AcLoopAnswer> {
        self.0.ac_loop(request, bpt)
    }
}

#[cfg(not(feature = "ac"))]
impl<S> SeccCaps<S> {
    pub fn ac_charge_parameters(&mut self, _bpt: bool, _request: &ac::AcChargeParameterDiscoveryReq<'_>) -> Result<ac::AcCpdResEnergyTransferModeChoice, ResponseCode> {
        Err(ResponseCode::FailedWrongChargeParameter)
    }
    pub fn ac_loop(&mut self, _request: &ac::AcChargeLoopReq<'_>, _bpt: bool) -> Option<AcLoopAnswer> {
        None
    }
}

#[cfg(feature = "inverted-pantograph")]
impl<S: InvertedPantographSecc> SeccCaps<S> {
    pub fn acdp_positioning(&mut self, request: AcdpPositioningRequest) -> AcdpPositioning {
        self.0.acdp_positioning(request)
    }
    pub fn acdp_connect(&mut self, electrical: acdp::ElectricalChargingDeviceStatus) -> AcdpConnection {
        self.0.acdp_connect(electrical)
    }
    pub fn acdp_disconnect(&mut self, electrical: acdp::ElectricalChargingDeviceStatus) -> AcdpConnection {
        self.0.acdp_disconnect(electrical)
    }
    pub fn acdp_system_status(&mut self, vehicle: &acdp::EvTechnicalStatus) -> AcdpSystemStatus {
        self.0.acdp_system_status(vehicle)
    }
    pub fn acdp_emergency(&mut self) {
        self.0.acdp_emergency()
    }
}

#[cfg(not(feature = "inverted-pantograph"))]
impl<S> SeccCaps<S> {
    pub fn acdp_positioning(&mut self, _request: AcdpPositioningRequest) -> AcdpPositioning {
        AcdpPositioning::UNGUIDED
    }
    pub fn acdp_connect(&mut self, _electrical: acdp::ElectricalChargingDeviceStatus) -> AcdpConnection {
        AcdpConnection { finished: true, electrical: acdp::ElectricalChargingDeviceStatus::StateA, mechanical: acdp::MechanicalChargingDeviceStatus::Home }
    }
    pub fn acdp_disconnect(&mut self, _electrical: acdp::ElectricalChargingDeviceStatus) -> AcdpConnection {
        AcdpConnection { finished: true, electrical: acdp::ElectricalChargingDeviceStatus::StateA, mechanical: acdp::MechanicalChargingDeviceStatus::Home }
    }
    pub fn acdp_system_status(&mut self, _vehicle: &acdp::EvTechnicalStatus) -> AcdpSystemStatus {
        AcdpSystemStatus::HEALTHY
    }
    pub fn acdp_emergency(&mut self) {}
}

/// A decoded request of either message set.
pub enum Request<'a> {
    Common(common::Message<'a>),
    Dc(dc::Message<'a>),
    Ac(ac::Message<'a>),
    Acdp(acdp::Message<'a>),
}
