//! Typed log records. The core has no logging dependency: it hands records to a [`LogSink`]
//! and adapters render them (`log`/`tracing`, `defmt`, a ring buffer) or tests assert on
//! them. Records may borrow from the message being processed, because the sink is called
//! synchronously.

use exi::DecodeError;
use iso15118_20::common::{Authorization, ParameterSet, Service};
use iso15118_20::common_types::{Processing, ResponseCode};

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
            Record::DecodeFailed { .. } | Record::NoAuthorization { .. } | Record::NoService { .. } | Record::NoParameterSet { .. } => Level::Error,
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
