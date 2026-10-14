//! `V2G_CI_CommonMessages` (and the shared `CommonTypes` header): SessionSetup so far.
//!
//! The document grammar contains every global element of CommonMessages, CommonTypes and
//! xmldsig (54), sorted by local name, plus `SE(*)`: a 6-bit root code. `SessionSetupReq`
//! is 35 and `SessionSetupRes` 36; the Authorization pairs are 0..=3 (checked against
//! EXIficient).

use crate::app_protocol::decode_string;
use crate::arena::Arena;
use crate::bits::{BitReader, BitWriter};
use crate::services::*;
use crate::{DecodeError, EncodeError};

pub const MAX_IDENTIFIER_CHARS: usize = 255;

pub(crate) const HEADER: u8 = 0x80;
pub(crate) const ROOT_BITS: u32 = 6;
const ROOT_AUTHORIZATION_REQ: u32 = 0;
const ROOT_AUTHORIZATION_RES: u32 = 1;
const ROOT_AUTHORIZATION_SETUP_REQ: u32 = 2;
const ROOT_AUTHORIZATION_SETUP_RES: u32 = 3;
const ROOT_SESSION_SETUP_REQ: u32 = 35;
const ROOT_SESSION_SETUP_RES: u32 = 36;
pub(crate) const ROOT_SERVICE_DETAIL_REQ: u32 = 29;
pub(crate) const ROOT_SERVICE_DETAIL_RES: u32 = 30;
pub(crate) const ROOT_SERVICE_DISCOVERY_REQ: u32 = 31;
pub(crate) const ROOT_SERVICE_DISCOVERY_RES: u32 = 32;
pub(crate) const ROOT_SERVICE_SELECTION_REQ: u32 = 33;
pub(crate) const ROOT_SERVICE_SELECTION_RES: u32 = 34;

/// `responseCodeType`; the discriminant is the 6-bit EXI code (schema declaration order).
/// Ref: ISO 15118-20:2022 Annex A (response codes; page cited by EcoG, unchecked).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ResponseCode {
    Ok,
    OkCertificateExpiresSoon,
    OkNewSessionEstablished,
    OkOldSessionJoined,
    OkPowerToleranceConfirmed,
    WarningAuthorizationSelectionInvalid,
    WarningCertificateExpired,
    WarningCertificateNotYetValid,
    WarningCertificateRevoked,
    WarningCertificateValidationError,
    WarningChallengeInvalid,
    WarningEIMAuthorizationFailure,
    WarningEMSPUnknown,
    WarningEVPowerProfileViolation,
    WarningGeneralPnCAuthorizationError,
    WarningNoCertificateAvailable,
    WarningNoContractMatchingPCIDFound,
    WarningPowerToleranceNotConfirmed,
    WarningScheduleRenegotiationFailed,
    WarningStandbyNotAllowed,
    WarningWpt,
    Failed,
    FailedAssociationError,
    FailedContactorError,
    FailedEVPowerProfileInvalid,
    FailedEVPowerProfileViolation,
    FailedMeteringSignatureNotValid,
    FailedNoEnergyTransferServiceSelected,
    FailedNoServiceRenegotiationSupported,
    FailedPauseNotAllowed,
    FailedPowerDeliveryNotApplied,
    FailedPowerToleranceNotConfirmed,
    FailedScheduleRenegotiation,
    FailedScheduleSelectionInvalid,
    FailedSequenceError,
    FailedServiceIDInvalid,
    FailedServiceSelectionInvalid,
    FailedSignatureError,
    FailedUnknownSession,
    FailedWrongChargeParameter,
}

const RESPONSE_CODES: [ResponseCode; 40] = [
    ResponseCode::Ok,
    ResponseCode::OkCertificateExpiresSoon,
    ResponseCode::OkNewSessionEstablished,
    ResponseCode::OkOldSessionJoined,
    ResponseCode::OkPowerToleranceConfirmed,
    ResponseCode::WarningAuthorizationSelectionInvalid,
    ResponseCode::WarningCertificateExpired,
    ResponseCode::WarningCertificateNotYetValid,
    ResponseCode::WarningCertificateRevoked,
    ResponseCode::WarningCertificateValidationError,
    ResponseCode::WarningChallengeInvalid,
    ResponseCode::WarningEIMAuthorizationFailure,
    ResponseCode::WarningEMSPUnknown,
    ResponseCode::WarningEVPowerProfileViolation,
    ResponseCode::WarningGeneralPnCAuthorizationError,
    ResponseCode::WarningNoCertificateAvailable,
    ResponseCode::WarningNoContractMatchingPCIDFound,
    ResponseCode::WarningPowerToleranceNotConfirmed,
    ResponseCode::WarningScheduleRenegotiationFailed,
    ResponseCode::WarningStandbyNotAllowed,
    ResponseCode::WarningWpt,
    ResponseCode::Failed,
    ResponseCode::FailedAssociationError,
    ResponseCode::FailedContactorError,
    ResponseCode::FailedEVPowerProfileInvalid,
    ResponseCode::FailedEVPowerProfileViolation,
    ResponseCode::FailedMeteringSignatureNotValid,
    ResponseCode::FailedNoEnergyTransferServiceSelected,
    ResponseCode::FailedNoServiceRenegotiationSupported,
    ResponseCode::FailedPauseNotAllowed,
    ResponseCode::FailedPowerDeliveryNotApplied,
    ResponseCode::FailedPowerToleranceNotConfirmed,
    ResponseCode::FailedScheduleRenegotiation,
    ResponseCode::FailedScheduleSelectionInvalid,
    ResponseCode::FailedSequenceError,
    ResponseCode::FailedServiceIDInvalid,
    ResponseCode::FailedServiceSelectionInvalid,
    ResponseCode::FailedSignatureError,
    ResponseCode::FailedUnknownSession,
    ResponseCode::FailedWrongChargeParameter,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageHeader {
    pub session_id: [u8; 8],
    pub timestamp: u64,
}

/// `RationalNumberType`: `value * 10^exponent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RationalNumber {
    pub exponent: i8,
    pub value: i16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSetupReq<'a> {
    pub header: MessageHeader,
    pub evcc_id: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSetupRes<'a> {
    pub header: MessageHeader,
    pub response_code: ResponseCode,
    pub evse_id: &'a str,
}

/// `authorizationType`. Only EIM is implemented; selecting or decoding PnC is rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Authorization {
    Eim,
    Pnc,
}

/// The one or two authorization services an EVSE offers (`maxOccurs=2`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorizationServices {
    items: [Authorization; 2],
    len: u8,
}

impl AuthorizationServices {
    pub fn new(services: &[Authorization]) -> Option<Self> {
        match *services {
            [a] => Some(Self { items: [a, a], len: 1 }),
            [a, b] => Some(Self { items: [a, b], len: 2 }),
            _ => None,
        }
    }

    pub fn as_slice(&self) -> &[Authorization] {
        &self.items[..usize::from(self.len)]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorizationSetupReq {
    pub header: MessageHeader,
}

/// The authorization mode element of `AuthorizationSetupRes` is a choice between EIM (an
/// empty element) and PnC (challenge and providers). Only EIM is implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorizationSetupRes {
    pub header: MessageHeader,
    pub response_code: ResponseCode,
    pub authorization_services: AuthorizationServices,
    pub certificate_installation_service: bool,
}

/// `AuthorizationReq` for EIM (the mode element is empty).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorizationReq {
    pub header: MessageHeader,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Processing {
    Finished,
    Ongoing,
    OngoingWaitingForCustomerInteraction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorizationRes {
    pub header: MessageHeader,
    pub response_code: ResponseCode,
    pub evse_processing: Processing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message<'a> {
    SessionSetupReq(SessionSetupReq<'a>),
    SessionSetupRes(SessionSetupRes<'a>),
    AuthorizationSetupReq(AuthorizationSetupReq),
    AuthorizationSetupRes(AuthorizationSetupRes),
    AuthorizationReq(AuthorizationReq),
    AuthorizationRes(AuthorizationRes),
    ServiceDiscoveryReq(ServiceDiscoveryReq<'a>),
    ServiceDiscoveryRes(ServiceDiscoveryRes<'a>),
    ServiceDetailReq(ServiceDetailReq),
    ServiceDetailRes(ServiceDetailRes<'a>),
    ServiceSelectionReq(ServiceSelectionReq<'a>),
    ServiceSelectionRes(ServiceSelectionRes),
}

// ---------------------------------------------------------------- encode

pub fn encode_session_setup_req(msg: &SessionSetupReq<'_>, out: &mut [u8]) -> Result<usize, EncodeError> {
    let mut w = start(out)?;
    w.bits(ROOT_SESSION_SETUP_REQ, ROOT_BITS)?;
    encode_header(&mut w, &msg.header)?;
    encode_identifier(&mut w, msg.evcc_id)?;
    w.bits(0, 1)?; // EE(SessionSetupReq)
    Ok(1 + w.finish()?)
}

pub fn encode_session_setup_res(msg: &SessionSetupRes<'_>, out: &mut [u8]) -> Result<usize, EncodeError> {
    let mut w = start(out)?;
    w.bits(ROOT_SESSION_SETUP_RES, ROOT_BITS)?;
    encode_header(&mut w, &msg.header)?;
    w.bits(0, 2)?; // SE(ResponseCode), CH
    w.bits(msg.response_code as u32, 6)?;
    w.bits(0, 1)?; // EE(ResponseCode)
    encode_identifier(&mut w, msg.evse_id)?;
    w.bits(0, 1)?; // EE(SessionSetupRes)
    Ok(1 + w.finish()?)
}

pub fn encode_authorization_setup_req(msg: &AuthorizationSetupReq, out: &mut [u8]) -> Result<usize, EncodeError> {
    let mut w = start(out)?;
    w.bits(ROOT_AUTHORIZATION_SETUP_REQ, ROOT_BITS)?;
    encode_header(&mut w, &msg.header)?;
    w.bits(0, 1)?; // EE(AuthorizationSetupReq)
    Ok(1 + w.finish()?)
}

pub fn encode_authorization_setup_res(msg: &AuthorizationSetupRes, out: &mut [u8]) -> Result<usize, EncodeError> {
    let mut w = start(out)?;
    w.bits(ROOT_AUTHORIZATION_SETUP_RES, ROOT_BITS)?;
    encode_header(&mut w, &msg.header)?;
    encode_response_code(&mut w, msg.response_code)?;
    let services = msg.authorization_services.as_slice();
    for (i, svc) in services.iter().enumerate() {
        // First entry: SE only. Second: SE(AuthorizationServices) is code 0 of the 2-bit
        // state that also offers SE(CertificateInstallationService) as code 1.
        w.bits(0, if i == 0 { 1 } else { 2 })?;
        w.bits(0, 1)?; // CH
        w.bits(*svc as u32, 1)?;
        w.bits(0, 1)?; // EE
    }
    // maxOccurs=2 is unrolled: after one entry the state is {another, next} (2 bits),
    // after two only `next` remains (1 bit).
    if services.len() == 1 {
        w.bits(1, 2)?;
    } else {
        w.bits(0, 1)?;
    }
    w.bits(0, 1)?; // CH
    w.bits(u32::from(msg.certificate_installation_service), 1)?;
    w.bits(0, 1)?; // EE(CertificateInstallationService)
    w.bits(0, 2)?; // choice: SE(EIM_ASResAuthorizationMode)
    w.bits(0, 1)?; // EE(EIM_ASResAuthorizationMode), an empty element
    w.bits(0, 1)?; // EE(AuthorizationSetupRes)
    Ok(1 + w.finish()?)
}

pub fn encode_authorization_req(msg: &AuthorizationReq, out: &mut [u8]) -> Result<usize, EncodeError> {
    let mut w = start(out)?;
    w.bits(ROOT_AUTHORIZATION_REQ, ROOT_BITS)?;
    encode_header(&mut w, &msg.header)?;
    w.bits(0, 2)?; // SE(SelectedAuthorizationService), CH
    w.bits(Authorization::Eim as u32, 1)?;
    w.bits(0, 1)?; // EE
    w.bits(0, 2)?; // choice: SE(EIM_AReqAuthorizationMode)
    w.bits(0, 1)?; // EE(EIM_AReqAuthorizationMode)
    w.bits(0, 1)?; // EE(AuthorizationReq)
    Ok(1 + w.finish()?)
}

pub fn encode_authorization_res(msg: &AuthorizationRes, out: &mut [u8]) -> Result<usize, EncodeError> {
    let mut w = start(out)?;
    w.bits(ROOT_AUTHORIZATION_RES, ROOT_BITS)?;
    encode_header(&mut w, &msg.header)?;
    encode_response_code(&mut w, msg.response_code)?;
    w.bits(0, 2)?; // SE(EVSEProcessing), CH
    w.bits(msg.evse_processing as u32, 2)?;
    w.bits(0, 1)?; // EE(EVSEProcessing)
    w.bits(0, 1)?; // EE(AuthorizationRes)
    Ok(1 + w.finish()?)
}

/// SE(ResponseCode), CH, 6-bit enum, EE.
pub(crate) fn encode_response_code(w: &mut BitWriter<'_>, code: ResponseCode) -> Result<(), EncodeError> {
    w.bits(0, 2)?;
    w.bits(code as u32, 6)?;
    w.bits(0, 1)
}

pub(crate) fn start(out: &mut [u8]) -> Result<BitWriter<'_>, EncodeError> {
    let (first, rest) = out.split_first_mut().ok_or(EncodeError::BufferTooSmall)?;
    *first = HEADER;
    Ok(BitWriter::new(rest))
}

pub(crate) fn encode_header(w: &mut BitWriter<'_>, h: &MessageHeader) -> Result<(), EncodeError> {
    w.bits(0, 1)?; // SE(Header)
    w.bits(0, 2)?; // SE(SessionID), CH
    w.uint(8)?; // hexBinary length
    for b in h.session_id {
        w.bits(u32::from(b), 8)?;
    }
    w.bits(0, 3)?; // EE(SessionID), SE(TimeStamp), CH
    w.uint64(h.timestamp)?;
    w.bits(0, 1)?; // EE(TimeStamp)
    w.bits(1, 2)?; // EE(Header): the state also offers the optional Signature
    Ok(())
}

/// `EVCCID` / `EVSEID`: SE, CH, string, EE.
fn encode_identifier(w: &mut BitWriter<'_>, s: &str) -> Result<(), EncodeError> {
    let chars = s.chars().count();
    if chars > MAX_IDENTIFIER_CHARS {
        return Err(EncodeError::InvalidValue("identifier longer than 255 characters"));
    }
    // EXIficient codes an empty string element with event code `1` instead of CH, but the
    // vectors so far cannot tell whether that is an escape code (and how wide the second
    // part is) or an extra EE production. Refuse rather than guess.
    if chars == 0 {
        return Err(EncodeError::InvalidValue("empty identifier not supported yet"));
    }
    w.bits(0, 2)?; // SE, CH
    w.uint(chars as u32 + 2)?; // string table miss; a message holds only one string
    for c in s.chars() {
        w.uint(c as u32)?;
    }
    w.bits(0, 1)?; // EE
    Ok(())
}

// ---------------------------------------------------------------- decode

pub fn decode<'s>(buf: &[u8], arena: &'s Arena<'_>) -> Result<Message<'s>, DecodeError> {
    let (&header, body) = buf.split_first().ok_or(DecodeError::UnexpectedEof)?;
    if header != HEADER {
        return Err(DecodeError::Unsupported("EXI header with options or cookie"));
    }
    let mut r = BitReader::new(body);
    match r.bits(ROOT_BITS)? {
        ROOT_SESSION_SETUP_REQ => {
            let header = decode_header(&mut r)?;
            let evcc_id = decode_identifier(&mut r, arena)?;
            expect_zero(&mut r, 1)?; // EE(SessionSetupReq)
            Ok(Message::SessionSetupReq(SessionSetupReq { header, evcc_id }))
        }
        ROOT_SESSION_SETUP_RES => {
            let header = decode_header(&mut r)?;
            expect_zero(&mut r, 2)?; // SE(ResponseCode), CH
            let response_code = *RESPONSE_CODES
                .get(r.bits(6)? as usize)
                .ok_or(DecodeError::InvalidValue("unknown ResponseCode"))?;
            expect_zero(&mut r, 1)?; // EE(ResponseCode)
            let evse_id = decode_identifier(&mut r, arena)?;
            expect_zero(&mut r, 1)?; // EE(SessionSetupRes)
            Ok(Message::SessionSetupRes(SessionSetupRes { header, response_code, evse_id }))
        }
        ROOT_AUTHORIZATION_SETUP_REQ => {
            let header = decode_header(&mut r)?;
            expect_zero(&mut r, 1)?; // EE(AuthorizationSetupReq)
            Ok(Message::AuthorizationSetupReq(AuthorizationSetupReq { header }))
        }
        ROOT_AUTHORIZATION_SETUP_RES => {
            let header = decode_header(&mut r)?;
            let response_code = decode_response_code(&mut r)?;
            let mut items = [Authorization::Eim; 2];
            let mut len = 0usize;
            expect_zero(&mut r, 1)?; // SE(AuthorizationServices)
            loop {
                expect_zero(&mut r, 1)?; // CH
                items[len] = decode_authorization(&mut r)?;
                len += 1;
                expect_zero(&mut r, 1)?; // EE
                if len == 2 {
                    expect_zero(&mut r, 1)?; // only SE(CertificateInstallationService) remains
                    break;
                }
                match r.bits(2)? {
                    0 => {} // another AuthorizationServices
                    1 => break, // SE(CertificateInstallationService)
                    _ => return Err(DecodeError::Unsupported("unexpected event code")),
                }
            }
            expect_zero(&mut r, 1)?; // CH
            let certificate_installation_service = r.bit()?;
            expect_zero(&mut r, 1)?; // EE
            match r.bits(2)? {
                0 => {}
                1 => return Err(DecodeError::Unsupported("PnC authorization mode")),
                _ => return Err(DecodeError::Unsupported("unexpected event code")),
            }
            expect_zero(&mut r, 2)?; // EE(EIM_ASResAuthorizationMode), EE(AuthorizationSetupRes)
            let authorization_services = AuthorizationServices { items, len: len as u8 };
            Ok(Message::AuthorizationSetupRes(AuthorizationSetupRes {
                header,
                response_code,
                authorization_services,
                certificate_installation_service,
            }))
        }
        ROOT_AUTHORIZATION_REQ => {
            let header = decode_header(&mut r)?;
            expect_zero(&mut r, 2)?; // SE(SelectedAuthorizationService), CH
            if decode_authorization(&mut r)? != Authorization::Eim {
                return Err(DecodeError::Unsupported("PnC authorization"));
            }
            expect_zero(&mut r, 1)?; // EE
            match r.bits(2)? {
                0 => {}
                1 => return Err(DecodeError::Unsupported("PnC authorization mode")),
                _ => return Err(DecodeError::Unsupported("unexpected event code")),
            }
            expect_zero(&mut r, 2)?; // EE(EIM_AReqAuthorizationMode), EE(AuthorizationReq)
            Ok(Message::AuthorizationReq(AuthorizationReq { header }))
        }
        ROOT_AUTHORIZATION_RES => {
            let header = decode_header(&mut r)?;
            let response_code = decode_response_code(&mut r)?;
            expect_zero(&mut r, 2)?; // SE(EVSEProcessing), CH
            let evse_processing = match r.bits(2)? {
                0 => Processing::Finished,
                1 => Processing::Ongoing,
                2 => Processing::OngoingWaitingForCustomerInteraction,
                _ => return Err(DecodeError::InvalidValue("unknown EVSEProcessing")),
            };
            expect_zero(&mut r, 2)?; // EE(EVSEProcessing), EE(AuthorizationRes)
            Ok(Message::AuthorizationRes(AuthorizationRes { header, response_code, evse_processing }))
        }
        ROOT_SERVICE_DISCOVERY_REQ => decode_service_discovery_req(&mut r, arena).map(Message::ServiceDiscoveryReq),
        ROOT_SERVICE_DISCOVERY_RES => decode_service_discovery_res(&mut r, arena).map(Message::ServiceDiscoveryRes),
        ROOT_SERVICE_DETAIL_REQ => decode_service_detail_req(&mut r).map(Message::ServiceDetailReq),
        ROOT_SERVICE_DETAIL_RES => decode_service_detail_res(&mut r, arena).map(Message::ServiceDetailRes),
        ROOT_SERVICE_SELECTION_REQ => decode_service_selection_req(&mut r, arena).map(Message::ServiceSelectionReq),
        ROOT_SERVICE_SELECTION_RES => decode_service_selection_res(&mut r).map(Message::ServiceSelectionRes),
        _ => Err(DecodeError::Unsupported("message not implemented")),
    }
}

/// SE(ResponseCode), CH, 6-bit enum, EE.
pub(crate) fn decode_response_code(r: &mut BitReader<'_>) -> Result<ResponseCode, DecodeError> {
    expect_zero(r, 2)?;
    let code = *RESPONSE_CODES
        .get(r.bits(6)? as usize)
        .ok_or(DecodeError::InvalidValue("unknown ResponseCode"))?;
    expect_zero(r, 1)?;
    Ok(code)
}

fn decode_authorization(r: &mut BitReader<'_>) -> Result<Authorization, DecodeError> {
    Ok(if r.bit()? { Authorization::Pnc } else { Authorization::Eim })
}

pub(crate) fn expect_zero(r: &mut BitReader<'_>, n: u32) -> Result<(), DecodeError> {
    match r.bits(n)? {
        0 => Ok(()),
        _ => Err(DecodeError::Unsupported("unexpected event code")),
    }
}

pub(crate) fn decode_header(r: &mut BitReader<'_>) -> Result<MessageHeader, DecodeError> {
    expect_zero(r, 1)?; // SE(Header)
    expect_zero(r, 2)?; // SE(SessionID), CH
    if r.uint()? != 8 {
        return Err(DecodeError::InvalidValue("SessionID must be 8 bytes"));
    }
    let mut session_id = [0u8; 8];
    for b in &mut session_id {
        *b = r.bits(8)? as u8;
    }
    expect_zero(r, 3)?; // EE(SessionID), SE(TimeStamp), CH
    let timestamp = r.uint64()?;
    expect_zero(r, 1)?; // EE(TimeStamp)
    match r.bits(2)? {
        1 => Ok(MessageHeader { session_id, timestamp }), // EE(Header)
        0 => Err(DecodeError::Unsupported("message signature")),
        _ => Err(DecodeError::Unsupported("unexpected event code")),
    }
}

fn decode_identifier<'s>(r: &mut BitReader<'_>, arena: &'s Arena<'_>) -> Result<&'s str, DecodeError> {
    expect_zero(r, 1)?; // SE
    if r.bit()? {
        return Err(DecodeError::Unsupported("empty string element"));
    }
    let len = r
        .uint()?
        .checked_sub(2)
        .ok_or(DecodeError::Unsupported("string table hit"))? as usize;
    if len == 0 {
        return Err(DecodeError::Unsupported("empty string element"));
    }
    if len > MAX_IDENTIFIER_CHARS {
        return Err(DecodeError::InvalidValue("identifier longer than 255 characters"));
    }
    let s = decode_string(r, len, Some(arena))?;
    expect_zero(r, 1)?; // EE
    Ok(s)
}

/// A `RationalNumberType` element whose start tag is event code `se_code` in a `se_bits`-wide
/// state: `Exponent` (8-bit offset) and `Value` (EXI integer).
pub(crate) fn put_rational(w: &mut BitWriter<'_>, se_code: u32, se_bits: u32, v: RationalNumber) -> Result<(), EncodeError> {
    w.bits(se_code, se_bits)?;
    w.bits(0, 2)?; // SE(Exponent), CH
    w.bits((i32::from(v.exponent) + 128) as u32, 8)?;
    w.bits(0, 3)?; // EE, SE(Value), CH
    w.int(i32::from(v.value))?;
    w.bits(0, 2) // EE(Value), EE(rational)
}

/// Reads what [`put_rational`] wrote for `se_code == 0` (the caller checks the start tag).
pub(crate) fn get_rational(r: &mut BitReader<'_>, se_bits: u32) -> Result<RationalNumber, DecodeError> {
    expect_zero(r, se_bits + 2)?;
    let exponent = (r.bits(8)? as i32 - 128) as i8;
    expect_zero(r, 3)?;
    let value = i16::try_from(r.int()?).map_err(|_| DecodeError::InvalidValue("value exceeds short"))?;
    expect_zero(r, 2)?;
    Ok(RationalNumber { exponent, value })
}
