//! Drives the DIN SPEC 70121 session machine against the simulated charger in memory: no sockets
//! and no clock, the timers a step asks for are ignored.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use evcc_core::din::{Config, Session};
use evcc_core::ev::{DcConnector, DcLimits, Ev, ScheduleRequest};
use evcc_core::log::{LogSink, NoLog, Record};
use evcc_core::session::{Buffers, ControlPilot, EndReason, Event, Notice, State};
use evcc_core::{Timer, TimerCmd};
use evse_sim::{Config as EvseConfig, Evse};
use iso15118_20::common_types::RationalNumber;

fn rn(exponent: i8, value: i16) -> RationalNumber {
    RationalNumber { exponent, value }
}

struct TestEv {
    connector: DcConnector,
    reads: Cell<u32>,
}

impl TestEv {
    fn new(connector: DcConnector) -> Self {
        Self { connector, reads: Cell::new(0) }
    }
}

impl Ev for TestEv {
    fn evcc_id(&self) -> &str {
        "WMIV1234567890ABCDEX"
    }
    fn evcc_mac(&self) -> [u8; 6] {
        [0x00, 0x24, 0x12, 0xE2, 0xB4, 0x90]
    }
    fn now_micros(&self) -> u64 {
        0
    }
    fn dc_connector(&self) -> DcConnector {
        self.connector
    }
    fn dc_limits(&self) -> DcLimits {
        DcLimits {
            maximum_charge_power: rn(3, 100),
            minimum_charge_power: rn(0, 100),
            maximum_charge_current: rn(0, 200),
            minimum_charge_current: rn(0, 1),
            maximum_voltage: rn(0, 450),
            minimum_voltage: rn(0, 200),
            target_soc: Some(80),
        }
    }
    fn battery_voltage(&self) -> RationalNumber {
        rn(0, 400)
    }
    /// The first reading is below the battery voltage, the later ones match it.
    fn inlet_voltage(&self) -> RationalNumber {
        let n = self.reads.get();
        self.reads.set(n + 1);
        rn(0, if n == 0 { 300 } else { 400 })
    }
    fn schedule_request(&self) -> ScheduleRequest {
        ScheduleRequest { departure_time: 3600, minimum_soc: None, target_soc: Some(80), target_energy: rn(3, 40), maximum_energy: rn(3, 60), minimum_energy: rn(0, 0) }
    }
    fn present_soc(&self) -> Option<u8> {
        Some(30)
    }
}

/// What the vehicle sent, by name, in order.
type Sent = Rc<RefCell<Vec<String>>>;

struct Log(Sent);

impl LogSink for Log {
    fn record(&mut self, r: &Record<'_>) {
        if let Record::MsgTx { msg } = r {
            self.0.borrow_mut().push(format!("{msg:?}"));
        }
    }
}

struct Run {
    end: EndReason,
    states: Vec<State>,
    notices: Vec<Notice>,
    /// The charger's own log.
    log: Vec<String>,
    /// The messages the vehicle sent.
    sent: Vec<String>,
}

enum Input {
    Event(Event),
    Bytes(Vec<u8>),
}

/// A session between the vehicle and the charger; after `stop_after` charge loop answers the
/// vehicle asks to stop. Returns once the machine ended.
fn run(connector: DcConnector, evse: EvseConfig, config: Config, stop_after: Option<u32>) -> Run {
    run_host(connector, evse, config, stop_after, Host::default())
}

/// What the host does with the control pilot: report what it is told to set (`report`), and report a reading of its own
/// when the n-th charge loop answer arrives (`inject`).
#[derive(Default)]
struct Host {
    report: bool,
    inject: Option<(u32, control_pilot::Reading)>,
}

fn run_host(connector: DcConnector, evse: EvseConfig, config: Config, stop_after: Option<u32>, host: Host) -> Run {
    let (mut rx, mut tx, mut arena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let sent: Sent = Rc::default();
    let mut session = Session::new(config, TestEv::new(connector), Log(sent.clone()), Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    let mut evse = Evse::new(evse);
    let (mut states, mut notices) = (Vec::new(), Vec::new());
    let mut loops = 0;
    let mut queue = std::collections::VecDeque::from([Input::Event(Event::Start { secured: false })]);
    let mut guard = 0;
    while let Some(input) = queue.pop_front() {
        guard += 1;
        assert!(guard < 500, "the session does not end: {states:?}");
        let step = match input {
            Input::Event(e) => session.handle(e),
            Input::Bytes(bytes) => {
                session.rx_space()[..bytes.len()].copy_from_slice(&bytes);
                session.handle(Event::Received(bytes.len()))
            }
        };
        states.push(session.state());
        if let Some(reason) = step.end {
            let sent = sent.borrow().clone();
            return Run { end: reason, states, notices, log: evse.take_log(), sent };
        }
        if let Some(n) = step.send {
            let frame = session.tx()[..n].to_vec();
            for out in evse.handle(&frame) {
                if let Some(bytes) = out.send {
                    queue.push_back(Input::Bytes(bytes));
                }
            }
        }
        if let Some(notice) = step.notice {
            notices.push(notice);
            let reply = match notice {
                Notice::CloseContactors => Some(Event::ContactorsClosed),
                Notice::StopEnergyTransfer { .. } => Some(Event::EnergyTransferStopped),
                Notice::OpenContactors => Some(Event::ContactorsOpened),
                Notice::ControlPilot(p) => {
                    if host.report {
                        // The host sets the pilot and measures it before it says so.
                        let state = match p {
                            ControlPilot::B => control_pilot::State::B,
                            ControlPilot::C => control_pilot::State::C,
                        };
                        queue.push_front(Input::Event(Event::ControlPilotChanged));
                        queue.push_front(Input::Event(Event::Pilot(control_pilot::Reading { state, pwm: control_pilot::Pwm::Duty(5) })));
                        continue;
                    }
                    Some(Event::ControlPilotChanged)
                }
                Notice::ChargeLoop(_) => {
                    loops += 1;
                    if let Some((n, reading)) = host.inject.filter(|(n, _)| *n == loops) {
                        let _ = n;
                        queue.push_front(Input::Event(Event::Pilot(reading)));
                        continue;
                    }
                    (Some(loops) == stop_after).then_some(Event::StopRequested)
                }
                _ => None,
            };
            if let Some(e) = reply {
                queue.push_front(Input::Event(e));
            }
        }
        // A charger that keeps its voltage up: the bound on the repeated requests runs out.
        if sent.borrow().iter().filter(|m| m.contains("WeldingDetectionReq")).count() >= 4 {
            queue.push_front(Input::Event(Event::TimerExpired(Timer::Ongoing)));
        }
        // A pause before the next request: it is over at once.
        if step.timers.iter().any(|c| matches!(c, TimerCmd::Arm { timer: Timer::Poll, .. })) {
            queue.push_back(Input::Event(Event::TimerExpired(Timer::Poll)));
        }
    }
    panic!("nothing left to do: {states:?}\n{:?}", evse.take_log());
}

const NORMAL: [&str; 20] = [
    "SupportedAppProtocolReq",
    "SessionSetupReq",
    "ServiceDiscoveryReq",
    "ServicePaymentSelectionReq",
    "ContractAuthenticationReq",
    "ChargeParameterDiscoveryReq",
    "CableCheckReq",
    "PreChargeReq",
    "PreChargeReq",
    "PowerDeliveryReq",
    "CurrentDemandReq",
    "CurrentDemandReq",
    "CurrentDemandReq",
    "PowerDeliveryReq",
    "WeldingDetectionReq",
    "SessionStopReq",
    "", "", "", "",
];

fn din() -> Config {
    Config::DIN
}

#[test]
fn a_session_runs_to_its_end() {
    let r = run(DcConnector::Extended, EvseConfig::default(), din(), Some(3));
    assert_eq!(r.end, EndReason::Completed, "{:?}\n{:?}\n{:?}", r.states, r.log, r.notices);
    assert_eq!(r.sent, NORMAL[..16], "{:?}", r.log);
    // The vehicle changes the control pilot to C before the cable check and to B after the energy transfer.
    let pilots: Vec<_> = r.notices.iter().filter_map(|n| if let Notice::ControlPilot(p) = n { Some(*p) } else { None }).collect();
    assert_eq!(pilots, [ControlPilot::C, ControlPilot::B]);
    assert!(r.notices.contains(&Notice::CloseContactors) && r.notices.contains(&Notice::OpenContactors));
    let loops = r.notices.iter().filter(|n| matches!(n, Notice::ChargeLoop(_))).count();
    assert_eq!(loops, 3);
    // The charger saw nothing it had to complain about.
    assert!(!r.log.iter().any(|l| l.contains("violation") || l.contains("out of sequence")), "{:?}", r.log);
}

#[test]
fn the_charger_ends_the_charge_with_evse_shutdown() {
    let evse = EvseConfig { din: evse_sim::Din { shutdown_after_loops: Some(2), ..Default::default() }, ..Default::default() };
    let r = run(DcConnector::Extended, evse, din(), None);
    assert_eq!(r.end, EndReason::Completed, "{:?}", r.log);
    // The second answer asks to stop: its notice is the stop.
    assert_eq!(r.notices.iter().filter(|n| matches!(n, Notice::ChargeLoop(_))).count(), 1);
    assert!(r.notices.contains(&Notice::StopEnergyTransfer { within_s: None }));
}

#[test]
fn a_vehicle_that_wants_to_stop_early_sends_session_stop_next() {
    // V2G-DC-648: before the power is on the next request is SessionStopReq. The charger answers
    // the first message of the loop; the stop arrives with it.
    let (mut rx, mut tx, mut arena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let sent: Sent = Rc::default();
    let mut session = Session::new(din(), TestEv::new(DcConnector::Extended), Log(sent.clone()), Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    let mut evse = Evse::new(EvseConfig::default());
    let mut pump = |session: &mut Session<'_, TestEv, Log>, step: evcc_core::session::Step| -> Option<evcc_core::session::Step> {
        let n = step.send?;
        let frame = session.tx()[..n].to_vec();
        let bytes = evse.handle(&frame).into_iter().find_map(|o| o.send)?;
        session.rx_space()[..bytes.len()].copy_from_slice(&bytes);
        Some(session.handle(Event::Received(bytes.len())))
    };
    let mut step = session.handle(Event::Start { secured: false });
    // SAP, SessionSetup.
    for _ in 0..2 {
        step = pump(&mut session, step).unwrap();
    }
    assert_eq!(session.state(), State::AwaitServiceDiscoveryRes);
    assert_eq!(session.handle(Event::StopRequested), Default::default());
    let step = pump(&mut session, step).unwrap();
    assert_eq!(session.state(), State::AwaitSessionStopRes);
    let step = pump(&mut session, step).unwrap();
    assert_eq!(step.end, Some(EndReason::Completed));
    assert_eq!(sent.borrow().last().map(String::as_str), Some("SessionStopReq"));
}

#[test]
fn ongoing_responses_are_repeated() {
    let evse = EvseConfig {
        authorization_ongoing_responses: 2,
        cable_check_ongoing_responses: 3,
        din: evse_sim::Din { cpd_ongoing_responses: 1, ..Default::default() },
        ..Default::default()
    };
    let r = run(DcConnector::Extended, evse, din(), Some(1));
    assert_eq!(r.end, EndReason::Completed, "{:?}", r.log);
    let count = |name: &str| r.sent.iter().filter(|m| *m == name).count();
    assert_eq!((count("ContractAuthenticationReq"), count("ChargeParameterDiscoveryReq"), count("CableCheckReq")), (3, 2, 4));
}

#[test]
fn an_inlet_the_charger_does_not_offer_ends_the_session() {
    // The vehicle has a plain inlet (the core pins), the charger offers the extended pins.
    let r = run(DcConnector::Core, EvseConfig::default(), din(), None);
    assert_eq!(r.end, EndReason::NoCompatibleService);
    let r = run(DcConnector::Core, EvseConfig { din: evse_sim::Din { energy_transfer: evse_sim::DinEnergyTransfer::DcCore, ..Default::default() }, ..Default::default() }, din(), Some(1));
    assert_eq!(r.end, EndReason::Completed);
}

#[test]
fn a_failing_response_ends_the_session() {
    use din70121::types::ResponseCode as Code;
    let evse = EvseConfig { cpd_code: iso15118_20::common_types::ResponseCode::FailedWrongChargeParameter, ..Default::default() };
    let r = run(DcConnector::Extended, evse, din(), None);
    assert_eq!(r.end, EndReason::RejectedDin(Code::FailedWrongChargeParameter), "{:?}", r.log);
    let evse = EvseConfig { setup_code: iso15118_20::common_types::ResponseCode::Failed, ..Default::default() };
    assert_eq!(run(DcConnector::Extended, evse, din(), None).end, EndReason::RejectedDin(Code::Failed));
}

#[test]
fn no_welding_detection_goes_straight_to_the_stop() {
    let r = run(DcConnector::Extended, EvseConfig::default(), Config { welding_detection: false, ..din() }, Some(1));
    assert_eq!(r.end, EndReason::Completed);
    assert!(!r.sent.iter().any(|m| m == "WeldingDetectionReq"));
}

#[test]
fn a_charger_without_din_refuses_the_protocol() {
    let evse = EvseConfig { protocols: vec![evse_sim::Protocol::Iso20], ..Default::default() };
    assert_eq!(run(DcConnector::Extended, evse, din(), None).end, EndReason::NoCommonProtocol);
}

#[test]
fn a_contactor_that_stays_welded_ends_the_session_with_that_reason() {
    let evse = EvseConfig {  welding_voltage_v: 400, ..Default::default() };
    let r = run(DcConnector::Extended, evse, din(), Some(1));
    assert_eq!(r.end, EndReason::ContactorWelded, "{:?}", r.log);
    assert!(!r.sent.iter().any(|m| m == "SessionStopReq"));
}

#[test]
fn a_host_that_reports_the_pilot_does_not_disturb_a_session() {
    use control_pilot::Reading;
    // The pilot goes to C before the cable check and to B at the stop, as asked: nothing there is an emergency.
    let r = run_host(DcConnector::Extended, EvseConfig::default(), din(), Some(3), Host { report: true, inject: None });
    assert_eq!(r.end, EndReason::Completed, "{:?}\n{:?}", r.states, r.log);
    let _ = Reading::CHARGING;
}

#[test]
fn an_emergency_on_the_pilot_ends_the_session_at_once_and_sends_nothing_more() {
    use control_pilot::{Pwm, Reading, State as Cp};
    for reading in [
        // The charger turned its oscillator off (IEC 61851-23 CC.3.4.3, scenario 2) ...
        Reading { state: Cp::C, pwm: Pwm::Off },
        // ... or changed the duty cycle (V2G-DC-656) ...
        Reading { state: Cp::C, pwm: Pwm::Duty(50) },
        // ... the state left C without the vehicle's own change (CC.4.3).
        Reading { state: Cp::B, pwm: Pwm::Duty(5) },
        Reading { state: Cp::Invalid, pwm: Pwm::Duty(5) },
    ] {
        let r = run_host(DcConnector::Extended, EvseConfig::default(), din(), None, Host { report: true, inject: Some((2, reading)) });
        assert_eq!(r.end, EndReason::Emergency(reading), "{:?}", r.states);
        assert_eq!(r.sent.iter().filter(|m| *m == "PowerDeliveryReq").count(), 1, "only the start of the power: {:?}", r.sent);
        assert!(!r.sent.iter().any(|m| m == "SessionStopReq"), "{:?}", r.sent);
        assert_eq!(r.sent.last().map(String::as_str), Some("CurrentDemandReq"), "nothing after the emergency: {:?}", r.sent);
    }
}

#[test]
fn a_pilot_in_state_e_or_f_closes_the_connection() {
    use control_pilot::{Pwm, Reading, State as Cp};
    for state in [Cp::E, Cp::F] {
        let reading = Reading { state, pwm: Pwm::Off };
        let r = run_host(DcConnector::Extended, EvseConfig::default(), din(), None, Host { report: true, inject: Some((2, reading)) });
        assert_eq!(r.end, EndReason::PilotFault(reading));
    }
}

#[test]
fn a_pilot_reading_before_the_start_is_remembered_but_not_acted_on() {
    use control_pilot::Reading;
    // The first reading of a session may come before `Start`: nothing happens, the session starts.
    let (mut rx, mut tx, mut arena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let mut session = Session::new(din(), TestEv::new(DcConnector::Extended), NoLog, Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    assert_eq!(session.handle(Event::Pilot(Reading::CONNECTED)), evcc_core::session::Step::default());
    assert_eq!(session.state(), State::Idle);
}
