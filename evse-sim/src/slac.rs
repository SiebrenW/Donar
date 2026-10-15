//! The EVSE side of SLAC (ISO 15118-3:2015 Annex A.9), for testing an EV.
//!
//! Feed it Ethernet frames; it returns the frames to send. It plays a charger whose modem
//! measures a fixed attenuation, so the EV decides "directly connected" or not by the knob.

use homeplug::{BROADCAST, Frame, GROUPS, Mac, Message, RunId};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone)]
pub struct Config {
    pub mac: Mac,
    /// The attenuation (dB) reported for every carrier group.
    pub attenuation: u8,
    /// Requests of the first kind that are not answered, counted per message type.
    pub ignore_parm: u32,
    pub ignore_match: u32,
    /// Never send CM_ATTEN_CHAR.IND (a charger that cannot measure).
    pub no_report: bool,
    pub nmk: [u8; 16],
}

impl Default for Config {
    fn default() -> Self {
        Self {
            mac: [0x02, 0, 0, 0, 0, 0x02],
            attenuation: 5,
            ignore_parm: 0,
            ignore_match: 0,
            no_report: false,
            nmk: [0x5A; 16],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Unmatched,
    /// Answered a parameter request from `ev`; waiting for the measurement.
    Parameters { ev: Mac, run_id: RunId },
    /// Receiving sounds; `expected` is the number announced.
    Sounding { ev: Mac, run_id: RunId, expected: u8, received: u8 },
    /// Sent the report; waiting for the match request.
    Reported { ev: Mac, run_id: RunId },
    Matched { ev: Mac },
}

pub struct Evse {
    config: Config,
    state: State,
    parm_seen: u32,
    match_seen: u32,
}

/// The network ID for a key (HomePlug Green PHY 4.4.3.1): PBKDF1 with SHA-256 and five
/// iterations, no salt, security level 0. The octet layout follows the common
/// implementations (first seven octets of the digest, the last one shifted right by four
/// bits); it has not been checked against a modem.
pub fn nid(nmk: &[u8; 16]) -> [u8; 7] {
    let mut digest = Sha256::digest(nmk);
    for _ in 1..5 {
        digest = Sha256::digest(digest);
    }
    let mut nid: [u8; 7] = digest[..7].try_into().unwrap();
    nid[6] >>= 4;
    nid
}

impl Evse {
    pub fn new(config: Config) -> Self {
        Self { config, state: State::Unmatched, parm_seen: 0, match_seen: 0 }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn mac(&self) -> Mac {
        self.config.mac
    }

    /// Handles a received frame and returns the frames to send in response.
    pub fn handle(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let Some(frame) = Frame::parse(bytes) else { return Vec::new() };
        if frame.dst != BROADCAST && frame.dst != self.config.mac {
            return Vec::new();
        }
        let Some(message) = frame.message() else { return Vec::new() };
        let from = frame.src;
        let mut out = Vec::new();
        match (self.state, message) {
            // A new request restarts the exchange, also from the same EV (V2G3-A09-16).
            (State::Unmatched | State::Parameters { .. } | State::Sounding { .. } | State::Reported { .. }, Message::ParmReq { run_id }) => {
                self.parm_seen += 1;
                if self.parm_seen > self.config.ignore_parm {
                    self.state = State::Parameters { ev: from, run_id };
                    out.push(self.frame(
                        Message::ParmCnf { num_sounds: 10, time_out: 6, forwarding_sta: from, run_id },
                        from,
                    ));
                }
            }
            (State::Parameters { ev, run_id }, Message::StartAttenChar { num_sounds, run_id: r, .. }) if ev == from && r == run_id => {
                self.state = State::Sounding { ev, run_id, expected: num_sounds, received: 0 };
            }
            (State::Sounding { ev, run_id, expected, received }, Message::StartAttenChar { .. }) => {
                // The three repeats of the start message.
                self.state = State::Sounding { ev, run_id, expected, received };
            }
            (State::Sounding { ev, run_id, expected, received }, Message::MnbcSound { run_id: r, .. }) if ev == from && r == run_id => {
                let received = received + 1;
                if received >= expected {
                    self.state = State::Reported { ev, run_id };
                    if !self.config.no_report {
                        out.push(self.frame(
                            Message::AttenCharInd {
                                source: ev,
                                run_id,
                                num_sounds: received,
                                groups: [self.config.attenuation; GROUPS],
                            },
                            ev,
                        ));
                    }
                } else {
                    self.state = State::Sounding { ev, run_id, expected, received };
                }
            }
            (State::Reported { ev, run_id }, Message::MatchReq { pev, evse, run_id: r })
                if ev == from && pev == from && evse == self.config.mac && r == run_id =>
            {
                self.match_seen += 1;
                if self.match_seen > self.config.ignore_match {
                    let nmk = self.config.nmk;
                    out.push(self.frame(Message::MatchCnf { pev, evse, run_id, nid: nid(&nmk), nmk }, ev));
                    self.state = State::Matched { ev };
                }
            }
            // A retransmitted request after the answer was lost (V2G3-A09-97).
            (State::Matched { ev }, Message::MatchReq { pev, evse, run_id }) if ev == from && pev == from && evse == self.config.mac => {
                let nmk = self.config.nmk;
                out.push(self.frame(Message::MatchCnf { pev, evse, run_id, nid: nid(&nmk), nmk }, ev));
            }
            _ => {}
        }
        out
    }

    fn frame(&self, m: Message, dst: Mac) -> Vec<u8> {
        let mut buf = vec![0u8; 128];
        let n = m.write(dst, self.config.mac, &mut buf).expect("frame fits");
        buf.truncate(n);
        buf
    }
}
