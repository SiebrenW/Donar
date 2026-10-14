//! `supportedAppProtocol` handshake (namespace `urn:iso:15118:2:2010:AppProtocol`).
//!
//! Event-code widths below are for default (non-strict) fidelity, as real stacks use and
//! as verified against EXIficient: a state with one production plus the escape code takes
//! 1 bit, the document root and the repeat state take 2 bits. Every typed value is
//! preceded by a 1-bit CH code and followed by a 1-bit EE code.

use crate::arena::Arena;
use crate::bits::{BitReader, BitWriter};
use crate::{DecodeError, EncodeError};

pub const MAX_APP_PROTOCOLS: usize = 20;
pub const MAX_NAMESPACE_CHARS: usize = 100;

/// EXI header byte: distinguishing bits `10`, no options, version 0.
const HEADER: u8 = 0x80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppProtocol<'a> {
    pub namespace: &'a str,
    pub version_major: u32,
    pub version_minor: u32,
    pub schema_id: u8,
    /// 1..=20
    pub priority: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedAppProtocolReq<'a> {
    /// 1..=20 entries.
    pub app_protocols: &'a [AppProtocol<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseCode {
    OkSuccessfulNegotiation,
    OkSuccessfulNegotiationWithMinorDeviation,
    FailedNoNegotiation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupportedAppProtocolRes {
    pub response_code: ResponseCode,
    pub schema_id: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message<'a> {
    Req(SupportedAppProtocolReq<'a>),
    Res(SupportedAppProtocolRes),
}

/// EXI string table for `ProtocolNamespace` values seen so far in this stream. A repeated
/// value is coded as a hit (`0`, then a compact index) instead of the full string.
struct StringTable<'s> {
    entries: [&'s str; MAX_APP_PROTOCOLS],
    len: usize,
}

impl<'s> StringTable<'s> {
    fn new() -> Self {
        Self { entries: [""; MAX_APP_PROTOCOLS], len: 0 }
    }

    fn position(&self, s: &str) -> Option<usize> {
        self.entries[..self.len].iter().position(|e| *e == s)
    }

    fn push(&mut self, s: &'s str) {
        self.entries[self.len] = s;
        self.len += 1;
    }

    /// Bits of a compact identifier for the current table size.
    fn index_bits(&self) -> u32 {
        if self.len <= 1 { 0 } else { usize::BITS - (self.len - 1).leading_zeros() }
    }
}

// ---------------------------------------------------------------- encode

pub fn encode_req(msg: &SupportedAppProtocolReq<'_>, out: &mut [u8]) -> Result<usize, EncodeError> {
    let n = msg.app_protocols.len();
    if n == 0 || n > MAX_APP_PROTOCOLS {
        return Err(EncodeError::InvalidValue("AppProtocol count must be 1..=20"));
    }
    let first = out.first_mut().ok_or(EncodeError::BufferTooSmall)?;
    *first = HEADER;
    let mut w = BitWriter::new(&mut out[1..]);
    w.bits(0, 2)?; // SE(supportedAppProtocolReq)
    let mut table = StringTable::new();
    for (i, ap) in msg.app_protocols.iter().enumerate() {
        if i == 0 {
            w.bits(0, 1)?; // SE(AppProtocol)
        } else {
            w.bits(0, 2)?; // another AppProtocol
        }
        encode_app_protocol(&mut w, ap, &mut table)?;
    }
    // The grammar unrolls maxOccurs=20: only EE remains after the 20th entry.
    if n < MAX_APP_PROTOCOLS {
        w.bits(1, 2)?; // EE
    } else {
        w.bits(0, 1)?; // EE
    }
    Ok(1 + w.finish()?)
}

fn encode_app_protocol<'a>(
    w: &mut BitWriter<'_>,
    ap: &AppProtocol<'a>,
    table: &mut StringTable<'a>,
) -> Result<(), EncodeError> {
    let chars = ap.namespace.chars().count();
    if chars > MAX_NAMESPACE_CHARS {
        return Err(EncodeError::InvalidValue("ProtocolNamespace longer than 100 characters"));
    }
    if !(1..=20).contains(&ap.priority) {
        return Err(EncodeError::InvalidValue("Priority must be 1..=20"));
    }
    w.bits(0, 2)?; // SE(ProtocolNamespace), CH
    match table.position(ap.namespace) {
        Some(index) => {
            w.uint(0)?; // local value hit
            w.bits(index as u32, table.index_bits())?;
        }
        None => {
            w.uint(chars as u32 + 2)?; // miss
            for c in ap.namespace.chars() {
                w.uint(c as u32)?;
            }
            table.push(ap.namespace);
        }
    }
    w.bits(0, 3)?; // EE, SE(VersionNumberMajor), CH
    w.uint(ap.version_major)?;
    w.bits(0, 3)?; // EE, SE(VersionNumberMinor), CH
    w.uint(ap.version_minor)?;
    w.bits(0, 3)?; // EE, SE(SchemaID), CH
    w.bits(u32::from(ap.schema_id), 8)?;
    w.bits(0, 3)?; // EE, SE(Priority), CH
    w.bits(u32::from(ap.priority) - 1, 5)?;
    w.bits(0, 2)?; // EE(Priority), EE(AppProtocol)
    Ok(())
}

pub fn encode_res(msg: &SupportedAppProtocolRes, out: &mut [u8]) -> Result<usize, EncodeError> {
    let first = out.first_mut().ok_or(EncodeError::BufferTooSmall)?;
    *first = HEADER;
    let mut w = BitWriter::new(&mut out[1..]);
    w.bits(1, 2)?; // SE(supportedAppProtocolRes)
    w.bits(0, 2)?; // SE(ResponseCode), CH
    w.bits(
        match msg.response_code {
            ResponseCode::OkSuccessfulNegotiation => 0,
            ResponseCode::OkSuccessfulNegotiationWithMinorDeviation => 1,
            ResponseCode::FailedNoNegotiation => 2,
        },
        2,
    )?;
    w.bits(0, 1)?; // EE(ResponseCode)
    match msg.schema_id {
        Some(id) => {
            w.bits(0, 3)?; // SE(SchemaID) (2 bits), CH
            w.bits(u32::from(id), 8)?;
            w.bits(0, 2)?; // EE(SchemaID), EE(Res)
        }
        None => w.bits(1, 2)?, // EE(Res)
    }
    Ok(1 + w.finish()?)
}

// ---------------------------------------------------------------- decode

/// Decodes either message. Strings are stored in `arena`.
pub fn decode<'s>(buf: &[u8], arena: &'s Arena<'_>) -> Result<Message<'s>, DecodeError> {
    let (&header, body) = buf.split_first().ok_or(DecodeError::UnexpectedEof)?;
    if header != HEADER {
        return Err(DecodeError::Unsupported("EXI header with options or cookie"));
    }
    let mut r = BitReader::new(body);
    match r.bits(2)? {
        0 => decode_req(&mut r, arena).map(Message::Req),
        1 => decode_res(&mut r).map(Message::Res),
        _ => Err(DecodeError::Unsupported("escape event code at document root")),
    }
}

/// Reads `n` event-code bits that must all be zero (the schema-expected production).
fn expect_zero(r: &mut BitReader<'_>, n: u32) -> Result<(), DecodeError> {
    match r.bits(n)? {
        0 => Ok(()),
        _ => Err(DecodeError::Unsupported("unexpected event code")),
    }
}

fn decode_req<'s>(
    r: &mut BitReader<'_>,
    arena: &'s Arena<'_>,
) -> Result<SupportedAppProtocolReq<'s>, DecodeError> {
    // Pass 1 validates and counts without storing strings, so pass 2 can size the list.
    let mut scan = r.clone();
    let mut scan_table = StringTable::new();
    let mut count = 0usize;
    loop {
        // 1 bit before the first entry and once all 20 are present (only EE left), else 2.
        let (code, ee) = match count {
            0 => (scan.bits(1)?, u32::MAX),
            MAX_APP_PROTOCOLS => (scan.bits(1)?, 0),
            _ => (scan.bits(2)?, 1),
        };
        if code == ee {
            break;
        }
        if code != 0 || count == MAX_APP_PROTOCOLS {
            return Err(DecodeError::Unsupported("unexpected event code"));
        }
        decode_app_protocol(&mut scan, None, &mut scan_table)?;
        count += 1;
    }
    let mut table = StringTable::new();
    let app_protocols = arena.alloc_slice_with(count, |i| {
        // Event codes were validated in pass 1; entry 0 uses 1 bit, later ones 2.
        r.bits(if i == 0 { 1 } else { 2 })?;
        decode_app_protocol(r, Some(arena), &mut table)
    })?;
    // The closing EE was validated in pass 1.
    Ok(SupportedAppProtocolReq { app_protocols })
}

fn decode_app_protocol<'s>(
    r: &mut BitReader<'_>,
    arena: Option<&'s Arena<'_>>,
    table: &mut StringTable<'s>,
) -> Result<AppProtocol<'s>, DecodeError> {
    expect_zero(r, 2)?; // SE(ProtocolNamespace), CH
    // Only this element carries strings, so the local and global tables are identical and
    // both hit codes (0 and 1) address the same entries.
    let namespace = match r.uint()? {
        hit @ (0 | 1) => {
            let _ = hit;
            let index = r.bits(table.index_bits())? as usize;
            *table.entries[..table.len]
                .get(index)
                .ok_or(DecodeError::InvalidValue("string table index out of range"))?
        }
        n => {
            let len = (n - 2) as usize;
            if len > MAX_NAMESPACE_CHARS {
                return Err(DecodeError::InvalidValue("ProtocolNamespace longer than 100 characters"));
            }
            let s = decode_string(r, len, arena)?;
            table.push(s);
            s
        }
    };
    expect_zero(r, 3)?;
    let version_major = r.uint()?;
    expect_zero(r, 3)?;
    let version_minor = r.uint()?;
    expect_zero(r, 3)?;
    let schema_id = r.bits(8)? as u8;
    expect_zero(r, 3)?;
    let priority = r.bits(5)? as u8 + 1;
    if priority > 20 {
        return Err(DecodeError::InvalidValue("Priority must be 1..=20"));
    }
    expect_zero(r, 2)?;
    Ok(AppProtocol { namespace, version_major, version_minor, schema_id, priority })
}

/// Reads `len` code points. With an arena the UTF-8 is stored there; without one the
/// characters are validated and skipped.
pub(crate) fn decode_string<'s>(
    r: &mut BitReader<'_>,
    len: usize,
    arena: Option<&'s Arena<'_>>,
) -> Result<&'s str, DecodeError> {
    let char_at = |r: &mut BitReader<'_>| {
        char::from_u32(r.uint()?).ok_or(DecodeError::InvalidValue("invalid code point"))
    };
    let Some(arena) = arena else {
        for _ in 0..len {
            char_at(r)?;
        }
        return Ok("");
    };
    let mut sizing = r.clone();
    let mut bytes = 0;
    for _ in 0..len {
        bytes += char_at(&mut sizing)?.len_utf8();
    }
    let out = arena.alloc_bytes(bytes)?;
    let mut at = 0;
    for _ in 0..len {
        at += char_at(r)?.encode_utf8(&mut out[at..]).len();
    }
    core::str::from_utf8(out).map_err(|_| DecodeError::InvalidValue("invalid UTF-8"))
}

fn decode_res(r: &mut BitReader<'_>) -> Result<SupportedAppProtocolRes, DecodeError> {
    expect_zero(r, 2)?; // SE(ResponseCode), CH
    let response_code = match r.bits(2)? {
        0 => ResponseCode::OkSuccessfulNegotiation,
        1 => ResponseCode::OkSuccessfulNegotiationWithMinorDeviation,
        2 => ResponseCode::FailedNoNegotiation,
        _ => return Err(DecodeError::InvalidValue("unknown ResponseCode")),
    };
    expect_zero(r, 1)?; // EE(ResponseCode)
    match r.bits(2)? {
        0 => {
            expect_zero(r, 1)?; // CH
            let id = r.bits(8)? as u8;
            expect_zero(r, 2)?; // EE(SchemaID), EE(Res)
            Ok(SupportedAppProtocolRes { response_code, schema_id: Some(id) })
        }
        1 => Ok(SupportedAppProtocolRes { response_code, schema_id: None }),
        _ => Err(DecodeError::Unsupported("unexpected event code")),
    }
}
