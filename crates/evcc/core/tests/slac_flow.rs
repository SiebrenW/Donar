//! The EV side of SLAC against the simulated EVSE, in virtual time.

use std::collections::VecDeque;

use evcc_core::slac::{Config, Event, Failure, Machine, Outcome, Toggle, Validation};
use evcc_core::{Timer, TimerCmd};
use evse_sim::slac::{Config as EvseConfig, Evse};
use homeplug::{Frame, GROUPS, Mac, Message, mmtype};

const EV: Mac = [0x02, 0, 0, 0, 0, 0x01];
const RUN: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 0];

struct Result {
    outcome: Outcome,
    elapsed_ms: u64,
    /// What the EV sent, with the time.
    sent: Vec<(u64, Message)>,
    set_key: Vec<([u8; 7], [u8; 16])>,
    /// The control pilot toggles the EV asked for, with the time.
    toggles: Vec<(u64, Toggle)>,
    /// Where the validation requests went.
    validate_to: Vec<Mac>,
}

/// What the control pilot shows the charger: the toggles the EV made, plus `offset`.
#[derive(Clone, Copy)]
struct Pilot {
    offset: i16,
}

impl Pilot {
    const HONEST: Pilot = Pilot { offset: 0 };
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
    drive_with(config, evses, link_up_after_ms, noise, Pilot::HONEST)
}

fn drive_with(config: Config, evses: &mut [Evse], link_up_after_ms: Option<u64>, noise: &[Vec<u8>], pilot: Pilot) -> Result {
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
    let mut toggles: Vec<(u64, Toggle)> = Vec::new();
    let mut validate_to = Vec::new();
    // Chargers counting toggles: (time the window ends, charger index).
    let mut counting: Vec<(u64, usize)> = Vec::new();
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
            let parsed = Frame::parse(&frame).unwrap();
            if matches!(parsed.message(), Some(Message::ValidateReq { .. })) {
                validate_to.push(parsed.dst);
            }
            sent.push((now, parsed.message().unwrap()));
            for (i, evse) in evses.iter_mut().enumerate() {
                queue.extend(evse.handle(&frame));
                if let Some(ms) = evse.validation_window_ms()
                    && !counting.iter().any(|&(_, j)| j == i)
                {
                    counting.push((now + u64::from(ms), i));
                }
            }
        }
        if let Some(t) = step.toggle {
            toggles.push((now, t));
        }
        if let Some(key) = step.set_key {
            set_key.push(key);
            link = link_up_after_ms.map(|d| now + d);
        }
        if let Some(outcome) = step.outcome {
            return Result { outcome, elapsed_ms: now, sent, set_key, toggles, validate_to };
        }
        if let Some(f) = queue.pop_front() {
            frame_storage = f;
            event = Event::Frame(&frame_storage);
            continue;
        }
        // Nothing queued: advance to the earliest timer or the link coming up.
        let next_timer = timers.iter().enumerate().filter_map(|(i, t)| t.map(|t| (t, i))).min();
        // A charger's listening window ends before anything later happens.
        if let Some(pos) = counting.iter().enumerate().min_by_key(|(_, (t, _))| *t).map(|(p, _)| p) {
            let (t, i) = counting[pos];
            if next_timer.is_none_or(|(timer, _)| t <= timer) && link.is_none_or(|l| t <= l) {
                counting.remove(pos);
                now = now.max(t);
                let made = toggles.last().map_or(0, |(_, t)| i16::from(t.count));
                queue.extend(evses[i].finish_validation((made + pilot.offset).clamp(0, 255) as u8));
                if let Some(f) = queue.pop_front() {
                    frame_storage = f;
                    event = Event::Frame(&frame_storage);
                    continue;
                }
            }
        }
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

/// The wiring between the plug and the modem costs attenuation that is no part of the path to the charger: a correction on
/// either side (the vehicle's, on what it receives; the charger's, on what it reports) brings a direct charger back under the threshold.
#[test]
fn a_correction_for_the_wiring_makes_a_charger_direct() {
    let retry = Config { rate_ms: 100, repetition_ms: 1500, ..Config::DEFAULT };
    // 15 dB measured is no direct charger (the threshold is 10) ...
    let r = drive(retry, &mut [evse_at(0, 15)], Some(10), &[]);
    assert_eq!(r.outcome, Outcome::Failed(Failure::NoDirectEvse));
    // ... but 6 dB of it are the vehicle's own wiring,
    let r = drive(Config { attenuation_correction_db: [-6; GROUPS], ..retry }, &mut [evse_at(0, 15)], Some(10), &[]);
    assert!(matches!(r.outcome, Outcome::Matched { .. }), "{:?}", r.outcome);
    // or the charger's.
    let mut corrected = [Evse::new(EvseConfig { mac: [0x02, 0, 0, 0, 0, 0x10], attenuation: 15, attenuation_correction_db: vec![-6], ..Default::default() })];
    let r = drive(retry, &mut corrected, Some(10), &[]);
    assert!(matches!(r.outcome, Outcome::Matched { .. }), "{:?}", r.outcome);
    // The correction is per group: 6 dB off half of the groups is 3 dB off the average, not enough; off all of them it is.
    let half: [i8; GROUPS] = std::array::from_fn(|g| if g < GROUPS / 2 { -6 } else { 0 });
    let r = drive(Config { attenuation_correction_db: half, ..retry }, &mut [evse_at(0, 15)], Some(10), &[]);
    assert_eq!(r.outcome, Outcome::Failed(Failure::NoDirectEvse));
    // A correction does not wrap around below zero.
    let r = drive(Config { attenuation_correction_db: [-100; GROUPS], ..retry }, &mut [evse_at(0, 15)], Some(10), &[]);
    assert!(matches!(r.outcome, Outcome::Matched { .. }), "{:?}", r.outcome);
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

// ------------------------------------------------------- validation by control pilot toggles

fn validating(validation: Validation) -> Config {
    Config { validation, rate_ms: 100, repetition_ms: 3000, ..Config::DEFAULT }
}

fn validate_reqs(r: &Result) -> Vec<(u64, u8)> {
    r.sent.iter().filter_map(|(t, m)| if let Message::ValidateReq { timer, .. } = m { Some((*t, *timer)) } else { None }).collect()
}

fn evse_with(n: u8, attenuation: u8, validation: evse_sim::slac::Validation) -> Evse {
    Evse::new(EvseConfig { mac: [0x02, 0, 0, 0, 0, 0x10 + n], attenuation, validation, ..Default::default() })
}

#[test]
fn an_ambiguous_charger_is_validated_by_toggling_the_control_pilot_and_then_matched() {
    let mut evses = [evse_at(0, 15)];
    let r = drive(validating(Validation::Ambiguous), &mut evses, Some(10), &[]);
    assert!(matches!(r.outcome, Outcome::Matched { .. }), "{:?}", r.outcome);
    // Step 1 (timer 0, unicast), step 2 (the listening time), then the match request.
    let reqs = validate_reqs(&r);
    assert_eq!(reqs.len(), 2, "{reqs:?}");
    assert_eq!(reqs[0].1, 0);
    let [(at, toggle)] = r.toggles[..] else { panic!("{:?}", r.toggles) };
    assert!((1..=3).contains(&toggle.count) && toggle.state_ms == 300);
    // The listening time covers the 2n+1 states of the sequence and is at most 3.5 s.
    // (the field is the time in units of 100 ms less one: 0x00 is 100 ms).
    assert!((u32::from(reqs[1].1) + 1) * 100 >= (2 * u32::from(toggle.count) + 1) * 300);
    assert!(reqs[1].1 <= 34);
    assert_eq!(at, reqs[1].0);
    // The match request follows the answer, which came when the listening time was over.
    let match_at = r.sent.iter().find(|(_, m)| m.mmtype() == mmtype::SLAC_MATCH_REQ).unwrap().0;
    assert_eq!(match_at, reqs[1].0 + (u64::from(reqs[1].1) + 1) * 100);
}

#[test]
fn both_validation_requests_go_to_the_charger_address() {
    // Table A.6 says broadcast for the second one; a charger may refuse a broadcast one, and one that does not look at the
    // address takes either.
    let mut evses = [evse_at(0, 15)];
    let r = drive(validating(Validation::Ambiguous), &mut evses, Some(10), &[]);
    assert_eq!(r.validate_to, vec![evses[0].mac(); 2]);
}

#[test]
fn a_charger_that_is_surely_the_one_is_not_validated_unless_asked_to() {
    for (validation, expected) in [(Validation::Never, 0), (Validation::Ambiguous, 0), (Validation::Always, 2)] {
        let mut evses = [evse_at(0, 5)];
        let r = drive(validating(validation), &mut evses, Some(10), &[]);
        assert!(matches!(r.outcome, Outcome::Matched { .. }), "{validation:?}: {:?}", r.outcome);
        assert_eq!(validate_reqs(&r).len(), expected, "{validation:?}");
    }
}

#[test]
fn the_toggles_and_their_timing_follow_the_configuration() {
    let mut evses = [evse_at(0, 15)];
    let config = Config { toggles: 2, toggle_state_ms: 200, ..validating(Validation::Ambiguous) };
    let r = drive(config, &mut evses, Some(10), &[]);
    assert_eq!(r.toggles.iter().map(|(_, t)| (t.count, t.state_ms)).collect::<Vec<_>>(), [(2, 200)]);
    // Five states of 200 ms, in units of 100 ms, and one more, less one for the encoding (0x00 is 100 ms).
    assert_eq!(validate_reqs(&r)[1].1, 10);
}

#[test]
fn a_charger_that_counts_another_number_is_not_matched() {
    for offset in [-1, 1] {
        let mut evses = [evse_at(0, 15)];
        let r = drive_with(validating(Validation::Ambiguous), &mut evses, Some(10), &[], Pilot { offset });
        assert_eq!(r.outcome, Outcome::Failed(Failure::ValidationFailed), "offset {offset}");
        assert_eq!(kinds(&r.sent, mmtype::SLAC_MATCH_REQ), 0);
    }
}

#[test]
fn the_next_candidate_is_validated_when_the_first_does_not_support_it() {
    use evse_sim::slac::Validation as Evs;
    // The closer one cannot validate and is only ambiguous; the other can and gets matched.
    let mut evses = [evse_with(0, 12, Evs::Failure), evse_with(1, 18, Evs::Ready)];
    let r = drive(validating(Validation::Ambiguous), &mut evses, Some(10), &[]);
    assert_eq!(r.outcome, Outcome::Matched { evse: evses[1].mac(), nid: evse_sim::slac::nid(&[0x5A; 16]), nmk: [0x5A; 16] });
    assert_eq!(r.toggles.len(), 1);
    // Both were asked in step 1, only the second in step 2.
    assert_eq!(validate_reqs(&r).iter().filter(|(_, t)| *t == 0).count(), 2);
}

#[test]
fn a_charger_that_cannot_validate_is_used_if_it_is_surely_the_one() {
    use evse_sim::slac::Validation as Evs;
    // The attenuation is below the direct threshold: "failure" (no such feature) is no reason
    // to refuse it (V2G3-A09-51, first option).
    let mut evses = [evse_with(0, 5, Evs::Failure)];
    let r = drive(validating(Validation::Always), &mut evses, Some(10), &[]);
    assert!(matches!(r.outcome, Outcome::Matched { .. }), "{:?}", r.outcome);
    assert!(r.toggles.is_empty());
    // An ambiguous one is not, unless the vehicle accepts those anyway.
    let mut evses = [evse_with(0, 15, Evs::Failure)];
    let r = drive(validating(Validation::Ambiguous), &mut evses, Some(10), &[]);
    assert_eq!(r.outcome, Outcome::Failed(Failure::ValidationFailed));
    let mut evses = [evse_with(0, 15, Evs::Failure)];
    let r = drive(Config { accept_potential: true, ..validating(Validation::Ambiguous) }, &mut evses, Some(10), &[]);
    assert!(matches!(r.outcome, Outcome::Matched { .. }), "{:?}", r.outcome);
}

#[test]
fn a_busy_charger_is_asked_again_and_a_silent_one_times_out() {
    use evse_sim::slac::Validation as Evs;
    let mut evses = [evse_with(0, 15, Evs::NotReady(1))];
    let r = drive(validating(Validation::Ambiguous), &mut evses, Some(10), &[]);
    assert!(matches!(r.outcome, Outcome::Matched { .. }), "{:?}", r.outcome);
    assert_eq!(validate_reqs(&r).iter().filter(|(_, t)| *t == 0).count(), 2);

    let mut evses = [evse_with(0, 15, Evs::Silent)];
    let r = drive(validating(Validation::Ambiguous), &mut evses, Some(10), &[]);
    assert_eq!(r.outcome, Outcome::Failed(Failure::ValidationFailed));
    // The first request and the two retries, 200 ms apart (TT_match_response, C_EV_match_retry).
    let firsts: Vec<u64> = validate_reqs(&r).iter().filter(|(_, t)| *t == 0).map(|(at, _)| *at).collect();
    let first = firsts[0];
    assert_eq!(firsts[..3], [first, first + 200, first + 400]);
}

#[test]
fn not_required_is_followed_for_a_sure_charger_and_overruled_for_an_ambiguous_one() {
    use evse_sim::slac::Validation as Evs;
    let mut evses = [evse_with(0, 5, Evs::NotRequired)];
    let r = drive(validating(Validation::Always), &mut evses, Some(10), &[]);
    assert!(matches!(r.outcome, Outcome::Matched { .. }));
    assert!(r.toggles.is_empty());
    let mut evses = [evse_with(0, 15, Evs::NotRequired)];
    let r = drive(validating(Validation::Ambiguous), &mut evses, Some(10), &[]);
    assert!(matches!(r.outcome, Outcome::Matched { .. }));
    assert_eq!(r.toggles.len(), 1);
}

#[test]
fn without_validation_an_ambiguous_charger_is_still_refused_or_accepted_by_policy() {
    let mut evses = [evse_at(0, 15)];
    let r = drive(Config { rate_ms: 100, repetition_ms: 1500, ..Config::DEFAULT }, &mut evses, Some(10), &[]);
    assert_eq!(r.outcome, Outcome::Failed(Failure::NoDirectEvse));
    assert!(validate_reqs(&r).is_empty());
}
