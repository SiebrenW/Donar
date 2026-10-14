//! `V2G_CI_CommonMessages` (and the shared `CommonTypes` header): SessionSetup so far.
//!
//! The document grammar contains every global element of CommonMessages, CommonTypes and
//! xmldsig (54), sorted by local name, plus `SE(*)`: a 6-bit root code. `SessionSetupReq`
//! is 35 and `SessionSetupRes` 36 (checked against EXIficient).

use crate::app_protocol::decode_string;
use crate::arena::Arena;
use crate::bits::{BitReader, BitWriter};
use crate::{DecodeError, EncodeError};

pub const MAX_IDENTIFIER_CHARS: usize = 255;

const HEADER: u8 = 0x80;
const ROOT_BITS: u32 = 6;
const ROOT_SESSION_SETUP_REQ: u32 = 35;
const ROOT_SESSION_SETUP_RES: u32 = 36;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message<'a> {
    SessionSetupReq(SessionSetupReq<'a>),
    SessionSetupRes(SessionSetupRes<'a>),
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

fn start(out: &mut [u8]) -> Result<BitWriter<'_>, EncodeError> {
    let (first, rest) = out.split_first_mut().ok_or(EncodeError::BufferTooSmall)?;
    *first = HEADER;
    Ok(BitWriter::new(rest))
}

fn encode_header(w: &mut BitWriter<'_>, h: &MessageHeader) -> Result<(), EncodeError> {
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
        _ => Err(DecodeError::Unsupported("message not implemented")),
    }
}

fn expect_zero(r: &mut BitReader<'_>, n: u32) -> Result<(), DecodeError> {
    match r.bits(n)? {
        0 => Ok(()),
        _ => Err(DecodeError::Unsupported("unexpected event code")),
    }
}

fn decode_header(r: &mut BitReader<'_>) -> Result<MessageHeader, DecodeError> {
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
