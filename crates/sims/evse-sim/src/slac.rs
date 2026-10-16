//! The EVSE side of SLAC (ISO 15118-3:2015 Annex A.9), for testing an EV.
//!
//! Feed it Ethernet frames; it returns the frames to send. It plays a charger whose modem
//! measures a fixed attenuation, so the EV decides "directly connected" or not by the knob.

use homeplug::{BROADCAST, Frame, GROUPS, Mac, Message, RunId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
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
    /// How the charger answers CM_VALIDATE.REQ (validation by control pilot toggles).
    pub validation: Validation,
}

/// The charger's side of the validation by BCB-Toggle (ISO 15118-3 A.9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Validation {
    /// Ready; counts the toggles in the second step (the host reports them with
    /// [`Evse::finish_validation`]).
    Ready,
    /// Answers the first request with "not ready" this many times, then "ready".
    NotReady(u32),
    /// "Not required" in the first step; counts all the same if the EV goes on.
    NotRequired,
    /// "Failure": the feature is not implemented (V2G3-A09-53).
    Failure,
    /// Does not answer at all.
    Silent,
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
            validation: Validation::Ready,
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
    validate_seen: u32,
    /// The EV asked for the second step: the charger counts toggles for this long (ms).
    counting: Option<(Mac, u32)>,
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
        Self { config, state: State::Unmatched, parm_seen: 0, match_seen: 0, validate_seen: 0, counting: None }
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
            // Step 1 of the validation (V2G3-A09-75 ff.) and, with a listening time, step 2.
            (State::Reported { ev, .. } | State::Matched { ev }, Message::ValidateReq { timer, result }) if ev == from => {
                if result != homeplug::validate_result::READY {
                    // V2G3-A09-84: anything but "ready" fails the matching; nothing is sent.
                    return out;
                }
                use homeplug::validate_result::{FAILURE, NOT_READY, NOT_REQUIRED, READY};
                if timer == 0 {
                    self.validate_seen += 1;
                    let answer = match self.config.validation {
                        Validation::Silent => return out,
                        Validation::Failure => FAILURE,
                        Validation::NotRequired => NOT_REQUIRED,
                        Validation::NotReady(n) if self.validate_seen <= n => NOT_READY,
                        Validation::NotReady(_) | Validation::Ready => READY,
                    };
                    out.push(self.frame(Message::ValidateCnf { toggle_num: 0, result: answer }, ev));
                } else if matches!(self.config.validation, Validation::Ready | Validation::NotReady(_) | Validation::NotRequired) {
                    self.counting = Some((ev, u32::from(timer) * 100));
                }
            }
            _ => {}
        }
        out
    }

    /// How long the charger is to count toggles (the listening time of the second request), if
    /// it was asked to. The host waits that long, counts the toggles on the control pilot and
    /// calls [`Self::finish_validation`].
    pub fn validation_window_ms(&self) -> Option<u32> {
        self.counting.map(|(_, ms)| ms)
    }

    /// The listening time is over: answer with the number of toggles seen.
    pub fn finish_validation(&mut self, counted: u8) -> Vec<Vec<u8>> {
        match self.counting.take() {
            Some((ev, _)) => vec![self.frame(Message::ValidateCnf { toggle_num: counted, result: homeplug::validate_result::SUCCESS }, ev)],
            None => Vec::new(),
        }
    }

    fn frame(&self, m: Message, dst: Mac) -> Vec<u8> {
        let mut buf = vec![0u8; 128];
        let n = m.write(dst, self.config.mac, &mut buf).expect("frame fits");
        buf.truncate(n);
        buf
    }
}
