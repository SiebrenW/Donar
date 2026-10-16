//! HomePlug Green PHY management messages (MMEs) used for SLAC (ISO 15118-3:2015 Annex A,
//! HomePlug Green PHY 1.1.1 section 11.5), framed as Ethernet type 0x88E1.
//!
//! `no_std`, no allocation, no I/O. Shared by the EV side and the EVSE simulator. Parsing
//! returns `None` for anything that deviates from the message definitions (Tables A.2, A.4,
//! A.7): the standard says to ignore such messages (V2G3-A09-09, 14, 35, 41, 47, 95, 98).
//! Frames may carry padding after the message (Ethernet pads to 60 octets), which is accepted.
#![no_std]

#[cfg(test)]
extern crate std;

pub type Mac = [u8; 6];

pub const BROADCAST: Mac = [0xFF; 6];
pub const ETHERTYPE: u16 = 0x88E1;
/// Management message version of HomePlug Green PHY 1.0 and AV 1.1.
pub const MMV: u8 = 0x01;
/// Destination, source, EtherType, MMV and MMTYPE.
pub const HEADER_LEN: usize = 17;
/// The shortest Ethernet frame without the FCS; shorter ones are padded.
pub const MIN_FRAME: usize = 60;

/// Number of OFDM carrier groups in an attenuation profile.
pub const GROUPS: usize = 58;

pub mod mmtype {
    pub const SET_KEY_REQ: u16 = 0x6008;
    pub const SET_KEY_CNF: u16 = 0x6009;
    /// CM_VALIDATE.REQ / .CNF (Tables A.5, A.6): the base is 0x6044 in the HomePlug MME
    /// numbering that open implementations use; ISO 15118-3 only refers to the HomePlug spec.
    pub const VALIDATE_REQ: u16 = 0x6044;
    pub const VALIDATE_CNF: u16 = 0x6045;
    pub const SLAC_PARM_REQ: u16 = 0x6064;
    pub const SLAC_PARM_CNF: u16 = 0x6065;
    pub const START_ATTEN_CHAR_IND: u16 = 0x606A;
    pub const ATTEN_CHAR_IND: u16 = 0x606E;
    pub const ATTEN_CHAR_RSP: u16 = 0x606F;
    pub const MNBC_SOUND_IND: u16 = 0x6076;
    pub const SLAC_MATCH_REQ: u16 = 0x607C;
    pub const SLAC_MATCH_CNF: u16 = 0x607D;
}

pub type RunId = [u8; 8];

/// The `Result` field of CM_VALIDATE (Tables A.5, A.6).
pub mod validate_result {
    pub const NOT_READY: u8 = 0;
    pub const READY: u8 = 1;
    pub const SUCCESS: u8 = 2;
    pub const FAILURE: u8 = 3;
    pub const NOT_REQUIRED: u8 = 4;
}

/// A SLAC message with the fields the standard makes variable; the rest is fixed by the
/// definitions. Application and security type are always "PEV-EVSE matching, no security".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    ParmReq { run_id: RunId },
    /// The EVSE's answer. `time_out` is in units of 100 ms (Table A.2: 0x06).
    ParmCnf { num_sounds: u8, time_out: u8, forwarding_sta: Mac, run_id: RunId },
    StartAttenChar { num_sounds: u8, time_out: u8, forwarding_sta: Mac, run_id: RunId },
    MnbcSound { cnt: u8, run_id: RunId, rnd: [u8; 16] },
    AttenCharInd { source: Mac, run_id: RunId, num_sounds: u8, groups: [u8; GROUPS] },
    /// `result` 0 means a successful SLAC process.
    AttenCharRsp { source: Mac, run_id: RunId, result: u8 },
    MatchReq { pev: Mac, evse: Mac, run_id: RunId },
    MatchCnf { pev: Mac, evse: Mac, run_id: RunId, nid: [u8; 7], nmk: [u8; 16] },
    /// The EV asks the charger to validate the match by control pilot toggles. `timer` is 0 in
    /// the first request and the time the charger listens in units of 100 ms in the second
    /// (Table A.6); `result` is "ready". The signal type is always 0 (PEV S2 toggles).
    ValidateReq { timer: u8, result: u8 },
    /// The answer: `toggle_num` is 0 in the first and the number of toggles seen in the second.
    ValidateCnf { toggle_num: u8, result: u8 },
    /// Configures the local modem (Table A.8); not sent over the power line.
    SetKeyReq { cco_capability: u8, nid: [u8; 7], nmk: [u8; 16] },
    SetKeyCnf { result: u8 },
}

/// A parsed frame; the payload still has to go through [`Message::parse`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    pub dst: Mac,
    pub src: Mac,
    pub mmtype: u16,
    pub payload: &'a [u8],
}

impl<'a> Frame<'a> {
    /// Splits an Ethernet frame. `None` if it is not a HomePlug MME of the right version.
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.len() < HEADER_LEN || bytes[12..14] != ETHERTYPE.to_be_bytes() || bytes[14] != MMV {
            return None;
        }
        Some(Frame {
            dst: bytes[0..6].try_into().ok()?,
            src: bytes[6..12].try_into().ok()?,
            mmtype: u16::from_le_bytes([bytes[15], bytes[16]]),
            payload: &bytes[HEADER_LEN..],
        })
    }

    pub fn message(&self) -> Option<Message> {
        Message::parse(self.mmtype, self.payload)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferTooSmall;

fn mac(b: &[u8]) -> Mac {
    b[..6].try_into().unwrap()
}

fn run_id(b: &[u8]) -> RunId {
    b[..8].try_into().unwrap()
}

impl Message {
    pub fn mmtype(&self) -> u16 {
        match self {
            Message::ParmReq { .. } => mmtype::SLAC_PARM_REQ,
            Message::ParmCnf { .. } => mmtype::SLAC_PARM_CNF,
            Message::StartAttenChar { .. } => mmtype::START_ATTEN_CHAR_IND,
            Message::MnbcSound { .. } => mmtype::MNBC_SOUND_IND,
            Message::AttenCharInd { .. } => mmtype::ATTEN_CHAR_IND,
            Message::AttenCharRsp { .. } => mmtype::ATTEN_CHAR_RSP,
            Message::MatchReq { .. } => mmtype::SLAC_MATCH_REQ,
            Message::MatchCnf { .. } => mmtype::SLAC_MATCH_CNF,
            Message::ValidateReq { .. } => mmtype::VALIDATE_REQ,
            Message::ValidateCnf { .. } => mmtype::VALIDATE_CNF,
            Message::SetKeyReq { .. } => mmtype::SET_KEY_REQ,
            Message::SetKeyCnf { .. } => mmtype::SET_KEY_CNF,
        }
    }

    /// The message body (the part after the MMTYPE) into `out`; returns its length.
    fn encode(&self, out: &mut [u8; 128]) -> usize {
        out.fill(0);
        match self {
            Message::ParmReq { run_id } => {
                out[2..10].copy_from_slice(run_id);
                10
            }
            Message::ParmCnf { num_sounds, time_out, forwarding_sta, run_id } => {
                out[0..6].copy_from_slice(&BROADCAST);
                out[6] = *num_sounds;
                out[7] = *time_out;
                out[8] = 0x01; // RESP_TYPE: other GP station
                out[9..15].copy_from_slice(forwarding_sta);
                out[17..25].copy_from_slice(run_id);
                25
            }
            Message::StartAttenChar { num_sounds, time_out, forwarding_sta, run_id } => {
                out[2] = *num_sounds;
                out[3] = *time_out;
                out[4] = 0x01;
                out[5..11].copy_from_slice(forwarding_sta);
                out[11..19].copy_from_slice(run_id);
                19
            }
            Message::MnbcSound { cnt, run_id, rnd } => {
                out[19] = *cnt;
                out[20..28].copy_from_slice(run_id);
                out[36..52].copy_from_slice(rnd);
                52
            }
            Message::AttenCharInd { source, run_id, num_sounds, groups } => {
                out[2..8].copy_from_slice(source);
                out[8..16].copy_from_slice(run_id);
                out[50] = *num_sounds;
                out[51] = GROUPS as u8;
                out[52..52 + GROUPS].copy_from_slice(groups);
                52 + GROUPS
            }
            Message::AttenCharRsp { source, run_id, result } => {
                out[2..8].copy_from_slice(source);
                out[8..16].copy_from_slice(run_id);
                out[50] = *result;
                51
            }
            Message::MatchReq { pev, evse, run_id } => {
                out[2..4].copy_from_slice(&0x003Eu16.to_le_bytes());
                out[21..27].copy_from_slice(pev);
                out[44..50].copy_from_slice(evse);
                out[50..58].copy_from_slice(run_id);
                66
            }
            Message::MatchCnf { pev, evse, run_id, nid, nmk } => {
                out[2..4].copy_from_slice(&0x0056u16.to_le_bytes());
                out[21..27].copy_from_slice(pev);
                out[44..50].copy_from_slice(evse);
                out[50..58].copy_from_slice(run_id);
                out[66..73].copy_from_slice(nid);
                out[74..90].copy_from_slice(nmk);
                90
            }
            Message::ValidateReq { timer, result } => {
                out[1] = *timer;
                out[2] = *result;
                3
            }
            Message::ValidateCnf { toggle_num, result } => {
                out[1] = *toggle_num;
                out[2] = *result;
                3
            }
            Message::SetKeyReq { cco_capability, nid, nmk } => {
                out[0] = 0x01; // key type NMK
                out[9] = 0x04; // PID: HLE protocol
                out[13] = *cco_capability;
                out[14..21].copy_from_slice(nid);
                out[21] = 0x01; // NewEKS: NMK
                out[22..38].copy_from_slice(nmk);
                38
            }
            Message::SetKeyCnf { result } => {
                out[0] = *result;
                1
            }
        }
    }

    /// Writes a complete Ethernet frame, padded to the minimum length. Returns its length.
    pub fn write(&self, dst: Mac, src: Mac, out: &mut [u8]) -> Result<usize, BufferTooSmall> {
        let mut body = [0u8; 128];
        let len = self.encode(&mut body);
        let total = (HEADER_LEN + len).max(MIN_FRAME);
        let out = out.get_mut(..total).ok_or(BufferTooSmall)?;
        out[0..6].copy_from_slice(&dst);
        out[6..12].copy_from_slice(&src);
        out[12..14].copy_from_slice(&ETHERTYPE.to_be_bytes());
        out[14] = MMV;
        out[15..17].copy_from_slice(&self.mmtype().to_le_bytes());
        out[HEADER_LEN..HEADER_LEN + len].copy_from_slice(&body[..len]);
        out[HEADER_LEN + len..].fill(0);
        Ok(total)
    }

    /// Decodes the body of a message of type `mmtype`; `None` if it is unknown or deviates
    /// from the definition. Trailing padding is ignored.
    pub fn parse(mmtype: u16, b: &[u8]) -> Option<Message> {
        // Application type 0 (PEV-EVSE matching), security type 0 (none).
        let plain = |b: &[u8]| b.len() >= 2 && b[0] == 0 && b[1] == 0;
        let min = |n: usize| (b.len() >= n && plain(b)).then_some(());
        match mmtype {
            mmtype::SLAC_PARM_REQ => min(10).map(|()| Message::ParmReq { run_id: run_id(&b[2..]) }),
            mmtype::SLAC_PARM_CNF => {
                // The EVSE announces broadcast M-Sounds towards another station; here the
                // application and security type come after FORWARDING_STA (Table A.2).
                (b.len() >= 25 && b[0..6] == BROADCAST && b[8] == 0x01 && b[15] == 0 && b[16] == 0).then_some(())?;
                Some(Message::ParmCnf { num_sounds: b[6], time_out: b[7], forwarding_sta: mac(&b[9..]), run_id: run_id(&b[17..]) })
            }
            mmtype::START_ATTEN_CHAR_IND => {
                min(19)?;
                (b[4] == 0x01).then_some(())?;
                Some(Message::StartAttenChar { num_sounds: b[2], time_out: b[3], forwarding_sta: mac(&b[5..]), run_id: run_id(&b[11..]) })
            }
            mmtype::MNBC_SOUND_IND => {
                min(52)?;
                Some(Message::MnbcSound { cnt: b[19], run_id: run_id(&b[20..]), rnd: b[36..52].try_into().unwrap() })
            }
            mmtype::ATTEN_CHAR_IND => {
                min(52 + GROUPS)?;
                (b[51] as usize == GROUPS).then_some(())?;
                Some(Message::AttenCharInd {
                    source: mac(&b[2..]),
                    run_id: run_id(&b[8..]),
                    num_sounds: b[50],
                    groups: b[52..52 + GROUPS].try_into().unwrap(),
                })
            }
            mmtype::ATTEN_CHAR_RSP => min(51).map(|()| Message::AttenCharRsp { source: mac(&b[2..]), run_id: run_id(&b[8..]), result: b[50] }),
            mmtype::SLAC_MATCH_REQ => {
                min(66)?;
                (b[2..4] == 0x003Eu16.to_le_bytes()).then_some(())?;
                Some(Message::MatchReq { pev: mac(&b[21..]), evse: mac(&b[44..]), run_id: run_id(&b[50..]) })
            }
            mmtype::SLAC_MATCH_CNF => {
                min(90)?;
                (b[2..4] == 0x0056u16.to_le_bytes()).then_some(())?;
                Some(Message::MatchCnf {
                    pev: mac(&b[21..]),
                    evse: mac(&b[44..]),
                    run_id: run_id(&b[50..]),
                    nid: b[66..73].try_into().unwrap(),
                    nmk: b[74..90].try_into().unwrap(),
                })
            }
            // No application and security type here: signal type 0 (PEV S2 toggles), then two fields.
            mmtype::VALIDATE_REQ => (b.len() >= 3 && b[0] == 0).then(|| Message::ValidateReq { timer: b[1], result: b[2] }),
            mmtype::VALIDATE_CNF => (b.len() >= 3 && b[0] == 0).then(|| Message::ValidateCnf { toggle_num: b[1], result: b[2] }),
            mmtype::SET_KEY_REQ => {
                (b.len() >= 38 && b[0] == 0x01 && b[21] == 0x01).then_some(())?;
                Some(Message::SetKeyReq { cco_capability: b[13], nid: b[14..21].try_into().unwrap(), nmk: b[22..38].try_into().unwrap() })
            }
            mmtype::SET_KEY_CNF => (!b.is_empty()).then(|| Message::SetKeyCnf { result: b[0] }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EV: Mac = [0x02, 0, 0, 0, 0, 0x01];
    const EVSE: Mac = [0x02, 0, 0, 0, 0, 0x02];
    const RUN: RunId = [1, 2, 3, 4, 5, 6, 7, 8];

    fn round_trip(m: Message) -> usize {
        let mut buf = [0u8; 200];
        let n = m.write(EVSE, EV, &mut buf).unwrap();
        let f = Frame::parse(&buf[..n]).unwrap();
        assert_eq!((f.dst, f.src), (EVSE, EV));
        assert_eq!(f.message(), Some(m), "{m:?}");
        n
    }

    #[test]
    fn every_message_survives_a_round_trip() {
        round_trip(Message::ParmReq { run_id: RUN });
        round_trip(Message::ValidateReq { timer: 22, result: validate_result::READY });
        round_trip(Message::ValidateCnf { toggle_num: 3, result: validate_result::SUCCESS });
        round_trip(Message::ParmCnf { num_sounds: 10, time_out: 6, forwarding_sta: EV, run_id: RUN });
        round_trip(Message::StartAttenChar { num_sounds: 10, time_out: 6, forwarding_sta: EV, run_id: RUN });
        round_trip(Message::MnbcSound { cnt: 9, run_id: RUN, rnd: [7; 16] });
        round_trip(Message::AttenCharInd { source: EV, run_id: RUN, num_sounds: 10, groups: [12; GROUPS] });
        round_trip(Message::AttenCharRsp { source: EV, run_id: RUN, result: 0 });
        round_trip(Message::MatchReq { pev: EV, evse: EVSE, run_id: RUN });
        round_trip(Message::MatchCnf { pev: EV, evse: EVSE, run_id: RUN, nid: [3; 7], nmk: [9; 16] });
        round_trip(Message::SetKeyReq { cco_capability: 0, nid: [3; 7], nmk: [9; 16] });
        round_trip(Message::SetKeyCnf { result: 0 });
    }

    #[test]
    fn short_messages_are_padded_to_the_ethernet_minimum() {
        assert_eq!(round_trip(Message::ParmReq { run_id: RUN }), MIN_FRAME);
        assert_eq!(round_trip(Message::AttenCharInd { source: EV, run_id: RUN, num_sounds: 1, groups: [0; GROUPS] }), HEADER_LEN + 52 + GROUPS);
    }

    #[test]
    fn parm_req_wire_format() {
        let mut buf = [0u8; 64];
        let n = Message::ParmReq { run_id: RUN }.write(BROADCAST, EV, &mut buf).unwrap();
        assert_eq!(&buf[..27], &[
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x02, 0, 0, 0, 0, 0x01, 0x88, 0xE1, 0x01, 0x64, 0x60, 0x00, 0x00, 1, 2, 3, 4, 5, 6, 7, 8
        ]);
        assert_eq!(n, 60);
    }

    #[test]
    fn match_lengths_follow_the_tables() {
        let mut buf = [0u8; 200];
        assert_eq!(Message::MatchReq { pev: EV, evse: EVSE, run_id: RUN }.write(EVSE, EV, &mut buf).unwrap(), HEADER_LEN + 66);
        assert_eq!(Message::MatchCnf { pev: EV, evse: EVSE, run_id: RUN, nid: [0; 7], nmk: [0; 16] }.write(EV, EVSE, &mut buf).unwrap(), HEADER_LEN + 90);
        // MVFLength is 0x3E and 0x56, little endian.
        assert_eq!(&buf[HEADER_LEN + 2..HEADER_LEN + 4], &[0x56, 0x00]);
    }

    #[test]
    fn deviating_content_is_rejected() {
        let mut buf = [0u8; 200];
        let n = Message::ParmReq { run_id: RUN }.write(BROADCAST, EV, &mut buf).unwrap();
        // Wrong application type, wrong security type, wrong EtherType, wrong version.
        for (at, v) in [(HEADER_LEN, 1u8), (HEADER_LEN + 1, 1), (12, 0x08), (14, 0x02)] {
            let mut bad = buf;
            bad[at] = v;
            assert!(Frame::parse(&bad[..n]).and_then(|f| f.message()).is_none(), "byte {at}");
        }
        // Too short to be a message.
        assert!(Message::parse(mmtype::SLAC_MATCH_CNF, &[0; 89]).is_none());
        assert!(Message::parse(0x1234, &[0; 100]).is_none());
        let mut cnf = [0u8; 200];
        let n = Message::MatchCnf { pev: EV, evse: EVSE, run_id: RUN, nid: [0; 7], nmk: [0; 16] }.write(EV, EVSE, &mut cnf).unwrap();
        cnf[HEADER_LEN + 2] = 0x3E; // the length of a request
        assert!(Frame::parse(&cnf[..n]).and_then(|f| f.message()).is_none());
    }

    #[test]
    fn output_buffer_too_small_is_an_error() {
        let mut buf = [0u8; 59];
        assert_eq!(Message::ParmReq { run_id: RUN }.write(BROADCAST, EV, &mut buf), Err(BufferTooSmall));
    }
}
