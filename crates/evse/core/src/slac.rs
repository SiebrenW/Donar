//! The EVSE side of the matching process (ISO 15118-3:2015 Annex A.9 and clauses 6, 7 and 9): answers the vehicle's
//! SLAC messages, measures nothing itself but averages what the modem reports, validates the match by control pilot
//! toggles when the vehicle asks, hands out a fresh network key and tells the higher layers when the data link is up.
//!
//! Sans-IO like the other machines. The host feeds the Ethernet frames of the charging interface (also the modem's
//! `CM_ATTEN_PROFILE.IND`), the control pilot readings, the timer expiries and what the modem and the higher layers
//! report ([`Event`]), and carries out the [`Step`]: a frame to send, the modem to configure, toggles to count,
//! timers, a notice for the higher layers (`D-LINK_READY.indication`) and a request for the pilot.
//!
//! Up to [`PARALLEL`] vehicles are served at once (V2G3-M09-01: crosstalk makes more than one hear the charger);
//! the one that sends `CM_SLAC_MATCH.REQ` first gets the key and the others are dropped (V2G3-A09-118). While a vehicle
//! validates, another that asks to validate is told "not ready" (V2G3-A09-78).
//!
//! Choices the standard leaves open:
//! * the averaging of the attenuation profiles is the arithmetic mean over the groups, corrected per group
//!   ([`Config::attenuation_correction_db`]); a host whose modem reports no profile gives a fixed
//!   [`Config::attenuation`];
//! * a vehicle that sends no sound at all gets a `CM_ATTEN_CHAR.IND` with `NumSounds` 0 (HomePlug Green PHY 11.5.47);
//! * the validation window is the `Timer` field of the second request plus one, times 100 ms (HomePlug Green PHY Table
//!   11-148 and ISO 15118-3 Table A.6: 0x00 is 100 ms); a toggle sequence takes 600 ms or more, so a 0x00 is the first step again
//!   (V2G3-A09-77) and is told by the state;
//! * the key is drawn at every return to "Unmatched" and set on the modem when the vehicle is answered with it, or
//!   at once when the network is left (V2G3-A09-121: "reset the NMK").
//!
//! Not here: the amplitude map exchange (V2G3-A09-106 ff.), which the modem's firmware does; the data link is reported
//! [`Config::link_ready_ms`] after the modem found the vehicle (V2G3-A09-117). Also not here: the oscillator itself and
//! the contactors; the machine asks the host for what the pilot has to do ([`PilotRequest`]).

use control_pilot::{Pwm, Reading, State as Cp};
use homeplug::{BROADCAST, Frame, GROUPS, Mac, Message, RunId, validate_result};

/// `C_EVSE_match_parallel` (Table 3).
pub const PARALLEL: usize = 5;

/// Table A.1 and Table 3.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// `TT_match_response`: how long to wait for the vehicle's answer to a report before repeating it.
    pub response_ms: u32,
    /// `TT_match_sequence`: how long to wait for the next request of the vehicle.
    pub sequence_ms: u32,
    /// `TT_EVSE_SLAC_init` (maximum): from the trigger until the first request.
    pub init_ms: u32,
    /// `TT_EVSE_match_MNBC`: from the first `CM_START_ATTEN_CHAR.IND` until the average is calculated.
    pub mnbc_ms: u32,
    /// `TT_EVSE_match_session`: from the report until `CM_SLAC_MATCH.REQ` or `CM_VALIDATE.REQ`.
    pub session_ms: u32,
    /// `TT_match_join`: from the key to the link.
    pub join_ms: u32,
    /// `TT_amp_map_exchange`: how long after the link is detected the higher layers are told, to give a vehicle the time to
    /// ask for an amplitude map.
    pub link_ready_ms: u32,
    /// `C_EV_match_retry`: repeats of a message the vehicle does not answer.
    pub retries: u8,
    /// Number of sounds announced in `CM_SLAC_PARM.CNF` (`C_EV_match_MNBC`).
    pub sounds: u8,
    /// The attenuation (dB) of every group when the modem reports no profile: for a host without a modem that measures (the
    /// simulator). `None` reports the largest attenuation and no sounds, which a vehicle ignores; a charger that claimed to be
    /// close because its modem failed to report would be matched with a vehicle on the next socket.
    pub attenuation: Option<u8>,
    /// Correction (dB) added to the attenuation of each group before it is reported: the modem measures through the wiring
    /// to the plug, which is no part of the path to the vehicle (negative where it costs).
    pub attenuation_correction_db: [i8; GROUPS],
    /// What the charger answers to the first `CM_VALIDATE.REQ`.
    pub validation: Validation,
    /// `T_step_EF`: how long the pilot stays in state E or F to restart the connection.
    pub step_ef_ms: u32,
    /// `C_sequ_retry`: how often an AC charger restarts the connection when no vehicle asks for matching (V2G3-M06-07).
    pub sequ_retry: u8,
    /// An AC charger (V2G3-M06-07 applies).
    pub ac: bool,
    /// `T_conn_max_comm`: how long after a wake-up the link may take before the connection is restarted (V2G3-M07-27).
    pub wake_ms: u32,
    /// What a link error does while the oscillator is on and not at 5 % (V2G3-M07-10).
    pub nominal_error: NominalError,
}

/// What the charger answers to the first `CM_VALIDATE.REQ` of a vehicle (V2G3-A09-58).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validation {
    /// "Ready": the toggles are counted (V2G3-A09-79).
    Ready,
    /// "Not required": the vehicle may go on with the second step or the match (V2G3-A09-81).
    NotRequired,
    /// "Failure": the charger cannot count toggles (V2G3-A09-53, 80).
    Failure,
}

/// V2G3-M07-10: two ways to react to `D-LINK_ERROR.request` with a nominal duty cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NominalError {
    /// Option A: as with 5 % (V2G3-M07-11): leave the network and cycle the pilot through X1 and E/F.
    LikeFivePercent,
    /// Option B: stay in X2, leave the network and be ready for a new matching (V2G3-M07-12).
    KeepOscillator,
}

impl Config {
    pub const DEFAULT: Config = Config {
        response_ms: 200,
        sequence_ms: 400,
        init_ms: 50_000,
        mnbc_ms: 600,
        session_ms: 10_000,
        join_ms: 12_000,
        link_ready_ms: 200,
        retries: 2,
        sounds: 10,
        attenuation: None,
        attenuation_correction_db: [0; GROUPS],
        validation: Validation::Ready,
        step_ef_ms: 4_000,
        sequ_retry: 2,
        ac: false,
        wake_ms: 8_000,
        nominal_error: NominalError::LikeFivePercent,
    };
}

impl Default for Config {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Where the charger gets the network membership key from (V2G3-A09-92: random every time it switches to a private network).
pub trait Random {
    fn nmk(&mut self) -> [u8; 16];
}

/// The timers of the machine; `u8` is the slot of the vehicle's process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timer {
    /// `TT_EVSE_SLAC_init`.
    Init,
    /// `TT_match_join`.
    Join,
    /// `TT_amp_map_exchange`: the link is detected, the higher layers are told when it expires.
    LinkReady,
    /// `T_conn_max_comm` after a wake-up.
    Wake,
    /// `TT_match_sequence`.
    Sequence(u8),
    /// `TT_EVSE_match_MNBC`.
    Mnbc(u8),
    /// `TT_match_response`.
    Response(u8),
    /// `TT_EVSE_match_session`.
    Session(u8),
    /// `TT_EVSE_vald_toggle`.
    Toggle(u8),
}

impl Timer {
    /// The number of different timers, for the host's table.
    pub const COUNT: usize = 4 + 5 * PARALLEL;

    /// The timer with the number [`Timer::index`] gave it.
    pub fn from_index(i: usize) -> Option<Timer> {
        match i {
            0 => Some(Timer::Init),
            1 => Some(Timer::Join),
            2 => Some(Timer::LinkReady),
            3 => Some(Timer::Wake),
            _ if i >= Self::COUNT => None,
            _ => {
                let (kind, slot) = ((i - 4) / PARALLEL, ((i - 4) % PARALLEL) as u8);
                Some(match kind {
                    0 => Timer::Sequence(slot),
                    1 => Timer::Mnbc(slot),
                    2 => Timer::Response(slot),
                    3 => Timer::Session(slot),
                    _ => Timer::Toggle(slot),
                })
            }
        }
    }

    /// A number below [`Timer::COUNT`], different for every timer.
    pub fn index(self) -> usize {
        match self {
            Timer::Init => 0,
            Timer::Join => 1,
            Timer::LinkReady => 2,
            Timer::Wake => 3,
            Timer::Sequence(s) => 4 + usize::from(s),
            Timer::Mnbc(s) => 4 + PARALLEL + usize::from(s),
            Timer::Response(s) => 4 + 2 * PARALLEL + usize::from(s),
            Timer::Session(s) => 4 + 3 * PARALLEL + usize::from(s),
            Timer::Toggle(s) => 4 + 4 * PARALLEL + usize::from(s),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerCmd {
    /// (Re)arm the timer to fire in `ms` milliseconds, replacing a pending expiry.
    Arm { timer: Timer, ms: u32 },
    Cancel(Timer),
}

/// The timers cancelled before the commands of a step apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Clear {
    #[default]
    Nothing,
    /// The timers of the vehicles' processes (sequence, sounds, response, session, toggles).
    Processes,
    /// All timers.
    All,
}

impl Clear {
    /// Whether `timer` is cancelled by it.
    pub fn applies(self, timer: Timer) -> bool {
        match self {
            Clear::Nothing => false,
            Clear::All => true,
            Clear::Processes => matches!(timer, Timer::Sequence(_) | Timer::Mnbc(_) | Timer::Response(_) | Timer::Session(_) | Timer::Toggle(_)),
        }
    }
}

/// The timer commands of one step: first the timers of `clear` are cancelled, then the commands apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timers {
    pub clear: Clear,
    cmds: [Option<TimerCmd>; 6],
}

impl Timers {
    fn with(mut self, cmd: TimerCmd) -> Self {
        match self.cmds.iter_mut().find(|c| c.is_none()) {
            Some(slot) => *slot = Some(cmd),
            None => debug_assert!(false, "more than six timer commands in one step"),
        }
        self
    }

    pub fn iter(&self) -> impl Iterator<Item = TimerCmd> + '_ {
        self.cmds.iter().flatten().copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    /// An Ethernet frame of the charging interface: the vehicle's messages and the modem's `CM_ATTEN_PROFILE.IND`.
    Frame(&'a [u8]),
    TimerExpired(Timer),
    /// A change of the control pilot, as measured. A transition from A, E or F to B, C or D triggers the matching
    /// process (V2G3-M06-11); A ends it and leaves the network (V2G3-M09-19); E and F end it (V2G3-A09-127).
    Pilot(Reading),
    /// The modem detected a station in its logical network: the vehicle joined.
    LinkUp,
    /// The data link was lost (V2G3-M07-03).
    LinkDown,
    /// The answer to [`Toggles::Read`]: the BCB-Toggles counted in the window, or `None` if the host could not count.
    Toggles(Option<u8>),
    /// `D-LINK_ERROR.request` of the higher layers (V2G3-M07-04 ff.).
    LinkError,
    /// `D-LINK_TERMINATE.request` (V2G3-M09-17).
    LinkTerminate,
    /// `D-LINK_PAUSE.request` (V2G3-M07-20): the network parameters are kept.
    LinkPause,
    /// The vehicle woke the charger up by a BCB-Toggle (V2G3-M07-26).
    Wake,
}

/// What the host does with the BCB-Toggles on the control pilot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggles {
    /// Start counting (the counter is reset) for `ms`; at the end the machine asks for the result.
    Start { ms: u32 },
    /// The window is over: send [`Event::Toggles`] with the count.
    Read,
}

/// What the pilot function is asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PilotRequest {
    /// State E or F for `ms` and then X1 or X2 again, first X1 if the oscillator is on (`via_x1`). The state change that
    /// ends it triggers the matching process again.
    ErrorCycle { via_x1: bool, ms: u32 },
    /// The oscillator off and no more retries (V2G3-M06-07).
    X1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// No `CM_START_ATTEN_CHAR.IND` after the parameter exchange (V2G3-A09-40).
    NoStart,
    /// The vehicle did not answer the report (V2G3-A09-46).
    NoAnswer,
    /// Neither `CM_SLAC_MATCH.REQ` nor `CM_VALIDATE.REQ` came (V2G3-A09-96).
    NoRequest,
    /// The vehicle did not follow up the validation (V2G3-A09-83, 90).
    ValidationAbandoned,
    /// A `CM_VALIDATE.REQ` with a result other than "ready" (V2G3-A09-84).
    ValidationRefused,
    /// The logical network was set up, but the vehicle did not join (V2G3-A09-104).
    NoLink,
    /// No link after a wake-up (V2G3-M07-27).
    NoLinkAfterWake,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    /// `D-LINK_READY.indication` (V2G3-M12-01): the data link is up or gone.
    LinkReady(bool),
    /// A matching process failed (the machine waits for the next request).
    Failed { ev: Mac, reason: Failure },
    /// No vehicle asked for matching in `TT_EVSE_SLAC_init` (V2G3-A09-13).
    NoSlac,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Step {
    /// Length of the frame at the start of `tx` to send.
    pub send: Option<usize>,
    /// Configure the local modem with this network ID and key (`CM_SET_KEY.REQ`).
    pub set_key: Option<([u8; 7], [u8; 16])>,
    pub toggles: Option<Toggles>,
    pub timers: Timers,
    pub notice: Option<Notice>,
    pub pilot: Option<PilotRequest>,
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

/// Where the machine is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// No vehicle (state A) or nothing asked within `TT_EVSE_SLAC_init`: requests are not answered.
    Idle,
    /// Answers requests of up to [`PARALLEL`] vehicles, `processes` are being served.
    Unmatched { processes: usize },
    /// The key is given to `ev`; `linked` once the modem found it.
    Matched { ev: Mac, linked: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// `CM_SLAC_PARM.CNF` sent; waiting for `CM_START_ATTEN_CHAR.IND`.
    Parm,
    /// Collecting the sounds and profiles.
    Sounding,
    /// `CM_ATTEN_CHAR.IND` sent; `left` repeats remain until it is answered.
    Reported { left: u8 },
    /// Step 1 of the validation answered with `answer`; `left` repeats remain for the answer.
    Validating { left: u8, answer: u8 },
    /// Counting toggles.
    Counting,
    /// Step 2 answered, waiting for the match request.
    Counted,
}

#[derive(Debug, Clone, Copy)]
struct Process {
    ev: Mac,
    run_id: RunId,
    phase: Phase,
    /// The number of sounds announced.
    expected: u8,
    /// `CM_MNBC_SOUND.IND` seen.
    sounds: u8,
    /// `CM_ATTEN_PROFILE.IND` of the modem seen.
    profiles: u8,
    sum: [u16; GROUPS],
    /// The report that was sent, for a repeat.
    report: [u8; GROUPS],
    reported_sounds: u8,
}

#[derive(Debug, Clone, Copy)]
struct Match {
    ev: Mac,
    nid: [u8; 7],
    nmk: [u8; 16],
    linked: bool,
    paused: bool,
    /// Woken up from a pause: the link is reported as soon as it is there (V2G3-M07-21).
    woke: bool,
}

pub struct Machine<R> {
    config: Config,
    mac: Mac,
    random: R,
    pilot: Option<Reading>,
    init_open: bool,
    nmk: [u8; 16],
    processes: [Option<Process>; PARALLEL],
    matched: Option<Match>,
    /// `D-LINK_READY.indication(link established)` was given and no `(no link)` since.
    announced: bool,
    /// Restarts of the connection since the trigger.
    restarts: u8,
}

impl<R: Random> Machine<R> {
    pub fn new(config: Config, mac: Mac, mut random: R) -> Self {
        let nmk = random.nmk();
        Self { config, mac, random, pilot: None, init_open: false, nmk, processes: [None; PARALLEL], matched: None, announced: false, restarts: 0 }
    }

    pub fn state(&self) -> State {
        if let Some(m) = self.matched {
            return State::Matched { ev: m.ev, linked: m.linked };
        }
        if !self.init_open && self.processes.iter().all(Option::is_none) {
            return State::Idle;
        }
        State::Unmatched { processes: self.processes.iter().flatten().count() }
    }

    /// The vehicle is connected as far as the pilot shows (V2G3-A09-03).
    fn connected(&self) -> bool {
        self.pilot.is_some_and(|r| matches!(r.state, Cp::B | Cp::C | Cp::D))
    }

    pub fn handle(&mut self, event: Event<'_>, tx: &mut [u8]) -> Step {
        match event {
            Event::Pilot(reading) => self.on_pilot(reading),
            Event::Frame(bytes) => self.on_frame(bytes, tx),
            Event::TimerExpired(timer) => self.on_timer(timer, tx),
            Event::Toggles(count) => self.on_toggles(count, tx),
            Event::LinkUp => self.on_link_up(),
            Event::LinkDown => match self.matched.as_mut() {
                Some(m) if m.linked => {
                    m.linked = false;
                    self.announce(false, Step::default().cancel(Timer::LinkReady))
                }
                _ => Step::default(),
            },
            Event::LinkError => self.on_error(),
            // Nothing more is matched until the pilot starts the next process (V2G3-A09-121: the module is "Unmatched" and the
            // vehicle has been told to end the session).
            Event::LinkTerminate => {
                self.init_open = false;
                self.leave()
            }
            Event::LinkPause => {
                if let Some(m) = self.matched.as_mut() {
                    m.paused = true;
                }
                Step::default()
            }
            Event::Wake => match self.matched.as_mut() {
                // V2G3-M07-22, 26: the module is configured to the last known parameters.
                Some(m) if m.paused => {
                    m.paused = false;
                    m.woke = true;
                    m.linked = false;
                    self.announced = false;
                    Step { set_key: Some((m.nid, m.nmk)), ..Step::default() }.arm(Timer::Wake, self.config.wake_ms)
                }
                _ => Step::default(),
            },
        }
    }

    // --- control pilot ---

    fn on_pilot(&mut self, reading: Reading) -> Step {
        let before = self.pilot.map(|r| r.state);
        let after_error = matches!(before, Some(Cp::E | Cp::F));
        self.pilot = Some(reading);
        let from_idle = before.is_none_or(|s| matches!(s, Cp::A | Cp::E | Cp::F | Cp::Invalid));
        match reading.state {
            Cp::B | Cp::C | Cp::D if from_idle => self.trigger(after_error),
            // V2G3-A09-126, M09-19: the vehicle is gone.
            Cp::A => self.stop(true),
            // V2G3-A09-127: the matching process stops; the network is left when the connection is started anew.
            Cp::E | Cp::F => self.stop(false),
            _ => Step::default(),
        }
    }

    /// V2G3-A09-12: the matching process is triggered, `TT_EVSE_SLAC_init` starts.
    fn trigger(&mut self, after_error: bool) -> Step {
        let leaving = self.matched.is_some();
        let mut step = if leaving { self.leave() } else { Step::default() };
        self.processes = [None; PARALLEL];
        self.init_open = true;
        step.timers.clear = Clear::All;
        // The retries of the AC charger count the restarts it made itself, from the plug-in.
        if !after_error {
            self.restarts = 0;
        }
        step.arm(Timer::Init, self.config.init_ms)
    }

    /// The vehicle is gone or the pilot shows an error: no process goes on. With `leave`, the network is left as well.
    fn stop(&mut self, leave: bool) -> Step {
        self.init_open = false;
        self.processes = [None; PARALLEL];
        if self.matched.is_some_and(|m| leave || !m.linked) {
            let mut step = self.leave();
            step.timers.clear = Clear::All;
            return step;
        }
        Step { timers: Timers { clear: Clear::All, ..Timers::default() }, ..Step::default() }
    }

    /// Leaves the logical network: a new key resets the modem's (V2G3-A09-121), the state is "Unmatched". The higher layers
    /// are told if they had been told the link was up (V2G3-M09-18).
    fn leave(&mut self) -> Step {
        self.matched = None;
        self.nmk = self.random.nmk();
        let nid = homeplug::modem::network_id(&self.nmk);
        let step = Step { set_key: Some((nid, self.nmk)), ..Step::default() };
        // The vehicle that asks again can be served; the init window stays as it is.
        let step = step.cancel(Timer::Join).cancel(Timer::LinkReady).cancel(Timer::Wake);
        self.announce(false, step)
    }

    fn announce(&mut self, up: bool, mut step: Step) -> Step {
        if self.announced != up {
            self.announced = up;
            step.notice = Some(Notice::LinkReady(up));
        }
        step
    }

    fn on_error(&mut self) -> Step {
        if self.matched.is_none() && !self.connected() {
            return Step::default();
        }
        let oscillating = self.pilot.is_some_and(|r| matches!(r.pwm, Pwm::Duty(_)));
        let five = self.pilot.is_some_and(|r| r.pwm.is_five_percent());
        let mut step = self.leave();
        if oscillating && !five && self.config.nominal_error == NominalError::KeepOscillator {
            // V2G3-M07-12: stay in X2 and be ready for the next matching.
            self.init_open = true;
            return step.arm(Timer::Init, self.config.init_ms);
        }
        // V2G3-M07-04, 05, 07 to 09: through X1 if the oscillator is on, then E/F for at least T_step_EF.
        self.init_open = false;
        step.pilot = Some(PilotRequest::ErrorCycle { via_x1: oscillating, ms: self.config.step_ef_ms });
        step
    }

    // --- timers ---

    fn on_timer(&mut self, timer: Timer, tx: &mut [u8]) -> Step {
        match timer {
            Timer::Init => {
                self.init_open = false;
                if !self.processes.iter().all(Option::is_none) || self.matched.is_some() || !self.connected() {
                    return Step::default();
                }
                let mut step = Step { notice: Some(Notice::NoSlac), ..Step::default() };
                // V2G3-M06-07: an AC charger restarts the connection a few times before it gives up.
                if self.config.ac {
                    let oscillating = self.pilot.is_some_and(|r| matches!(r.pwm, Pwm::Duty(_)));
                    if self.restarts < self.config.sequ_retry && oscillating {
                        self.restarts += 1;
                        step.pilot = Some(PilotRequest::ErrorCycle { via_x1: false, ms: self.config.step_ef_ms });
                    } else if oscillating {
                        step.pilot = Some(PilotRequest::X1);
                    }
                }
                step
            }
            Timer::Join => {
                // V2G3-A09-104.
                let ev = self.matched.map_or(BROADCAST, |m| m.ev);
                let mut step = self.leave();
                step.notice = Some(Notice::Failed { ev, reason: Failure::NoLink });
                step
            }
            Timer::LinkReady => self.announce(true, Step::default()),
            Timer::Wake => {
                // V2G3-M07-27.
                let ev = self.matched.map_or(BROADCAST, |m| m.ev);
                let oscillating = self.pilot.is_some_and(|r| matches!(r.pwm, Pwm::Duty(_)));
                let mut step = self.leave();
                step.notice = Some(Notice::Failed { ev, reason: Failure::NoLinkAfterWake });
                step.pilot = Some(PilotRequest::ErrorCycle { via_x1: oscillating, ms: self.config.step_ef_ms });
                self.init_open = false;
                step
            }
            Timer::Sequence(s) => self.sequence_expired(s, tx),
            Timer::Mnbc(s) => self.report(s, tx),
            Timer::Response(s) => {
                let Some(p) = self.processes[usize::from(s)] else { return Step::default() };
                match p.phase {
                    Phase::Reported { left: 0 } => self.fail(s, Failure::NoAnswer),
                    Phase::Reported { left } => {
                        self.processes[usize::from(s)].as_mut().unwrap().phase = Phase::Reported { left: left - 1 };
                        self.send_report(s, tx).arm(Timer::Response(s), self.config.response_ms)
                    }
                    _ => Step::default(),
                }
            }
            Timer::Session(s) => match self.processes[usize::from(s)] {
                Some(_) => self.fail(s, Failure::NoRequest),
                None => Step::default(),
            },
            Timer::Toggle(s) => match self.processes[usize::from(s)] {
                Some(Process { phase: Phase::Counting, .. }) => Step { toggles: Some(Toggles::Read), ..Step::default() },
                _ => Step::default(),
            },
        }
    }

    fn sequence_expired(&mut self, s: u8, tx: &mut [u8]) -> Step {
        let Some(p) = self.processes[usize::from(s)] else { return Step::default() };
        match p.phase {
            // V2G3-A09-40.
            Phase::Parm => self.fail(s, Failure::NoStart),
            // V2G3-A09-82, 83: the answer of step 1 is repeated.
            Phase::Validating { left: 0, .. } => self.fail(s, Failure::ValidationAbandoned),
            Phase::Validating { left, answer } => {
                self.processes[usize::from(s)].as_mut().unwrap().phase = Phase::Validating { left: left - 1, answer };
                self.send(Message::ValidateCnf { toggle_num: 0, result: answer }, p.ev, tx).arm(Timer::Sequence(s), self.config.sequence_ms)
            }
            // V2G3-A09-90.
            Phase::Counted => self.fail(s, Failure::ValidationAbandoned),
            _ => Step::default(),
        }
    }

    /// The process failed: it is dropped, the others and the machine go on.
    fn fail(&mut self, s: u8, reason: Failure) -> Step {
        let ev = self.processes[usize::from(s)].take().map_or(BROADCAST, |p| p.ev);
        let mut step = Step { notice: Some(Notice::Failed { ev, reason }), ..Step::default() };
        step = step.cancel(Timer::Sequence(s)).cancel(Timer::Mnbc(s)).cancel(Timer::Response(s)).cancel(Timer::Session(s));
        step.cancel(Timer::Toggle(s))
    }

    // --- frames ---

    fn send(&self, message: Message, dst: Mac, tx: &mut [u8]) -> Step {
        match message.write(dst, self.mac, tx) {
            Ok(n) => Step { send: Some(n), ..Step::default() },
            Err(_) => Step::default(),
        }
    }

    fn slot_of(&self, ev: Mac) -> Option<u8> {
        self.processes.iter().position(|p| p.is_some_and(|p| p.ev == ev)).map(|i| i as u8)
    }

    fn on_frame(&mut self, bytes: &[u8], tx: &mut [u8]) -> Step {
        let Some(frame) = Frame::parse(bytes) else { return Step::default() };
        if frame.dst != BROADCAST && frame.dst != self.mac {
            return Step::default();
        }
        let Some(message) = frame.message() else { return Step::default() };
        let from = frame.src;
        // V2G3-A09-118: once matched, no SLAC message is processed but a repeated request of the vehicle (V2G3-A09-97).
        if let Some(m) = self.matched {
            return match message {
                Message::MatchReq { pev, evse, run_id } if from == m.ev && pev == from && evse == self.mac => {
                    self.send(Message::MatchCnf { pev, evse, run_id, nid: m.nid, nmk: m.nmk }, from, tx)
                }
                _ => Step::default(),
            };
        }
        if !self.connected() {
            return Step::default();
        }
        match message {
            Message::ParmReq { run_id } => self.on_parm_req(from, run_id, tx),
            Message::StartAttenChar { run_id, num_sounds, .. } => self.with_process(from, run_id, |m, s| m.on_start(s, num_sounds)),
            Message::MnbcSound { run_id, .. } => self.with_process(from, run_id, |m, s| m.on_sound(s, tx)),
            Message::AttenProfileInd { pev, groups } => match self.slot_of(pev) {
                Some(s) => self.on_profile(s, groups, tx),
                None => Step::default(),
            },
            Message::AttenCharRsp { source, run_id, .. } if source == from => self.with_process(from, run_id, |m, s| m.on_report_answered(s)),
            Message::ValidateReq { timer, result } => match self.slot_of(from) {
                Some(s) => self.on_validate_req(s, timer, result, tx),
                None => Step::default(),
            },
            Message::MatchReq { pev, evse, run_id } if pev == from && evse == self.mac => match self.slot_of(from) {
                Some(s) if self.processes[usize::from(s)].is_some_and(|p| p.run_id == run_id && !matches!(p.phase, Phase::Parm | Phase::Sounding | Phase::Counting)) => {
                    self.on_match_req(s, tx)
                }
                _ => Step::default(),
            },
            _ => Step::default(),
        }
    }

    /// Runs `f` for the process of `ev` if its run id is `run_id`.
    fn with_process(&mut self, ev: Mac, run_id: RunId, f: impl FnOnce(&mut Self, u8) -> Step) -> Step {
        match self.slot_of(ev) {
            Some(s) if self.processes[usize::from(s)].is_some_and(|p| p.run_id == run_id) => f(self, s),
            _ => Step::default(),
        }
    }

    fn on_parm_req(&mut self, ev: Mac, run_id: RunId, tx: &mut [u8]) -> Step {
        // V2G3-A09-11, 03: only while the window is open.
        if !self.init_open {
            return Step::default();
        }
        // V2G3-A09-16: a request of a vehicle that is served already is a retry; the process starts over.
        let slot = match self.slot_of(ev) {
            Some(s) => s,
            None => match self.processes.iter().position(Option::is_none) {
                Some(i) => i as u8,
                None => return Step::default(),
            },
        };
        self.processes[usize::from(slot)] =
            Some(Process { ev, run_id, phase: Phase::Parm, expected: 0, sounds: 0, profiles: 0, sum: [0; GROUPS], report: [0; GROUPS], reported_sounds: 0 });
        let cnf = Message::ParmCnf { num_sounds: self.config.sounds, time_out: 6, forwarding_sta: ev, run_id };
        let mut step = self.send(cnf, ev, tx);
        step = step.cancel(Timer::Mnbc(slot)).cancel(Timer::Response(slot)).cancel(Timer::Session(slot));
        step.arm(Timer::Sequence(slot), self.config.sequence_ms)
    }

    fn on_start(&mut self, s: u8, announced: u8) -> Step {
        let p = self.processes[usize::from(s)].as_mut().unwrap();
        // The vehicle repeats the message; one is enough (V2G3-A09-39).
        if p.phase != Phase::Parm {
            return Step::default();
        }
        p.phase = Phase::Sounding;
        // The vehicle says how many sounds it sends.
        p.expected = if announced == 0 { self.config.sounds } else { announced };
        // V2G3-A09-42.
        Step::default().cancel(Timer::Sequence(s)).arm(Timer::Mnbc(s), self.config.mnbc_ms)
    }

    fn on_sound(&mut self, s: u8, tx: &mut [u8]) -> Step {
        let p = self.processes[usize::from(s)].as_mut().unwrap();
        if p.phase != Phase::Sounding {
            return Step::default();
        }
        p.sounds = p.sounds.saturating_add(1);
        self.all_in(s, tx)
    }

    fn on_profile(&mut self, s: u8, groups: [u8; GROUPS], tx: &mut [u8]) -> Step {
        let p = self.processes[usize::from(s)].as_mut().unwrap();
        if p.phase != Phase::Sounding {
            return Step::default();
        }
        for (sum, g) in p.sum.iter_mut().zip(groups) {
            *sum += u16::from(g);
        }
        p.profiles = p.profiles.saturating_add(1);
        self.all_in(s, tx)
    }

    /// V2G3-A09-44: with all sounds in, the average is calculated without waiting for the timer.
    fn all_in(&mut self, s: u8, tx: &mut [u8]) -> Step {
        let p = self.processes[usize::from(s)].as_ref().unwrap();
        let done = if p.profiles > 0 { p.profiles >= p.expected } else { p.sounds >= p.expected };
        if done { self.report(s, tx) } else { Step::default() }
    }

    /// V2G3-A09-44, 45: the report of the average goes out and the vehicle has `TT_match_response` to answer.
    fn report(&mut self, s: u8, tx: &mut [u8]) -> Step {
        let corrections = self.config.attenuation_correction_db;
        let fixed = self.config.attenuation;
        let retries = self.config.retries;
        let Some(p) = self.processes[usize::from(s)].as_mut() else { return Step::default() };
        if p.phase != Phase::Sounding {
            return Step::default();
        }
        // The profiles the modem gave are averaged (V2G3-A09-19, rounded); without any, the fixed measurement stands in, or the
        // largest attenuation and no sounds (V2G3-A09-36: a vehicle ignores that report).
        match (p.profiles, fixed) {
            (0, None) => {
                p.report = [255; GROUPS];
                p.reported_sounds = 0;
            }
            (0, Some(fixed)) => {
                for (g, out) in p.report.iter_mut().enumerate() {
                    *out = (i32::from(fixed) + i32::from(corrections[g])).clamp(0, 255) as u8;
                }
                p.reported_sounds = p.sounds;
            }
            (n, _) => {
                for (g, out) in p.report.iter_mut().enumerate() {
                    let mean = (p.sum[g] + u16::from(n) / 2) / u16::from(n);
                    *out = (i32::from(mean) + i32::from(corrections[g])).clamp(0, 255) as u8;
                }
                p.reported_sounds = n;
            }
        }
        p.phase = Phase::Reported { left: retries };
        let mut step = self.send_report(s, tx).cancel(Timer::Mnbc(s)).arm(Timer::Response(s), self.config.response_ms);
        step = step.arm(Timer::Session(s), self.config.session_ms);
        step
    }

    fn send_report(&self, s: u8, tx: &mut [u8]) -> Step {
        let p = self.processes[usize::from(s)].as_ref().unwrap();
        self.send(Message::AttenCharInd { source: p.ev, run_id: p.run_id, num_sounds: p.reported_sounds, groups: p.report }, p.ev, tx)
    }

    fn on_report_answered(&mut self, s: u8) -> Step {
        let p = self.processes[usize::from(s)].as_ref().unwrap();
        if matches!(p.phase, Phase::Reported { .. }) { Step::default().cancel(Timer::Response(s)) } else { Step::default() }
    }

    fn validating(&self) -> bool {
        self.processes.iter().flatten().any(|p| matches!(p.phase, Phase::Validating { .. } | Phase::Counting | Phase::Counted))
    }

    fn on_validate_req(&mut self, s: u8, timer: u8, result: u8, tx: &mut [u8]) -> Step {
        let phase = self.processes[usize::from(s)].as_ref().unwrap().phase;
        let ev = self.processes[usize::from(s)].as_ref().unwrap().ev;
        // Not before the report: the measurement is the vehicle's basis for it.
        if matches!(phase, Phase::Parm | Phase::Sounding) {
            return Step::default();
        }
        // V2G3-A09-84.
        if result != validate_result::READY {
            return self.fail(s, Failure::ValidationRefused);
        }
        // The first request has been made: the session timer is satisfied (V2G3-A09-96).
        let step = Step::default().cancel(Timer::Session(s)).cancel(Timer::Response(s));
        let answered = Step::default().cancel(Timer::Response(s));
        if timer == 0 {
            // Step 1 (V2G3-A09-75).
            if let Phase::Validating { answer, .. } = phase {
                // V2G3-A09-77: the answer is repeated.
                let mut resend = self.send(Message::ValidateCnf { toggle_num: 0, result: answer }, ev, tx);
                resend.timers = step.timers;
                return resend;
            }
            if !matches!(phase, Phase::Reported { .. }) {
                return Step::default();
            }
            let answer = if self.validating() {
                // V2G3-A09-78.
                return self.send(Message::ValidateCnf { toggle_num: 0, result: validate_result::NOT_READY }, ev, tx);
            } else {
                match self.config.validation {
                    Validation::Ready => validate_result::READY,
                    Validation::NotRequired => validate_result::NOT_REQUIRED,
                    Validation::Failure => validate_result::FAILURE,
                }
            };
            let mut out = self.send(Message::ValidateCnf { toggle_num: 0, result: answer }, ev, tx);
            out.timers = step.timers;
            if answer == validate_result::FAILURE {
                // The vehicle decides how to go on (V2G3-A09-51); it still has to send the match request in time.
                out.timers = answered.timers;
                return out;
            }
            // V2G3-A09-82.
            self.processes[usize::from(s)].as_mut().unwrap().phase = Phase::Validating { left: self.config.retries, answer };
            return out.arm(Timer::Sequence(s), self.config.sequence_ms);
        }
        // Step 2: the window of the toggles (V2G3-A09-85).
        if !matches!(phase, Phase::Validating { .. }) {
            return Step::default();
        }
        // `Timer` + 1 units of 100 ms (HomePlug Green PHY Table 11-148). 0 is the first step, which the vehicle repeats.
        let window = (u32::from(timer) + 1) * 100;
        self.processes[usize::from(s)].as_mut().unwrap().phase = Phase::Counting;
        let mut out = step.cancel(Timer::Sequence(s)).arm(Timer::Toggle(s), window);
        out.toggles = Some(Toggles::Start { ms: window });
        out
    }

    fn on_toggles(&mut self, count: Option<u8>, tx: &mut [u8]) -> Step {
        let Some(s) = self.processes.iter().position(|p| p.is_some_and(|p| p.phase == Phase::Counting)) else { return Step::default() };
        let s = s as u8;
        let ev = self.processes[usize::from(s)].as_ref().unwrap().ev;
        self.processes[usize::from(s)].as_mut().unwrap().phase = Phase::Counted;
        // V2G3-A09-87, 88.
        let (toggle_num, result) = match count {
            Some(n) => (n, validate_result::SUCCESS),
            None => (0, validate_result::FAILURE),
        };
        // V2G3-A09-89.
        self.send(Message::ValidateCnf { toggle_num, result }, ev, tx).arm(Timer::Sequence(s), self.config.sequence_ms)
    }

    fn on_match_req(&mut self, s: u8, tx: &mut [u8]) -> Step {
        let p = self.processes[usize::from(s)].unwrap();
        let nmk = self.nmk;
        let nid = homeplug::modem::network_id(&nmk);
        self.processes = [None; PARALLEL];
        self.matched = Some(Match { ev: p.ev, nid, nmk, linked: false, paused: false, woke: false });
        // V2G3-A09-99, 103, 105.
        let mut step = self.send(Message::MatchCnf { pev: p.ev, evse: self.mac, run_id: p.run_id, nid, nmk }, p.ev, tx);
        step.set_key = Some((nid, nmk));
        step.timers.clear = Clear::Processes;
        step.arm(Timer::Join, self.config.join_ms)
    }

    fn on_link_up(&mut self) -> Step {
        let Some(m) = self.matched.as_mut() else { return Step::default() };
        if m.linked {
            return Step::default();
        }
        m.linked = true;
        let step = Step::default().cancel(Timer::Join).cancel(Timer::Wake);
        if m.woke {
            m.woke = false;
            return self.announce(true, step);
        }
        step.arm(Timer::LinkReady, self.config.link_ready_ms)
    }
}
