//! End to end over real TCP on localhost: the EV session machine (through the Linux adapter)
//! against the simulated EVSE.

use std::net::{TcpListener, TcpStream};
use std::thread;

use evcc_core::ev::{DcLimits, Ev};
use evcc_core::log::NoLog;
use evcc_core::session::{Buffers, Config, EndReason, Session, State as SessionState};
use evcc_linux::{Outcome, run};
use evse_sim::State as EvseState;
use iso15118_20::common_types::{RationalNumber, ResponseCode};

struct Car;
impl Ev for Car {
    fn evcc_id(&self) -> &str {
        "WMIV1234567890ABCDEX"
    }
    fn dc_limits(&self) -> DcLimits {
        let rn = |exponent, value| RationalNumber { exponent, value };
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
    fn header_timestamp(&self) -> u64 {
        1_700_000_000
    }
}

/// Starts the simulator on an ephemeral port; returns its address and the thread's result.
fn start(config: evse_sim::Config) -> (std::net::SocketAddr, thread::JoinHandle<std::io::Result<EvseState>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    (addr, thread::spawn(move || evse_sim::serve_one(&listener, config)))
}

fn run_ev(addr: std::net::SocketAddr, config: Config) -> Outcome {
    let mut stream = TcpStream::connect(addr).unwrap();
    let (mut rx, mut tx, mut arena) = ([0u8; 1024], [0u8; 1024], [0u8; 4096]);
    let mut session = Session::new(config, Car, NoLog, Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    run(&mut stream, &mut session, false).unwrap()
}

#[test]
fn session_is_set_up_over_tcp() {
    let evse = evse_sim::Config::default();
    let want = evse.session_id;
    let (addr, server) = start(evse);
    assert_eq!(run_ev(addr, Config::DC_20), Outcome::Reached { session_id: want, state: SessionState::ChargeParametersDiscovered });
    assert_eq!(server.join().unwrap().unwrap(), EvseState::ChargeParametersDiscovered);
}

#[test]
fn evse_rejecting_the_protocol_ends_the_session() {
    let (addr, server) = start(evse_sim::Config { reject_protocol: true, ..Default::default() });
    assert_eq!(run_ev(addr, Config::DC_20), Outcome::Ended(EndReason::NoCommonProtocol));
    server.join().unwrap().unwrap();
}

#[test]
fn failing_response_code_is_reported_to_the_ev() {
    let (addr, server) = start(evse_sim::Config { setup_code: ResponseCode::FailedSequenceError, ..Default::default() });
    assert_eq!(run_ev(addr, Config::DC_20), Outcome::Ended(EndReason::Rejected(ResponseCode::FailedSequenceError)));
    server.join().unwrap().unwrap();
}

#[test]
fn silent_evse_makes_the_ev_time_out() {
    let (addr, server) = start(evse_sim::Config { silent_on_setup: true, ..Default::default() });
    let config = Config { response_timeout_ms: 150, ..Config::DC_20 };
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
    assert_eq!(run_ev(addr, Config::DC_20), Outcome::ConnectionClosed);
}

#[test]
fn authorization_polling_ends_when_the_evse_finishes() {
    let (addr, server) = start(evse_sim::Config { authorization_ongoing_responses: 3, ..Default::default() });
    let outcome = run_ev(addr, Config::DC_20);
    assert!(matches!(outcome, Outcome::Reached { .. }), "{outcome:?}");
    assert_eq!(server.join().unwrap().unwrap(), EvseState::ChargeParametersDiscovered);
}

#[test]
fn evse_that_never_finishes_authorization_hits_the_ongoing_timeout() {
    let (addr, server) = start(evse_sim::Config { authorization_never_finishes: true, ..Default::default() });
    // Responses arrive instantly, so only the ongoing timer can end this.
    let config = Config { ongoing_timeout_ms: 200, ..Config::DC_20 };
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
        run_ev(addr, Config::DC_20),
        Outcome::Ended(EndReason::Rejected(ResponseCode::WarningEimAuthorizationFailure))
    );
    server.join().unwrap().unwrap();
}

#[test]
fn evse_offering_only_pnc_leaves_no_usable_authorization() {
    let (addr, server) = start(evse_sim::Config { offered_authorization: vec![iso15118_20::common::Authorization::PnC], ..Default::default() });
    assert_eq!(run_ev(addr, Config::DC_20), Outcome::Ended(EndReason::NoCompatibleAuthorization));
    drop(server);
}

#[test]
fn evse_without_dc_leaves_no_usable_service() {
    let (addr, server) = start(evse_sim::Config { energy_services: vec![1], ..Default::default() });
    assert_eq!(run_ev(addr, Config::DC_20), Outcome::Ended(EndReason::NoCompatibleService));
    drop(server);
}

#[test]
fn evse_with_only_scheduled_control_leaves_no_usable_parameter_set() {
    let scheduled = vec![("ControlMode".to_string(), 1), ("MobilityNeedsMode".to_string(), 1)];
    let (addr, server) = start(evse_sim::Config { parameter_sets: vec![(1, scheduled)], ..Default::default() });
    assert_eq!(run_ev(addr, Config::DC_20), Outcome::Ended(EndReason::NoCompatibleParameterSet));
    drop(server);
}

#[test]
fn the_dynamic_parameter_set_is_chosen_among_several() {
    let set = |id, control_mode| (id, vec![("ControlMode".to_string(), control_mode), ("MobilityNeedsMode".to_string(), 1)]);
    let (addr, server) = start(evse_sim::Config { parameter_sets: vec![set(4, 1), set(9, 2)], ..Default::default() });
    let outcome = run_ev(addr, Config::DC_20);
    assert!(matches!(outcome, Outcome::Reached { .. }), "{outcome:?}");
    server.join().unwrap().unwrap();
}

#[test]
fn refused_service_selection_is_reported() {
    let (addr, server) = start(evse_sim::Config { selection_code: ResponseCode::FailedServiceSelectionInvalid, ..Default::default() });
    assert_eq!(
        run_ev(addr, Config::DC_20),
        Outcome::Ended(EndReason::Rejected(ResponseCode::FailedServiceSelectionInvalid))
    );
    server.join().unwrap().unwrap();
}

#[test]
fn refused_charge_parameters_are_reported() {
    let (addr, server) = start(evse_sim::Config { cpd_code: ResponseCode::FailedWrongChargeParameter, ..Default::default() });
    assert_eq!(
        run_ev(addr, Config::DC_20),
        Outcome::Ended(EndReason::Rejected(ResponseCode::FailedWrongChargeParameter))
    );
    server.join().unwrap().unwrap();
}
