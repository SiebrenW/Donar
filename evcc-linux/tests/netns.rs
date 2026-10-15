//! Linux only, needs root: SDP multicast discovery and a session across a veth pair between
//! two network namespaces. Run with `cargo xtask linux-test`.
#![cfg(target_os = "linux")]

mod common;
mod lab;

use std::net::{Ipv6Addr, SocketAddrV6, TcpListener, UdpSocket};
use std::thread;

use evcc_core::session::{Config, EndReason};
use evcc_linux::{Outcome, sdp};
use lab::{IFACE, Lab, SECC_ADDR};
use v2gtp::sdp::{Response, Security, Transport};

#[test]
fn discovery_by_multicast_and_a_session_across_a_veth_pair() {
    let lab = Lab::new();
    thread::scope(|s| {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let lab = &lab;
        let secc = s.spawn(move || {
            lab::in_ns(&lab.secc, || {
                let udp = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, v2gtp::sdp::SDP_PORT)).unwrap();
                let tcp = TcpListener::bind((Ipv6Addr::UNSPECIFIED, 0)).unwrap();
                let response = Response {
                    address: SECC_ADDR.parse::<Ipv6Addr>().unwrap().octets(),
                    port: tcp.local_addr().unwrap().port(),
                    security: Security::NoTls,
                    transport: Transport::Tcp,
                };
                ready_tx.send(()).unwrap();
                let seen = evse_sim::answer_discovery(&udp, response, 0).unwrap();
                let state = evse_sim::serve_one(&tcp, evse_sim::Config::default()).unwrap();
                (seen, state)
            })
        });
        ready_rx.recv().unwrap();
        let outcome = lab::in_ns(&lab.evcc, || {
            let ifindex = sdp::interface_index(IFACE).unwrap();
            let socket = sdp::socket().unwrap();
            let cfg = evcc_core::sdp::Config::new(Security::NoTls);
            let evcc_core::sdp::Outcome::Found(response) =
                sdp::discover(&socket, sdp::multicast_destination(ifindex), cfg).unwrap()
            else {
                panic!("no SECC found")
            };
            assert_eq!(SocketAddrV6::new(Ipv6Addr::from(response.address), response.port, 0, ifindex), sdp::address(&response, ifindex));
            let mut stream = sdp::connect(&response, ifindex).unwrap();
            let run = Config { poll_interval_ms: 10, charge_loop_interval_ms: 10, allow_unsecured: true, ..Config::DC_20 };
            common::run_car(&mut stream, false, run, Some(3)).unwrap()
        });
        assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
        let (seen, state) = secc.join().unwrap();
        assert_eq!(seen, 1);
        assert_eq!(state, evse_sim::State::Stopped);
    });
}
