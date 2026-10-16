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
}
