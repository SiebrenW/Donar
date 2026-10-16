//! EV side of SLAC, the signal level attenuation characterization (ISO 15118-3:2015 Annex A.9):
//! finds out which charger is directly connected and obtains the network key to join its
//! logical network.
//!
//! Sans-IO like the other machines. The host feeds received Ethernet frames, timer expiries
//! and the report that the powerline link is up, and sends the frame in `tx` when
//! [`Step::send`] is set. On [`Step::set_key`] it configures the local modem (CM_SET_KEY.REQ,
//! `homeplug::Message::SetKeyReq`); the link then comes up and the host reports
//! [`Event::LinkUp`].
//!
//! Sequence (V2G3-A09-xx): CM_SLAC_PARM.REQ broadcast, CM_SLAC_PARM.CNF; three
//! CM_START_ATTEN_CHAR.IND; ten CM_MNBC_SOUND.IND at 20 ms intervals; CM_ATTEN_CHAR.IND from
//! each charger, each answered with CM_ATTEN_CHAR.RSP; the charger with an average attenuation
//! below the direct threshold is the connected one; CM_SLAC_MATCH.REQ / .CNF; join.
//!
//! Choices the standard leaves open: the first valid CM_SLAC_PARM.CNF starts the measurement
//! (V2G3-A09-25) and the EVs sends right away instead of waiting out `TP_match_sequence`, a
//! maximum.
//!
//! Validation by control pilot toggling (A.9.3, ISO 15118-3 9.4), per [`Config::validation`]:
//! a charger whose attenuation is between the thresholds ("potentially found"), or any chosen
//! one when asked to, is asked with CM_VALIDATE.REQ (step 1) whether it can validate; when it
//! is ready the EV sends the step 2 request with the time to listen and has the host toggle
//! the control pilot B-C-B ([`Step::toggle`]); the charger answers with the number of toggles
//! it counted, and only if that is the number made is the charger matched (V2G3-A09-73). Any
//! other result moves on to the next candidate. Without validation a charger between the
//! thresholds is only used when [`Config::accept_potential`] allows it (V2G3-A09-49).

use homeplug::{BROADCAST, Frame, GROUPS, Mac, Message, RunId};

use crate::{Timer, TimerCmd, Timers};

/// Table A.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// `C_EV_match_MNBC`: M-Sounds sent.
    pub sounds: u8,
    /// `C_EV_match_retry`: retries of a message in the matching process.
    pub retries: u8,
    /// `C_EV_start_atten_char_inds`.
    pub start_atten_chars: u8,
    /// `TP_EV_batch_msg_interval`, between 20 and 50 ms.
    pub batch_interval_ms: u32,
    /// `TT_match_response`.
    pub response_ms: u32,
    /// `TT_EV_atten_results`.
    pub atten_results_ms: u32,
    /// `TT_match_join`.
    pub join_ms: u32,
    /// `TT_matching_repetition`: a failed run is repeated for this long.
    pub repetition_ms: u32,
    /// `TT_matching_rate`: pause before a repeat.
    pub rate_ms: u32,
    /// `C_EV_match_signalattn_direct` (dB): at or below this the charger is surely the one.
    pub direct_db: u8,
    /// `C_EV_match_signalattn_indirect` (dB): above this no direct charger is found.
    pub indirect_db: u8,
    /// Use a charger between the thresholds without validating it by control pilot toggles.
    pub accept_potential: bool,
    /// Whether to validate the match by control pilot toggles (needs a host that can toggle).
    pub validation: Validation,
    /// `C_EV_vald_nb_toggles` (1 to 3): the toggles made; 0 picks a number per run
    /// ("chosen randomly", V2G3 9.4).
    pub toggles: u8,
    /// `TP_EV_vald_state_duration` (200 to 400 ms): how long each B or C state lasts.
    pub toggle_state_ms: u32,
}

/// When the EV validates a match by control pilot toggles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validation {
    /// Never; chargers between the thresholds depend on [`Config::accept_potential`].
    Never,
    /// For chargers between the thresholds ("EVSE_POTENTIALLY_FOUND", V2G3-M09-07).
    Ambiguous,
    /// For every charger it is about to match with (V2G3-A09-48: "if the EV insists to").
    Always,
}

impl Config {
    pub const DEFAULT: Config = Config {
        sounds: 10,
        retries: 2,
        start_atten_chars: 3,
        batch_interval_ms: 20,
        response_ms: 200,
        atten_results_ms: 1200,
        join_ms: 12_000,
        repetition_ms: 10_000,
        rate_ms: 400,
        direct_db: 10,
        indirect_db: 20,
        accept_potential: false,
        validation: Validation::Never,
        toggles: 0,
        toggle_state_ms: 300,
    };
}

impl Default for Config {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    /// Begin matching. `mac` is the EV host's address; `run_id` should be random.
    Start { mac: Mac, run_id: RunId },
    /// An Ethernet frame received on the charging interface.
    Frame(&'a [u8]),
    TimerExpired(Timer),
    /// The powerline link to the charger is established (the local modem joined the network).
    LinkUp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// No CM_SLAC_PARM.CNF.
    NoResponse,
    /// A charger answered but sent no attenuation report.
    NoAttenuation,
    /// No charger is clearly connected directly.
    NoDirectEvse,
    /// No CM_SLAC_MATCH.CNF.
    NoMatch,
    /// The link did not come up after the network parameters were set.
    JoinTimeout,
    /// Every candidate failed the validation by control pilot toggles.
    ValidationFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The link to `evse` is up. `nid` and `nmk` are the logical network's.
    Matched { evse: Mac, nid: [u8; 7], nmk: [u8; 16] },
    /// Matching failed and the time for repetitions is used up.
    Failed(Failure),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Step {
    /// Length of the frame at the start of `tx` to send.
    pub send: Option<usize>,
    /// Configure the local modem with this network ID and key (CM_SET_KEY.REQ).
    pub set_key: Option<([u8; 7], [u8; 16])>,
    /// Toggle the control pilot now (the charger's validation window has begun).
    pub toggle: Option<Toggle>,
    pub timers: Timers,
    pub outcome: Option<Outcome>,
}

/// The BCB-Toggle sequence of ISO 15118-3 Figure 12: from state B, `count` times C and B again,
/// each state lasting `state_ms`. The host runs it while the machine's timer for the window
/// runs; the pilot ends in state B.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toggle {
    pub count: u8,
    pub state_ms: u32,
}

const MAX_EVSE: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Candidate {
    mac: Mac,
    /// Mean attenuation of the report, once received.
    attenuation: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// CM_SLAC_PARM.REQ sent; `left` retries remain.
    Parm { left: u8 },
    /// Sending `StartAttenChar` number `sent` (1-based) or, after those, sound `sent`.
    StartChars { sent: u8 },
    Sounds { sent: u8 },
    /// All sounds sent; collecting reports until `SlacResults` expires or all arrived.
    Collecting,
    /// Step 1 of the validation sent to `candidates[chosen]`; `left` retries remain.
    Validate1 { left: u8, chosen: usize },
    /// Step 2 sent: `window` while the toggles are made, then the answer is awaited.
    Validate2 { chosen: usize, count: u8, window: bool },
    /// CM_SLAC_MATCH.REQ sent to `candidates[chosen]`.
    Match { left: u8, chosen: usize },
    Joining { chosen: usize, nid: [u8; 7], nmk: [u8; 16] },
    /// A run failed; waiting `rate_ms` before the next one.
    Pause(Failure),
    Done,
}

pub struct Machine {
    config: Config,
    state: State,
    mac: Mac,
    base_run: RunId,
    attempt: u8,
    run_id: RunId,
    candidates: [Option<Candidate>; MAX_EVSE],
    /// `TT_matching_repetition` has run out: the next failure is final.
    out_of_time: bool,
    /// Candidates that were validated (or tried) and dropped in this run.
    dropped: [bool; MAX_EVSE],
    /// A validation was attempted in this run (decides which failure is reported).
    validated: bool,
}

impl Machine {
    pub const fn new(config: Config) -> Self {
        Self {
            config,
            state: State::Idle,
            mac: [0; 6],
            base_run: [0; 8],
            attempt: 0,
            run_id: [0; 8],
            candidates: [None; MAX_EVSE],
            out_of_time: false,
            dropped: [false; MAX_EVSE],
            validated: false,
        }
    }

    pub fn handle(&mut self, event: Event<'_>, tx: &mut [u8]) -> Step {
        match event {
            Event::Start { mac, run_id } => {
                self.mac = mac;
                self.base_run = run_id;
                self.attempt = 0;
                self.out_of_time = false;
                let mut step = self.begin(tx);
                step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::SlacOverall, ms: self.config.repetition_ms });
                step
            }
            Event::TimerExpired(timer) => self.timer(timer, tx),
            Event::Frame(bytes) => match Frame::parse(bytes) {
                Some(f) if f.dst == self.mac => match f.message() {
                    Some(m) => self.message(f.src, m, tx),
                    None => Step::default(),
                },
                _ => Step::default(),
            },
            Event::LinkUp => match self.state {
                State::Joining { chosen, nid, nmk } => {
                    self.state = State::Done;
                    let evse = self.candidates[chosen].map_or(BROADCAST, |c| c.mac);
                    Step {
                        timers: Timers::one(TimerCmd::Cancel(Timer::Slac)).with(TimerCmd::Cancel(Timer::SlacOverall)),
                        outcome: Some(Outcome::Matched { evse, nid, nmk }),
                        ..Step::default()
                    }
                }
                _ => Step::default(),
            },
        }
    }

    /// A run uses its own run id: the first is the host's, repeats count up in the last octet.
    fn begin(&mut self, tx: &mut [u8]) -> Step {
        self.run_id = self.base_run;
        self.run_id[7] = self.run_id[7].wrapping_add(self.attempt);
        self.attempt = self.attempt.wrapping_add(1);
        self.candidates = [None; MAX_EVSE];
        self.dropped = [false; MAX_EVSE];
        self.validated = false;
        self.state = State::Parm { left: self.config.retries };
        self.send(Message::ParmReq { run_id: self.run_id }, BROADCAST, tx)
            .arm(Timer::Slac, self.config.response_ms)
    }

    fn timer(&mut self, timer: Timer, tx: &mut [u8]) -> Step {
        match (timer, self.state) {
            (Timer::SlacOverall, _) => {
                self.out_of_time = true;
                match self.state {
                    State::Pause(reason) => self.finish(reason),
                    _ => Step::default(),
                }
            }
            (Timer::Slac, State::Parm { left }) => {
                if left == 0 {
                    return self.fail(Failure::NoResponse, tx);
                }
                self.state = State::Parm { left: left - 1 };
                self.send(Message::ParmReq { run_id: self.run_id }, BROADCAST, tx).arm(Timer::Slac, self.config.response_ms)
            }
            (Timer::Slac, State::StartChars { sent }) if sent < self.config.start_atten_chars => {
                self.state = State::StartChars { sent: sent + 1 };
                self.start_atten_char(tx).arm(Timer::Slac, self.config.batch_interval_ms)
            }
            // The interval after the last CM_START_ATTEN_CHAR.IND has passed: first sound.
            (Timer::Slac, State::StartChars { .. }) => self.sound(0, tx),
            (Timer::Slac, State::Sounds { sent }) => self.sound(sent, tx),
            (Timer::SlacResults, State::Collecting) => self.decide(tx),
            (Timer::SlacResults, State::StartChars { .. } | State::Sounds { .. }) => {
                // The window ran out before the sounds were sent (a very slow host): the
                // reports that arrive after the last sound are still collected.
                Step::default()
            }
            (Timer::Slac, State::Validate1 { left, chosen }) => {
                if left == 0 {
                    // V2G3-A09-63: no answer to any retry, the next candidate.
                    return self.drop_candidate(chosen, tx);
                }
                self.state = State::Validate1 { left: left - 1, chosen };
                self.validate_req(chosen, 0, tx).arm(Timer::Slac, self.config.response_ms)
            }
            // The toggling is over: wait for the answer (V2G3-A09-70).
            (Timer::Slac, State::Validate2 { chosen, count, window: true }) => {
                self.state = State::Validate2 { chosen, count, window: false };
                Step::default().arm(Timer::Slac, self.config.response_ms)
            }
            // V2G3-A09-71.
            (Timer::Slac, State::Validate2 { chosen, window: false, .. }) => self.drop_candidate(chosen, tx),
            (Timer::Slac, State::Match { left, chosen }) => {
                if left == 0 {
                    return self.fail(Failure::NoMatch, tx);
                }
                self.state = State::Match { left: left - 1, chosen };
                self.match_req(chosen, tx).arm(Timer::Slac, self.config.response_ms)
            }
            (Timer::Slac, State::Joining { .. }) => self.fail(Failure::JoinTimeout, tx),
            (Timer::Slac, State::Pause(reason)) if self.out_of_time => self.finish(reason),
            (Timer::Slac, State::Pause(_)) => self.begin(tx),
            _ => Step::default(),
        }
    }

    fn message(&mut self, src: Mac, m: Message, tx: &mut [u8]) -> Step {
        match (self.state, m) {
            (State::Parm { .. }, Message::ParmCnf { run_id, .. }) if run_id == self.run_id => {
                self.remember(src);
                self.state = State::StartChars { sent: 1 };
                let mut step = self.start_atten_char(tx).arm(Timer::Slac, self.config.batch_interval_ms);
                step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::SlacResults, ms: self.config.atten_results_ms });
                step
            }
            // Further chargers that answer while the measurement starts are noted.
            (State::StartChars { .. } | State::Sounds { .. }, Message::ParmCnf { run_id, .. }) if run_id == self.run_id => {
                self.remember(src);
                Step::default()
            }
            (
                State::StartChars { .. } | State::Sounds { .. } | State::Collecting,
                Message::AttenCharInd { source, run_id, num_sounds, groups },
            ) if run_id == self.run_id && source == self.mac && num_sounds > 0 => {
                // A report from a charger that did not answer the parameter request is used too.
                self.remember(src);
                let sum: u32 = groups.iter().map(|&g| u32::from(g)).sum();
                if let Some(c) = self.candidates.iter_mut().flatten().find(|c| c.mac == src) {
                    c.attenuation = Some((sum / GROUPS as u32) as u8);
                }
                let response = Message::AttenCharRsp { source: self.mac, run_id: self.run_id, result: 0 };
                let step = self.send(response, src, tx);
                let all_in = self.candidates.iter().flatten().all(|c| c.attenuation.is_some());
                if matches!(self.state, State::Collecting) && all_in {
                    // Reports from all known chargers are in, so there is no need to wait for
                    // more. The answer goes out first; the results timer is set to expire at
                    // once and the decision (CM_SLAC_MATCH.REQ) follows in the next step.
                    return step.arm(Timer::SlacResults, 0);
                }
                step
            }
            (State::Validate1 { chosen, .. }, Message::ValidateCnf { toggle_num: 0, result }) if self.is_candidate(chosen, src) => {
                use homeplug::validate_result::{FAILURE, NOT_READY, NOT_REQUIRED, READY};
                let found = self.candidates[chosen].and_then(|c| c.attenuation).is_some_and(|a| a <= self.config.direct_db);
                match result {
                    // V2G3-A09-67: ready, so the toggling follows.
                    READY => self.validate_step2(chosen, tx),
                    // V2G3-A09-50: not required; a charger that is surely the one is taken at
                    // its word, an uncertain one is made to do it.
                    NOT_REQUIRED if found => self.begin_match(chosen, tx),
                    NOT_REQUIRED => self.validate_step2(chosen, tx),
                    // V2G3-A09-66: busy; ask again until the retries are used up.
                    NOT_READY => {
                        let left = match self.state {
                            State::Validate1 { left, .. } => left,
                            _ => 0,
                        };
                        if left == 0 {
                            return self.drop_candidate(chosen, tx);
                        }
                        self.state = State::Validate1 { left: left - 1, chosen };
                        self.validate_req(chosen, 0, tx).arm(Timer::Slac, self.config.response_ms)
                    }
                    // V2G3-A09-51: no validation here; what was obvious or allowed is used.
                    FAILURE if found || self.config.accept_potential => self.begin_match(chosen, tx),
                    // Success is not a step 1 answer (V2G3-A09-65), anything else is garbage.
                    _ => self.drop_candidate(chosen, tx),
                }
            }
            // A repeated step 1 answer ("ready") is not the step 2 answer.
            (State::Validate2 { chosen, count, .. }, Message::ValidateCnf { toggle_num, result })
                if self.is_candidate(chosen, src) && !(toggle_num == 0 && result == homeplug::validate_result::READY) =>
            {
                // V2G3-A09-73: only the number of toggles made confirms the match.
                if result == homeplug::validate_result::SUCCESS && toggle_num == count {
                    self.begin_match(chosen, tx)
                } else {
                    self.drop_candidate(chosen, tx)
                }
            }
            (State::Match { chosen, .. }, Message::MatchCnf { pev, evse, run_id, nid, nmk })
                if run_id == self.run_id && pev == self.mac && self.candidates[chosen].is_some_and(|c| c.mac == evse && c.mac == src) =>
            {
                self.state = State::Joining { chosen, nid, nmk };
                Step { set_key: Some((nid, nmk)), ..Step::default() }.arm(Timer::Slac, self.config.join_ms)
            }
            _ => Step::default(),
        }
    }

    /// Chooses the charger to match with once the reports are in (or time is up).
    fn decide(&mut self, tx: &mut [u8]) -> Step {
        let reported = self.candidates.iter().flatten().any(|c| c.attenuation.is_some());
        match self.best_candidate() {
            Some(chosen) => self.proceed(chosen, tx).cancel(Timer::SlacResults),
            None if reported => self.fail(Failure::NoDirectEvse, tx),
            None => self.fail(Failure::NoAttenuation, tx),
        }
    }

    /// The candidate with the lowest attenuation that may be used and was not dropped.
    fn best_candidate(&self) -> Option<usize> {
        let mut best: Option<(usize, u8)> = None;
        for (i, c) in self.candidates.iter().enumerate() {
            let Some(Candidate { attenuation: Some(a), .. }) = c else { continue };
            if self.dropped[i] {
                continue;
            }
            let validating = self.config.validation != Validation::Never;
            let usable = *a <= self.config.direct_db || *a <= self.config.indirect_db && (validating || self.config.accept_potential);
            if usable && best.is_none_or(|(_, b)| *a < b) {
                best = Some((i, *a));
            }
        }
        best.map(|(i, _)| i)
    }

    /// Validates the candidate first if the configuration asks for it, else matches.
    fn proceed(&mut self, chosen: usize, tx: &mut [u8]) -> Step {
        let attenuation = self.candidates[chosen].and_then(|c| c.attenuation).unwrap_or(u8::MAX);
        let validate = match self.config.validation {
            Validation::Never => false,
            Validation::Ambiguous => attenuation > self.config.direct_db,
            Validation::Always => true,
        };
        if !validate {
            return self.begin_match(chosen, tx);
        }
        self.validated = true;
        self.state = State::Validate1 { left: self.config.retries, chosen };
        self.validate_req(chosen, 0, tx).arm(Timer::Slac, self.config.response_ms)
    }

    fn begin_match(&mut self, chosen: usize, tx: &mut [u8]) -> Step {
        self.state = State::Match { left: self.config.retries, chosen };
        self.match_req(chosen, tx).arm(Timer::Slac, self.config.response_ms)
    }

    /// The validation of `chosen` did not confirm it: the next candidate, or the run fails.
    fn drop_candidate(&mut self, chosen: usize, tx: &mut [u8]) -> Step {
        self.dropped[chosen] = true;
        match self.best_candidate() {
            Some(next) => self.proceed(next, tx),
            None => self.fail(Failure::ValidationFailed, tx),
        }
    }

    fn is_candidate(&self, chosen: usize, src: Mac) -> bool {
        self.candidates[chosen].is_some_and(|c| c.mac == src)
    }

    /// CM_VALIDATE.REQ: step 1 (unicast, timer 0) or step 2 (broadcast, the listening time).
    fn validate_req(&self, chosen: usize, timer: u8, tx: &mut [u8]) -> Step {
        let evse = self.candidates[chosen].map_or(BROADCAST, |c| c.mac);
        let dst = if timer == 0 { evse } else { BROADCAST };
        self.send(Message::ValidateReq { timer, result: homeplug::validate_result::READY }, dst, tx)
    }

    /// Step 2: the time to listen covers the whole sequence (V2G3-A09-67) and is the window
    /// the host toggles in (V2G3-A09-68, 69).
    fn validate_step2(&mut self, chosen: usize, tx: &mut [u8]) -> Step {
        let count = match self.config.toggles {
            0 => 1 + self.run_id[7] % 3,
            n => n.clamp(1, 3),
        };
        let states = 2 * u32::from(count) + 1;
        // The listening time is sent in units of 100 ms, with a margin of one unit.
        let units = (states * self.config.toggle_state_ms).div_ceil(100) + 1;
        let units = units.min(35) as u8;
        self.state = State::Validate2 { chosen, count, window: true };
        let mut step = self.validate_req(chosen, units, tx).arm(Timer::Slac, u32::from(units) * 100);
        step.toggle = Some(Toggle { count, state_ms: self.config.toggle_state_ms });
        step
    }

    /// Sends M-Sound number `sent` (0-based), counting down from `sounds - 1`.
    fn sound(&mut self, sent: u8, tx: &mut [u8]) -> Step {
        let cnt = self.config.sounds.saturating_sub(sent + 1);
        let last = sent + 1 >= self.config.sounds;
        self.state = if last { State::Collecting } else { State::Sounds { sent: sent + 1 } };
        let step = self.send(Message::MnbcSound { cnt, run_id: self.run_id, rnd: self.rnd(cnt) }, BROADCAST, tx);
        if last { step } else { step.arm(Timer::Slac, self.config.batch_interval_ms) }
    }

    fn match_req(&self, chosen: usize, tx: &mut [u8]) -> Step {
        let evse = self.candidates[chosen].map_or(BROADCAST, |c| c.mac);
        self.send(Message::MatchReq { pev: self.mac, evse, run_id: self.run_id }, evse, tx)
    }

    fn start_atten_char(&self, tx: &mut [u8]) -> Step {
        let m = Message::StartAttenChar {
            num_sounds: self.config.sounds,
            time_out: 6, // TT_EVSE_match_MNBC, 600 ms in units of 100 ms
            forwarding_sta: self.mac,
            run_id: self.run_id,
        };
        self.send(m, BROADCAST, tx)
    }

    fn remember(&mut self, mac: Mac) {
        if self.candidates.iter().flatten().any(|c| c.mac == mac) {
            return;
        }
        if let Some(slot) = self.candidates.iter_mut().find(|c| c.is_none()) {
            *slot = Some(Candidate { mac, attenuation: None });
        }
    }

    /// A run failed: repeat it after `rate_ms` while time remains, else give up.
    fn fail(&mut self, reason: Failure, _tx: &mut [u8]) -> Step {
        if self.out_of_time {
            return self.finish(reason);
        }
        self.state = State::Pause(reason);
        Step::default().arm(Timer::Slac, self.config.rate_ms).cancel(Timer::SlacResults)
    }

    fn finish(&mut self, reason: Failure) -> Step {
        self.state = State::Done;
        Step {
            timers: Timers::one(TimerCmd::Cancel(Timer::Slac)).with(TimerCmd::Cancel(Timer::SlacResults)),
            outcome: Some(Outcome::Failed(reason)),
            ..Step::default()
        }
    }

    fn send(&self, m: Message, dst: Mac, tx: &mut [u8]) -> Step {
        // The buffer must hold a full frame (the longest is 107 octets); a host that passes
        // less cannot do SLAC, which shows up as the process never getting anywhere.
        let send = m.write(dst, self.mac, tx).ok();
        Step { send, ..Step::default() }
    }

    /// The random value of an M-Sound. SLAC is unsecured here, so it only has to differ.
    fn rnd(&self, cnt: u8) -> [u8; 16] {
        let mut x = u64::from_le_bytes(self.run_id) ^ (u64::from(cnt) << 56) | 1;
        let mut out = [0u8; 16];
        for chunk in out.chunks_mut(8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            chunk.copy_from_slice(&x.to_le_bytes());
        }
        out
    }
}

impl Step {
    fn arm(mut self, timer: Timer, ms: u32) -> Self {
        self.timers = self.timers.with(TimerCmd::Arm { timer, ms });
        self
    }

    fn cancel(mut self, timer: Timer) -> Self {
        self.timers = self.timers.with(TimerCmd::Cancel(timer));
        self
    }
}
