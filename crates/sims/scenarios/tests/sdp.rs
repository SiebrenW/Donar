//! SECC discovery over UDP on the IPv6 loopback, followed by a session on the TCP port the
//! (simulated) charger announces.

mod common;

use std::net::{Ipv6Addr, TcpListener, UdpSocket};
use std::thread;

use evcc_core::session::{Config, EndReason};
use evcc_linux::Outcome;
use evcc_linux::sdp;
use v2gtp::sdp::{Response, Security, Transport};

const LOOPBACK: u32 = 0; // no scope needed for ::1

fn charger(security: Security, ignore: u32) -> (std::net::SocketAddr, thread::JoinHandle<std::io::Result<u32>>, TcpListener) {
    let udp = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
    let tcp = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
    let response = Response {
        address: Ipv6Addr::LOCALHOST.octets(),
        port: tcp.local_addr().unwrap().port(),
        security,
        transport: Transport::Tcp,
    };
    let addr = udp.local_addr().unwrap();
    (addr, thread::spawn(move || evse_sim::answer_discovery(&udp, response, ignore)), tcp)
}

fn config(security: Security) -> evcc_core::sdp::Config {
    evcc_core::sdp::Config { request_interval_ms: 40, ..evcc_core::sdp::Config::new(security) }
}

#[test]
fn the_charger_is_found_and_a_session_runs_on_its_port() {
    let (dest, udp, tcp) = charger(Security::NoTls, 0);
    let socket = sdp::socket().unwrap();
    let found = sdp::discover(&socket, dest, config(Security::NoTls)).unwrap();
    let evcc_core::sdp::Outcome::Found(response) = found else { panic!("{found:?}") };
    let server = thread::spawn(move || evse_sim::serve_one(&tcp, evse_sim::Config::default()));
    let mut stream = sdp::connect(&response, LOOPBACK).unwrap();
    let run = Config { poll_interval_ms: 10, charge_loop_interval_ms: 10, allow_unsecured: true, ..Config::DC_20 };
    let outcome = common::run_car(&mut stream, false, run, Some(3)).unwrap();
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    server.join().unwrap().unwrap();
    assert_eq!(udp.join().unwrap().unwrap(), 1);
}

#[test]
fn unanswered_requests_are_repeated() {
    let (dest, udp, _tcp) = charger(Security::Tls, 2);
    let socket = sdp::socket().unwrap();
    let started = std::time::Instant::now();
    let found = sdp::discover(&socket, dest, config(Security::Tls)).unwrap();
    assert!(matches!(found, evcc_core::sdp::Outcome::Found(_)), "{found:?}");
    assert!(started.elapsed() >= std::time::Duration::from_millis(80));
    assert_eq!(udp.join().unwrap().unwrap(), 3);
}

#[test]
fn no_charger_means_no_response_after_the_last_request() {
    // A bound socket that never answers.
    let silent = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
    let socket = sdp::socket().unwrap();
    let cfg = evcc_core::sdp::Config { max_requests: 3, ..config(Security::Tls) };
    let started = std::time::Instant::now();
    let found = sdp::discover(&socket, silent.local_addr().unwrap(), cfg).unwrap();
    assert_eq!(found, evcc_core::sdp::Outcome::NoResponse);
    assert!(started.elapsed() >= std::time::Duration::from_millis(100));
    // Exactly three requests arrived.
    silent.set_nonblocking(true).unwrap();
    let mut n = 0;
    while silent.recv(&mut [0u8; 64]).is_ok() {
        n += 1;
    }
    assert_eq!(n, 3);
}

#[test]
fn a_response_with_the_wrong_security_is_not_used() {
    // The EV asks for TLS; the charger offers plain TCP only. It must keep waiting.
    let (dest, _udp, _tcp) = charger(Security::NoTls, 0);
    let socket = sdp::socket().unwrap();
    let cfg = evcc_core::sdp::Config { max_requests: 3, ..config(Security::Tls) };
    assert_eq!(sdp::discover(&socket, dest, cfg).unwrap(), evcc_core::sdp::Outcome::NoResponse);
}

#[test]
fn the_multicast_destination_is_all_nodes_on_the_interface() {
    let d = sdp::multicast_destination(7);
    assert_eq!(d.to_string(), "[ff02::1%7]:15118");
}
