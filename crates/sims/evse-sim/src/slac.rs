//! The EVSE side of SLAC (ISO 15118-3:2015 Annex A.9), for testing an EV: the charger's machine
//! ([`evse_core::slac`]) behind a clock, with the faults a test wants to inject.
//!
//! Feed it Ethernet frames; it returns the frames to send. It plays a charger whose modem
//! measures a fixed attenuation, so the EV decides "directly connected" or not by the knob. Time is the host's:
//! [`Evse::at`] sets it, [`Evse::poll`] lets the timers of the machine run out, [`Evse::next_deadline`] says when
//! to call it. A host that never does (a test in virtual time) has a charger that waits forever.

use control_pilot::Reading;
use evse_core::slac::{self as core, Event, Machine, Notice, PilotRequest, Random, Timer, TimerCmd, Toggles};
use homeplug::{Frame, GROUPS, Mac, Message};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub mac: Mac,
    /// The attenuation (dB) reported for every carrier group.
    pub attenuation: u8,
    /// Correction (dB) per carrier group added to the attenuation before it is reported to the vehicle: the modem measures
    /// through the wiring between itself and the plug, which is no part of the path to the vehicle (negative where it
    /// costs). One value for all groups, or one per group (58); none for no correction.
    pub attenuation_correction_db: Vec<i8>,
    /// Requests of the first kind that are not answered, counted per message type.
    pub ignore_parm: u32,
    pub ignore_match: u32,
    /// Never send CM_ATTEN_CHAR.IND (a charger that cannot measure).
    pub no_report: bool,
    pub nmk: [u8; 16],
    /// How the charger answers CM_VALIDATE.REQ (validation by control pilot toggles).
    pub validation: Validation,
    /// The timers and limits of the machine.
    #[serde(skip)]
    #[schemars(skip)]
    pub machine: core::Config,
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
            attenuation_correction_db: Vec::new(),
            ignore_parm: 0,
            ignore_match: 0,
            no_report: false,
            nmk: [0x5A; 16],
            validation: Validation::Ready,
            machine: core::Config::DEFAULT,
        }
    }
}

/// The correction of group `g`: the one value for all, the value of the group, or none.
pub fn correction(values: &[i8], g: usize) -> i8 {
    match values.len() {
        1 => values[0],
        GROUPS => values[g],
        _ => 0,
    }
}


#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Waiting for a vehicle, or serving some.
    Unmatched,
    Matched { ev: Mac },
}

/// The same key every time: a test knows it.
struct Fixed([u8; 16]);

impl Random for Fixed {
    fn nmk(&mut self) -> [u8; 16] {
        self.0
    }
}

pub struct Evse {
    config: Config,
    machine: Machine<Fixed>,
    now: u64,
    deadlines: Vec<Option<u64>>,
    parm_seen: u32,
    match_seen: u32,
    validate_seen: u32,
    /// The EV asked for the second step: the charger counts toggles for this long (ms).
    counting: Option<u32>,
    toggle_timer: Option<Timer>,
    /// The host reports the pilot itself; otherwise the vehicle is taken to be there and the cable is the link.
    pilot_driven: bool,
    no_slac: bool,
    /// What the machine asked for since the host looked.
    pub notices: Vec<Notice>,
    pub pilot_requests: Vec<PilotRequest>,
    pub keys: Vec<([u8; 7], [u8; 16])>,
}

/// The network ID for a key ([`homeplug::modem::network_id`]).
pub fn nid(nmk: &[u8; 16]) -> [u8; 7] {
    homeplug::modem::network_id(nmk)
}

impl Evse {
    pub fn new(config: Config) -> Self {
        let machine_config = core::Config {
            sounds: 10,
            attenuation: config.attenuation,
            attenuation_correction_db: std::array::from_fn(|g| correction(&config.attenuation_correction_db, g)),
            validation: match config.validation {
                Validation::NotRequired => core::Validation::NotRequired,
                Validation::Failure => core::Validation::Failure,
                _ => core::Validation::Ready,
            },
            ..config.machine
        };
        let mut machine = Machine::new(machine_config, config.mac, Fixed(config.nmk));
        let mut tx = [0u8; 256];
        // A host without a pilot: the vehicle is there.
        let step = machine.handle(Event::Pilot(Reading::CONNECTED), &mut tx);
        let mut evse = Self {
            config,
            machine,
            now: 0,
            deadlines: vec![None; Timer::COUNT],
            parm_seen: 0,
            match_seen: 0,
            validate_seen: 0,
            counting: None,
            toggle_timer: None,
            pilot_driven: false,
            no_slac: false,
            notices: Vec::new(),
            pilot_requests: Vec::new(),
            keys: Vec::new(),
        };
        evse.apply(step);
        evse
    }

    pub fn state(&self) -> State {
        match self.machine.state() {
            core::State::Matched { ev, .. } => State::Matched { ev },
            _ => State::Unmatched,
        }
    }

    pub fn mac(&self) -> Mac {
        self.config.mac
    }

    /// Sets the time of the next calls (milliseconds on any clock that never goes back).
    pub fn at(&mut self, now_ms: u64) {
        self.now = now_ms;
    }

    /// When [`Evse::poll`] has something to do.
    pub fn next_deadline(&self) -> Option<u64> {
        self.deadlines.iter().flatten().min().copied()
    }

    /// Lets the timers that have run out at `now_ms` fire; returns the frames to send.
    pub fn poll(&mut self, now_ms: u64) -> Vec<Vec<u8>> {
        self.now = now_ms;
        let mut out = Vec::new();
        while let Some((i, _)) = self.deadlines.iter().enumerate().filter_map(|(i, d)| d.filter(|d| *d <= now_ms).map(|d| (i, d))).min_by_key(|(_, d)| *d) {
            self.deadlines[i] = None;
            let timer = Timer::from_index(i).expect("a timer");
            out.extend(self.feed(Event::TimerExpired(timer)));
        }
        out
    }

    /// Reports a pilot reading (a host with a pilot; without one the vehicle is connected from the start).
    pub fn pilot(&mut self, reading: Reading) -> Vec<Vec<u8>> {
        self.pilot_driven = true;
        self.feed(Event::Pilot(reading))
    }

    /// The modem found the vehicle in the network.
    pub fn link_up(&mut self) -> Vec<Vec<u8>> {
        self.feed(Event::LinkUp)
    }

    /// One call of the machine: the frames to send, and whether it asks for the toggle count.
    fn call(&mut self, event: Event<'_>) -> (Vec<Vec<u8>>, bool) {
        let mut tx = [0u8; 256];
        let step = self.machine.handle(event, &mut tx);
        let mut out = Vec::new();
        if let Some(n) = step.send {
            out.extend(self.filtered(tx[..n].to_vec()));
        }
        let read = step.toggles == Some(Toggles::Read);
        self.apply(step);
        (out, read)
    }

    fn feed(&mut self, event: Event<'_>) -> Vec<Vec<u8>> {
        let (mut out, read) = self.call(event);
        if read {
            // The listening time is over and nobody counted: a failure (V2G3-A09-88).
            out.extend(self.call(Event::Toggles(None)).0);
        }
        if !self.pilot_driven {
            // No modem and no pilot: the vehicle that has the key is on the cable, and a charger nobody asked for a
            // long time waits for the next one as a charger that sees a new plug-in does.
            if matches!(self.machine.state(), core::State::Matched { linked: false, .. }) {
                self.call(Event::LinkUp);
            }
            if std::mem::take(&mut self.no_slac) {
                self.call(Event::Pilot(Reading { state: control_pilot::State::A, pwm: control_pilot::Pwm::Off }));
                self.call(Event::Pilot(Reading::CONNECTED));
            }
        }
        out
    }

    fn apply(&mut self, step: core::Step) {
        for (i, d) in self.deadlines.iter_mut().enumerate() {
            if step.timers.clear.applies(Timer::from_index(i).unwrap()) {
                *d = None;
            }
        }
        for cmd in step.timers.iter() {
            match cmd {
                TimerCmd::Arm { timer, ms } => {
                    self.deadlines[timer.index()] = Some(self.now + u64::from(ms));
                    if matches!(timer, Timer::Toggle(_)) {
                        self.toggle_timer = Some(timer);
                    }
                }
                TimerCmd::Cancel(timer) => self.deadlines[timer.index()] = None,
            }
        }
        if let Some(key) = step.set_key {
            self.keys.push(key);
        }
        if let Some(Toggles::Start { ms }) = step.toggles {
            self.counting = Some(ms);
        }
        self.no_slac |= step.notice == Some(Notice::NoSlac);
        self.notices.extend(step.notice);
        self.pilot_requests.extend(step.pilot);
    }

    /// The faults the test asked for, on what the machine sends.
    fn filtered(&mut self, bytes: Vec<u8>) -> Vec<Vec<u8>> {
        let Some(message) = Frame::parse(&bytes).and_then(|f| f.message()) else { return vec![bytes] };
        match message {
            Message::AttenCharInd { .. } if self.config.no_report => Vec::new(),
            Message::ValidateCnf { toggle_num: 0, result } if result != homeplug::validate_result::FAILURE => {
                self.validate_seen += 1;
                match self.config.validation {
                    Validation::Silent => Vec::new(),
                    Validation::NotReady(n) if self.validate_seen <= n => {
                        let dst = Frame::parse(&bytes).unwrap().dst;
                        let mut out = vec![0u8; 256];
                        let n = Message::ValidateCnf { toggle_num: 0, result: homeplug::validate_result::NOT_READY }.write(dst, self.config.mac, &mut out).unwrap();
                        out.truncate(n);
                        vec![out]
                    }
                    _ => vec![bytes],
                }
            }
            _ => vec![bytes],
        }
    }

    /// Handles a received frame and returns the frames to send in response.
    pub fn handle(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        match Frame::parse(bytes).and_then(|f| f.message()) {
            Some(Message::ParmReq { .. }) => {
                self.parm_seen += 1;
                if self.parm_seen <= self.config.ignore_parm {
                    return Vec::new();
                }
            }
            Some(Message::MatchReq { .. }) => {
                self.match_seen += 1;
                if self.match_seen <= self.config.ignore_match {
                    return Vec::new();
                }
            }
            // The validation: nothing is counted for a charger that does not answer.
            Some(Message::ValidateReq { timer, .. }) if timer != 0 && matches!(self.config.validation, Validation::Silent) => return Vec::new(),
            _ => {}
        }
        self.feed(Event::Frame(bytes))
    }

    /// How long the charger is to count toggles (the listening time of the second request), if
    /// it was asked to. The host waits that long, counts the toggles on the control pilot and
    /// calls [`Self::finish_validation`].
    pub fn validation_window_ms(&self) -> Option<u32> {
        self.counting
    }

    /// The listening time is over: answer with the number of toggles seen.
    pub fn finish_validation(&mut self, counted: u8) -> Vec<Vec<u8>> {
        let (Some(_), Some(timer)) = (self.counting.take(), self.toggle_timer.take()) else { return Vec::new() };
        self.deadlines[timer.index()] = None;
        // The window is over: the machine asks for the count, and the host has it.
        let (mut out, read) = self.call(Event::TimerExpired(timer));
        if read {
            out.extend(self.call(Event::Toggles(Some(counted))).0);
        }
        out
    }
}
