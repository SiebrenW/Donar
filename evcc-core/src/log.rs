//! Typed log records. The core has no logging dependency: it hands records to a [`LogSink`]
//! and adapters render them (`log`/`tracing`, `defmt`, a ring buffer) or tests assert on
//! them. Records may borrow from the message being processed, because the sink is called
//! synchronously.

use exi::DecodeError;
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
    /// The `EVSEProcessing` value of an `AuthorizationRes`.
    EvseProcessing { processing: Processing },
    /// The energy transfer service and parameter set the EVSE accepted.
    ServiceSelected { service_id: u16, parameter_set_id: u16 },
    RxPayload { payload_type: u16, payload: &'a [u8] },
    DecodeFailed { state: State, error: DecodeError },
    Ended { reason: EndReason },
}

impl Record<'_> {
    pub fn level(&self) -> Level {
        match self {
            Record::Started { .. }
            | Record::StateChange { .. }
            | Record::SessionSetUp { .. }
            | Record::ServiceSelected { .. } => Level::Info,
            Record::Response { code, .. } if !crate::session::is_ok(*code) => Level::Info,
            Record::Response { .. } | Record::EvseProcessing { .. } | Record::MsgTx { .. } | Record::MsgRx { .. } => {
                Level::Debug
            }
            Record::RxPayload { .. } => Level::Trace,
            Record::DecodeFailed { .. } => Level::Error,
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
