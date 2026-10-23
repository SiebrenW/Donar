//! The charger's matching machine (ISO 15118-3 Annex A.9, clauses 6, 7, 9): requirement by requirement against scripted
//! frames, and the whole exchange against the vehicle's machine in virtual time.

use control_pilot::{Pwm, Reading, State as Cp};
use evse_core::slac::{
    Config, Event, Failure, Machine, NominalError, Notice, PARALLEL, PilotRequest, Random, State, Step, Timer, TimerCmd, Toggles, Validation,
};
use homeplug::{BROADCAST, GROUPS, Mac, Message, RunId, validate_result};

const EVSE: Mac = [0x02, 0, 0, 0, 0, 0x10];
const EV: Mac = [0x02, 0, 0, 0, 0, 0x01];
const EV2: Mac = [0x02, 0, 0, 0, 0, 0x02];
const RUN: RunId = [1, 2, 3, 4, 5, 6, 7, 8];

struct Counting(u8);

impl Random for Counting {
    fn nmk(&mut self) -> [u8; 16] {
        self.0 += 1;
        [self.0; 16]
    }
}

fn nmk(n: u8) -> [u8; 16] {
    [n; 16]
}

fn frame(m: Message, dst: Mac, src: Mac) -> Vec<u8> {
    let mut buf = vec![0u8; 256];
    let n = m.write(dst, src, &mut buf).unwrap();
    buf.truncate(n);
    buf
}

const B5: Reading = Reading::CONNECTED;
const B_NOMINAL: Reading = Reading { state: Cp::B, pwm: Pwm::Duty(53) };
const X1: Reading = Reading { state: Cp::B, pwm: Pwm::Off };

/// A machine with a connected vehicle (pilot B, 5 %), the init window open, and the helpers to drive it.
struct Bench {
    m: Machine<Counting>,
    tx: [u8; 256],
    /// Everything armed: the timer, what remains.
    timers: Vec<(Timer, u32)>,
}

impl Bench {
    fn with(config: Config) -> Self {
        let mut b = Self { m: Machine::new(config, EVSE, Counting(0)), tx: [0; 256], timers: Vec::new() };
        b.event(Event::Pilot(B5));
        b
    }

    fn new() -> Self {
        Self::with(Config { attenuation: Some(5), ..Config::DEFAULT })
    }

    fn event(&mut self, e: Event<'_>) -> Step {
        let step = self.m.handle(e, &mut self.tx);
        self.timers.retain(|(t, _)| !step.timers.clear.applies(*t));
        for cmd in step.timers.iter() {
            match cmd {
                TimerCmd::Arm { timer, ms } => {
                    self.timers.retain(|(t, _)| *t != timer);
                    self.timers.push((timer, ms));
                }
                TimerCmd::Cancel(timer) => self.timers.retain(|(t, _)| *t != timer),
            }
        }
        step
    }

    /// The message of the frame sent in the step.
    fn sent(&self, step: &Step) -> Option<Message> {
        let n = step.send?;
        homeplug::Frame::parse(&self.tx[..n]).and_then(|f| f.message())
    }

    fn dst(&self, step: &Step) -> Mac {
        homeplug::Frame::parse(&self.tx[..step.send.unwrap()]).unwrap().dst
    }

    fn frame(&mut self, bytes: &[u8]) -> Step {
        self.event(Event::Frame(bytes))
    }

    fn armed(&self, t: Timer) -> Option<u32> {
        self.timers.iter().find(|(x, _)| *x == t).map(|(_, ms)| *ms)
    }

    fn parm(&mut self, ev: Mac, run: RunId) -> Step {
        self.frame(&frame(Message::ParmReq { run_id: run }, BROADCAST, ev))
    }

    fn start(&mut self, ev: Mac, run: RunId) -> Step {
        self.frame(&frame(Message::StartAttenChar { num_sounds: 10, time_out: 6, forwarding_sta: BROADCAST, run_id: run }, BROADCAST, ev))
    }

    fn sound(&mut self, ev: Mac, run: RunId, cnt: u8) -> Step {
        self.frame(&frame(Message::MnbcSound { cnt, run_id: run, rnd: [0; 16] }, BROADCAST, ev))
    }

    /// Request, start, ten sounds; returns the report's step.
    fn measured(&mut self, ev: Mac, run: RunId) -> Step {
        self.parm(ev, run);
        self.start(ev, run);
        let mut last = Step::default();
        for i in (0..10).rev() {
            last = self.sound(ev, run, i);
        }
        last
    }

    fn answer_report(&mut self, ev: Mac, run: RunId) -> Step {
        self.frame(&frame(Message::AttenCharRsp { source: ev, run_id: run, result: 0 }, EVSE, ev))
    }

    fn validate(&mut self, ev: Mac, timer: u8, result: u8) -> Step {
        self.frame(&frame(Message::ValidateReq { timer, result }, EVSE, ev))
    }

    fn match_req(&mut self, ev: Mac, run: RunId) -> Step {
        self.frame(&frame(Message::MatchReq { pev: ev, evse: EVSE, run_id: run }, EVSE, ev))
    }

    fn expire(&mut self, t: Timer) -> Step {
        assert!(self.armed(t).is_some(), "{t:?} is not armed: {:?}", self.timers);
        self.timers.retain(|(x, _)| *x != t);
        self.event(Event::TimerExpired(t))
    }

    /// A vehicle matched and linked; returns the steps of the link coming up.
    fn matched(&mut self) {
        self.measured(EV, RUN);
        self.answer_report(EV, RUN);
        self.match_req(EV, RUN);
    }
}

#[test]
fn a_request_is_answered_with_the_parameters_within_the_window() {
    let mut b = Bench::new();
    assert_eq!(b.m.state(), State::Unmatched { processes: 0 });
    assert_eq!(b.armed(Timer::Init), Some(50_000), "V2G3-A09-12: TT_EVSE_SLAC_init starts with the trigger");
    let step = b.parm(EV, RUN);
    assert_eq!(b.sent(&step), Some(Message::ParmCnf { num_sounds: 10, time_out: 6, forwarding_sta: EV, run_id: RUN }));
    assert_eq!(b.dst(&step), EV);
    assert_eq!(b.armed(Timer::Sequence(0)), Some(400), "V2G3-A09-39: TT_match_sequence starts with the answer");
    assert_eq!(b.m.state(), State::Unmatched { processes: 1 });
}

#[test]
fn nothing_is_answered_without_a_vehicle_on_the_pilot() {
    // V2G3-A09-03.
    let mut m = Machine::new(Config::DEFAULT, EVSE, Counting(0));
    let mut tx = [0u8; 256];
    let request = frame(Message::ParmReq { run_id: RUN }, BROADCAST, EV);
    assert_eq!(m.handle(Event::Frame(&request), &mut tx).send, None, "no pilot reading yet");
    m.handle(Event::Pilot(Reading { state: Cp::A, pwm: Pwm::Off }), &mut tx);
    assert_eq!(m.handle(Event::Frame(&request), &mut tx).send, None, "state A");
    m.handle(Event::Pilot(B5), &mut tx);
    assert!(m.handle(Event::Frame(&request), &mut tx).send.is_some());
}

#[test]
fn no_request_in_the_window_means_no_slac_and_late_requests_are_not_answered() {
    let mut b = Bench::new();
    let step = b.expire(Timer::Init);
    assert_eq!(step.notice, Some(Notice::NoSlac), "V2G3-A09-13");
    assert_eq!(b.m.state(), State::Idle);
    assert_eq!(b.parm(EV, RUN).send, None, "V2G3-A09-11: only as long as the window is open");
}

#[test]
fn a_frame_for_another_station_or_with_a_bad_content_is_ignored() {
    // V2G3-A09-14.
    let mut b = Bench::new();
    let to_other = frame(Message::ParmReq { run_id: RUN }, [2, 9, 9, 9, 9, 9], EV);
    assert_eq!(b.frame(&to_other).send, None);
    let mut bad = frame(Message::ParmReq { run_id: RUN }, BROADCAST, EV);
    bad[19] = 1; // application type
    assert_eq!(b.frame(&bad).send, None);
    assert_eq!(b.armed(Timer::Init), Some(50_000), "the window keeps running");
    assert_eq!(b.m.state(), State::Unmatched { processes: 0 });
}

#[test]
fn a_new_request_of_a_served_vehicle_starts_the_process_over() {
    // V2G3-A09-16.
    let mut b = Bench::new();
    b.parm(EV, RUN);
    b.start(EV, RUN);
    assert!(b.armed(Timer::Mnbc(0)).is_some());
    let run2 = [9; 8];
    let step = b.parm(EV, run2);
    assert_eq!(b.sent(&step), Some(Message::ParmCnf { num_sounds: 10, time_out: 6, forwarding_sta: EV, run_id: run2 }));
    assert_eq!(b.m.state(), State::Unmatched { processes: 1 });
    assert_eq!(b.armed(Timer::Mnbc(0)), None);
    assert_eq!(b.armed(Timer::Sequence(0)), Some(400));
    // The old run id is not served any more.
    assert_eq!(b.start(EV, RUN).timers.iter().count(), 0);
}

#[test]
fn no_start_message_fails_the_process() {
    let mut b = Bench::new();
    b.parm(EV, RUN);
    let step = b.expire(Timer::Sequence(0));
    assert_eq!(step.notice, Some(Notice::Failed { ev: EV, reason: Failure::NoStart }), "V2G3-A09-40");
    assert_eq!(b.m.state(), State::Unmatched { processes: 0 });
}

#[test]
fn the_report_goes_out_when_all_sounds_are_in_and_carries_the_fixed_attenuation() {
    let config = Config { attenuation: Some(7), ..Config::DEFAULT };
    let mut b = Bench::with(config);
    b.parm(EV, RUN);
    let start = b.start(EV, RUN);
    assert_eq!(b.armed(Timer::Sequence(0)), None, "V2G3-A09-39: one start message stops the timer");
    assert_eq!(b.armed(Timer::Mnbc(0)), Some(600), "V2G3-A09-42");
    assert_eq!(start.send, None);
    for i in (1..10).rev() {
        assert_eq!(b.sound(EV, RUN, i).send, None);
    }
    let step = b.sound(EV, RUN, 0);
    assert_eq!(b.sent(&step), Some(Message::AttenCharInd { source: EV, run_id: RUN, num_sounds: 10, groups: [7; GROUPS] }));
    assert_eq!(b.armed(Timer::Response(0)), Some(200), "V2G3-A09-45");
    assert_eq!(b.armed(Timer::Session(0)), Some(10_000));
    assert_eq!(b.armed(Timer::Mnbc(0)), None);
}

#[test]
fn the_profiles_of_the_modem_are_averaged_with_the_correction_per_group() {
    // V2G3-A09-19.
    let mut config = Config::DEFAULT;
    config.attenuation_correction_db[0] = -2;
    config.attenuation_correction_db[1] = 100;
    let mut b = Bench::with(config);
    b.parm(EV, RUN);
    b.start(EV, RUN);
    let profile = |level: u8| {
        let mut g = [level; GROUPS];
        g[2] = level.saturating_mul(2);
        frame(Message::AttenProfileInd { pev: EV, groups: g }, EVSE, [0x00, 0xB0, 0x52, 0, 0, 1])
    };
    let mut last = Step::default();
    for level in [10, 20, 30, 40, 50, 60, 70, 80, 90, 100] {
        assert!(last.send.is_none());
        last = b.frame(&profile(level));
    }
    let Some(Message::AttenCharInd { num_sounds, groups, .. }) = b.sent(&last) else { panic!("no report: {last:?}") };
    assert_eq!(num_sounds, 10);
    assert_eq!(groups[0], 53, "the mean 55 less 2");
    assert_eq!(groups[1], 155, "clamped values are exact below 256");
    assert_eq!(groups[2], 110);
}

#[test]
fn the_mean_is_rounded_and_a_report_without_profiles_claims_no_closeness() {
    // Profiles 10, 11 and 11 average 10.67, which is 11.
    let mut b = Bench::with(Config::DEFAULT);
    b.parm(EV, RUN);
    b.start(EV, RUN);
    let mut last = Step::default();
    for level in [10, 11, 11, 11, 11, 11, 11, 11, 11, 11] {
        last = b.frame(&frame(Message::AttenProfileInd { pev: EV, groups: [level; GROUPS] }, EVSE, [0x00, 0xB0, 0x52, 0, 0, 1]));
    }
    let Some(Message::AttenCharInd { groups, .. }) = b.sent(&last) else { panic!() };
    assert_eq!(groups[0], 11, "10.9 rounds up");
    // Sounds but no profile from the modem and no fixed measurement: the largest attenuation and no sounds, which a vehicle
    // ignores.
    let mut c = Bench::with(Config::DEFAULT);
    let report = c.measured(EV, RUN);
    assert_eq!(c.sent(&report), Some(Message::AttenCharInd { source: EV, run_id: RUN, num_sounds: 0, groups: [255; GROUPS] }));
}

#[test]
fn the_timer_ends_the_collection_with_what_arrived() {
    // V2G3-A09-43, 44.
    let mut b = Bench::new();
    b.parm(EV, RUN);
    b.start(EV, RUN);
    for i in (7..10).rev() {
        b.sound(EV, RUN, i);
    }
    let step = b.expire(Timer::Mnbc(0));
    let Some(Message::AttenCharInd { num_sounds, .. }) = b.sent(&step) else { panic!() };
    assert_eq!(num_sounds, 3);
}

#[test]
fn a_vehicle_that_sent_no_sound_is_told_so() {
    let mut b = Bench::new();
    b.parm(EV, RUN);
    b.start(EV, RUN);
    let step = b.expire(Timer::Mnbc(0));
    let Some(Message::AttenCharInd { num_sounds, .. }) = b.sent(&step) else { panic!() };
    assert_eq!(num_sounds, 0, "HomePlug Green PHY 11.5.47");
}

#[test]
fn an_unanswered_report_is_repeated_twice_and_then_the_process_fails() {
    // V2G3-A09-46.
    let mut b = Bench::new();
    let report = b.measured(EV, RUN);
    let first = b.sent(&report).unwrap();
    for _ in 0..2 {
        let again = b.expire(Timer::Response(0));
        assert_eq!(b.sent(&again), Some(first));
        assert_eq!(b.armed(Timer::Response(0)), Some(200));
    }
    let step = b.expire(Timer::Response(0));
    assert_eq!(step.notice, Some(Notice::Failed { ev: EV, reason: Failure::NoAnswer }));
    assert_eq!(b.m.state(), State::Unmatched { processes: 0 });
}

#[test]
fn an_answered_report_is_not_repeated_and_the_session_timer_limits_the_wait() {
    let mut b = Bench::new();
    b.measured(EV, RUN);
    b.answer_report(EV, RUN);
    assert_eq!(b.armed(Timer::Response(0)), None);
    let step = b.expire(Timer::Session(0));
    assert_eq!(step.notice, Some(Notice::Failed { ev: EV, reason: Failure::NoRequest }), "V2G3-A09-96");
}

#[test]
fn the_match_request_gets_the_key_and_the_charger_waits_for_the_link() {
    let mut b = Bench::new();
    b.measured(EV, RUN);
    b.answer_report(EV, RUN);
    let step = b.match_req(EV, RUN);
    let key = nmk(1);
    let nid = homeplug::modem::network_id(&key);
    assert_eq!(b.sent(&step), Some(Message::MatchCnf { pev: EV, evse: EVSE, run_id: RUN, nid, nmk: key }));
    assert_eq!(step.set_key, Some((nid, key)), "V2G3-A09-105");
    assert_eq!(b.armed(Timer::Join), Some(12_000), "V2G3-A09-103");
    assert_eq!(b.m.state(), State::Matched { ev: EV, linked: false });
    // V2G3-A09-97: a repeated request is answered again.
    let again = b.match_req(EV, RUN);
    assert_eq!(b.sent(&again), Some(Message::MatchCnf { pev: EV, evse: EVSE, run_id: RUN, nid, nmk: key }));
    // V2G3-A09-118: nothing else is processed.
    assert_eq!(b.parm(EV2, [3; 8]).send, None);
    assert_eq!(b.match_req(EV2, RUN).send, None);
}

#[test]
fn a_match_request_with_a_wrong_address_is_ignored() {
    let mut b = Bench::new();
    b.measured(EV, RUN);
    b.answer_report(EV, RUN);
    let wrong_evse = frame(Message::MatchReq { pev: EV, evse: [9; 6], run_id: RUN }, EVSE, EV);
    assert_eq!(b.frame(&wrong_evse).send, None);
    let wrong_pev = frame(Message::MatchReq { pev: EV2, evse: EVSE, run_id: RUN }, EVSE, EV);
    assert_eq!(b.frame(&wrong_pev).send, None);
    assert_eq!(b.match_req(EV, [7; 8]).send, None, "another run");
    assert_eq!(b.m.state(), State::Unmatched { processes: 1 });
}

#[test]
fn the_link_is_reported_to_the_higher_layers_after_the_amplitude_map_window() {
    let mut b = Bench::new();
    b.matched();
    let up = b.event(Event::LinkUp);
    assert_eq!(up.notice, None);
    assert_eq!(b.armed(Timer::Join), None);
    assert_eq!(b.armed(Timer::LinkReady), Some(200), "V2G3-A09-117");
    assert_eq!(b.m.state(), State::Matched { ev: EV, linked: true });
    let ready = b.expire(Timer::LinkReady);
    assert_eq!(ready.notice, Some(Notice::LinkReady(true)), "V2G3-M09-16");
    // V2G3-M07-03: a lost link is reported as such.
    let down = b.event(Event::LinkDown);
    assert_eq!(down.notice, Some(Notice::LinkReady(false)));
}

#[test]
fn a_vehicle_that_never_joins_fails_the_match_and_the_key_is_replaced() {
    let mut b = Bench::new();
    b.matched();
    let step = b.expire(Timer::Join);
    assert_eq!(step.notice, Some(Notice::Failed { ev: EV, reason: Failure::NoLink }), "V2G3-A09-104");
    let fresh = nmk(2);
    assert_eq!(step.set_key, Some((homeplug::modem::network_id(&fresh), fresh)));
    assert_ne!(fresh, nmk(1), "V2G3-A09-92");
    assert_eq!(b.m.state(), State::Unmatched { processes: 0 });
    // The window of the trigger is still open: the vehicle may try again.
    assert_eq!(b.armed(Timer::Init), Some(50_000));
    assert!(b.parm(EV, [4; 8]).send.is_some());
}

#[test]
fn terminate_leaves_the_network_with_a_new_key_and_reports_no_link() {
    let mut b = Bench::new();
    b.matched();
    b.event(Event::LinkUp);
    b.expire(Timer::LinkReady);
    let step = b.event(Event::LinkTerminate);
    assert_eq!(step.notice, Some(Notice::LinkReady(false)), "V2G3-M09-18");
    let fresh = nmk(2);
    assert_eq!(step.set_key, Some((homeplug::modem::network_id(&fresh), fresh)), "V2G3-A09-121");
    assert_eq!(b.m.state(), State::Idle, "nothing is matched until the pilot starts the next process");
    assert_eq!(b.parm(EV, [6; 8]).send, None);
    // Terminate without a link up announced reports nothing.
    let mut c = Bench::new();
    c.matched();
    assert_eq!(c.event(Event::LinkTerminate).notice, None);
}

#[test]
fn state_a_ends_the_matching_and_leaves_the_network() {
    // V2G3-A09-126, M09-19.
    let mut b = Bench::new();
    b.matched();
    b.event(Event::LinkUp);
    b.expire(Timer::LinkReady);
    let step = b.event(Event::Pilot(Reading { state: Cp::A, pwm: Pwm::Off }));
    assert_eq!(step.notice, Some(Notice::LinkReady(false)));
    assert!(step.set_key.is_some());
    assert_eq!(b.m.state(), State::Idle);
    // A process in the making is dropped without a modem call.
    let mut c = Bench::new();
    c.parm(EV, RUN);
    let step = c.event(Event::Pilot(Reading { state: Cp::A, pwm: Pwm::Off }));
    assert_eq!(step.set_key, None);
    assert_eq!(c.m.state(), State::Idle);
    assert!(c.timers.is_empty());
}

#[test]
fn state_e_or_f_ends_the_matching_process_and_the_next_b_starts_a_new_one() {
    // V2G3-A09-127, M06-11.
    let mut b = Bench::new();
    b.parm(EV, RUN);
    b.event(Event::Pilot(Reading { state: Cp::E, pwm: Pwm::Off }));
    assert_eq!(b.m.state(), State::Idle);
    assert_eq!(b.parm(EV, RUN).send, None);
    b.event(Event::Pilot(B5));
    assert_eq!(b.armed(Timer::Init), Some(50_000));
    assert!(b.parm(EV, RUN).send.is_some());
    // B to C is no trigger.
    let step = b.event(Event::Pilot(Reading::CHARGING));
    assert_eq!(step.timers.iter().count(), 0);
}

#[test]
fn at_most_five_vehicles_are_served_and_the_first_to_match_wins() {
    // V2G3-M09-01.
    let mut b = Bench::new();
    let macs: Vec<Mac> = (0..PARALLEL as u8 + 1).map(|i| [2, 0, 0, 0, 1, i]).collect();
    for (i, mac) in macs.iter().enumerate() {
        let sent = b.parm(*mac, [i as u8; 8]).send.is_some();
        assert_eq!(sent, i < PARALLEL, "vehicle {i}");
    }
    assert_eq!(b.m.state(), State::Unmatched { processes: PARALLEL });
    // The second in the list reports first and matches.
    for (i, mac) in macs.iter().enumerate().take(PARALLEL) {
        b.start(*mac, [i as u8; 8]);
    }
    for i in (0..10).rev() {
        for (j, mac) in macs.iter().enumerate().take(PARALLEL) {
            b.sound(*mac, [j as u8; 8], i);
        }
    }
    let step = b.match_req(macs[1], [1; 8]);
    assert!(matches!(b.sent(&step), Some(Message::MatchCnf { .. })));
    assert_eq!(b.m.state(), State::Matched { ev: macs[1], linked: false });
    assert_eq!(b.match_req(macs[0], [0; 8]).send, None);
    assert!(b.timers.iter().all(|(t, _)| matches!(t, Timer::Join | Timer::Init)), "{:?}", b.timers);
}

mod validation {
    use super::*;

    #[test]
    fn step_1_is_answered_ready_and_step_2_counts_the_toggles() {
        let mut b = Bench::new();
        b.measured(EV, RUN);
        b.answer_report(EV, RUN);
        let step = b.validate(EV, 0, validate_result::READY);
        assert_eq!(b.sent(&step), Some(Message::ValidateCnf { toggle_num: 0, result: validate_result::READY }), "V2G3-A09-75");
        assert_eq!(b.armed(Timer::Sequence(0)), Some(400), "V2G3-A09-82");
        assert_eq!(b.armed(Timer::Session(0)), None, "either request satisfies the session timer");
        // V2G3-A09-77: a repeated step 1 is answered with the same.
        let again = b.validate(EV, 0, validate_result::READY);
        assert_eq!(b.sent(&again), Some(Message::ValidateCnf { toggle_num: 0, result: validate_result::READY }));
        // Step 2: the field is 21, which is 2.2 s of listening (0x00 is 100 ms).
        let two = b.validate(EV, 21, validate_result::READY);
        assert_eq!(two.toggles, Some(Toggles::Start { ms: 2200 }), "V2G3-A09-85");
        assert_eq!(b.armed(Timer::Toggle(0)), Some(2200));
        assert_eq!(b.armed(Timer::Sequence(0)), None);
        let read = b.expire(Timer::Toggle(0));
        assert_eq!(read.toggles, Some(Toggles::Read), "V2G3-A09-86");
        let done = b.event(Event::Toggles(Some(3)));
        assert_eq!(b.sent(&done), Some(Message::ValidateCnf { toggle_num: 3, result: validate_result::SUCCESS }), "V2G3-A09-87");
        assert_eq!(b.armed(Timer::Sequence(0)), Some(400), "V2G3-A09-89");
        // The match request ends the wait.
        let m = b.match_req(EV, RUN);
        assert!(matches!(b.sent(&m), Some(Message::MatchCnf { .. })));
    }

    #[test]
    fn a_count_that_could_not_be_made_is_a_failure() {
        // V2G3-A09-88.
        let mut b = Bench::new();
        b.measured(EV, RUN);
        b.validate(EV, 0, validate_result::READY);
        b.validate(EV, 10, validate_result::READY);
        b.expire(Timer::Toggle(0));
        let done = b.event(Event::Toggles(None));
        assert_eq!(b.sent(&done), Some(Message::ValidateCnf { toggle_num: 0, result: validate_result::FAILURE }));
    }

    #[test]
    fn another_vehicle_is_told_not_ready_while_one_validates() {
        // V2G3-A09-78, M09-13.
        let mut b = Bench::new();
        b.measured(EV, RUN);
        b.measured(EV2, [2; 8]);
        b.validate(EV, 0, validate_result::READY);
        let step = b.validate(EV2, 0, validate_result::READY);
        assert_eq!(b.sent(&step), Some(Message::ValidateCnf { toggle_num: 0, result: validate_result::NOT_READY }));
        assert_eq!(b.dst(&step), EV2);
        // It may ask again once the first is through (it matched, or failed).
        b.expire(Timer::Sequence(0));
        b.expire(Timer::Sequence(0));
        b.expire(Timer::Sequence(0));
        let again = b.validate(EV2, 0, validate_result::READY);
        assert_eq!(b.sent(&again), Some(Message::ValidateCnf { toggle_num: 0, result: validate_result::READY }));
    }

    #[test]
    fn the_answer_is_repeated_and_then_the_validation_is_given_up() {
        // V2G3-A09-82, 83.
        let mut b = Bench::new();
        b.measured(EV, RUN);
        b.validate(EV, 0, validate_result::READY);
        for _ in 0..2 {
            let again = b.expire(Timer::Sequence(0));
            assert_eq!(b.sent(&again), Some(Message::ValidateCnf { toggle_num: 0, result: validate_result::READY }));
        }
        let step = b.expire(Timer::Sequence(0));
        assert_eq!(step.notice, Some(Notice::Failed { ev: EV, reason: Failure::ValidationAbandoned }));
    }

    #[test]
    fn a_request_whose_result_is_not_ready_fails_the_process() {
        // V2G3-A09-84.
        let mut b = Bench::new();
        b.measured(EV, RUN);
        let step = b.validate(EV, 0, validate_result::NOT_READY);
        assert_eq!(step.notice, Some(Notice::Failed { ev: EV, reason: Failure::ValidationRefused }));
    }

    #[test]
    fn a_charger_that_cannot_count_says_failure_and_one_that_need_not_says_not_required() {
        // V2G3-A09-53, 80, 81.
        let mut b = Bench::with(Config { validation: Validation::Failure, ..Config::DEFAULT });
        b.measured(EV, RUN);
        let step = b.validate(EV, 0, validate_result::READY);
        assert_eq!(b.sent(&step), Some(Message::ValidateCnf { toggle_num: 0, result: validate_result::FAILURE }));
        assert_eq!(b.validate(EV, 5, validate_result::READY).toggles, None, "no counting");
        assert!(b.armed(Timer::Session(0)).is_some(), "the vehicle still has to match in time");
        let mut c = Bench::with(Config { validation: Validation::NotRequired, ..Config::DEFAULT });
        c.measured(EV, RUN);
        let step = c.validate(EV, 0, validate_result::READY);
        assert_eq!(c.sent(&step), Some(Message::ValidateCnf { toggle_num: 0, result: validate_result::NOT_REQUIRED }));
        // The vehicle may go on with step 2 or the match.
        assert!(c.validate(EV, 5, validate_result::READY).toggles.is_some());
    }

    #[test]
    fn a_step_2_without_step_1_is_ignored() {
        let mut b = Bench::new();
        b.measured(EV, RUN);
        assert_eq!(b.validate(EV, 5, validate_result::READY).toggles, None);
    }
}

mod link_layer {
    use super::*;

    fn linked() -> Bench {
        let mut b = Bench::new();
        b.matched();
        b.event(Event::LinkUp);
        b.expire(Timer::LinkReady);
        b
    }

    #[test]
    fn a_link_error_with_five_percent_goes_through_x1_and_e_or_f() {
        // V2G3-M07-05, 06, 07, 08.
        let mut b = linked();
        let step = b.event(Event::LinkError);
        assert_eq!(step.pilot, Some(PilotRequest::ErrorCycle { via_x1: true, ms: 4000 }));
        assert_eq!(step.notice, Some(Notice::LinkReady(false)));
        assert_eq!(b.m.state(), State::Idle, "unmatched, and not listening, until the pilot is back");
        assert!(step.set_key.is_some(), "V2G3-A09-121");
        // The pilot back in B starts the matching process anew.
        b.event(Event::Pilot(Reading { state: Cp::E, pwm: Pwm::Off }));
        b.event(Event::Pilot(B5));
        assert_eq!(b.armed(Timer::Init), Some(50_000));
        assert_eq!(b.m.state(), State::Unmatched { processes: 0 });
    }

    #[test]
    fn a_link_error_in_x1_goes_through_e_or_f_only() {
        // V2G3-M07-04.
        let mut b = Bench::new();
        b.event(Event::Pilot(X1));
        b.matched();
        let step = b.event(Event::LinkError);
        assert_eq!(step.pilot, Some(PilotRequest::ErrorCycle { via_x1: false, ms: 4000 }));
    }

    #[test]
    fn a_link_error_with_a_nominal_duty_cycle_follows_the_option_chosen() {
        // V2G3-M07-10 to 12.
        let mut a = Bench::new();
        a.event(Event::Pilot(B_NOMINAL));
        a.matched();
        assert_eq!(a.event(Event::LinkError).pilot, Some(PilotRequest::ErrorCycle { via_x1: true, ms: 4000 }), "option A");
        let mut b = Bench::with(Config { nominal_error: NominalError::KeepOscillator, ..Config::DEFAULT });
        b.event(Event::Pilot(B_NOMINAL));
        b.matched();
        let step = b.event(Event::LinkError);
        assert_eq!(step.pilot, None, "option B: the pilot stays");
        assert!(step.set_key.is_some());
        assert_eq!(b.m.state(), State::Unmatched { processes: 0 }, "ready for a new matching");
        assert!(b.parm(EV, [5; 8]).send.is_some());
    }

    #[test]
    fn a_pause_keeps_the_network_and_the_wake_up_gives_the_module_the_last_parameters() {
        // V2G3-M07-20, 22, 26.
        let mut b = linked();
        let key = nmk(1);
        let nid = homeplug::modem::network_id(&key);
        assert_eq!(b.event(Event::LinkPause).set_key, None);
        assert_eq!(b.m.state(), State::Matched { ev: EV, linked: true });
        let wake = b.event(Event::Wake);
        assert_eq!(wake.set_key, Some((nid, key)));
        assert_eq!(b.armed(Timer::Wake), Some(8000), "V2G3-M07-27");
        // V2G3-M07-21: the link is reported at once after a wake-up, and the wake timer ends.
        let up = b.event(Event::LinkUp);
        assert_eq!(up.notice, Some(Notice::LinkReady(true)));
        assert_eq!(b.armed(Timer::Wake), None);
        assert_eq!(b.armed(Timer::LinkReady), None);
    }

    #[test]
    fn no_link_after_the_wake_up_restarts_the_connection() {
        // V2G3-M07-27.
        let mut b = linked();
        b.event(Event::LinkPause);
        b.event(Event::Wake);
        let step = b.expire(Timer::Wake);
        assert_eq!(step.notice, Some(Notice::Failed { ev: EV, reason: Failure::NoLinkAfterWake }));
        assert_eq!(step.pilot, Some(PilotRequest::ErrorCycle { via_x1: true, ms: 4000 }));
    }

    #[test]
    fn an_ac_charger_restarts_the_connection_twice_and_then_goes_to_x1() {
        // V2G3-M06-07.
        let mut b = Bench::with(Config { ac: true, ..Config::DEFAULT });
        for restart in 0..2 {
            let step = b.expire(Timer::Init);
            assert_eq!(step.pilot, Some(PilotRequest::ErrorCycle { via_x1: false, ms: 4000 }), "restart {restart}");
            // The pilot goes through E and back: the window opens again.
            b.event(Event::Pilot(Reading { state: Cp::E, pwm: Pwm::Off }));
            b.event(Event::Pilot(B5));
            assert_eq!(b.armed(Timer::Init), Some(50_000));
        }
        assert_eq!(b.expire(Timer::Init).pilot, Some(PilotRequest::X1));
        // A new plug-in has its retries again.
        b.event(Event::Pilot(Reading { state: Cp::A, pwm: Pwm::Off }));
        b.event(Event::Pilot(B5));
        assert!(matches!(b.expire(Timer::Init).pilot, Some(PilotRequest::ErrorCycle { .. })));
    }

    #[test]
    fn a_dc_charger_does_not_restart_the_connection() {
        let mut b = Bench::new();
        assert_eq!(b.expire(Timer::Init).pilot, None);
    }
}

// --- against the vehicle ---

mod against_the_vehicle {
    use std::collections::VecDeque;

    use evcc_core::slac as ev;
    use evcc_core::{Timer as EvTimer, TimerCmd as EvTimerCmd};

    use super::*;

    /// A charger whose modem measures 5 dB, as the simulator's does.
    const CLOSE: Config = Config { attenuation: Some(5), ..Config::DEFAULT };

    struct Vehicle {
        mac: Mac,
        m: ev::Machine,
        timers: [Option<u64>; 3],
        outcome: Option<ev::Outcome>,
        toggles: Vec<(u64, ev::Toggle)>,
        link: Option<u64>,
    }

    struct World {
        now: u64,
        evse: Machine<Counting>,
        evse_timers: Vec<Option<u64>>,
        evse_toggle_window: Option<(u64, u64)>,
        vehicles: Vec<Vehicle>,
        notices: Vec<(u64, Notice)>,
        keys: Vec<([u8; 7], [u8; 16])>,
        /// Frames the EVSE sent: (time, destination, message).
        evse_sent: Vec<(u64, Mac, Message)>,
        queue: VecDeque<(Option<usize>, Vec<u8>)>,
        link_delay: u64,
    }

    /// Every timer of the machine, in the order of [`Timer::index`].
    fn all_timers() -> Vec<Timer> {
        let mut all = vec![Timer::Init, Timer::Join, Timer::LinkReady, Timer::Wake];
        for kind in 0..5 {
            for s in 0..PARALLEL as u8 {
                all.push([Timer::Sequence, Timer::Mnbc, Timer::Response, Timer::Session, Timer::Toggle][kind](s));
            }
        }
        assert!(all.iter().enumerate().all(|(i, t)| t.index() == i));
        all
    }

    fn ev_slot(t: EvTimer) -> usize {
        match t {
            EvTimer::Slac => 0,
            EvTimer::SlacResults => 1,
            EvTimer::SlacOverall => 2,
            other => panic!("{other:?}"),
        }
    }

    impl World {
        fn new(config: Config, vehicles: &[(Mac, ev::Config)]) -> Self {
            let mut w = World {
                now: 0,
                evse: Machine::new(config, EVSE, Counting(0)),
                evse_timers: vec![None; Timer::COUNT],
                evse_toggle_window: None,
                vehicles: vehicles
                    .iter()
                    .map(|(mac, c)| Vehicle { mac: *mac, m: ev::Machine::new(*c), timers: [None; 3], outcome: None, toggles: Vec::new(), link: None })
                    .collect(),
                notices: Vec::new(),
                keys: Vec::new(),
                evse_sent: Vec::new(),
                queue: VecDeque::new(),
                link_delay: 50,
            };
            w.evse_event(Event::Pilot(B5));
            for i in 0..w.vehicles.len() {
                let mac = w.vehicles[i].mac;
                w.vehicle_event(i, ev::Event::Start { mac, run_id: [i as u8 + 1, 0, 0, 0, 0, 0, 0, 0] });
            }
            w
        }

        fn evse_event(&mut self, e: Event<'_>) {
            let mut tx = [0u8; 256];
            let step = self.evse.handle(e, &mut tx);
            for (i, slot) in self.evse_timers.iter_mut().enumerate() {
                if step.timers.clear.applies(all_timers()[i]) {
                    *slot = None;
                }
            }
            for cmd in step.timers.iter() {
                match cmd {
                    TimerCmd::Arm { timer, ms } => self.evse_timers[timer.index()] = Some(self.now + u64::from(ms)),
                    TimerCmd::Cancel(timer) => self.evse_timers[timer.index()] = None,
                }
            }
            if let Some(n) = step.send {
                let f = homeplug::Frame::parse(&tx[..n]).unwrap();
                self.evse_sent.push((self.now, f.dst, f.message().unwrap()));
                self.queue.push_back((None, tx[..n].to_vec()));
            }
            if let Some(key) = step.set_key {
                self.keys.push(key);
                // The vehicle that joins brings the link up.
                for v in &mut self.vehicles {
                    if v.link.is_some() && v.link != Some(0) {
                        v.link = v.link.map(|l| l.max(self.now));
                    }
                }
            }
            if let Some(Toggles::Start { ms }) = step.toggles {
                self.evse_toggle_window = Some((self.now, self.now + u64::from(ms)));
            }
            if step.toggles == Some(Toggles::Read) {
                let (from, to) = self.evse_toggle_window.take().unwrap();
                let count: u32 = self.vehicles.iter().flat_map(|v| v.toggles.iter()).filter(|(t, _)| *t >= from && *t <= to).map(|(_, t)| u32::from(t.count)).sum();
                self.evse_event(Event::Toggles(Some(count as u8)));
            }
            if let Some(n) = step.notice {
                self.notices.push((self.now, n));
            }
        }

        fn vehicle_event(&mut self, i: usize, e: ev::Event<'_>) {
            let mut tx = [0u8; 128];
            let step = self.vehicles[i].m.handle(e, &mut tx);
            let now = self.now;
            for cmd in step.timers.iter() {
                match cmd {
                    EvTimerCmd::Arm { timer, ms } => self.vehicles[i].timers[ev_slot(timer)] = Some(now + u64::from(ms)),
                    EvTimerCmd::Cancel(timer) => self.vehicles[i].timers[ev_slot(timer)] = None,
                }
            }
            if let Some(n) = step.send {
                self.queue.push_back((Some(i), tx[..n].to_vec()));
            }
            if let Some(t) = step.toggle {
                self.vehicles[i].toggles.push((now, t));
            }
            if step.set_key.is_some() {
                self.vehicles[i].link = Some(now + self.link_delay);
            }
            if let Some(o) = step.outcome {
                self.vehicles[i].outcome = Some(o);
            }
        }

        /// Runs until no vehicle has anything left to do, or `limit_ms`.
        fn run(&mut self, limit_ms: u64) {
            loop {
                while let Some((from, bytes)) = self.queue.pop_front() {
                    match from {
                        Some(_) => self.evse_event(Event::Frame(&bytes)),
                        None => {
                            for i in 0..self.vehicles.len() {
                                self.vehicle_event(i, ev::Event::Frame(&bytes));
                            }
                        }
                    }
                }
                // The next thing that happens.
                let mut next: Option<(u64, usize, usize)> = None; // (time, kind, index)
                let mut consider = |t: u64, kind: usize, idx: usize| {
                    if next.is_none_or(|n| (t, kind, idx) < n) {
                        next = Some((t, kind, idx));
                    }
                };
                for (i, t) in self.evse_timers.iter().enumerate() {
                    if let Some(t) = t {
                        consider(*t, 0, i);
                    }
                }
                for (v, veh) in self.vehicles.iter().enumerate() {
                    for (s, t) in veh.timers.iter().enumerate() {
                        if let Some(t) = t {
                            consider(*t, 1, v * 3 + s);
                        }
                    }
                    if let Some(l) = veh.link {
                        consider(l, 2, v);
                    }
                }
                let Some((t, kind, idx)) = next else { return };
                if t > limit_ms {
                    return;
                }
                self.now = self.now.max(t);
                match kind {
                    0 => {
                        self.evse_timers[idx] = None;
                        self.evse_event(Event::TimerExpired(all_timers()[idx]));
                    }
                    1 => {
                        let (v, s) = (idx / 3, idx % 3);
                        self.vehicles[v].timers[s] = None;
                        let timer = [EvTimer::Slac, EvTimer::SlacResults, EvTimer::SlacOverall][s];
                        self.vehicle_event(v, ev::Event::TimerExpired(timer));
                    }
                    _ => {
                        self.vehicles[idx].link = Some(u64::MAX);
                        // Only the vehicle that was handed the key joins the network of the charger.
                        let joined = matches!(self.evse.state(), State::Matched { ev, .. } if ev == self.vehicles[idx].mac);
                        if joined {
                            self.evse_event(Event::LinkUp);
                            self.vehicle_event(idx, ev::Event::LinkUp);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_vehicle_matches_and_joins_and_the_higher_layers_are_told() {
        let mut w = World::new(CLOSE, &[(EV, ev::Config::DEFAULT)]);
        w.run(60_000);
        let key = nmk(1);
        assert_eq!(w.vehicles[0].outcome, Some(ev::Outcome::Matched { evse: EVSE, nid: homeplug::modem::network_id(&key), nmk: key }));
        assert_eq!(w.keys, vec![(homeplug::modem::network_id(&key), key)]);
        let ready = w.notices.iter().find(|(_, n)| *n == Notice::LinkReady(true)).expect("D-LINK_READY");
        assert!(ready.0 >= 200, "after the amplitude map window: {ready:?}");
        assert_eq!(w.evse.state(), State::Matched { ev: EV, linked: true });
        // Parameters, one report, one answer: the order of Figure A.1 on the charger's side.
        let kinds: Vec<u16> = w.evse_sent.iter().map(|(_, _, m)| m.mmtype()).collect();
        assert_eq!(kinds, vec![homeplug::mmtype::SLAC_PARM_CNF, homeplug::mmtype::ATTEN_CHAR_IND, homeplug::mmtype::SLAC_MATCH_CNF]);
    }

    #[test]
    fn a_vehicle_that_is_not_sure_validates_by_toggles_and_is_matched_when_the_numbers_agree() {
        let vehicle = ev::Config { validation: ev::Validation::Always, toggles: 2, ..ev::Config::DEFAULT };
        let mut w = World::new(CLOSE, &[(EV, vehicle)]);
        w.run(60_000);
        assert!(matches!(w.vehicles[0].outcome, Some(ev::Outcome::Matched { .. })), "{:?}", w.vehicles[0].outcome);
        let counted: Vec<u8> = w
            .evse_sent
            .iter()
            .filter_map(|(_, _, m)| match m {
                Message::ValidateCnf { toggle_num, result } if *result == validate_result::SUCCESS => Some(*toggle_num),
                _ => None,
            })
            .collect();
        assert_eq!(counted, vec![2], "V2G3-A09-73");
    }

    #[test]
    fn a_charger_that_cannot_count_leaves_the_decision_to_a_vehicle_that_is_sure() {
        // V2G3-A09-51: the attenuation is below the direct threshold, so "failure" does not stop the match.
        let vehicle = ev::Config { validation: ev::Validation::Always, ..ev::Config::DEFAULT };
        let mut w = World::new(Config { validation: Validation::Failure, ..CLOSE }, &[(EV, vehicle)]);
        w.run(60_000);
        assert!(matches!(w.vehicles[0].outcome, Some(ev::Outcome::Matched { .. })));
    }

    #[test]
    fn several_vehicles_at_once_one_gets_the_key() {
        let macs = [EV, EV2, [2, 0, 0, 0, 0, 3]];
        let mut w = World::new(CLOSE, &macs.map(|m| (m, ev::Config::DEFAULT)));
        w.run(60_000);
        let State::Matched { ev: winner, .. } = w.evse.state() else { panic!("{:?}", w.evse.state()) };
        let matched: Vec<Mac> = w.vehicles.iter().filter(|v| matches!(v.outcome, Some(ev::Outcome::Matched { .. }))).map(|v| v.mac).collect();
        assert_eq!(matched, vec![winner]);
        let cnfs: Vec<Mac> = w.evse_sent.iter().filter(|(_, _, m)| matches!(m, Message::MatchCnf { .. })).map(|(_, d, _)| *d).collect();
        assert!(cnfs.iter().all(|d| *d == winner), "only the winner is given the key: {cnfs:?}");
        assert_eq!(w.keys.len(), 1);
    }
}
