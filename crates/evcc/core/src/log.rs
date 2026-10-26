//! Typed log records. The core has no logging dependency: it hands records to a [`LogSink`]
//! and adapters render them (`log`/`tracing`, `defmt`, a ring buffer) or tests assert on
//! them. Records may borrow from the message being processed, because the sink is called
//! synchronously.

use exi::DecodeError;
use iso15118_20::common::{Authorization, ParameterSet, Service};
use iso15118_20::common_types::{Processing, RationalNumber, ResponseCode};

use crate::session::{EndReason, State};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    /// Session milestones and non-OK response codes.
    Info,
    /// Per-message events.
    Debug,
    /// Raw payloads.
    Trace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    SupportedAppProtocolReq,
    SupportedAppProtocolRes,
    SessionSetupReq,
    SessionSetupRes,
    AcdpVehiclePositioningReq,
    AcdpVehiclePositioningRes,
    AcdpSystemStatusReq,
    AcdpSystemStatusRes,
    AcdpConnectReq,
    AcdpConnectRes,
    AcdpDisconnectReq,
    AcdpDisconnectRes,
    AuthorizationSetupReq,
    AuthorizationSetupRes,
    AuthorizationReq,
    AuthorizationRes,
    ServiceDiscoveryReq,
    ServiceDiscoveryRes,
    ServiceDetailReq,
    ServiceDetailRes,
    ServiceSelectionReq,
    ServiceSelectionRes,
    DcChargeParameterDiscoveryReq,
    DcChargeParameterDiscoveryRes,
    ScheduleExchangeReq,
    ScheduleExchangeRes,
    DcCableCheckReq,
    DcCableCheckRes,
    DcPreChargeReq,
    DcPreChargeRes,
    PowerDeliveryReq,
    PowerDeliveryRes,
    DcChargeLoopReq,
    DcChargeLoopRes,
    SessionStopReq,
    SessionStopRes,
    DcWeldingDetectionReq,
    DcWeldingDetectionRes,
    AcChargeParameterDiscoveryReq,
    AcChargeParameterDiscoveryRes,
    AcChargeLoopReq,
    AcChargeLoopRes,
    MeteringConfirmationReq,
    MeteringConfirmationRes,
    // DIN SPEC 70121 (the messages it shares with ISO 15118-20 keep their kinds above).
    ServicePaymentSelectionReq,
    ServicePaymentSelectionRes,
    ContractAuthenticationReq,
    ContractAuthenticationRes,
    ChargeParameterDiscoveryReq,
    ChargeParameterDiscoveryRes,
    CableCheckReq,
    CableCheckRes,
    PreChargeReq,
    PreChargeRes,
    CurrentDemandReq,
    CurrentDemandRes,
    WeldingDetectionReq,
    WeldingDetectionRes,
    // ISO 15118-2 only.
    PaymentDetailsReq,
    PaymentDetailsRes,
    AuthorizationReqIso2,
    AuthorizationResIso2,
    ChargingStatusReq,
    ChargingStatusRes,
    MeteringReceiptReq,
    MeteringReceiptRes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record<'a> {
    Started { secured: bool },
    StateChange { from: State, to: State },
    MsgTx { msg: MsgKind },
    MsgRx { msg: MsgKind },
    /// A response code from the EVSE.
    Response { msg: MsgKind, code: ResponseCode },
    SessionSetUp { session_id: [u8; 8], evse_id: &'a str },
    /// A paused session was resumed (`OK_OldSessionJoined`).
    SessionResumed { session_id: [u8; 8] },
    /// The `EVSEProcessing` value of an `AuthorizationRes`.
    EvseProcessing { processing: Processing },
    /// The charger broke a rule of the standard with a value in a response (the requirement is
    /// the one it breaks, `item` the element, `value` what was sent and `limit` what it may be).
    Violation { requirement: &'static str, item: &'static str, value: RationalNumber, limit: RationalNumber },
    /// The charger answered with a response code that the message may not carry (Tables 224, 226).
    ResponseCodeNotAllowed { msg: MsgKind, code: ResponseCode },
    /// The authorization service the vehicle selected.
    AuthorizationSelected { service: Authorization },
    /// The schedule tuple the vehicle selected of those the charger offered (Scheduled mode), given the
    /// energy it needs and the seconds until it leaves.
    ScheduleSelected { tuple_id: u32, offered: usize, need_wh: u32, departure_s: u32 },
    /// ACDP: the charger's answer to a connect or disconnect request.
    Pantograph { processing: Processing, electrical: iso15118_20::acdp::ElectricalChargingDeviceStatus, mechanical: iso15118_20::acdp::MechanicalChargingDeviceStatus },
    /// ACDP: the charger's answer to a positioning request.
    VehiclePositioning { processing: Processing, evse_positioning_support: bool, x_deviation_mm: i16, y_deviation_mm: i16, window_x_mm: i16, window_y_mm: i16, in_charge_position: bool },
    /// The charger has no power and said so (`GoToPause`, V2G20-2121): the vehicle pauses the session.
    GoToPause,
    /// The charger provided mobility needs (`MobilityNeedsMode` 2); what it named, which the vehicle now uses.
    MobilityNeedsFromSecc { needs: crate::ev::MobilityNeeds },
    /// The charger named mobility needs although the vehicle did not choose `MobilityNeedsMode` 2.
    MobilityNeedsIgnored { needs: crate::ev::MobilityNeeds },
    /// The energy transfer service and parameter set the EVSE accepted.
    ServiceSelected { service_id: u16, parameter_set_id: u16 },
    RxPayload { payload_type: u16, payload: &'a [u8] },
    /// A frame that is not a valid response to the request in flight was dropped (V2G20-440).
    Ignored { state: State, payload_type: u16 },
    DecodeFailed { state: State, error: DecodeError },
    /// The charger offers no authorization service the vehicle can use.
    NoAuthorization { wanted: Authorization, offered: &'a [Authorization] },
    /// The charger does not offer the energy transfer service the vehicle wants.
    NoService { wanted: u16, offered: &'a [Service] },
    /// None of the offered parameter sets has the vehicle's inlet (`connector`, the value of
    /// the `Connector` parameter) and every `(name, value)` of the vehicle's filter.
    NoParameterSet { connector: i32, filter: &'static [(&'static str, i32)], offered: &'a [ParameterSet<'a>] },
    Ended { reason: EndReason },
    /// What the host is to watch on the control pilot changed.
    PilotGuard { guard: control_pilot::Guard },
    /// A response code of a DIN SPEC 70121 charger.
    #[cfg(feature = "din")]
    DinResponse { msg: MsgKind, code: din70121::types::ResponseCode },
    /// The charger does not offer the energy transfer the vehicle wants (DIN SPEC 70121).
    #[cfg(feature = "din")]
    DinNoEnergyService { wanted: din70121::types::EvRequestedEnergyTransfer, offered: din70121::types::EvseSupportedEnergyTransfer },
    /// A response code of an ISO 15118-2 charger.
    #[cfg(feature = "iso2")]
    Iso2Response { msg: MsgKind, code: iso15118_2::types::ResponseCode },
    /// A value-added service the charger described, with how many parameter sets it has.
    #[cfg(feature = "iso2")]
    Iso2ServiceDetail { service_id: u16, parameter_sets: usize },
    /// The charger does not offer the energy transfer mode the vehicle wants (ISO 15118-2).
    #[cfg(feature = "iso2")]
    Iso2NoEnergyMode { wanted: iso15118_2::types::EnergyTransferMode, offered: &'a [iso15118_2::types::EnergyTransferMode] },
    /// The charger offers no payment option the vehicle can use (ISO 15118-2).
    #[cfg(feature = "iso2")]
    Iso2NoPayment { wanted: iso15118_2::types::PaymentOption, offered: &'a [iso15118_2::types::PaymentOption] },
    /// The charger offers no payment option the vehicle can use: DIN SPEC 70121 has only
    /// `ExternalPayment` (V2G-DC-252).
    #[cfg(feature = "din")]
    DinNoPayment { offered: &'a [din70121::types::PaymentOption] },
}

impl Record<'_> {
    pub fn level(&self) -> Level {
        match self {
            Record::Started { .. }
            | Record::StateChange { .. }
            | Record::SessionSetUp { .. }
            | Record::SessionResumed { .. }
            | Record::AuthorizationSelected { .. }
            | Record::ScheduleSelected { .. }
            | Record::MobilityNeedsFromSecc { .. }
            | Record::GoToPause
            | Record::VehiclePositioning { .. }
            | Record::Pantograph { .. }
            | Record::MobilityNeedsIgnored { .. }
            | Record::ServiceSelected { .. } => Level::Info,
            #[cfg(feature = "iso20")]
            Record::Response { code, .. } if !crate::session::is_ok(*code) => Level::Info,
            #[cfg(feature = "iso2")]
            Record::Iso2ServiceDetail { .. } => Level::Info,
            #[cfg(feature = "din")]
            Record::DinResponse { code, .. } if !crate::din::is_ok(*code) => Level::Info,
            #[cfg(feature = "din")]
            Record::DinResponse { .. } => Level::Debug,
            #[cfg(feature = "iso2")]
            Record::Iso2Response { code, .. } if !crate::iso2::is_ok(*code) => Level::Info,
            #[cfg(feature = "iso2")]
            Record::Iso2Response { .. } => Level::Debug,
            #[cfg(feature = "din")]
            Record::DinNoEnergyService { .. } | Record::DinNoPayment { .. } => Level::Error,
            #[cfg(feature = "iso2")]
            Record::Iso2NoEnergyMode { .. } | Record::Iso2NoPayment { .. } => Level::Error,
            Record::Response { .. } | Record::EvseProcessing { .. } | Record::MsgTx { .. } | Record::MsgRx { .. } => {
                Level::Debug
            }
            Record::RxPayload { .. } => Level::Trace,
            Record::PilotGuard { .. } => Level::Debug,
            Record::DecodeFailed { .. } | Record::Violation { .. } | Record::ResponseCodeNotAllowed { .. } | Record::NoAuthorization { .. } | Record::NoService { .. } | Record::NoParameterSet { .. } => Level::Error,
            Record::Ignored { .. } => Level::Info,
            Record::Ended { reason } => match reason {
                EndReason::Completed => Level::Info,
                _ => Level::Error,
            },
        }
    }
}

pub trait LogSink {
    fn record(&mut self, record: &Record<'_>);
}

/// Discards everything.
pub struct NoLog;

impl LogSink for NoLog {
    fn record(&mut self, _: &Record<'_>) {}
}

impl MsgKind {
    /// The response message this is (the request kinds have none), for the response code tables.
    pub fn response_message(self) -> Option<iso15118_20::ResponseMessage> {
        use iso15118_20::ResponseMessage as M;
        Some(match self {
            MsgKind::SessionSetupRes => M::SessionSetup,
            MsgKind::AuthorizationSetupRes => M::AuthorizationSetup,
            MsgKind::AuthorizationRes => M::Authorization,
            MsgKind::ServiceDiscoveryRes => M::ServiceDiscovery,
            MsgKind::ServiceDetailRes => M::ServiceDetail,
            MsgKind::ServiceSelectionRes => M::ServiceSelection,
            MsgKind::ScheduleExchangeRes => M::ScheduleExchange,
            MsgKind::PowerDeliveryRes => M::PowerDelivery,
            MsgKind::DcChargeParameterDiscoveryRes => M::DcChargeParameterDiscovery,
            MsgKind::DcCableCheckRes => M::DcCableCheck,
            MsgKind::DcPreChargeRes => M::DcPreCharge,
            MsgKind::DcChargeLoopRes => M::DcChargeLoop,
            MsgKind::DcWeldingDetectionRes => M::DcWeldingDetection,
            MsgKind::AcChargeParameterDiscoveryRes => M::AcChargeParameterDiscovery,
            MsgKind::AcChargeLoopRes => M::AcChargeLoop,
            MsgKind::MeteringConfirmationRes => M::MeteringConfirmation,
            MsgKind::SessionStopRes => M::SessionStop,
            _ => return None,
        })
    }
}
