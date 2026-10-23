//! The QCA modem driver of the EV against a fake modem on the other end of a virtual cable: it
//! answers the version request and the key request the way the real modem does (the version
//! frame is the one captured from a QCA7005).
#![cfg(target_os = "linux")]

use std::time::Duration;

use evcc_linux::slac::{KeyMessage, Modem, PacketSocket, QcaModem};
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
    infos: u32,
    statuses: u32,
    keys: Vec<([u8; 7], [u8; 16])>,
}

/// A modem: answers the first `answers` requests, the key request with `result`; the first `lone_infos`
/// network information requests with a network of its own, the later ones with one it has joined. With
/// `down_statuses` it also answers the link status request (no link for that many, then a link); without it
/// ignores that request.
fn fake_modem(answers: u32, result: u8, lone_infos: u32, down_statuses: Option<u32>, ready: std::sync::mpsc::Sender<()>) -> Seen {
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
            modem::VS_SET_KEY_REQ => {
                // Version 0, the type, the OUI, the status; no network id in the request.
                seen.keys.push(([0; 7], frame[21..37].try_into().unwrap()));
                reply.extend_from_slice(&[0x00, 0x51, 0xA0, 0x00, 0xB0, 0x52, result]);
            }
            modem::VS_PL_LNK_STATUS_REQ => {
                seen.statuses += 1;
                let Some(down) = down_statuses else { continue };
                reply.extend_from_slice(&[0x00, 0xB9, 0xA0, 0x00, 0xB0, 0x52, 0x00, u8::from(seen.statuses > down)]);
            }
            modem::VS_NW_INFO_REQ => {
                seen.infos += 1;
                // The network of the key given (the test's), led by the modem itself or by a charger.
                let cco: [u8; 6] = if seen.infos <= lone_infos { [0x00, 0x01, 0x87, 0x05, 0xF5, 0x06] } else { [0x00, 0x01, 0x87, 0x05, 0xF5, 0x77] };
                reply.extend_from_slice(&[0x01, 0x39, 0xA0, 0x00, 0x00, 0x00, 0xB0, 0x52, 0x00, 0x00, 0x22, 0x00, 0x00, 0x01]);
                reply.extend_from_slice(&seen.keys.last().map_or([0; 7], |k| k.0));
                reply.extend_from_slice(&[0, 0, 0x0E, 0x01, 0, 0, 0, 0, 0x02]);
                reply.extend_from_slice(&cco);
                reply.push(0x01);
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
    let modem = std::thread::spawn(move || charger.run_in(|| fake_modem(5, homeplug::modem::SET_KEY_DONE, 1, None, ready)).unwrap());
    up.recv().unwrap();
    let seen = ev
        .run_in(|| {
            let mut driver = QcaModem::open(INTERFACE).unwrap().with_settle(Duration::from_millis(100));
            assert_eq!(driver.version().unwrap(), "MAC-QCA7005-3.3.0.31-00-20231004-CS");
            assert!(!driver.link_up().unwrap(), "no key yet");
            let nmk = [0x5A; 16];
            let nid = homeplug::modem::network_id(&nmk);
            driver.set_key(nid, nmk).unwrap();
            // The modem is its own coordinator first, then it has joined the charger's network.
            assert!(!driver.link_up().unwrap(), "the network needs a moment");
            assert!(driver.link_up().unwrap());
            (nid, nmk)
        })
        .unwrap();
    let got = modem.join().unwrap();
    // The link status request goes unanswered once (a modem that does not know it); then the network information it is.
    assert_eq!(got, Seen { versions: 1, infos: 2, statuses: 1, keys: vec![seen] });
}

#[test]
fn a_refused_key_is_an_error() {
    let Some(mut sb) = sandbox() else { return };
    let (ev, charger) = (sb.node("ev").unwrap(), sb.node("modem").unwrap());
    let (ev, charger) = sb.connect(&ev, &charger).unwrap();
    let (ready, up) = std::sync::mpsc::channel();
    let modem = std::thread::spawn(move || charger.run_in(|| fake_modem(1, 0, 0, None, ready)).unwrap());
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

#[test]
fn a_modem_without_the_network_message_falls_back_to_the_settle_time() {
    let Some(mut sb) = sandbox() else { return };
    let (ev, charger) = (sb.node("ev").unwrap(), sb.node("modem").unwrap());
    let (ev, charger) = sb.connect(&ev, &charger).unwrap();
    let (ready, up) = std::sync::mpsc::channel();
    // It answers the key request only; the network information request goes unanswered.
    let modem = std::thread::spawn(move || charger.run_in(|| fake_modem(1, homeplug::modem::SET_KEY_DONE, 0, None, ready)).unwrap());
    up.recv().unwrap();
    ev.run_in(|| {
        let mut driver = QcaModem::open(INTERFACE).unwrap().with_settle(Duration::from_millis(500)).with_reply_timeout(Duration::from_millis(50));
        driver.set_key([1; 7], [2; 16]).unwrap();
        assert!(!driver.link_up().unwrap(), "inside the settle time");
        std::thread::sleep(Duration::from_millis(500));
        assert!(driver.link_up().unwrap());
    })
    .unwrap();
    modem.join().unwrap();
}

#[test]
fn the_link_is_read_from_the_link_status_when_the_modem_has_it() {
    let Some(mut sb) = sandbox() else { return };
    let (ev, charger) = (sb.node("ev").unwrap(), sb.node("modem").unwrap());
    let (ev, charger) = sb.connect(&ev, &charger).unwrap();
    let (ready, up) = std::sync::mpsc::channel();
    let modem = std::thread::spawn(move || charger.run_in(|| fake_modem(3, homeplug::modem::SET_KEY_DONE, 0, Some(1), ready)).unwrap());
    up.recv().unwrap();
    ev.run_in(|| {
        let mut driver = QcaModem::open(INTERFACE).unwrap();
        driver.set_key([1; 7], [2; 16]).unwrap();
        assert!(!driver.link_up().unwrap(), "no link yet");
        assert!(driver.link_up().unwrap());
    })
    .unwrap();
    let got = modem.join().unwrap();
    assert_eq!((got.statuses, got.infos), (2, 0), "the network information is not asked when the link status is answered");
}

#[test]
fn the_vendor_key_message_gives_the_key_without_a_network_id() {
    let Some(mut sb) = sandbox() else { return };
    let (ev, charger) = (sb.node("ev").unwrap(), sb.node("modem").unwrap());
    let (ev, charger) = sb.connect(&ev, &charger).unwrap();
    let (ready, up) = std::sync::mpsc::channel();
    let modem = std::thread::spawn(move || charger.run_in(|| fake_modem(1, 0, 0, None, ready)).unwrap());
    up.recv().unwrap();
    ev.run_in(|| {
        let mut driver = QcaModem::open(INTERFACE).unwrap().with_key_message(KeyMessage::Vendor);
        // The vendor message reports success as status 0.
        driver.set_key([1; 7], [0x77; 16]).unwrap();
    })
    .unwrap();
    let got = modem.join().unwrap();
    assert_eq!(got.keys, vec![([0; 7], [0x77; 16])]);
}

#[test]
fn two_keys_are_given_at_a_distance() {
    let Some(mut sb) = sandbox() else { return };
    let (ev, charger) = (sb.node("ev").unwrap(), sb.node("modem").unwrap());
    let (ev, charger) = sb.connect(&ev, &charger).unwrap();
    let (ready, up) = std::sync::mpsc::channel();
    let modem = std::thread::spawn(move || charger.run_in(|| fake_modem(2, homeplug::modem::SET_KEY_DONE, 0, None, ready)).unwrap());
    up.recv().unwrap();
    let spacing = Duration::from_millis(400);
    let (first_took, second_took) = ev
        .run_in(|| {
            let mut driver = QcaModem::open(INTERFACE).unwrap().with_key_spacing(spacing);
            let t = std::time::Instant::now();
            driver.set_key([1; 7], [2; 16]).unwrap();
            let first = t.elapsed();
            let t = std::time::Instant::now();
            driver.set_key([3; 7], [4; 16]).unwrap();
            (first, t.elapsed())
        })
        .unwrap();
    assert!(first_took < spacing, "the first key does not wait: {first_took:?}");
    assert!(second_took >= spacing - Duration::from_millis(50), "the second waits for the spacing: {second_took:?}");
    assert_eq!(modem.join().unwrap().keys.len(), 2);
}

/// What the scripted modem does: answers the key request, the link status requests with `script` (the last entry for
/// every later one) and sends the indications `(milliseconds after the start, HAR)`.
struct Wire {
    script: Vec<bool>,
    indications: Vec<(u64, u8)>,
    run_for: Duration,
}

/// Runs `wire` and returns how many times the host acknowledged an indication.
fn scripted_modem(wire: Wire, ready: std::sync::mpsc::Sender<()>) -> u32 {
    let socket = PacketSocket::open(INTERFACE).unwrap();
    ready.send(()).unwrap();
    let start = std::time::Instant::now();
    let (mut statuses, mut acks, mut sent) = (0, 0, 0);
    let mut buf = [0u8; 2048];
    while start.elapsed() < wire.run_for {
        while let Some(&(at, har)) = wire.indications.get(sent) {
            if start.elapsed() < Duration::from_millis(at) {
                break;
            }
            // Version 0.1: the action, the version, session 7, one retry outstanding, the retry timer.
            let mut ind = vec![0xFF; 6];
            ind.extend_from_slice(&[0x00, 0x01, 0x87, 0x05, 0xF5, 0x06, 0x88, 0xE1, 0x00, 0x62, 0xA0, 0x00, 0xB0, 0x52, har, 0x00, 0x01, 7, 1, 0, 0x64, 0x00]);
            ind.resize(60, 0);
            socket.send(&ind).unwrap();
            sent += 1;
        }
        let Some(n) = socket.recv(&mut buf, Some(Duration::from_millis(10))).unwrap() else { continue };
        let frame = &buf[..n];
        if frame.len() < 17 {
            continue;
        }
        let mut reply = Vec::new();
        reply.extend_from_slice(&frame[6..12]);
        reply.extend_from_slice(&[0x00, 0x01, 0x87, 0x05, 0xF5, 0x06, 0x88, 0xE1]);
        match u16::from_le_bytes([frame[15], frame[16]]) {
            modem::SET_KEY_REQ => reply.extend_from_slice(&[0x01, 0x09, 0x60, 0x00, 0x00, 1, 0, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0]),
            modem::VS_PL_LNK_STATUS_REQ => {
                let link = wire.script[statuses.min(wire.script.len() - 1)];
                statuses += 1;
                reply.extend_from_slice(&[0x00, 0xB9, 0xA0, 0x00, 0xB0, 0x52, 0x00, u8::from(link)]);
            }
            modem::VS_HST_ACTION_RSP => {
                // Version 0.1: OUI, status 0, version, then what the indication carried.
                assert_eq!(&frame[17..27], &[0x00, 0xB0, 0x52, 0, 0, 1, frame[23], 7, 1, 0], "the answer repeats the indication");
                acks += 1;
                continue;
            }
            _ => continue,
        }
        reply.resize(60, 0);
        socket.send(&reply).unwrap();
    }
    acks
}

/// The driver with a link that is up, watched; runs `check` on it.
fn watched<T: Send + 'static>(wire: Wire, link_poll: Duration, check: impl FnOnce(&mut QcaModem) -> T + Send + 'static) -> Option<(T, u32)> {
    let mut sb = sandbox()?;
    let (ev, charger) = (sb.node("ev").unwrap(), sb.node("modem").unwrap());
    let (ev, charger) = sb.connect(&ev, &charger).unwrap();
    let (ready, up) = std::sync::mpsc::channel();
    let modem = std::thread::spawn(move || charger.run_in(|| scripted_modem(wire, ready)).unwrap());
    up.recv().unwrap();
    let result = ev
        .run_in(move || {
            let mut driver = QcaModem::open(INTERFACE).unwrap().with_link_poll(link_poll).with_link_loss_debounce(Duration::from_millis(300));
            driver.set_key([1; 7], [2; 16]).unwrap();
            assert!(driver.link_up().unwrap());
            check(&mut driver)
        })
        .unwrap();
    Some((result, modem.join().unwrap()))
}

/// Calls `link_lost` every 20 ms for up to `within`; how long it took until it said so.
fn until_lost(driver: &mut QcaModem, within: Duration) -> Option<Duration> {
    let start = std::time::Instant::now();
    while start.elapsed() < within {
        if driver.link_lost().unwrap() {
            return Some(start.elapsed());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

#[test]
fn a_link_indication_of_the_modem_is_acknowledged() {
    let wire = Wire { script: vec![true], indications: vec![(50, homeplug::modem::HAR_LINK_ONLINE)], run_for: Duration::from_millis(800) };
    let Some((lost, acks)) = watched(wire, Duration::from_millis(50), |driver| {
        std::thread::sleep(Duration::from_millis(200));
        driver.link_lost().unwrap()
    }) else {
        return;
    };
    assert!(!lost);
    assert_eq!(acks, 1);
}

#[test]
fn a_link_that_stays_down_is_lost_after_the_debounce() {
    let wire = Wire { script: vec![true, true, false], indications: vec![], run_for: Duration::from_millis(2000) };
    let Some((took, _)) = watched(wire, Duration::from_millis(50), |driver| until_lost(driver, Duration::from_millis(1500))) else { return };
    let took = took.expect("the lost link is reported");
    // The first status without a link is polled 100 ms in; the debounce runs from there.
    assert!(took >= Duration::from_millis(300), "{took:?}");
}

#[test]
fn a_link_that_comes_back_within_the_debounce_is_not_lost() {
    let wire = Wire { script: vec![true, true, false, true], indications: vec![], run_for: Duration::from_millis(1500) };
    let Some((took, _)) = watched(wire, Duration::from_millis(50), |driver| until_lost(driver, Duration::from_millis(900))) else { return };
    assert_eq!(took, None);
}

#[test]
fn an_offline_indication_starts_the_debounce_and_an_online_one_ends_it() {
    use homeplug::modem::{HAR_LINK_OFFLINE, HAR_LINK_ONLINE};
    // The status (up) is not asked in this time; only the indications speak.
    let (rarely, within) = (Duration::from_secs(30), Duration::from_millis(900));
    let wire = Wire { script: vec![true], indications: vec![(50, HAR_LINK_OFFLINE)], run_for: Duration::from_millis(1200) };
    let Some((took, acks)) = watched(wire, rarely, move |driver| until_lost(driver, within)) else { return };
    assert!(took.expect("the lost link is reported") >= Duration::from_millis(250));
    assert_eq!(acks, 1);
    let wire = Wire { script: vec![true], indications: vec![(50, HAR_LINK_OFFLINE), (150, HAR_LINK_ONLINE)], run_for: Duration::from_millis(1200) };
    let Some((took, acks)) = watched(wire, rarely, move |driver| until_lost(driver, within)) else { return };
    assert_eq!(took, None);
    assert_eq!(acks, 2);
}
