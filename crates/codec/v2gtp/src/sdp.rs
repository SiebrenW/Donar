//! SECC Discovery Protocol messages (UDP, multicast `FF02::1`, port 15118).

use crate::{BufferTooSmall, HEADER_LEN, PayloadType, write_header};

pub const SDP_PORT: u16 = 15118;
/// All-nodes link-local multicast group the EVCC sends its request to.
pub const MULTICAST_ADDR: [u8; 16] = [0xff, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    Tls,
    NoTls,
}

impl Security {
    fn to_byte(self) -> u8 {
        match self {
            Security::Tls => 0x00,
            Security::NoTls => 0x10,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x00 => Some(Security::Tls),
            0x10 => Some(Security::NoTls),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Tcp,
    /// Only allowed for SDP itself, never for V2G messages.
    Udp,
}

impl Transport {
    fn to_byte(self) -> u8 {
        match self {
            Transport::Tcp => 0x00,
            Transport::Udp => 0x10,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x00 => Some(Transport::Tcp),
            0x10 => Some(Transport::Udp),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdpError {
    /// Payload has the wrong length.
    BadLength,
    /// Security or transport byte is not a defined value.
    BadValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub security: Security,
    pub transport: Transport,
}

impl Request {
    pub const PAYLOAD_LEN: usize = 2;

    /// Writes the complete datagram (V2GTP header + payload); returns its length.
    pub fn to_datagram(&self, out: &mut [u8]) -> Result<usize, BufferTooSmall> {
        let total = HEADER_LEN + Self::PAYLOAD_LEN;
        let payload = out.get_mut(HEADER_LEN..total).ok_or(BufferTooSmall)?;
        payload[0] = self.security.to_byte();
        payload[1] = self.transport.to_byte();
        write_header(out, PayloadType::SDP_REQUEST, Self::PAYLOAD_LEN)?;
        Ok(total)
    }

    pub fn from_payload(payload: &[u8]) -> Result<Self, SdpError> {
        let [security, transport] = payload else { return Err(SdpError::BadLength) };
        Ok(Self {
            security: Security::from_byte(*security).ok_or(SdpError::BadValue)?,
            transport: Transport::from_byte(*transport).ok_or(SdpError::BadValue)?,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Response {
    pub address: [u8; 16],
    pub port: u16,
    pub security: Security,
    pub transport: Transport,
}

impl Response {
    pub const PAYLOAD_LEN: usize = 20;

    pub fn to_datagram(&self, out: &mut [u8]) -> Result<usize, BufferTooSmall> {
        let total = HEADER_LEN + Self::PAYLOAD_LEN;
        let payload = out.get_mut(HEADER_LEN..total).ok_or(BufferTooSmall)?;
        payload[..16].copy_from_slice(&self.address);
        payload[16..18].copy_from_slice(&self.port.to_be_bytes());
        payload[18] = self.security.to_byte();
        payload[19] = self.transport.to_byte();
        write_header(out, PayloadType::SDP_RESPONSE, Self::PAYLOAD_LEN)?;
        Ok(total)
    }

    pub fn from_payload(payload: &[u8]) -> Result<Self, SdpError> {
        if payload.len() != Self::PAYLOAD_LEN {
            return Err(SdpError::BadLength);
        }
        let mut address = [0u8; 16];
        address.copy_from_slice(&payload[..16]);
        Ok(Self {
            address,
            port: u16::from_be_bytes([payload[16], payload[17]]),
            security: Security::from_byte(payload[18]).ok_or(SdpError::BadValue)?,
            transport: Transport::from_byte(payload[19]).ok_or(SdpError::BadValue)?,
        })
    }
}

// ---------------------------------------------------------------------------- wireless

/// Coupling types (ISO 15118-20 Tables 23 and 27).
pub mod coupling {
    /// EN 50696:2021 Annex A.2: infrastructure mounted inverse pantograph, cross rail contacts.
    pub const PANTOGRAPH_CROSS_RAIL: u8 = 0x00;
    /// EN 50696:2021 Annex A.3: infrastructure mounted inverse pantograph, in line contacts.
    pub const PANTOGRAPH_IN_LINE: u8 = 0x01;
    /// IEC 61980 wireless power transfer.
    pub const WPT: u8 = 0x10;
    /// Not applicable.
    pub const NOT_APPLICABLE: u8 = 0xFF;
}

/// The bits of the first P2PS/PPD byte of a wireless request (Table 24; bit 0 for the PPD on the infrastructure is the one
/// V2G20-4003 sets for ACDP).
pub mod p2ps {
    pub const PPD_ON_INFRASTRUCTURE: u8 = 1 << 0;
    pub const PPD_ON_EV: u8 = 1 << 1;
    pub const WPT_LF_ON_INFRASTRUCTURE: u8 = 1 << 2;
    pub const WPT_LF_ON_EV: u8 = 1 << 3;
    pub const WPT_CAM_ON_EV: u8 = 1 << 4;
    pub const ACD_U_INFRARED: u8 = 1 << 5;
    pub const ACD_DIRECTED_WLAN: u8 = 1 << 6;
}

/// An identifier of at most `N` ASCII letters and digits (EVID, EVSEID: Table 25), sent zero padded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsciiId<const N: usize>([u8; N]);

impl<const N: usize> AsciiId<N> {
    pub const EMPTY: Self = Self([0; N]);

    /// None when the text is longer than `N` or has anything but ASCII letters and digits.
    pub fn new(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() > N || !bytes.iter().all(u8::is_ascii_alphanumeric) {
            return None;
        }
        let mut id = [0u8; N];
        id[..bytes.len()].copy_from_slice(bytes);
        Some(Self(id))
    }

    pub fn as_str(&self) -> &str {
        let end = self.0.iter().position(|&b| b == 0).unwrap_or(N);
        // Letters and digits only, checked in `new` and `from_bytes`.
        core::str::from_utf8(&self.0[..end]).unwrap_or("")
    }

    pub fn is_empty(&self) -> bool {
        self.0[0] == 0
    }

    fn from_bytes(bytes: &[u8]) -> Result<Self, SdpError> {
        let mut id = [0u8; N];
        id.copy_from_slice(bytes);
        let end = id.iter().position(|&b| b == 0).unwrap_or(N);
        // Letters and digits up to the first zero, only zeros after it.
        if !id[..end].iter().all(u8::is_ascii_alphanumeric) || id[end..].iter().any(|&b| b != 0) {
            return Err(SdpError::BadValue);
        }
        Ok(Self(id))
    }
}

/// An EVID: any temporary identifier of the vehicle, at most 20 characters; "ZZ00000" when it has none (V2G20-2281).
pub type EvId = AsciiId<20>;
/// An EVSEID in the wireless SDP: at most 37 characters (bytes 26 to 62 of the request, 23 to 59 of the response).
pub type EvseId = AsciiId<37>;

/// The SECC discovery request for wireless communication (Tables 22 to 25), payload type 0x9002.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WirelessRequest {
    pub security: Security,
    pub transport: Transport,
    /// Bytes 3 and 4: see [`p2ps`]; byte 4 is reserved.
    pub p2ps_ppd: [u8; 2],
    /// Byte 5: see [`coupling`].
    pub coupling_type: u8,
    pub evid: EvId,
    /// Left empty by a vehicle that uses a PPD on the infrastructure (V2G20-4100).
    pub evse_id: EvseId,
}

impl WirelessRequest {
    pub const PAYLOAD_LEN: usize = 62;

    pub fn to_datagram(&self, out: &mut [u8]) -> Result<usize, BufferTooSmall> {
        let total = HEADER_LEN + Self::PAYLOAD_LEN;
        let payload = out.get_mut(HEADER_LEN..total).ok_or(BufferTooSmall)?;
        payload[0] = self.security.to_byte();
        payload[1] = self.transport.to_byte();
        payload[2..4].copy_from_slice(&self.p2ps_ppd);
        payload[4] = self.coupling_type;
        payload[5..25].copy_from_slice(&self.evid.0);
        payload[25..62].copy_from_slice(&self.evse_id.0);
        write_header(out, PayloadType::SDP_REQUEST_WIRELESS, Self::PAYLOAD_LEN)?;
        Ok(total)
    }

    pub fn from_payload(payload: &[u8]) -> Result<Self, SdpError> {
        if payload.len() != Self::PAYLOAD_LEN {
            return Err(SdpError::BadLength);
        }
        Ok(Self {
            security: Security::from_byte(payload[0]).ok_or(SdpError::BadValue)?,
            transport: Transport::from_byte(payload[1]).ok_or(SdpError::BadValue)?,
            p2ps_ppd: [payload[2], payload[3]],
            coupling_type: payload[4],
            evid: EvId::from_bytes(&payload[5..25])?,
            evse_id: EvseId::from_bytes(&payload[25..62])?,
        })
    }
}

/// Byte 21 of the wireless response (Table 27).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagStatus {
    /// No EVID has been detected by the pairing and positioning device yet (V2G20-4102).
    Ongoing,
    FinishedWithEvseId,
    FinishedWithoutEvseId,
    /// Error: no SECC.
    ErrorNoSecc,
}

impl DiagStatus {
    fn to_byte(self) -> u8 {
        match self {
            DiagStatus::Ongoing => 0x00,
            DiagStatus::FinishedWithEvseId => 0x01,
            DiagStatus::FinishedWithoutEvseId => 0x02,
            DiagStatus::ErrorNoSecc => 0x10,
        }
    }

    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x00 => Some(DiagStatus::Ongoing),
            0x01 => Some(DiagStatus::FinishedWithEvseId),
            0x02 => Some(DiagStatus::FinishedWithoutEvseId),
            0x10 => Some(DiagStatus::ErrorNoSecc),
            _ => None,
        }
    }
}

/// The SECC discovery response for wireless communication (Tables 26 and 27), payload type 0x9003.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WirelessResponse {
    pub address: [u8; 16],
    pub port: u16,
    pub security: Security,
    pub transport: Transport,
    pub diag_status: DiagStatus,
    pub coupling_type: u8,
    pub evse_id: EvseId,
}

impl WirelessResponse {
    pub const PAYLOAD_LEN: usize = 59;

    pub fn to_datagram(&self, out: &mut [u8]) -> Result<usize, BufferTooSmall> {
        let total = HEADER_LEN + Self::PAYLOAD_LEN;
        let payload = out.get_mut(HEADER_LEN..total).ok_or(BufferTooSmall)?;
        payload[..16].copy_from_slice(&self.address);
        payload[16..18].copy_from_slice(&self.port.to_be_bytes());
        payload[18] = self.security.to_byte();
        payload[19] = self.transport.to_byte();
        payload[20] = self.diag_status.to_byte();
        payload[21] = self.coupling_type;
        payload[22..59].copy_from_slice(&self.evse_id.0);
        write_header(out, PayloadType::SDP_RESPONSE_WIRELESS, Self::PAYLOAD_LEN)?;
        Ok(total)
    }

    pub fn from_payload(payload: &[u8]) -> Result<Self, SdpError> {
        if payload.len() != Self::PAYLOAD_LEN {
            return Err(SdpError::BadLength);
        }
        let mut address = [0u8; 16];
        address.copy_from_slice(&payload[..16]);
        Ok(Self {
            address,
            port: u16::from_be_bytes([payload[16], payload[17]]),
            security: Security::from_byte(payload[18]).ok_or(SdpError::BadValue)?,
            transport: Transport::from_byte(payload[19]).ok_or(SdpError::BadValue)?,
            diag_status: DiagStatus::from_byte(payload[20]).ok_or(SdpError::BadValue)?,
            coupling_type: payload[21],
            evse_id: EvseId::from_bytes(&payload[22..59])?,
        })
    }

    /// The address, port, security and transport as a wired response, for what connects.
    pub fn as_response(&self) -> Response {
        Response { address: self.address, port: self.port, security: self.security, transport: self.transport }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_bytes() {
        let mut out = [0u8; 16];
        let n = Request { security: Security::NoTls, transport: Transport::Tcp }.to_datagram(&mut out).unwrap();
        assert_eq!(&out[..n], &[0x01, 0xFE, 0x90, 0x00, 0, 0, 0, 2, 0x10, 0x00]);
        let n = Request { security: Security::Tls, transport: Transport::Tcp }.to_datagram(&mut out).unwrap();
        assert_eq!(&out[8..n], &[0x00, 0x00]);
    }

    #[test]
    fn response_round_trip() {
        let r = Response {
            address: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
            port: 50000,
            security: Security::Tls,
            transport: Transport::Tcp,
        };
        let mut out = [0u8; 32];
        let n = r.to_datagram(&mut out).unwrap();
        assert_eq!(n, 28);
        assert_eq!(&out[..8], &[0x01, 0xFE, 0x90, 0x01, 0, 0, 0, 20]);
        assert_eq!(Response::from_payload(&out[8..n]), Ok(r));
    }

    #[test]
    fn rejects_malformed_payloads() {
        assert_eq!(Request::from_payload(&[0x10]), Err(SdpError::BadLength));
        assert_eq!(Request::from_payload(&[0x05, 0x00]), Err(SdpError::BadValue));
        assert_eq!(Response::from_payload(&[0u8; 19]), Err(SdpError::BadLength));
        let mut p = [0u8; 20];
        p[19] = 0x77;
        assert_eq!(Response::from_payload(&p), Err(SdpError::BadValue));
    }

    #[test]
    fn wireless_request_bytes() {
        let r = WirelessRequest {
            security: Security::Tls,
            transport: Transport::Tcp,
            p2ps_ppd: [p2ps::PPD_ON_INFRASTRUCTURE, 0],
            coupling_type: coupling::PANTOGRAPH_IN_LINE,
            evid: EvId::new("ZZ00000").unwrap(),
            evse_id: EvseId::EMPTY,
        };
        let mut out = [0u8; 80];
        let n = r.to_datagram(&mut out).unwrap();
        assert_eq!(n, 70);
        assert_eq!(&out[..8], &[0x01, 0xFE, 0x90, 0x02, 0, 0, 0, 62]);
        assert_eq!(&out[8..14], &[0x00, 0x00, 0x01, 0x00, 0x01, b'Z']);
        assert_eq!(&out[14..20], b"Z00000");
        assert!(out[20..n].iter().all(|&b| b == 0), "the EVID and the EVSEID are zero padded");
        assert_eq!(WirelessRequest::from_payload(&out[8..n]), Ok(r));
        assert_eq!(r.evid.as_str(), "ZZ00000");
        assert!(r.evse_id.is_empty());
    }

    #[test]
    fn wireless_response_round_trip() {
        let r = WirelessResponse {
            address: [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2],
            port: 50000,
            security: Security::Tls,
            transport: Transport::Tcp,
            diag_status: DiagStatus::FinishedWithEvseId,
            coupling_type: coupling::PANTOGRAPH_IN_LINE,
            evse_id: EvseId::new("DE9XYZ1").unwrap(),
        };
        let mut out = [0u8; 80];
        let n = r.to_datagram(&mut out).unwrap();
        assert_eq!(n, 67);
        assert_eq!(&out[..8], &[0x01, 0xFE, 0x90, 0x03, 0, 0, 0, 59]);
        assert_eq!(&out[28..30], &[0x01, 0x01], "DiagStatus and CouplingType are bytes 21 and 22");
        assert_eq!(WirelessResponse::from_payload(&out[8..n]), Ok(r));
        assert_eq!(r.as_response().port, 50000);
    }

    #[test]
    fn wireless_payloads_are_checked() {
        assert_eq!(WirelessRequest::from_payload(&[0u8; 61]), Err(SdpError::BadLength));
        assert_eq!(WirelessResponse::from_payload(&[0u8; 58]), Err(SdpError::BadLength));
        let mut p = [0u8; 62];
        p[5] = b'-'; // not a letter or a digit
        assert_eq!(WirelessRequest::from_payload(&p), Err(SdpError::BadValue));
        p[5] = b'A';
        p[7] = b'B'; // after the padding started
        p[6] = 0;
        assert_eq!(WirelessRequest::from_payload(&p), Err(SdpError::BadValue));
        let mut r = [0u8; 59];
        r[20] = 0x03;
        assert_eq!(WirelessResponse::from_payload(&r), Err(SdpError::BadValue));
        assert!(EvId::new(&"A".repeat(21)).is_none());
        assert!(EvseId::new(&"A".repeat(37)).is_some());
        assert!(EvId::new("A-B").is_none());
    }
}
