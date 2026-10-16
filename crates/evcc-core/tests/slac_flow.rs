//! The EV side of SLAC against the simulated EVSE, in virtual time.

use std::collections::VecDeque;

use evcc_core::slac::{Config, Event, Failure, Machine, Outcome};
use evcc_core::{Timer, TimerCmd};
use evse_sim::slac::{Config as EvseConfig, Evse};
use homeplug::{Frame, Mac, Message, mmtype};

const EV: Mac = [0x02, 0, 0, 0, 0, 0x01];
const RUN: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 0];

struct Result {
    outcome: Outcome,
    elapsed_ms: u64,
    /// What the EV sent, with the time.
    sent: Vec<(u64, Message)>,
    set_key: Vec<([u8; 7], [u8; 16])>,
}

fn evse_at(n: u8, attenuation: u8) -> Evse {
    Evse::new(EvseConfig { mac: [0x02, 0, 0, 0, 0, 0x10 + n], attenuation, ..Default::default() })
}

fn kinds(sent: &[(u64, Message)], t: u16) -> usize {
    sent.iter().filter(|(_, m)| m.mmtype() == t).count()
}

/// Runs the machine until it has an outcome. `link_up_after_ms`: when the link comes up after
/// the key is set (None: never). `noise` frames are delivered to the EV first.
fn drive(config: Config, evses: &mut [Evse], link_up_after_ms: Option<u64>, noise: &[Vec<u8>]) -> Result {
    let mut m = Machine::new(config);
    let mut tx = [0u8; 128];
    let mut now = 0u64;
    let mut timers: [Option<u64>; 3] = [None; 3];
    let slot = |t: Timer| match t {
        Timer::Slac => 0,
        Timer::SlacResults => 1,
        Timer::SlacOverall => 2,
        other => panic!("{other:?}"),
    };
    let mut queue: VecDeque<Vec<u8>> = noise.iter().cloned().collect();
    let mut link: Option<u64> = None;
    let mut sent = Vec::new();
    let mut set_key = Vec::new();
    let mut event = Event::Start { mac: EV, run_id: RUN };
    let mut frame_storage;
    loop {
        let step = m.handle(event, &mut tx);
        for cmd in step.timers.iter() {
            match cmd {
                TimerCmd::Arm { timer, ms } => timers[slot(timer)] = Some(now + u64::from(ms)),
                TimerCmd::Cancel(timer) => timers[slot(timer)] = None,
            }
        }
        if let Some(n) = step.send {
            let frame = tx[..n].to_vec();
            sent.push((now, Frame::parse(&frame).unwrap().message().unwrap()));
            for evse in evses.iter_mut() {
                queue.extend(evse.handle(&frame));
            }
        }
        if let Some(key) = step.set_key {
            set_key.push(key);
            link = link_up_after_ms.map(|d| now + d);
        }
        if let Some(outcome) = step.outcome {
            return Result { outcome, elapsed_ms: now, sent, set_key };
        }
        if let Some(f) = queue.pop_front() {
            frame_storage = f;
            event = Event::Frame(&frame_storage);
            continue;
        }
        // Nothing queued: advance to the earliest timer or the link coming up.
        let next_timer = timers.iter().enumerate().filter_map(|(i, t)| t.map(|t| (t, i))).min();
        match (next_timer, link) {
            (Some((t, _)), Some(l)) if l <= t => {
                now = now.max(l);
                link = None;
                event = Event::LinkUp;
            }
            (None, Some(l)) => {
                now = now.max(l);
                link = None;
                event = Event::LinkUp;
            }
            (Some((t, i)), _) => {
                now = now.max(t);
                timers[i] = None;
                event = Event::TimerExpired([Timer::Slac, Timer::SlacResults, Timer::SlacOverall][i]);
            }
            (None, None) => panic!("machine is stuck at {now} ms without an outcome"),
        }
    }
}

#[test]
fn a_directly_connected_charger_is_matched() {
    let mut evses = [evse_at(0, 5)];
    let r = drive(Config::DEFAULT, &mut evses, Some(300), &[]);
    let nmk = [0x5A; 16];
    assert_eq!(r.outcome, Outcome::Matched { evse: evses[0].mac(), nid: evse_sim::slac::nid(&nmk), nmk });
    assert_eq!(r.set_key, vec![(evse_sim::slac::nid(&nmk), nmk)]);
    // The sequence of Annex A: one parameter request, three start messages, ten sounds, one
    // response to the report, one match request.
    assert_eq!(kinds(&r.sent, mmtype::SLAC_PARM_REQ), 1);
    assert_eq!(kinds(&r.sent, mmtype::START_ATTEN_CHAR_IND), 3);
    assert_eq!(kinds(&r.sent, mmtype::MNBC_SOUND_IND), 10);
    assert_eq!(kinds(&r.sent, mmtype::ATTEN_CHAR_RSP), 1);
    assert_eq!(kinds(&r.sent, mmtype::SLAC_MATCH_REQ), 1);
    // Sounds count down from 9 to 0.
    let cnts: Vec<u8> = r.sent.iter().filter_map(|(_, m)| if let Message::MnbcSound { cnt, .. } = m { Some(*cnt) } else { None }).collect();
    assert_eq!(cnts, (0..10).rev().collect::<Vec<u8>>());
    // Batch messages are 20 ms apart (TP_EV_batch_msg_interval) and the link time is the rest.
    let starts: Vec<u64> = r.sent.iter().filter(|(_, m)| m.mmtype() == mmtype::START_ATTEN_CHAR_IND).map(|(t, _)| *t).collect();
    assert_eq!(starts, vec![0, 20, 40]);
    let sounds: Vec<u64> = r.sent.iter().filter(|(_, m)| m.mmtype() == mmtype::MNBC_SOUND_IND).map(|(t, _)| *t).collect();
    assert_eq!(sounds.first(), Some(&60));
    assert!(sounds.windows(2).all(|w| w[1] - w[0] == 20), "{sounds:?}");
    assert_eq!(r.elapsed_ms, 240 + 300);
}

#[test]
fn an_unanswered_parameter_request_is_repeated() {
    let mut evses = [Evse::new(EvseConfig { ignore_parm: 2, ..Default::default() })];
    let r = drive(Config::DEFAULT, &mut evses, Some(10), &[]);
    assert!(matches!(r.outcome, Outcome::Matched { .. }), "{:?}", r.outcome);
    let parms: Vec<u64> = r.sent.iter().filter(|(_, m)| m.mmtype() == mmtype::SLAC_PARM_REQ).map(|(t, _)| *t).collect();
    assert_eq!(parms, vec![0, 200, 400]);
}

#[test]
fn without_any_charger_the_run_is_repeated_until_the_time_is_up() {
    let r = drive(Config::DEFAULT, &mut [], None, &[]);
    assert_eq!(r.outcome, Outcome::Failed(Failure::NoResponse));
    // Each run is three requests 200 ms apart, runs are 400 ms apart, for 10 s in all.
    assert!(r.elapsed_ms >= 10_000 && r.elapsed_ms < 11_100, "{}", r.elapsed_ms);
    let runs: Vec<[u8; 8]> = r.sent.iter().filter_map(|(_, m)| if let Message::ParmReq { run_id } = m { Some(*run_id) } else { None }).collect();
    assert!(runs.len() > 3 * 10, "{}", runs.len());
    // Retransmissions keep the run id; a new run gets a new one.
    assert_eq!(runs[0], runs[1]);
    assert_ne!(runs[0], runs[3]);
}

#[test]
fn a_charger_that_is_not_directly_connected_is_not_used() {
    let mut evses = [evse_at(0, 15)];
    let r = drive(Config { rate_ms: 100, repetition_ms: 1500, ..Config::DEFAULT }, &mut evses, Some(10), &[]);
    assert_eq!(r.outcome, Outcome::Failed(Failure::NoDirectEvse));
    assert_eq!(kinds(&r.sent, mmtype::SLAC_MATCH_REQ), 0);
}

#[test]
fn a_charger_between_the_thresholds_can_be_accepted_by_policy() {
    let mut evses = [evse_at(0, 15)];
    let r = drive(Config { accept_potential: true, ..Config::DEFAULT }, &mut evses, Some(10), &[]);
    assert!(matches!(r.outcome, Outcome::Matched { .. }), "{:?}", r.outcome);
    let mut far = [evse_at(0, 25)];
    let r = drive(Config { accept_potential: true, repetition_ms: 1500, ..Config::DEFAULT }, &mut far, Some(10), &[]);
    assert_eq!(r.outcome, Outcome::Failed(Failure::NoDirectEvse));
}

#[test]
fn of_several_chargers_the_one_with_the_least_attenuation_is_chosen() {
    let mut evses = [evse_at(0, 9), evse_at(1, 3), evse_at(2, 30)];
    let r = drive(Config::DEFAULT, &mut evses, Some(10), &[]);
    let Outcome::Matched { evse, .. } = r.outcome else { panic!("{:?}", r.outcome) };
    assert_eq!(evse, evses[1].mac());
    // Every report is answered.
    assert_eq!(kinds(&r.sent, mmtype::ATTEN_CHAR_RSP), 3);
    // The match request went to that charger only.
    let Some((_, Message::MatchReq { evse: target, .. })) = r.sent.iter().find(|(_, m)| m.mmtype() == mmtype::SLAC_MATCH_REQ) else { panic!() };
    assert_eq!(*target, evses[1].mac());
}

#[test]
fn a_charger_that_cannot_measure_ends_the_run_when_the_window_closes() {
    let mut evses = [Evse::new(EvseConfig { no_report: true, ..Default::default() })];
    let r = drive(Config { repetition_ms: 1000, ..Config::DEFAULT }, &mut evses, Some(10), &[]);
    assert_eq!(r.outcome, Outcome::Failed(Failure::NoAttenuation));
    assert!(r.elapsed_ms >= 1200, "{}", r.elapsed_ms);
}

#[test]
fn a_lost_match_confirmation_is_requested_again() {
    let mut evses = [Evse::new(EvseConfig { ignore_match: 2, ..Default::default() })];
    let r = drive(Config::DEFAULT, &mut evses, Some(10), &[]);
    assert!(matches!(r.outcome, Outcome::Matched { .. }), "{:?}", r.outcome);
    assert_eq!(kinds(&r.sent, mmtype::SLAC_MATCH_REQ), 3);
}

#[test]
fn a_charger_that_never_confirms_the_match_fails_the_run() {
    let mut evses = [Evse::new(EvseConfig { ignore_match: 100, ..Default::default() })];
    let r = drive(Config { repetition_ms: 500, ..Config::DEFAULT }, &mut evses, Some(10), &[]);
    assert_eq!(r.outcome, Outcome::Failed(Failure::NoMatch));
}

#[test]
fn a_link_that_does_not_come_up_fails_after_the_join_time() {
    let mut evses = [evse_at(0, 5)];
    let r = drive(Config { repetition_ms: 500, ..Config::DEFAULT }, &mut evses, None, &[]);
    assert_eq!(r.outcome, Outcome::Failed(Failure::JoinTimeout));
    assert!(r.elapsed_ms >= 12_000, "{}", r.elapsed_ms);
}

#[test]
fn frames_for_others_and_garbage_are_ignored() {
    let mut noise = Vec::new();
    // Another EV's confirmation, a frame with the wrong run id and some junk.
    let other_ev: Mac = [0x02, 0, 0, 0, 0, 0x77];
    let mut buf = [0u8; 128];
    let n = Message::ParmCnf { num_sounds: 10, time_out: 6, forwarding_sta: other_ev, run_id: RUN }.write(other_ev, [0x02, 0, 0, 0, 0, 0x55], &mut buf).unwrap();
    noise.push(buf[..n].to_vec());
    let n = Message::ParmCnf { num_sounds: 10, time_out: 6, forwarding_sta: EV, run_id: [9; 8] }.write(EV, [0x02, 0, 0, 0, 0, 0x55], &mut buf).unwrap();
    noise.push(buf[..n].to_vec());
    noise.push(vec![0xAB; 5]);
    noise.push(vec![0; 60]);
    let mut evses = [evse_at(0, 5)];
    let r = drive(Config::DEFAULT, &mut evses, Some(10), &noise);
    let Outcome::Matched { evse, .. } = r.outcome else { panic!("{:?}", r.outcome) };
    assert_eq!(evse, evses[0].mac());
}
