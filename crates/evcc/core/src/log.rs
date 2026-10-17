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
}

impl Record<'_> {
    pub fn level(&self) -> Level {
        match self {
            Record::Started { .. }
            | Record::StateChange { .. }
            | Record::SessionSetUp { .. }
            | Record::SessionResumed { .. }
            | Record::AuthorizationSelected { .. }
            | Record::ServiceSelected { .. } => Level::Info,
            Record::Response { code, .. } if !crate::session::is_ok(*code) => Level::Info,
            Record::Response { .. } | Record::EvseProcessing { .. } | Record::MsgTx { .. } | Record::MsgRx { .. } => {
                Level::Debug
            }
            Record::RxPayload { .. } => Level::Trace,
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
            MsgKind::SessionStopRes => M::SessionStop,
            _ => return None,
        })
    }
}
