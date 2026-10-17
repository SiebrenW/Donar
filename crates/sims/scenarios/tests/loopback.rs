//! End to end over real TCP on localhost: the EV session machine (through the Linux adapter)
//! against the simulated EVSE.

use std::net::{TcpListener, TcpStream};
use std::thread;

mod common;

use evcc_core::session::{Config, EndReason};
use evcc_linux::Outcome;
use evse_sim::OfferedSet;
use evse_sim::State as EvseState;
use iso15118_20::common_types::ResponseCode;

/// The default configuration with a short poll pause, so polling tests stay quick.
fn fast() -> Config {
    Config { poll_interval_ms: 10, charge_loop_interval_ms: 10, allow_unsecured: true, ..Config::DC_20 }
}

/// Starts the simulator on an ephemeral port; returns its address and the thread's result.
fn start(config: evse_sim::Config) -> (std::net::SocketAddr, thread::JoinHandle<std::io::Result<EvseState>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    (addr, thread::spawn(move || evse_sim::serve_one(&listener, config)))
}

fn run_ev(addr: std::net::SocketAddr, config: Config) -> Outcome {
    run_ev_stopping_after(addr, config, Some(3))
}

/// Runs the car; after `stop_after` charge loop answers it asks to stop (None: never).
fn run_ev_stopping_after(addr: std::net::SocketAddr, config: Config, stop_after: Option<u32>) -> Outcome {
    let mut stream = TcpStream::connect(addr).unwrap();
    common::run_car(&mut stream, false, config, stop_after).unwrap()
}

#[test]
fn session_is_set_up_over_tcp() {
    let evse = evse_sim::Config::default();
    let (addr, server) = start(evse);
    assert_eq!(run_ev(addr, fast()), Outcome::Ended(EndReason::Completed));
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn evse_rejecting_the_protocol_ends_the_session() {
    let (addr, server) = start(evse_sim::Config { reject_protocol: true, ..Default::default() });
    assert_eq!(run_ev(addr, fast()), Outcome::Ended(EndReason::NoCommonProtocol));
    server.join().unwrap().unwrap();
}

#[test]
fn failing_response_code_is_reported_to_the_ev() {
    let (addr, server) = start(evse_sim::Config { setup_code: ResponseCode::FailedSequenceError, ..Default::default() });
    assert_eq!(run_ev(addr, fast()), Outcome::Ended(EndReason::Rejected(ResponseCode::FailedSequenceError)));
    server.join().unwrap().unwrap();
}

#[test]
fn silent_evse_makes_the_ev_time_out() {
    let (addr, server) = start(evse_sim::Config { silent_on_setup: true, ..Default::default() });
    let config = Config { response_timeout_ms: 150, ..fast() };
    let started = std::time::Instant::now();
    assert_eq!(run_ev(addr, config), Outcome::Ended(EndReason::Timeout));
    let took = started.elapsed();
    assert!(took >= std::time::Duration::from_millis(140) && took < std::time::Duration::from_secs(2), "{took:?}");
    drop(server); // the simulator thread ends when the EV closes the socket
}

#[test]
fn evse_hanging_up_is_reported() {
    // A server that accepts and immediately closes.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || drop(listener.accept()));
    assert_eq!(run_ev(addr, fast()), Outcome::ConnectionClosed);
}

#[test]
fn authorization_polling_ends_when_the_evse_finishes() {
    let (addr, server) = start(evse_sim::Config { authorization_ongoing_responses: 3, ..Default::default() });
    let outcome = run_ev(addr, fast());
    assert!(outcome == Outcome::Ended(EndReason::Completed), "{outcome:?}");
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn evse_that_never_finishes_authorization_hits_the_ongoing_timeout() {
    let (addr, server) = start(evse_sim::Config { authorization_never_finishes: true, ..Default::default() });
    // Responses arrive instantly, so only the ongoing timer can end this.
    let config = Config { ongoing_timeout_ms: 200, ..fast() };
    let started = std::time::Instant::now();
    assert_eq!(run_ev(addr, config), Outcome::Ended(EndReason::Timeout));
    let took = started.elapsed();
    assert!(took >= std::time::Duration::from_millis(190) && took < std::time::Duration::from_secs(2), "{took:?}");
    drop(server);
}

#[test]
fn refused_authorization_is_reported() {
    let (addr, server) = start(evse_sim::Config { authorization_code: ResponseCode::WarningEimAuthorizationFailure, ..Default::default() });
    assert_eq!(
        run_ev(addr, fast()),
        Outcome::Ended(EndReason::Rejected(ResponseCode::WarningEimAuthorizationFailure))
    );
    server.join().unwrap().unwrap();
}

#[test]
fn evse_offering_only_pnc_leaves_no_usable_authorization() {
    let (addr, server) = start(evse_sim::Config { offered_authorization: vec![iso15118_20::common::Authorization::PnC], ..Default::default() });
    assert_eq!(run_ev(addr, fast()), Outcome::Ended(EndReason::NoCompatibleAuthorization));
    drop(server);
}

#[test]
fn evse_without_dc_leaves_no_usable_service() {
    let (addr, server) = start(evse_sim::Config { energy_services: vec![1], ..Default::default() });
    assert_eq!(run_ev(addr, fast()), Outcome::Ended(EndReason::NoCompatibleService));
    drop(server);
}

#[test]
fn evse_with_only_scheduled_control_leaves_no_usable_parameter_set() {
    let scheduled = OfferedSet::new(1, [("ControlMode", 1), ("MobilityNeedsMode", 1)]);
    let (addr, server) = start(evse_sim::Config { parameter_sets: vec![scheduled], ..Default::default() });
    assert_eq!(run_ev(addr, fast()), Outcome::Ended(EndReason::NoCompatibleParameterSet));
    drop(server);
}

#[test]
fn the_dynamic_parameter_set_is_chosen_among_several() {
    let set = |id, control_mode| OfferedSet::new(id, [("Connector", 1), ("ControlMode", control_mode), ("MobilityNeedsMode", 1)]);
    let (addr, server) = start(evse_sim::Config { parameter_sets: vec![set(4, 1), set(9, 2)], ..Default::default() });
    let outcome = run_ev(addr, fast());
    assert!(outcome == Outcome::Ended(EndReason::Completed), "{outcome:?}");
    server.join().unwrap().unwrap();
}

#[test]
fn refused_service_selection_is_reported() {
    let (addr, server) = start(evse_sim::Config { selection_code: ResponseCode::FailedServiceSelectionInvalid, ..Default::default() });
    assert_eq!(
        run_ev(addr, fast()),
        Outcome::Ended(EndReason::Rejected(ResponseCode::FailedServiceSelectionInvalid))
    );
    server.join().unwrap().unwrap();
}

#[test]
fn refused_charge_parameters_are_reported() {
    let (addr, server) = start(evse_sim::Config { cpd_code: ResponseCode::FailedWrongChargeParameter, ..Default::default() });
    assert_eq!(
        run_ev(addr, fast()),
        Outcome::Ended(EndReason::Rejected(ResponseCode::FailedWrongChargeParameter))
    );
    server.join().unwrap().unwrap();
}

#[test]
fn schedule_exchange_polling_ends_when_the_evse_finishes() {
    let (addr, server) = start(evse_sim::Config { schedule_ongoing_responses: 3, ..Default::default() });
    let outcome = run_ev(addr, fast());
    assert!(outcome == Outcome::Ended(EndReason::Completed), "{outcome:?}");
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn refused_schedule_is_reported() {
    let (addr, server) = start(evse_sim::Config { schedule_code: ResponseCode::FailedScheduleSelectionInvalid, ..Default::default() });
    assert_eq!(
        run_ev(addr, fast()),
        Outcome::Ended(EndReason::Rejected(ResponseCode::FailedScheduleSelectionInvalid))
    );
    server.join().unwrap().unwrap();
}

#[test]
fn a_parameter_set_for_another_connector_is_not_chosen() {
    let set = OfferedSet::new(1, [("Connector", 2), ("ControlMode", 2), ("MobilityNeedsMode", 1)]);
    let (addr, server) = start(evse_sim::Config { parameter_sets: vec![set], ..Default::default() });
    assert_eq!(run_ev(addr, fast()), Outcome::Ended(EndReason::NoCompatibleParameterSet));
    drop(server);
}

#[test]
fn cable_check_and_pre_charge_run_to_power_delivery() {
    let (addr, server) = start(evse_sim::Config { cable_check_ongoing_responses: 3, ..Default::default() });
    let outcome = run_ev(addr, fast());
    assert!(outcome == Outcome::Ended(EndReason::Completed), "{outcome:?}");
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn failed_cable_check_is_reported() {
    let (addr, server) = start(evse_sim::Config { cable_check_code: ResponseCode::Failed, ..Default::default() });
    assert_eq!(run_ev(addr, fast()), Outcome::Ended(EndReason::Rejected(ResponseCode::Failed)));
    server.join().unwrap().unwrap();
}

#[test]
fn refused_power_delivery_is_reported() {
    let (addr, server) = start(evse_sim::Config { power_delivery_code: ResponseCode::FailedPowerDeliveryNotApplied, ..Default::default() });
    assert_eq!(
        run_ev(addr, fast()),
        Outcome::Ended(EndReason::Rejected(ResponseCode::FailedPowerDeliveryNotApplied))
    );
    server.join().unwrap().unwrap();
}

#[test]
fn the_evse_can_end_the_charge() {
    let (addr, server) = start(evse_sim::Config { terminate_after_loops: Some(4), ..Default::default() });
    assert_eq!(run_ev_stopping_after(addr, fast(), None), Outcome::Ended(EndReason::Completed));
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

fn run_recorded(addr: std::net::SocketAddr, stop_after: Option<u32>) -> (Outcome, common::Recording) {
    let mut stream = TcpStream::connect(addr).unwrap();
    common::run_car_recorded(&mut stream, fast(), stop_after)
}

#[test]
fn a_pause_request_is_answered_with_a_standby_period_that_ends_with_exit_standby() {
    let config = evse_sim::Config { pause_after_loops: Some(2), standby_loops: 2, ..Default::default() };
    let (addr, server) = start(config);
    // The car would charge until told otherwise: it only stops after the standby period.
    let (outcome, log) = run_recorded(addr, Some(12));
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(log.count("to: AwaitPowerDeliveryStandbyRes"), 1);
    // PowerDeliveryReq: Start, Standby, Start (leaving standby), Stop.
    assert_eq!(log.count("MsgTx { msg: PowerDeliveryReq }"), 4);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn a_refused_standby_period_leaves_the_charge_going() {
    let config = evse_sim::Config { pause_after_loops: Some(2), standby_allowed: false, ..Default::default() };
    let (addr, server) = start(config);
    let (outcome, log) = run_recorded(addr, Some(6));
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    // Start, the refused Standby, Stop.
    assert_eq!(log.count("MsgTx { msg: PowerDeliveryReq }"), 3);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn the_car_can_stop_during_a_standby_period() {
    let config = evse_sim::Config { pause_after_loops: Some(2), standby_loops: 1000, ..Default::default() };
    let (addr, server) = start(config);
    let (outcome, log) = run_recorded(addr, Some(6));
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    // Start, Standby, Stop: no second Start.
    assert_eq!(log.count("MsgTx { msg: PowerDeliveryReq }"), 3);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn the_evse_can_terminate_during_a_standby_period() {
    let config = evse_sim::Config { pause_after_loops: Some(2), standby_loops: 1000, terminate_after_loops: Some(6), ..Default::default() };
    let (addr, server) = start(config);
    let (outcome, _) = run_recorded(addr, None);
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

fn run_behaving(addr: std::net::SocketAddr, behaviour: evcc_sim::Behaviour) -> (Outcome, common::Recording) {
    let mut stream = TcpStream::connect(addr).unwrap();
    common::run_behaving_car(&mut stream, fast(), behaviour)
}

const CAR: evcc_sim::Behaviour = evcc_sim::Behaviour {
    connector: evcc_core::ev::DcConnector::Core,
    stop_after: None,
    renegotiate_after: None,
    renegotiate_schedule_after: None,
    stop_when_rescheduled: false,
    pause_after: None,
};

#[test]
fn a_service_renegotiation_asked_by_the_evse_restarts_the_service_flow_in_the_same_session() {
    let config = evse_sim::Config {
        service_renegotiation_supported: true,
        service_renegotiation_after_loops: Some(2),
        terminate_after_loops: Some(3),
        ..Default::default()
    };
    let (addr, server) = start(config);
    let (outcome, log) = run_behaving(addr, CAR);
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(log.count("SessionSetUp"), 1);
    // Two service flows through the same authorization.
    assert_eq!(log.count("MsgTx { msg: ServiceDiscoveryReq }"), 2);
    assert_eq!(log.count("MsgTx { msg: DcCableCheckReq }"), 2);
    // Start, Stop, Start, Stop; SessionStopReq for the renegotiation and for the end.
    assert_eq!(log.count("MsgTx { msg: PowerDeliveryReq }"), 4);
    assert_eq!(log.count("MsgTx { msg: SessionStopReq }"), 2);
    assert_eq!(log.count("MsgTx { msg: AuthorizationReq }"), 1);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn a_service_renegotiation_asked_by_the_car_is_honoured_when_the_evse_offered_it() {
    let config = evse_sim::Config { service_renegotiation_supported: true, ..Default::default() };
    let (addr, server) = start(config);
    let (outcome, log) = run_behaving(addr, evcc_sim::Behaviour { stop_after: Some(6), renegotiate_after: Some(2), ..CAR });
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(log.count("MsgTx { msg: ServiceDiscoveryReq }"), 2);
    assert_eq!(log.count("MsgTx { msg: PowerDeliveryReq }"), 4);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn without_the_evse_offering_it_service_renegotiation_is_neither_asked_for_nor_obeyed() {
    // The charger asks for it anyway (V2G20-1652: the car does not initiate) and the car's
    // own wish is dropped (V2G20-1476 needs the offer in the first ServiceDiscoveryRes).
    let config = evse_sim::Config {
        service_renegotiation_supported: false,
        service_renegotiation_after_loops: Some(2),
        ..Default::default()
    };
    let (addr, server) = start(config);
    let (outcome, log) = run_behaving(addr, evcc_sim::Behaviour { stop_after: Some(6), renegotiate_after: Some(3), ..CAR });
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(log.count("MsgTx { msg: ServiceDiscoveryReq }"), 1);
    assert_eq!(log.count("MsgTx { msg: PowerDeliveryReq }"), 2);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn a_charger_that_refuses_the_renegotiation_it_offered_ends_the_session_with_the_response_code() {
    // The car asks for a renegotiation the charger will refuse at SessionStopReq (it offered
    // it in ServiceDiscoveryRes, then answers FAILED_NoServiceRenegotiationSupported).
    // Built by lying in one place: offer it, then withdraw it before the stop.
    let config = evse_sim::Config {
        service_renegotiation_supported: true,
        refuse_service_renegotiation: true,
        ..Default::default()
    };
    let (addr, server) = start(config);
    let (outcome, _) = run_behaving(addr, evcc_sim::Behaviour { stop_after: Some(6), renegotiate_after: Some(2), ..CAR });
    assert_eq!(outcome, Outcome::Ended(EndReason::Rejected(ResponseCode::FailedNoServiceRenegotiationSupported)));
    drop(server);
}

#[test]
fn a_schedule_renegotiation_asked_by_the_evse_runs_on_the_side_stream_next_to_the_charge_loop() {
    let config = evse_sim::Config { schedule_renegotiation_after_loops: Some(2), ..Default::default() };
    let (addr, server) = start(config);
    let (outcome, log) = run_behaving(addr, evcc_sim::Behaviour { stop_when_rescheduled: true, ..CAR });
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    // Once in the main flow and once on the side stream.
    assert_eq!(log.count("MsgTx { msg: DcChargeParameterDiscoveryReq }"), 2);
    assert_eq!(log.count("MsgTx { msg: ScheduleExchangeReq }"), 2);
    // Start, the one that ends the renegotiation, Stop; one session, one connection.
    assert_eq!(log.count("MsgTx { msg: PowerDeliveryReq }"), 3);
    assert_eq!(log.count("SessionSetUp"), 1);
    // The charge loop was running when it started.
    assert!(log.count("MsgTx { msg: DcChargeLoopReq }") >= 2);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn a_schedule_renegotiation_asked_by_the_car_waits_for_ongoing_answers() {
    let config = evse_sim::Config { schedule_ongoing_responses: 2, ..Default::default() };
    let (addr, server) = start(config);
    let (outcome, log) = run_behaving(
        addr,
        evcc_sim::Behaviour { renegotiate_schedule_after: Some(2), stop_when_rescheduled: true, ..CAR },
    );
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    // The main exchange needed three requests, the side stream's too.
    assert_eq!(log.count("MsgTx { msg: ScheduleExchangeReq }"), 6);
    assert_eq!(log.count("MsgTx { msg: DcChargeParameterDiscoveryReq }"), 2);
    assert_eq!(log.count("MsgTx { msg: PowerDeliveryReq }"), 3);
    assert!(log.count("MsgTx { msg: DcChargeLoopReq }") > 2);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn a_side_stream_that_goes_unanswered_times_out_the_session() {
    let config = evse_sim::Config { side_stream_silent: true, ..Default::default() };
    let (addr, _server) = start(config);
    let mut stream = TcpStream::connect(addr).unwrap();
    let car = evcc_sim::Behaviour { renegotiate_schedule_after: Some(2), ..CAR };
    let (outcome, _) = common::run_behaving_car(&mut stream, Config { response_timeout_ms: 200, ..fast() }, car);
    assert_eq!(outcome, Outcome::Ended(EndReason::Timeout));
}

#[test]
fn a_renegotiation_the_car_asks_for_too_early_or_twice_is_not_started() {
    // Asked for in the first round: V2G20-1846 wants a ChargeLoopReq before it, which the
    // first round's answer has satisfied; asked for during one that runs: ignored.
    let config = evse_sim::Config { schedule_renegotiation_after_loops: Some(2), ..Default::default() };
    let (addr, server) = start(config);
    let car = evcc_sim::Behaviour { stop_after: Some(6), renegotiate_schedule_after: Some(2), ..CAR };
    let (outcome, log) = run_behaving(addr, car);
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    // The EVSE's notification and the car's wish came in the same round: one renegotiation.
    assert_eq!(log.count("MsgTx { msg: DcChargeParameterDiscoveryReq }"), 2);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

/// A SECC that serves two connections one after the other, like a charger that keeps a paused
/// session; the second sees the same configuration.
fn start_twice(config: evse_sim::Config) -> (std::net::SocketAddr, thread::JoinHandle<[EvseState; 2]>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    (
        addr,
        thread::spawn(move || {
            let first = evse_sim::serve_one(&listener, config.clone()).unwrap();
            [first, evse_sim::serve_one(&listener, config).unwrap()]
        }),
    )
}

fn run_with_resume(addr: std::net::SocketAddr, resume: Option<evcc_core::session::Resume>) -> (Outcome, common::Recording) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let config = Config { resume, ..fast() };
    common::run_behaving_car(&mut stream, config, evcc_sim::Behaviour { stop_after: Some(3), ..CAR })
}

fn pause_first_session(addr: std::net::SocketAddr) -> evcc_core::session::Resume {
    let mut stream = TcpStream::connect(addr).unwrap();
    let (outcome, _) = common::run_behaving_car(&mut stream, fast(), evcc_sim::Behaviour { pause_after: Some(2), ..CAR });
    let Outcome::Ended(EndReason::Paused(resume)) = outcome else { panic!("{outcome:?}") };
    resume
}

#[test]
fn a_paused_session_is_resumed_on_a_new_connection_without_authorization_or_service_selection() {
    let (addr, server) = start_twice(evse_sim::Config::default());
    let resume = pause_first_session(addr);
    assert_eq!(resume.session_id, evse_sim::Config::default().session_id);
    assert_eq!(resume.selected.service_id, 2);
    let (outcome, log) = run_with_resume(addr, Some(resume));
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(log.count("SessionResumed"), 1);
    for skipped in ["AuthorizationSetupReq", "AuthorizationReq", "ServiceDiscoveryReq", "ServiceSelectionReq"] {
        assert_eq!(log.count(&format!("MsgTx {{ msg: {skipped} }}")), 0, "{skipped}");
    }
    // Charge parameters, schedule, cable check, pre-charge and the charge itself again.
    assert_eq!(log.count("MsgTx { msg: DcChargeParameterDiscoveryReq }"), 1);
    assert_eq!(log.count("MsgTx { msg: DcCableCheckReq }"), 1);
    assert_eq!(server.join().unwrap(), [EvseState::Paused, EvseState::Stopped]);
}

#[test]
fn a_charger_that_forgot_the_paused_session_makes_the_car_authorize_again() {
    let (addr, server) = start_twice(evse_sim::Config { forget_paused_sessions: true, ..Default::default() });
    let resume = pause_first_session(addr);
    let (outcome, log) = run_with_resume(addr, Some(resume));
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    // V2G20-2615 ff.: a new session with a new id and the whole flow from authorization.
    assert_eq!(log.count("SessionResumed"), 0);
    assert_eq!(log.count("MsgTx { msg: AuthorizationReq }"), 1);
    assert_eq!(log.count("MsgTx { msg: ServiceDiscoveryReq }"), 1);
    assert_eq!(server.join().unwrap(), [EvseState::Paused, EvseState::Stopped]);
}

#[test]
fn the_evse_asking_for_a_pause_can_be_answered_with_a_session_pause() {
    let (addr, server) = start(evse_sim::Config { pause_after_loops: Some(2), ..Default::default() });
    let mut stream = TcpStream::connect(addr).unwrap();
    let config = Config { pause_on_notification: true, ..fast() };
    let (outcome, log) = common::run_behaving_car(&mut stream, config, CAR);
    assert!(matches!(outcome, Outcome::Ended(EndReason::Paused(_))), "{outcome:?}");
    assert_eq!(log.count("MsgTx { msg: SessionStopReq }"), 1);
    assert_eq!(log.count("to: AwaitPowerDeliveryStandbyRes"), 0);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Paused);
}

#[test]
fn the_vehicle_logs_which_parameters_it_wanted_and_what_was_offered() {
    // A charger with a set for the extended inlet only, written with the names a file may use.
    let set: OfferedSet = toml::from_str("id = 5\nvalues = { Connector = \"extended\", ControlMode = \"dynamic\", MobilityNeedsMode = \"evcc\" }").unwrap();
    let (addr, server) = start(evse_sim::Config { parameter_sets: vec![set], ..Default::default() });
    let mut stream = TcpStream::connect(addr).unwrap();
    let (outcome, log) = common::run_car_recorded(&mut stream, fast(), Some(3));
    assert_eq!(outcome, Outcome::Ended(EndReason::NoCompatibleParameterSet));
    assert_eq!(log.count("NoParameterSet"), 1);
    // The vehicle's inlet (core is 1), its filter and the offer with its parameters.
    let line = log.0.borrow().iter().find(|r| r.contains("NoParameterSet")).cloned().unwrap();
    for part in ["connector: 1", "ControlMode", "parameter_set_id: 5", "\"Connector\"", "ByteValue(2)"] {
        assert!(line.contains(part) || line.contains(&part.replace("ByteValue", "IntValue")), "{part} missing from {line}");
    }
    drop(server);
}

/// Serves one connection like `evse_sim::serve_one`, and returns what the charger logged.
fn serve_logged(listener: TcpListener, config: evse_sim::Config) -> std::io::Result<(EvseState, Vec<String>)> {
    use std::io::{Read, Write};
    let (mut stream, _) = listener.accept()?;
    let mut evse = evse_sim::Evse::new(config);
    let (mut buf, mut log) = ([0u8; 2048], Vec::new());
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let mut close = false;
        for out in evse.handle(&buf[..n]) {
            if let Some(bytes) = out.send {
                stream.write_all(&bytes)?;
            }
            close |= out.close;
        }
        log.extend(evse.take_log());
        if close {
            break;
        }
    }
    log.extend(evse.take_log());
    Ok((evse.state(), log))
}

#[test]
fn a_normal_session_breaks_no_rule_on_either_side() {
    // The two state machines against each other: values in the standard's bounds, response
    // codes from the tables, the charge loop within the discovery's limits (V2G20-2198, 2199,
    // 2654, 2655); neither side notes a violation.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || serve_logged(listener, evse_sim::Config::default()));
    let mut stream = TcpStream::connect(addr).unwrap();
    let (outcome, ev_log) = common::run_car_recorded(&mut stream, fast(), Some(5));
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    let (state, evse_log) = server.join().unwrap().unwrap();
    assert_eq!(state, EvseState::Stopped);
    let violations: Vec<&String> = evse_log.iter().filter(|l| l.contains("violation")).collect();
    assert!(violations.is_empty(), "the charger found: {violations:?}");
    assert_eq!(ev_log.count("Violation"), 0, "{:?}", ev_log.0.borrow());
    assert_eq!(ev_log.count("ResponseCodeNotAllowed"), 0, "{:?}", ev_log.0.borrow());
    // The charge loop was exercised.
    assert!(evse_log.iter().any(|l| l.contains("tx DC_ChargeLoopRes OK")), "{evse_log:?}");
}

#[test]
fn a_vehicle_that_raises_its_limits_in_the_charge_loop_is_noted_and_may_be_failed() {
    // A car that does not hold itself to V2G20-2198: with `enforce_cpd_limits` off and limits that
    // grow after the discovery. The charger notes it, and fails the loop when asked to.
    struct Greedy(evcc_sim::SimulatedEv, std::cell::Cell<u32>);
    impl evcc_core::ev::Ev for Greedy {
        fn evcc_id(&self) -> &str {
            self.0.evcc_id()
        }
        fn now_micros(&self) -> u64 {
            self.0.now_micros()
        }
        fn dc_connector(&self) -> evcc_core::ev::DcConnector {
            self.0.dc_connector()
        }
        fn dc_limits(&self) -> evcc_core::ev::DcLimits {
            // The first call is the discovery; later ones ask for more.
            let calls = self.1.get();
            self.1.set(calls + 1);
            let mut l = self.0.dc_limits();
            if calls > 0 {
                l.maximum_charge_current = iso15118_20::common_types::RationalNumber::from_f64(250.0);
            }
            l
        }
        fn battery_voltage(&self) -> iso15118_20::common_types::RationalNumber {
            self.0.battery_voltage()
        }
        fn inlet_voltage(&self) -> iso15118_20::common_types::RationalNumber {
            self.0.inlet_voltage()
        }
        fn schedule_request(&self) -> evcc_core::ev::ScheduleRequest {
            self.0.schedule_request()
        }
    }
    for (fail, expect_ended) in [(false, EndReason::Completed), (true, EndReason::Rejected(ResponseCode::Failed))] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let config = evse_sim::Config { fail_on_limit_violation: fail, ..Default::default() };
        let server = thread::spawn(move || serve_logged(listener, config));
        let mut stream = TcpStream::connect(addr).unwrap();
        let open = std::rc::Rc::new(std::cell::Cell::new(false));
        let car = Greedy(evcc_sim::SimulatedEv::new(open, evcc_core::ev::DcConnector::Core), std::cell::Cell::new(0));
        let config = Config { enforce_cpd_limits: false, ..fast() };
        let (mut rx, mut tx, mut arena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
        let mut session = evcc_core::session::Session::new(config, car, evcc_core::log::NoLog, evcc_core::session::Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
        let mut loops = 0;
        let outcome = evcc_linux::run(&mut stream, &mut session, false, &mut |n| match n {
            evcc_core::session::Notice::CloseContactors => Some(evcc_core::session::Event::ContactorsClosed),
            evcc_core::session::Notice::StopEnergyTransfer { .. } => Some(evcc_core::session::Event::EnergyTransferStopped),
            evcc_core::session::Notice::OpenContactors => Some(evcc_core::session::Event::ContactorsOpened),
            evcc_core::session::Notice::ChargeLoop(_) => {
                loops += 1;
                (loops == 3).then_some(evcc_core::session::Event::StopRequested)
            }
            _ => None,
        })
        .unwrap();
        assert_eq!(outcome, Outcome::Ended(expect_ended), "fail = {fail}");
        let (_, log) = server.join().unwrap().unwrap();
        assert!(log.iter().any(|l| l.contains("violation: V2G20-2198: EVMaximumChargeCurrent is 250")), "{log:?}");
    }
}

// ---------------------------------------------------------------- ISO 15118-20 AC

fn fast_ac() -> Config {
    Config { poll_interval_ms: 10, charge_loop_interval_ms: 10, allow_unsecured: true, ..Config::AC_20 }
}

fn run_ac(addr: std::net::SocketAddr, config: Config, stop_after: Option<u32>) -> (Outcome, common::Recording) {
    let mut stream = TcpStream::connect(addr).unwrap();
    common::run_car_recorded(&mut stream, config, stop_after)
}

#[test]
fn an_ac_session_goes_from_the_schedule_exchange_straight_to_power_delivery() {
    let (addr, server) = start(evse_sim::Config::default());
    let (outcome, log) = run_ac(addr, fast_ac(), Some(3));
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(log.count("MsgTx { msg: AcChargeParameterDiscoveryReq }"), 1);
    assert_eq!(log.count("MsgTx { msg: AcChargeLoopReq }"), 3);
    // Start and Stop; nothing of DC.
    assert_eq!(log.count("MsgTx { msg: PowerDeliveryReq }"), 2);
    assert_eq!(log.count("DcCableCheckReq") + log.count("DcPreChargeReq") + log.count("DcWeldingDetectionReq"), 0);
    assert_eq!(log.count("Notice::OpenContactors") + log.count("OpenContactors"), 0);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn a_bidirectional_ac_session_uses_the_bpt_variants() {
    let (addr, server) = start(evse_sim::Config::default());
    let config = Config { poll_interval_ms: 10, charge_loop_interval_ms: 10, allow_unsecured: true, ..Config::AC_BPT_20 };
    let (outcome, log) = run_ac(addr, config, Some(2));
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(log.count("MsgTx { msg: AcChargeLoopReq }"), 2);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn a_three_phase_car_does_not_choose_a_single_phase_set() {
    let single = OfferedSet::new(1, [("Connector", 1), ("EVSENominalVoltage", 230), ("ControlMode", 2), ("MobilityNeedsMode", 1), ("Pricing", 0)]);
    let (addr, server) = start(evse_sim::Config { ac_parameter_sets: vec![single], ..Default::default() });
    let (outcome, _) = run_ac(addr, fast_ac(), Some(2));
    assert_eq!(outcome, Outcome::Ended(EndReason::NoCompatibleParameterSet));
    drop(server);
}

#[test]
fn the_evse_can_end_an_ac_charge_and_renegotiate_the_schedule() {
    let config = evse_sim::Config { schedule_renegotiation_after_loops: Some(2), terminate_after_loops: Some(6), ..Default::default() };
    let (addr, server) = start(config);
    let (outcome, log) = run_ac(addr, fast_ac(), None);
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    // The side stream asked for the charge parameters again, once, next to the main one.
    assert_eq!(log.count("MsgTx { msg: AcChargeParameterDiscoveryReq }"), 2);
    // Start, the one that ends the renegotiation, Stop.
    assert_eq!(log.count("MsgTx { msg: PowerDeliveryReq }"), 3);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn a_scheduled_ac_session_selects_a_schedule_and_follows_it() {
    let (addr, server) = start(evse_sim::Config::default());
    let (outcome, log) = run_ac(addr, Config { poll_interval_ms: 10, charge_loop_interval_ms: 10, allow_unsecured: true, ..Config::AC_20.scheduled() }, Some(3));
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(log.count("MsgTx { msg: AcChargeLoopReq }"), 3);
    assert_eq!(log.count("MsgTx { msg: PowerDeliveryReq }"), 2);
    // Parameter set 4 of the charger: three phases, Scheduled.
    assert_eq!(log.count("ServiceSelected { service_id: 1, parameter_set_id: 4 }"), 1);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn a_scheduled_bidirectional_ac_session_and_a_renegotiation_of_the_schedule() {
    let config = evse_sim::Config { schedule_renegotiation_after_loops: Some(2), terminate_after_loops: Some(6), ..Default::default() };
    let (addr, server) = start(config);
    let (outcome, log) = run_ac(addr, Config { poll_interval_ms: 10, charge_loop_interval_ms: 10, allow_unsecured: true, ..Config::AC_BPT_20.scheduled() }, None);
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(log.count("ServiceSelected { service_id: 5, parameter_set_id: 4 }"), 1);
    assert_eq!(log.count("MsgTx { msg: ScheduleExchangeReq }"), 2);
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn a_dynamic_car_and_a_charger_offering_only_scheduled_sets_do_not_match() {
    let scheduled = OfferedSet::new(1, [("Connector", 2), ("EVSENominalVoltage", 400), ("ControlMode", 1), ("MobilityNeedsMode", 1), ("Pricing", 0)]);
    let (addr, server) = start(evse_sim::Config { ac_parameter_sets: vec![scheduled], ..Default::default() });
    let (outcome, _) = run_ac(addr, fast_ac(), Some(2));
    assert_eq!(outcome, Outcome::Ended(EndReason::NoCompatibleParameterSet));
    drop(server);
}
