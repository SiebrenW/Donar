//! Linux only, needs root: SDP multicast discovery and a session across a veth pair between
//! two network namespaces. Run with `cargo xtask linux-test`.
#![cfg(target_os = "linux")]

mod common;

use std::net::{Ipv6Addr, SocketAddrV6, TcpListener, UdpSocket};
use std::thread;

use evcc_core::session::{Config, EndReason};
use evcc_linux::slac::{self, NoModem, PacketSocket};
use evcc_linux::{Outcome, sdp, tls};
use sandbox::{INTERFACE as IFACE, Node, Options, Sandbox};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::pki::{Curve, pki};
use v2gtp::sdp::{Response, Security, Transport};

/// Two nodes joined by a cable: the EVCC at `fe80::1`, the SECC at `fe80::2`.
struct Lab {
    evcc: Node,
    secc: Node,
    _sandbox: Sandbox,
}

const SECC_ADDR: &str = "fe80::2";

impl Lab {
    fn new() -> Lab {
        // A test whose one side fails while the other waits for it for ever would hang the
        // whole run; end the process with a message instead.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(90));
            eprintln!("netns test still running after 90 s: probably hung, ending the run");
            std::process::exit(101);
        });
        let mut sandbox = Sandbox::new(Options::default()).expect("the sandbox needs root and a user (sudo)");
        let (evcc, secc) = (sandbox.node("evcc").unwrap(), sandbox.node("secc").unwrap());
        let (evcc, secc) = sandbox.connect(&evcc, &secc).unwrap();
        Lab { evcc, secc, _sandbox: sandbox }
    }
}

/// Sets the flag when dropped, also while unwinding from a panic.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Runs `f` in the node on its own thread (see `Node::run_in`).
fn in_ns<T: Send>(node: &Node, f: impl FnOnce() -> T + Send) -> T {
    node.run_in(f).unwrap()
}

#[test]
fn discovery_by_multicast_and_a_session_across_a_veth_pair() {
    let lab = Lab::new();
    thread::scope(|s| {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let lab = &lab;
        let secc = s.spawn(move || {
            in_ns(&lab.secc, || {
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
        let outcome = in_ns(&lab.evcc, || {
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

const RUN: [u8; 8] = [0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80];

/// The charger's SLAC responder on `socket`: answers frames until `stop` is set or, with
/// `until_matched`, the EV has its network key.
fn serve_slac(socket: &PacketSocket, evse: &mut evse_sim::slac::Evse, stop: &AtomicBool, until_matched: bool) {
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Relaxed) {
        if until_matched && matches!(evse.state(), evse_sim::slac::State::Matched { .. }) {
            return;
        }
        if let Some(n) = socket.recv(&mut buf, Some(Duration::from_millis(20))).unwrap() {
            for frame in evse.handle(&buf[..n]) {
                socket.send(&frame).unwrap();
            }
        }
    }
}

fn slac_between_namespaces(attenuation: u8, config: evcc_core::slac::Config) -> (evcc_core::slac::Outcome, Mac) {
    let lab = Lab::new();
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        let (lab, stop) = (&lab, &stop);
        // A failing assertion must end the charger's loop too, or the scope would never return.
        let _stop_on_exit = StopOnDrop(stop);
        let (mac_tx, mac_rx) = std::sync::mpsc::channel();
        let secc = s.spawn(move || {
            in_ns(&lab.secc, || {
                let socket = PacketSocket::open(IFACE).unwrap();
                mac_tx.send(socket.mac()).unwrap();
                let mut evse = evse_sim::slac::Evse::new(evse_sim::slac::Config { mac: socket.mac(), attenuation, ..Default::default() });
                serve_slac(&socket, &mut evse, stop, false);
            })
        });
        let secc_mac = mac_rx.recv().unwrap();
        let outcome = in_ns(&lab.evcc, || {
            let socket = PacketSocket::open(IFACE).unwrap();
            slac::run(&socket, config, RUN, &mut NoModem).unwrap()
        });
        stop.store(true, Ordering::Relaxed);
        secc.join().unwrap();
        (outcome, secc_mac)
    })
}

#[test]
fn slac_matches_the_charger_on_the_other_end_of_the_cable() {
    let (outcome, secc_mac) = slac_between_namespaces(5, evcc_core::slac::Config::DEFAULT);
    let nmk = [0x5A; 16];
    assert_eq!(outcome, evcc_core::slac::Outcome::Matched { evse: secc_mac, nid: evse_sim::slac::nid(&nmk), nmk });
}

#[test]
fn slac_refuses_a_charger_that_is_too_far_away() {
    let config = evcc_core::slac::Config { repetition_ms: 1500, ..evcc_core::slac::Config::DEFAULT };
    let (outcome, _) = slac_between_namespaces(40, config);
    assert_eq!(outcome, evcc_core::slac::Outcome::Failed(evcc_core::slac::Failure::NoDirectEvse));
}

type Mac = homeplug::Mac;

fn credentials<'a>(own: &'a common::pki::Identity, trust: &'a common::pki::Pki) -> tls::Credentials<'a> {
    tls::Credentials::new(&own.chain, &own.key, &trust.pem)
}

/// The whole bring-up of a charging session: SLAC, SDP by multicast, TLS 1.3 with mutual
/// authentication, and a DC session on top.
#[test]
fn slac_then_discovery_then_a_session_over_tls() {
    let lab = Lab::new();
    let (ca, secc_id) = pki(Curve::P521, "SECC");
    let (vca, evcc_id) = pki(Curve::P521, "EVCC");
    let server_context = tls::server_context(&credentials(&secc_id, &vca)).unwrap();
    let client_context = tls::client_context(&credentials(&evcc_id, &ca)).unwrap();
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        let (lab, stop) = (&lab, &stop);
        // A failing assertion must end the charger's loop too, or the scope would never return.
        let _stop_on_exit = StopOnDrop(stop);
        let secc = s.spawn(move || {
            in_ns(&lab.secc, || {
                let udp = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, v2gtp::sdp::SDP_PORT)).unwrap();
                let tcp = TcpListener::bind((Ipv6Addr::UNSPECIFIED, 0)).unwrap();
                let socket = PacketSocket::open(IFACE).unwrap();
                let mut evse = evse_sim::slac::Evse::new(evse_sim::slac::Config { mac: socket.mac(), ..Default::default() });
                serve_slac(&socket, &mut evse, stop, true);
                let response = Response {
                    address: SECC_ADDR.parse::<Ipv6Addr>().unwrap().octets(),
                    port: tcp.local_addr().unwrap().port(),
                    security: Security::Tls,
                    transport: Transport::Tcp,
                };
                evse_sim::answer_discovery(&udp, response, 0).unwrap();
                let (stream, _) = tcp.accept().unwrap();
                let mut stream = tls::accept(&server_context, stream).unwrap();
                evse_sim::serve(&mut stream, evse_sim::Config::default()).unwrap()
            })
        });
        let outcome = in_ns(&lab.evcc, || {
            let socket = PacketSocket::open(IFACE).unwrap();
            let matched = slac::run(&socket, evcc_core::slac::Config::DEFAULT, RUN, &mut NoModem).unwrap();
            assert!(matches!(matched, evcc_core::slac::Outcome::Matched { .. }), "{matched:?}");
            let ifindex = sdp::interface_index(IFACE).unwrap();
            let found = sdp::discover(&sdp::socket().unwrap(), sdp::multicast_destination(ifindex), evcc_core::sdp::Config::new(Security::Tls)).unwrap();
            let evcc_core::sdp::Outcome::Found(response) = found else { panic!("{found:?}") };
            let mut stream = tls::connect(&client_context, sdp::connect(&response, ifindex).unwrap()).unwrap();
            let run = Config { poll_interval_ms: 10, charge_loop_interval_ms: 10, ..Config::DC_20 };
            common::run_car(&mut stream, true, run, Some(3)).unwrap()
        });
        assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
        assert_eq!(secc.join().unwrap(), evse_sim::State::Stopped);
        stop.store(true, Ordering::Relaxed);
    });
}
