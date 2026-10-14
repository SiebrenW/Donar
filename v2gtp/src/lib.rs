//! V2G Transfer Protocol framing and SECC Discovery Protocol (SDP) messages.
//!
//! `no_std`, no allocation, no I/O. Shared by the EV side and the EVSE simulator.
#![no_std]

#[cfg(test)]
extern crate std;

pub mod sdp;

pub const HEADER_LEN: usize = 8;
const VERSION: u8 = 0x01;
const INVERSE_VERSION: u8 = 0xFE;

/// V2GTP payload type. Unknown values are preserved so callers can decide what to do.
/// In ISO 15118-20 the payload type also selects the schema (grammar) the EXI payload is
/// decoded with: `SAP` uses the AppProtocol schema, `MAINSTREAM` CommonMessages, and so on.
///
/// Ref: ISO 15118-20:2022 Table 12 (values as listed by EcoG's implementation; not yet
/// checked against the standard text). `SDP_REQUEST`/`SDP_RESPONSE` are common to
/// ISO 15118-2, -20 and DIN SPEC 70121.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadType(pub u16);

impl PayloadType {
    pub const SAP: Self = Self(0x8001);
    pub const MAINSTREAM: Self = Self(0x8002);
    pub const AC_MAINSTREAM: Self = Self(0x8003);
    pub const DC_MAINSTREAM: Self = Self(0x8004);
    pub const ACDP_MAINSTREAM: Self = Self(0x8005);
    pub const WPT_MAINSTREAM: Self = Self(0x8006);
    pub const SDP_REQUEST: Self = Self(0x9000);
    pub const SDP_RESPONSE: Self = Self(0x9001);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    BadVersion,
    BadInverseVersion,
    PayloadTooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferTooSmall;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    pub payload_type: PayloadType,
    pub payload: &'a [u8],
    /// Bytes of the input this frame occupied (header + payload).
    pub consumed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parse<'a> {
    /// Not enough bytes yet; `needed` is the total length required from the start of the
    /// buffer (just the header until the length field has been read).
    Incomplete { needed: usize },
    Frame(Frame<'a>),
}

/// Parses one frame from the start of `buf` (a TCP stream buffer or a UDP datagram).
/// `max_payload` bounds the accepted payload length, so a hostile length field cannot make
/// the host wait for or buffer gigabytes.
pub fn parse(buf: &[u8], max_payload: usize) -> Result<Parse<'_>, FrameError> {
    if buf.first().is_some_and(|&v| v != VERSION) {
        return Err(FrameError::BadVersion);
    }
    if buf.get(1).is_some_and(|&v| v != INVERSE_VERSION) {
        return Err(FrameError::BadInverseVersion);
    }
    let Some(header) = buf.get(..HEADER_LEN) else {
        return Ok(Parse::Incomplete { needed: HEADER_LEN });
    };
    let len = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if len > max_payload {
        return Err(FrameError::PayloadTooLarge);
    }
    let total = HEADER_LEN + len;
    match buf.get(HEADER_LEN..total) {
        Some(payload) => Ok(Parse::Frame(Frame {
            payload_type: PayloadType(u16::from_be_bytes([header[2], header[3]])),
            payload,
            consumed: total,
        })),
        None => Ok(Parse::Incomplete { needed: total }),
    }
}

/// Writes the 8-byte header at the start of `out`. Codecs encode their payload at
/// `out[HEADER_LEN..]` first, then call this with the payload length.
pub fn write_header(out: &mut [u8], payload_type: PayloadType, payload_len: usize) -> Result<(), BufferTooSmall> {
    let header = out.get_mut(..HEADER_LEN).ok_or(BufferTooSmall)?;
    let len = u32::try_from(payload_len).map_err(|_| BufferTooSmall)?;
    header[0] = VERSION;
    header[1] = INVERSE_VERSION;
    header[2..4].copy_from_slice(&payload_type.0.to_be_bytes());
    header[4..8].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

/// Copies `payload` behind a header into `out`; returns the total length.
pub fn frame(payload_type: PayloadType, payload: &[u8], out: &mut [u8]) -> Result<usize, BufferTooSmall> {
    let total = HEADER_LEN + payload.len();
    out.get_mut(HEADER_LEN..total).ok_or(BufferTooSmall)?.copy_from_slice(payload);
    write_header(out, payload_type, payload.len())?;
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip() {
        let mut out = [0u8; 32];
        let n = frame(PayloadType::DC_MAINSTREAM, &[1, 2, 3], &mut out).unwrap();
        assert_eq!(&out[..n], &[0x01, 0xFE, 0x80, 0x04, 0, 0, 0, 3, 1, 2, 3]);
        let Parse::Frame(f) = parse(&out[..n], 16).unwrap() else { panic!() };
        assert_eq!((f.payload_type, f.payload, f.consumed), (PayloadType::DC_MAINSTREAM, &[1u8, 2, 3][..], 11));
    }

    #[test]
    fn two_frames_in_a_stream_buffer() {
        let mut out = [0u8; 32];
        let a = frame(PayloadType::SAP, &[9], &mut out).unwrap();
        let b = frame(PayloadType::MAINSTREAM, &[7, 7], &mut out[a..]).unwrap();
        let Parse::Frame(first) = parse(&out[..a + b], 16).unwrap() else { panic!() };
        let Parse::Frame(second) = parse(&out[first.consumed..a + b], 16).unwrap() else { panic!() };
        assert_eq!((first.payload, second.payload), (&[9u8][..], &[7u8, 7][..]));
    }

    #[test]
    fn partial_input_reports_what_is_needed() {
        let mut out = [0u8; 32];
        let n = frame(PayloadType::SAP, &[1, 2, 3, 4], &mut out).unwrap();
        for cut in 0..n {
            let want = if cut < HEADER_LEN { HEADER_LEN } else { n };
            assert_eq!(parse(&out[..cut], 16), Ok(Parse::Incomplete { needed: want }), "cut {cut}");
        }
    }

    #[test]
    fn rejects_bad_headers_early() {
        assert_eq!(parse(&[0x02], 16), Err(FrameError::BadVersion));
        assert_eq!(parse(&[0x01, 0xFF], 16), Err(FrameError::BadInverseVersion));
        let huge = [0x01, 0xFE, 0x80, 0x01, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(parse(&huge, 1 << 16), Err(FrameError::PayloadTooLarge));
    }

    #[test]
    fn small_output_buffer_is_an_error() {
        assert_eq!(frame(PayloadType::SAP, &[1, 2, 3], &mut [0u8; 10]), Err(BufferTooSmall));
        assert_eq!(write_header(&mut [0u8; 4], PayloadType::SAP, 0), Err(BufferTooSmall));
    }
}
