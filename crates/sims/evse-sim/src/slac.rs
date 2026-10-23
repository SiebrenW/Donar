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
    /// An AC charger: without a request in `init_ms` it restarts the connection through state E or F, `sequ_retry` times, and
    /// then turns the oscillator off (V2G3-M06-07).
    pub ac: bool,
    /// `TT_EVSE_SLAC_init` (ms): how long the charger waits for the first request.
    pub init_ms: u32,
    /// `T_step_EF` (ms): how long the pilot stays in state E or F to restart the connection.
    pub step_ef_ms: u32,
    /// How long the oscillator is off (X1) when the pilot goes through it (V2G3-M06-06, M07-05); the standards point to
    /// IEC 61851-23 for the minimum, which is not checked here.
    pub x1_ms: u32,
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
            ac: false,
            init_ms: core::Config::DEFAULT.init_ms,
            step_ef_ms: core::Config::DEFAULT.step_ef_ms,
            x1_ms: 3_000,
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
    /// The key is given to `ev`; `linked` once the link is up.
    Matched { ev: Mac, linked: bool },
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
    /// The link comes from a modem ([`Serve`]), not from the cable.
    modem_link: bool,
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
        Self::build(config, false)
    }

    /// For a host that reports the pilot ([`Evse::pilot`]): the vehicle is not taken to be there until a reading says so.
    pub fn wired(config: Config) -> Self {
        Self::build(config, true)
    }

    fn build(config: Config, wired: bool) -> Self {
        let machine_config = core::Config {
            sounds: 10,
            attenuation: Some(config.attenuation),
            attenuation_correction_db: std::array::from_fn(|g| correction(&config.attenuation_correction_db, g)),
            validation: match config.validation {
                Validation::NotRequired => core::Validation::NotRequired,
                Validation::Failure => core::Validation::Failure,
                _ => core::Validation::Ready,
            },
            ac: config.ac,
            init_ms: config.init_ms,
            step_ef_ms: config.step_ef_ms,
            ..core::Config::DEFAULT
        };
        let mut machine = Machine::new(machine_config, config.mac, Fixed(config.nmk));
        let mut tx = [0u8; 256];
        // A host without a pilot: the vehicle is there.
        let step = if wired { core::Step::default() } else { machine.handle(Event::Pilot(Reading::CONNECTED), &mut tx) };
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
            pilot_driven: wired,
            modem_link: false,
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
            core::State::Matched { ev, linked } => State::Matched { ev, linked },
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

    /// The modem lost the link.
    pub fn link_down(&mut self) -> Vec<Vec<u8>> {
        self.feed(Event::LinkDown)
    }

    /// The link is the modem's to report ([`Evse::link_up`], [`Evse::link_down`]); the cable no longer counts as one.
    pub fn link_from_modem(mut self) -> Self {
        self.modem_link = true;
        self
    }

    /// The keys the machine asked the modem to take since the host looked (`CM_SET_KEY`).
    pub fn take_keys(&mut self) -> Vec<([u8; 7], [u8; 16])> {
        std::mem::take(&mut self.keys)
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
            if !self.modem_link && matches!(self.machine.state(), core::State::Matched { linked: false, .. }) {
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

#[cfg(target_os = "linux")]
pub use serve::Serve;

#[cfg(target_os = "linux")]
mod serve {
use super::*;

/// The charger's SLAC with the modem next to it: a [`Evse`] whose keys go to the modem and whose link comes from it. The
/// host reads the frames of the interface and calls [`Serve::step`] with the time, at least as often as
/// [`Serve::next_deadline`] and the modem's polling need.
pub struct Serve<M> {
    pub evse: Evse,
    modem: M,
    /// What went wrong with the modem since the host looked.
    pub errors: Vec<String>,
    keyed: bool,
}

impl<M: evcc_linux::slac::Modem> Serve<M> {
    pub fn new(evse: Evse, modem: M) -> Self {
        Self { evse: evse.link_from_modem(), modem, errors: Vec::new(), keyed: false }
    }

    pub fn next_deadline(&self) -> Option<u64> {
        self.evse.next_deadline()
    }

    /// A change of the control pilot, as measured; returns the frames to send. What the pilot is asked to do about
    /// it comes in [`Evse::pilot_requests`].
    pub fn pilot(&mut self, now_ms: u64, reading: Reading) -> Vec<Vec<u8>> {
        self.evse.at(now_ms);
        self.evse.pilot(reading)
    }

    /// One round: the timers, the frame (if one came), the keys to the modem, the link from the modem. Returns the frames to
    /// send.
    pub fn step(&mut self, now_ms: u64, frame: Option<&[u8]>) -> Vec<Vec<u8>> {
        self.evse.at(now_ms);
        let mut out = self.evse.poll(now_ms);
        if let Some(bytes) = frame {
            out.extend(self.evse.handle(bytes));
        }
        for (nid, nmk) in self.evse.take_keys() {
            match self.modem.set_key(nid, nmk) {
                Ok(()) => self.keyed = true,
                Err(e) => self.errors.push(format!("the modem refused the key: {e}")),
            }
        }
        match self.evse.state() {
            State::Matched { linked: false, .. } if self.keyed => match self.modem.link_up() {
                Ok(true) => out.extend(self.evse.link_up()),
                Ok(false) => {}
                Err(e) => self.errors.push(format!("the link check failed: {e}")),
            },
            State::Matched { linked: true, .. } => match self.modem.link_lost() {
                Ok(true) => out.extend(self.evse.link_down()),
                Ok(false) => {}
                Err(e) => self.errors.push(format!("the link check failed: {e}")),
            },
            _ => {}
        }
        out
    }
}

}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::cell::RefCell;
    use std::io;
    use std::rc::Rc;

    use evcc_linux::slac::Modem;
    use homeplug::{BROADCAST, validate_result};

    use super::*;
    use super::serve::Serve;

    const EV: Mac = [0x02, 0, 0, 0, 0, 0x01];
    const RUN: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    #[derive(Default)]
    struct Wire {
        keys: Vec<([u8; 7], [u8; 16])>,
        link: bool,
        lost: bool,
    }

    struct Fake(Rc<RefCell<Wire>>);

    impl Modem for Fake {
        fn set_key(&mut self, nid: [u8; 7], nmk: [u8; 16]) -> io::Result<()> {
            self.0.borrow_mut().keys.push((nid, nmk));
            Ok(())
        }

        fn link_up(&mut self) -> io::Result<bool> {
            Ok(self.0.borrow().link)
        }

        fn link_lost(&mut self) -> io::Result<bool> {
            Ok(self.0.borrow().lost)
        }
    }

    fn frame(m: Message, dst: Mac) -> Vec<u8> {
        let mut buf = vec![0u8; 256];
        let n = m.write(dst, EV, &mut buf).unwrap();
        buf.truncate(n);
        buf
    }

    /// The vehicle's side up to the match request; returns the time.
    fn match_up(serve: &mut Serve<Fake>, evse: Mac) -> u64 {
        let mut now = 0;
        serve.step(now, Some(&frame(Message::ParmReq { run_id: RUN }, BROADCAST)));
        serve.step(now, Some(&frame(Message::StartAttenChar { num_sounds: 3, time_out: 6, forwarding_sta: BROADCAST, run_id: RUN }, BROADCAST)));
        for cnt in (0..3).rev() {
            now += 20;
            serve.step(now, Some(&frame(Message::MnbcSound { cnt, run_id: RUN, rnd: [0; 16] }, BROADCAST)));
        }
        serve.step(now, Some(&frame(Message::AttenCharRsp { source: EV, run_id: RUN, result: 0 }, evse)));
        let out = serve.step(now, Some(&frame(Message::MatchReq { pev: EV, evse, run_id: RUN }, evse)));
        assert_eq!(out.len(), 1, "CM_SLAC_MATCH.CNF");
        now
    }

    fn charger() -> (Serve<Fake>, Rc<RefCell<Wire>>, Mac) {
        let wire = Rc::new(RefCell::new(Wire::default()));
        let evse = Evse::new(Config::default());
        let mac = evse.mac();
        (Serve::new(evse, Fake(wire.clone())), wire, mac)
    }

    #[test]
    fn the_key_goes_to_the_modem_and_the_link_is_the_modems_to_report() {
        let (mut serve, wire, mac) = charger();
        let now = match_up(&mut serve, mac);
        let nmk = Config::default().nmk;
        assert_eq!(wire.borrow().keys, vec![(nid(&nmk), nmk)]);
        // The cable is no link: the charger is matched but the modem has not found the vehicle.
        serve.step(now + 5_000, None);
        assert_eq!(serve.evse.state(), State::Matched { ev: EV, linked: false });
        assert!(serve.evse.notices.iter().all(|n| *n != Notice::LinkReady(true)));
        wire.borrow_mut().link = true;
        serve.step(now + 6_000, None);
        assert!(serve.next_deadline().is_some(), "the amplitude map window runs");
        serve.step(now + 6_300, None);
        assert!(serve.evse.notices.contains(&Notice::LinkReady(true)));
        // And lost again.
        wire.borrow_mut().lost = true;
        serve.step(now + 7_000, None);
        assert_eq!(serve.evse.notices.last(), Some(&Notice::LinkReady(false)));
    }

    #[test]
    fn a_vehicle_the_modem_never_finds_fails_the_match_and_the_modem_gets_a_new_key() {
        let (mut serve, wire, mac) = charger();
        let now = match_up(&mut serve, mac);
        serve.step(now + 12_000, None);
        assert_eq!(serve.evse.notices.last(), Some(&Notice::Failed { ev: EV, reason: core::Failure::NoLink }));
        assert_eq!(wire.borrow().keys.len(), 2, "the key of the match, then the one that leaves the network");
        assert_eq!(serve.evse.state(), State::Unmatched);
    }

    #[test]
    fn a_validating_vehicle_is_still_counted_with_a_modem() {
        let (mut serve, _wire, mac) = charger();
        serve.step(0, Some(&frame(Message::ParmReq { run_id: RUN }, BROADCAST)));
        serve.step(0, Some(&frame(Message::StartAttenChar { num_sounds: 1, time_out: 6, forwarding_sta: BROADCAST, run_id: RUN }, BROADCAST)));
        serve.step(1, Some(&frame(Message::MnbcSound { cnt: 0, run_id: RUN, rnd: [0; 16] }, BROADCAST)));
        let out = serve.step(2, Some(&frame(Message::ValidateReq { timer: 0, result: validate_result::READY }, mac)));
        assert_eq!(out.len(), 1);
        serve.step(3, Some(&frame(Message::ValidateReq { timer: 5, result: validate_result::READY }, mac)));
        assert_eq!(serve.evse.validation_window_ms(), Some(600));
    }
}
