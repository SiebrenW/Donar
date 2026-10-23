//! Messages for the powerline modem next to the host (a Qualcomm QCA7000 family chip or another
//! HomePlug Green PHY modem): the key the EV configures it with after SLAC (CM_SET_KEY, HomePlug
//! AV 1.1 section 11.5.5), and the vendor message that reads its software version (Qualcomm
//! Atheros VS_SW_VER), which tells a modem of that family from an absent or foreign one.
//!
//! The modem listens at the local device address; frames to it go out of the host's interface
//! like any other.
//!
//! Verified against a QCA7005 (firmware `MAC-QCA7005-3.3.0.31-00-20231004-CS`) on a Heliox
//! EVAChargeSE board: the version request and its answer (the test below uses the frame as it
//! came off the wire) and `CM_SET_KEY`, which the modem answered with result 1 and then used (its network id was the one computed here).

use crate::{ETHERTYPE, Mac, MIN_FRAME};

/// The address every QCA modem answers to from its host: the local device address.
pub const LOCAL_DEVICE: Mac = [0x00, 0xB0, 0x52, 0x00, 0x00, 0x01];

/// Qualcomm Atheros organizationally unique identifier, the first field of every vendor message.
pub const QCA_OUI: [u8; 3] = [0x00, 0xB0, 0x52];

/// Destination, source, EtherType, MMV, MMTYPE and, for management message version 1, the two
/// octets of fragmentation information.
const fn header(mmv: u8) -> usize {
    if mmv == 0x01 { 19 } else { 17 }
}

/// The network ID of a network membership key (HomePlug Green PHY 4.4.3.1): PBKDF1 with SHA-256
/// and five iterations, no salt, security level 0. The octet layout (the first seven octets of the
/// digest, the last one shifted right by four bits) follows the common implementations.
pub fn network_id(nmk: &[u8; 16]) -> [u8; 7] {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::digest(nmk);
    for _ in 1..5 {
        digest = Sha256::digest(digest);
    }
    let mut nid: [u8; 7] = digest[..7].try_into().unwrap();
    nid[6] >>= 4;
    nid
}

/// `Result` of CM_SET_KEY.CNF for a key that was taken.
pub const SET_KEY_DONE: u8 = 1;

pub const SET_KEY_REQ: u16 = 0x6008;
pub const SET_KEY_CNF: u16 = 0x6009;
/// Vendor message: software version (Qualcomm Atheros), request and confirmation.
pub const VS_SW_VER_REQ: u16 = 0xA000;
pub const VS_SW_VER_CNF: u16 = 0xA001;

/// Vendor message: network information, request and confirmation.
pub const VS_NW_INFO_REQ: u16 = 0xA038;
pub const VS_NW_INFO_CNF: u16 = 0xA039;

/// Vendor message: link status, request and confirmation.
pub const VS_PL_LNK_STATUS_REQ: u16 = 0xA0B8;
pub const VS_PL_LNK_STATUS_CNF: u16 = 0xA0B9;

/// Vendor message: set key, request and confirmation.
pub const VS_SET_KEY_REQ: u16 = 0xA050;
pub const VS_SET_KEY_CNF: u16 = 0xA051;
/// VS_HST_ACTION.IND: the modem tells its host something happened (it started, its link came up or went down).
pub const VS_HST_ACTION_IND: u16 = 0xA062;
/// VS_HST_ACTION.RSP: the host's acknowledgement; a modem repeats the indication until it gets it.
pub const VS_HST_ACTION_RSP: u16 = 0xA063;

/// `HAR` of VS_HST_ACTION.IND: the modem is a member of a network and there are peers.
pub const HAR_LINK_ONLINE: u8 = 0x0A;
/// `HAR` of VS_HST_ACTION.IND: the modem left its network and has no peers.
pub const HAR_LINK_OFFLINE: u8 = 0x0B;

/// `KeyType` of CM_SET_KEY: the network membership key.
const KEY_TYPE_NMK: u8 = 0x01;
/// `PID` of CM_SET_KEY.REQ: the key is given by the host (the modem does not run the key exchange).
const PID_HLE: u8 = 0x04;
/// `NewEKS` for a key given in the clear (`PID` 4): the encryption key select "NMK".
const EKS_NMK: u8 = 0x01;

/// Frame length of the shortest buffers that hold the messages here.
pub const FRAME_MAX: usize = 80;

/// The frame of a request: `payload` behind the header, padded to the minimum frame length.
fn frame(dst: Mac, src: Mac, mmv: u8, mmtype: u16, payload: &[u8], out: &mut [u8]) -> Option<usize> {
    let head = header(mmv);
    let total = (head + payload.len()).max(MIN_FRAME);
    let out = out.get_mut(..total)?;
    out[0..6].copy_from_slice(&dst);
    out[6..12].copy_from_slice(&src);
    out[12..14].copy_from_slice(&ETHERTYPE.to_be_bytes());
    out[14] = mmv;
    out[15..17].copy_from_slice(&mmtype.to_le_bytes());
    out[17..head].fill(0);
    out[head..head + payload.len()].copy_from_slice(payload);
    out[head + payload.len()..].fill(0);
    Some(total)
}

/// CM_SET_KEY.REQ: tells the modem to use `nmk` as the key of the network `nid`. The fields are
/// `KeyType` (NMK), `MyNonce`, `YourNonce` (zero), `PID` (host-provided key), `PRN` and `PMN`
/// (zero), `CCoCapability` (zero), `NID`, `NewEKS` and `NewKey`, behind the fragmentation
/// information of a version 1 header. Returns the frame length, or
/// `None` if `out` is too short ([`FRAME_MAX`] is enough).
pub fn set_key_req(src: Mac, nid: [u8; 7], nmk: [u8; 16], out: &mut [u8]) -> Option<usize> {
    let mut p = [0u8; 1 + 4 + 4 + 1 + 2 + 1 + 1 + 7 + 1 + 16];
    p[0] = KEY_TYPE_NMK;
    p[9] = PID_HLE;
    p[14..21].copy_from_slice(&nid);
    p[21] = EKS_NMK;
    p[22..38].copy_from_slice(&nmk);
    frame(LOCAL_DEVICE, src, 0x01, SET_KEY_REQ, &p, out)
}

/// The `Result` of CM_SET_KEY.CNF. A QCA7005 answers 1 when it has taken the key (and its network
/// then has the network ID that [`network_id`] gives for it), and 0 for a request it refuses (an
/// unknown key type).
pub fn parse_set_key_cnf(frame: &[u8]) -> Option<u8> {
    let body = body(frame, SET_KEY_CNF)?;
    body.first().copied()
}

/// VS_SW_VER.REQ: asks for the software version. The management message version is 0 (the
/// modem does not answer another one).
pub fn sw_ver_req(src: Mac, out: &mut [u8]) -> Option<usize> {
    frame(LOCAL_DEVICE, src, 0x00, VS_SW_VER_REQ, &QCA_OUI, out)
}

/// The software version a modem reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwVer<'a> {
    /// 0 for success.
    pub status: u8,
    /// The device id (0x22 for the QCA7000 family).
    pub device: u8,
    /// The version text, such as `MAC-QCA7005-3.3.0.31-00-20231004-CS`.
    pub version: &'a str,
}

/// VS_SW_VER.CNF: `MSTATUS`, `MDEVICEID`, `MVERLENGTH` and the NUL-terminated version.
pub fn parse_sw_ver_cnf(frame: &[u8]) -> Option<SwVer<'_>> {
    let body = body_vendor(frame, VS_SW_VER_CNF)?;
    let (&status, rest) = body.split_first()?;
    let (&device, rest) = rest.split_first()?;
    let (&length, rest) = rest.split_first()?;
    let text = rest.get(..usize::from(length))?;
    let text = text.split(|b| *b == 0).next().unwrap_or(&[]);
    Some(SwVer { status, device, version: core::str::from_utf8(text).ok()? })
}

/// VS_NW_INFO.REQ: asks which networks the modem belongs to. Management message version 1 (with
/// the fragmentation information), the OUI and nothing else.
pub fn nw_info_req(src: Mac, out: &mut [u8]) -> Option<usize> {
    frame(LOCAL_DEVICE, src, 0x01, VS_NW_INFO_REQ, &QCA_OUI, out)
}

/// The first network of a VS_NW_INFO.CNF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    pub nid: [u8; 7],
    /// The short network id the coordinator assigned.
    pub snid: u8,
    /// The terminal equipment identifier of this modem in the network.
    pub tei: u8,
    /// Role of this modem: 2 for the central coordinator (alone, or leading the network), 0 for a
    /// station that has joined.
    pub role: u8,
    /// The address of the central coordinator.
    pub cco: Mac,
    pub cco_tei: u8,
}

/// What a modem reports about its networks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NwInfo {
    /// The address the modem answered from: its own.
    pub modem: Mac,
    /// How many networks the modem belongs to.
    pub networks: u8,
    /// The first of them, if any.
    pub first: Option<Network>,
}

impl NwInfo {
    /// Whether the modem has joined the network `nid` that someone else coordinates: it is no longer
    /// alone in the network it formed when it was given the key.
    pub fn joined(&self, nid: [u8; 7]) -> bool {
        self.first.is_some_and(|n| n.nid == nid && n.cco != self.modem)
    }
}

/// VS_NW_INFO.CNF of a QCA7005, read as far as it was checked on one: five octets of header
/// (status, length), the number of networks, then per network the network id, two octets that
/// stayed zero, the short network id, the TEI, four zero octets, the role, the coordinator's
/// address and TEI, and the stations (a modem alone in its network lists none). The network id,
/// the coordinator and the TEI followed the key the modem was given (the lone coordinator is the
/// modem itself); the layout of a network with other stations was not seen on hardware, which is
/// why only the first network is read.
pub fn parse_nw_info_cnf(frame: &[u8]) -> Option<NwInfo> {
    let body = body_vendor(frame, VS_NW_INFO_CNF)?;
    let networks = *body.get(5)?;
    let first = if networks == 0 {
        None
    } else {
        let n = body.get(6..6 + 23)?;
        Some(Network {
            nid: n[0..7].try_into().ok()?,
            snid: n[9],
            tei: n[10],
            role: n[15],
            cco: n[16..22].try_into().ok()?,
            cco_tei: n[22],
        })
    };
    Some(NwInfo { modem: frame.get(6..12)?.try_into().ok()?, networks, first })
}

/// `EKS` of VS_SET_KEY.REQ: the key is a network membership key.
const VS_EKS_NMK: u8 = 0x01;
/// `PEKS` of VS_SET_KEY.REQ: the key is set on this station ("local set").
const VS_PEKS_LOCAL: u8 = 0x0F;

/// VS_SET_KEY.REQ: the vendor message that gives the modem its network membership key; the modem works the network id
/// out itself. Management message version 0, the OUI, `EKS` (NMK), the key, `PEKS` (local set), then the destination
/// address and the device access key, which stay zero. The layout is the one of the Qualcomm Atheros
/// QCA7000 documentation as other stacks use it; it was not checked on a modem here.
pub fn vs_set_key_req(src: Mac, nmk: [u8; 16], out: &mut [u8]) -> Option<usize> {
    let mut p = [0u8; 3 + 1 + 16 + 1 + 6 + 16];
    p[..3].copy_from_slice(&QCA_OUI);
    p[3] = VS_EKS_NMK;
    p[4..20].copy_from_slice(&nmk);
    p[20] = VS_PEKS_LOCAL;
    frame(LOCAL_DEVICE, src, 0x00, VS_SET_KEY_REQ, &p, out)
}

/// The `MSTATUS` of VS_SET_KEY.CNF: 0 when the modem took the key.
pub fn parse_vs_set_key_cnf(frame: &[u8]) -> Option<u8> {
    body_vendor(frame, VS_SET_KEY_CNF)?.first().copied()
}

/// VS_PL_LNK_STATUS.REQ: asks whether the modem has a powerline link to another station. Management message
/// version 0, the OUI and nothing else (the layout of the QCA7000 documentation as other stacks use it; not checked on a modem here).
pub fn lnk_status_req(src: Mac, out: &mut [u8]) -> Option<usize> {
    frame(LOCAL_DEVICE, src, 0x00, VS_PL_LNK_STATUS_REQ, &QCA_OUI, out)
}

/// VS_PL_LNK_STATUS.CNF: `MSTATUS` (0 for success) and the link status (1 for a link, 0 for none). `None` when it is
/// not the confirmation or too short.
pub fn parse_lnk_status_cnf(frame: &[u8]) -> Option<LinkStatus> {
    let body = body_vendor(frame, VS_PL_LNK_STATUS_CNF)?;
    let (&status, &link) = (body.first()?, body.get(1)?);
    Some(LinkStatus { status, link: link == 1 })
}

/// What a modem answers to VS_PL_LNK_STATUS.REQ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinkStatus {
    /// 0 for success.
    pub status: u8,
    /// The modem is a member of a network with another station.
    pub link: bool,
}

/// A VS_HST_ACTION.IND: what the modem reports and what the answer has to repeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostAction {
    /// The modem's address (the answer goes there).
    pub from: Mac,
    /// The host action request: [`HAR_LINK_ONLINE`], [`HAR_LINK_OFFLINE`] or another reason (a reboot, a firmware step).
    pub action: u8,
    /// The version of the indication, major and minor. From 0.1 on it carries a session id and a retry count, which the answer
    /// repeats.
    pub version: (u8, u8),
    session_id: u8,
    outstanding_retries: [u8; 2],
}

/// VS_HST_ACTION.IND as the modem sends it: the OUI, the `HAR`, the version (major, minor) and, from version 0.1 on, the
/// session id, the outstanding retries (two octets) and the retry timer. `None` when it is not that message or too short
/// (the layout is the one of the QCA7000 documentation as other stacks use it; not checked on a modem here).
pub fn parse_hst_action_ind(frame: &[u8]) -> Option<HostAction> {
    let b = body_vendor(frame, VS_HST_ACTION_IND)?;
    let (&action, &major, &minor) = (b.first()?, b.get(1)?, b.get(2)?);
    let mut found = HostAction { from: frame[6..12].try_into().ok()?, action, version: (major, minor), session_id: 0, outstanding_retries: [0; 2] };
    if (major, minor) == (0, 1) {
        found.session_id = *b.get(3)?;
        found.outstanding_retries = [*b.get(4)?, *b.get(5)?];
    }
    Some(found)
}

/// VS_HST_ACTION.RSP: tells the modem the indication `ind` arrived (`MSTATUS` 0), or that the host could not take it. Management
/// message version 0, the OUI, `MSTATUS` and the version, then for version 0.1 the `HAR`, the session id and the outstanding
/// retries of the indication. Without it the modem repeats the indication.
pub fn hst_action_rsp(src: Mac, ind: &HostAction, success: bool, out: &mut [u8]) -> Option<usize> {
    let mut p = [0u8; 3 + 1 + 2 + 1 + 1 + 2];
    p[..3].copy_from_slice(&QCA_OUI);
    p[3] = u8::from(!success);
    (p[4], p[5]) = ind.version;
    let len = if ind.version == (0, 1) {
        p[6] = ind.action;
        p[7] = ind.session_id;
        p[8..10].copy_from_slice(&ind.outstanding_retries);
        10
    } else {
        6
    };
    frame(ind.from, src, 0x00, VS_HST_ACTION_RSP, &p[..len], out)
}

/// The payload of a standard message of type `mmtype` (after the header).
fn body(frame: &[u8], mmtype: u16) -> Option<&[u8]> {
    if frame.len() < 17 || frame[12..14] != ETHERTYPE.to_be_bytes() || u16::from_le_bytes([frame[15], frame[16]]) != mmtype {
        return None;
    }
    frame.get(header(frame[14])..)
}

/// The payload of a vendor message (after the header and the OUI).
fn body_vendor(frame: &[u8], mmtype: u16) -> Option<&[u8]> {
    let b = body(frame, mmtype)?;
    (b.len() >= 3 && b[..3] == QCA_OUI).then(|| &b[3..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_indication_is_read_and_answered_with_what_it_carried() {
        // From the modem: version 0.1, session 3, two retries outstanding, a retry timer of 100 x 10 ms.
        let ind = hex("0001 8705 f505 00b0 5200 0001 88e1 00 62a0 00b052 0a 00 01 03 0200 6400");
        let found = parse_hst_action_ind(&ind).unwrap();
        assert_eq!((found.action, found.version), (HAR_LINK_ONLINE, (0, 1)));
        assert_eq!(found.from, [0x00, 0xB0, 0x52, 0x00, 0x00, 0x01]);
        let mut out = [0u8; FRAME_MAX];
        let n = hst_action_rsp([0x00, 0x01, 0x87, 0x05, 0xF5, 0x05], &found, true, &mut out).unwrap();
        assert_eq!(n, MIN_FRAME);
        assert_eq!(&out[..6], &found.from);
        assert_eq!(&out[12..17], &[0x88, 0xE1, 0x00, 0x63, 0xA0]);
        // OUI, MSTATUS 0, version 0.1, HAR, session, retries.
        assert_eq!(&out[17..27], &[0x00, 0xB0, 0x52, 0, 0, 1, 0x0A, 3, 2, 0]);
    }

    #[test]
    fn an_indication_of_version_0_0_is_answered_without_the_session() {
        let ind = hex("0001 8705 f505 00b0 5200 0001 88e1 00 62a0 00b052 0b 00 00");
        let found = parse_hst_action_ind(&ind).unwrap();
        assert_eq!((found.action, found.version), (HAR_LINK_OFFLINE, (0, 0)));
        let mut out = [0u8; FRAME_MAX];
        hst_action_rsp([0; 6], &found, false, &mut out).unwrap();
        assert_eq!(&out[17..23], &[0x00, 0xB0, 0x52, 1, 0, 0]);
        assert_eq!(&out[23..27], &[0; 4]);
    }

    #[test]
    fn a_short_or_foreign_indication_is_not_read() {
        let ind = hex("0001 8705 f505 00b0 5200 0001 88e1 00 62a0 00b052 0a 00 01 03");
        assert_eq!(parse_hst_action_ind(&ind), None);
        let other = hex("0001 8705 f505 00b0 5200 0001 88e1 00 62a0 123456 0a 00 00");
        assert_eq!(parse_hst_action_ind(&other), None);
    }

    #[test]
    fn the_version_request_is_what_the_vendor_tool_sends() {
        let mut out = [0u8; FRAME_MAX];
        let n = sw_ver_req([0x00, 0x01, 0x87, 0x05, 0xF5, 0x05], &mut out).unwrap();
        // Captured from plctool -r on the board: destination, source, EtherType, MMV 0, MMTYPE
        // 0xA000, the OUI (the tool adds a random cookie after it, which the modem ignores).
        let captured = hex("00b05200000100018705f50588e10000a000b052");
        assert_eq!(n, MIN_FRAME);
        assert_eq!(&out[..captured.len()], &captured[..]);
    }

    #[test]
    fn the_version_answer_of_a_qca7005_is_read() {
        // As captured, MMV 0, MMTYPE 0xA001, status 0, device 0x22, 0x24 octets of version.
        let mut frame = hex("0001 8705 f505 0001 8705 f506 88e1 00 01a0 00b052 00 22 24");
        frame.extend_from_slice(b"MAC-QCA7005-3.3.0.31-00-20231004-CS\0");
        frame.resize(120, 0xFF);
        let v = parse_sw_ver_cnf(&frame).unwrap();
        assert_eq!((v.status, v.device, v.version), (0, 0x22, "MAC-QCA7005-3.3.0.31-00-20231004-CS"));
    }

    #[test]
    fn the_key_request_carries_the_network_and_the_key() {
        let (nid, nmk) = ([1, 2, 3, 4, 5, 6, 7], [0xAA; 16]);
        let mut out = [0u8; FRAME_MAX];
        let n = set_key_req([2, 0, 0, 0, 0, 1], nid, nmk, &mut out).unwrap();
        assert_eq!(n, MIN_FRAME);
        assert_eq!(&out[..6], &LOCAL_DEVICE);
        // The version 1 header with its fragmentation information.
        assert_eq!(&out[12..19], &[0x88, 0xE1, 0x01, 0x08, 0x60, 0x00, 0x00]);
        // KeyType, nonces, PID, PRN, PMN, CCo capability, NID, NewEKS, NewKey.
        assert_eq!(out[19], 1);
        assert_eq!(&out[20..28], &[0; 8]);
        assert_eq!(out[28], 4);
        assert_eq!(&out[33..40], &nid);
        assert_eq!(out[40], 1);
        assert_eq!(&out[41..57], &nmk);
    }

    #[test]
    fn the_network_id_and_the_key_frame_are_those_of_the_qualcomm_reference() {
        // The default key and network id of the Qualcomm Atheros `pev` emulator (pev.ini), and the
        // CM_SET_KEY.REQ it sends (captured on the board; its nonce is `AAAAAAAA`, ours is zero).
        let nmk = [0x50, 0xD3, 0xE4, 0x93, 0x3F, 0x85, 0x5B, 0x70, 0x40, 0x78, 0x4D, 0xF8, 0x15, 0xAA, 0x8D, 0xB7];
        let nid = network_id(&nmk);
        assert_eq!(nid, [0xB0, 0xF2, 0xE6, 0x95, 0x66, 0x6B, 0x03]);
        let mut reference = hex("00b052000001 000187 05f505 88e1 01 0860 0000 01 aaaaaaaa 00000000 04 0000 00 00 b0f2e695666b03 01 50d3e4933f855b7040784df815aa8db7");
        reference.resize(60, 0);
        let mut ours = [0u8; FRAME_MAX];
        let n = set_key_req([0x00, 0x01, 0x87, 0x05, 0xF5, 0x05], nid, nmk, &mut ours).unwrap();
        // The same but for the nonce.
        reference[20..24].fill(0);
        assert_eq!(&ours[..n], &reference[..]);
    }

    #[test]
    fn the_key_confirmation_gives_its_result() {
        // As the QCA7005 answered: version 1, the fragmentation information, the result 1, the
        // nonces and the protocol id 4.
        let mut frame = hex("0001 8705 f505 0001 8705 f506 88e1 01 0960 0000 01 e9f52492 00000000 04");
        frame.resize(60, 0);
        assert_eq!(parse_set_key_cnf(&frame), Some(SET_KEY_DONE));
        frame[19] = 0;
        assert_eq!(parse_set_key_cnf(&frame), Some(0));
        assert_eq!(parse_set_key_cnf(&frame[..10]), None);
    }

    #[test]
    fn the_network_information_of_a_lone_modem_is_read() {
        // Captured from the QCA7005: it holds the key of the Qualcomm reference (network id
        // b0f2e695666b03) and is its own coordinator; and with another key.
        let mut frame = hex("0001 8705 f505 0001 8705 f506 88e1 01 39a0 0000 00b052 0000 2200 00 01 b0f2e695666b03 0000 0e 01 00000000 02 000187 05f506 01");
        frame.resize(60, 0);
        let info = parse_nw_info_cnf(&frame).unwrap();
        let nid = [0xB0, 0xF2, 0xE6, 0x95, 0x66, 0x6B, 0x03];
        assert_eq!((info.networks, info.modem), (1, [0x00, 0x01, 0x87, 0x05, 0xF5, 0x06]));
        let n = info.first.unwrap();
        assert_eq!((n.nid, n.snid, n.tei, n.role, n.cco, n.cco_tei), (nid, 0x0E, 1, 2, info.modem, 1));
        assert!(!info.joined(nid), "its own coordinator: alone");
        // The same network led by another modem: joined; another network: not.
        let mut other = info;
        other.first.as_mut().unwrap().cco = [0x00, 0x01, 0x87, 0x05, 0xF5, 0x99];
        assert!(other.joined(nid));
        assert!(!other.joined([1; 7]));
        // No network at all.
        frame[19 + 3 + 5] = 0;
        let none = parse_nw_info_cnf(&frame).unwrap();
        assert_eq!((none.networks, none.first), (0, None));
        assert_eq!(parse_nw_info_cnf(&frame[..20]), None);
    }

    #[test]
    fn the_network_information_request_has_the_fragmentation_information() {
        let mut out = [0u8; FRAME_MAX];
        let n = nw_info_req([0x00, 0x01, 0x87, 0x05, 0xF5, 0x05], &mut out).unwrap();
        // As plctool -m sent it: version 1, type 0xA038, FMI, OUI, zeros.
        assert_eq!(&out[..22], &hex("00b052000001 000187 05f505 88e1 01 38a0 0000 00b052")[..]);
        assert_eq!(n, MIN_FRAME);
    }

    #[test]
    fn the_vendor_key_request_carries_the_key_and_no_network_id() {
        let nmk = [0xAA; 16];
        let mut out = [0u8; FRAME_MAX];
        let n = vs_set_key_req([2, 0, 0, 0, 0, 1], nmk, &mut out).unwrap();
        assert_eq!(n, MIN_FRAME);
        assert_eq!(&out[..6], &LOCAL_DEVICE);
        // Version 0: no fragmentation information; type 0xA050 little endian.
        assert_eq!(&out[12..17], &[0x88, 0xE1, 0x00, 0x50, 0xA0]);
        assert_eq!(&out[17..20], &QCA_OUI);
        assert_eq!(out[20], 1);
        assert_eq!(&out[21..37], &nmk);
        assert_eq!(out[37], 0x0F);
        assert!(out[38..60].iter().all(|b| *b == 0), "destination and device access key stay zero");
        // The answer: OUI, status.
        let mut cnf = hex("0001 8705 f505 0001 8705 f506 88e1 00 51a0 00b052 00");
        cnf.resize(60, 0);
        assert_eq!(parse_vs_set_key_cnf(&cnf), Some(0));
        cnf[20] = 1;
        assert_eq!(parse_vs_set_key_cnf(&cnf), Some(1));
        assert_eq!(parse_vs_set_key_cnf(&cnf[..18]), None);
    }

    #[test]
    fn the_link_status_is_asked_and_read() {
        let mut out = [0u8; FRAME_MAX];
        let n = lnk_status_req([0x00, 0x01, 0x87, 0x05, 0xF5, 0x05], &mut out).unwrap();
        assert_eq!(n, MIN_FRAME);
        assert_eq!(&out[..20], &hex("00b052000001 000187 05f505 88e1 00 b8a0 00b052")[..]);
        let mut cnf = hex("0001 8705 f505 0001 8705 f506 88e1 00 b9a0 00b052 00 01");
        cnf.resize(60, 0);
        assert_eq!(parse_lnk_status_cnf(&cnf), Some(LinkStatus { status: 0, link: true }));
        cnf[21] = 0;
        assert_eq!(parse_lnk_status_cnf(&cnf), Some(LinkStatus { status: 0, link: false }));
        // Another message type, a foreign OUI, a truncated frame.
        cnf[15] = 0x39;
        assert_eq!(parse_lnk_status_cnf(&cnf), None);
        cnf[15] = 0xB9;
        cnf[17] = 0x01;
        assert_eq!(parse_lnk_status_cnf(&cnf), None);
        assert_eq!(parse_lnk_status_cnf(&cnf[..21]), None);
    }

    fn hex(s: &str) -> std::vec::Vec<u8> {
        let s: std::string::String = s.split_whitespace().collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }
}
