//! The QCA modem driver of the EV against a fake modem on the other end of a virtual cable: it
//! answers the version request and the key request the way the real modem does (the version
//! frame is the one captured from a QCA7005).
#![cfg(target_os = "linux")]

use std::time::Duration;

use evcc_linux::slac::{Modem, PacketSocket, QcaModem};
use homeplug::modem::{self, LOCAL_DEVICE};
use sandbox::{INTERFACE, Options, Sandbox};

fn sandbox() -> Option<Sandbox> {
    // Needs root for the namespaces; skipped where that is not available.
    Sandbox::new(Options::default()).ok()
}

/// What the fake modem received.
#[derive(Debug, Default, PartialEq, Eq)]
struct Seen {
    versions: u32,
    keys: Vec<([u8; 7], [u8; 16])>,
}

/// A modem: answers the first `answers` requests, the key request with `result`.
fn fake_modem(answers: u32, result: u8, ready: std::sync::mpsc::Sender<()>) -> Seen {
    let socket = PacketSocket::open(INTERFACE).unwrap();
    ready.send(()).unwrap();
    let mut seen = Seen::default();
    let mut buf = [0u8; 2048];
    for _ in 0..answers {
        let Some(n) = socket.recv(&mut buf, Some(Duration::from_secs(5))).unwrap() else { break };
        let frame = &buf[..n];
        if frame[..6] != LOCAL_DEVICE {
            continue;
        }
        let mmtype = u16::from_le_bytes([frame[15], frame[16]]);
        let mut reply = Vec::new();
        reply.extend_from_slice(&frame[6..12]);
        reply.extend_from_slice(&[0x00, 0x01, 0x87, 0x05, 0xF5, 0x06]);
        reply.extend_from_slice(&[0x88, 0xE1]);
        match mmtype {
            modem::VS_SW_VER_REQ => {
                seen.versions += 1;
                reply.extend_from_slice(&[0x00, 0x01, 0xA0, 0x00, 0xB0, 0x52, 0x00, 0x22, 0x24]);
                reply.extend_from_slice(b"MAC-QCA7005-3.3.0.31-00-20231004-CS\0");
            }
            modem::SET_KEY_REQ => {
                seen.keys.push((frame[33..40].try_into().unwrap(), frame[41..57].try_into().unwrap()));
                // Version 1, the type, the fragmentation information, the result, nonces, protocol id.
                reply.extend_from_slice(&[0x01, 0x09, 0x60, 0x00, 0x00, result, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0]);
            }
            _ => continue,
        }
        reply.resize(reply.len().max(60), 0);
        socket.send(&reply).unwrap();
    }
    seen
}

#[test]
fn the_driver_reads_the_version_and_sets_the_key() {
    let Some(mut sb) = sandbox() else { return };
    let (ev, charger) = (sb.node("ev").unwrap(), sb.node("modem").unwrap());
    let (ev, charger) = sb.connect(&ev, &charger).unwrap();
    let (ready, up) = std::sync::mpsc::channel();
    let modem = std::thread::spawn(move || charger.run_in(|| fake_modem(2, 0, ready)).unwrap());
    up.recv().unwrap();
    let seen = ev
        .run_in(|| {
            let mut driver = QcaModem::open(INTERFACE).unwrap().with_settle(Duration::from_millis(100));
            assert_eq!(driver.version().unwrap(), "MAC-QCA7005-3.3.0.31-00-20231004-CS");
            assert!(!driver.link_up().unwrap(), "no key yet");
            let nmk = [0x5A; 16];
            let nid = homeplug::modem::network_id(&nmk);
            driver.set_key(nid, nmk).unwrap();
            assert!(!driver.link_up().unwrap(), "the network needs a moment");
            std::thread::sleep(Duration::from_millis(150));
            assert!(driver.link_up().unwrap());
            (nid, nmk)
        })
        .unwrap();
    let got = modem.join().unwrap();
    assert_eq!(got, Seen { versions: 1, keys: vec![seen] });
}

#[test]
fn a_refused_key_is_an_error() {
    let Some(mut sb) = sandbox() else { return };
    let (ev, charger) = (sb.node("ev").unwrap(), sb.node("modem").unwrap());
    let (ev, charger) = sb.connect(&ev, &charger).unwrap();
    let (ready, up) = std::sync::mpsc::channel();
    let modem = std::thread::spawn(move || charger.run_in(|| fake_modem(1, 1, ready)).unwrap());
    up.recv().unwrap();
    let error = ev
        .run_in(|| {
            let mut driver = QcaModem::open(INTERFACE).unwrap();
            driver.set_key([1; 7], [2; 16]).unwrap_err().to_string()
        })
        .unwrap();
    assert!(error.contains("refused"), "{error}");
    modem.join().unwrap();
}

#[test]
fn no_modem_means_a_timeout_not_a_hang() {
    let Some(mut sb) = sandbox() else { return };
    let (ev, charger) = (sb.node("ev").unwrap(), sb.node("silent").unwrap());
    let (ev, _charger) = sb.connect(&ev, &charger).unwrap();
    let error = ev.run_in(|| QcaModem::open(INTERFACE).unwrap().version().unwrap_err().kind()).unwrap();
    assert_eq!(error, std::io::ErrorKind::TimedOut);
}
