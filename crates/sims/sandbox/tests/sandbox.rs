//! Needs root (`cargo xtask linux-test` runs the tests through sudo) and the `ip`, `tc` and
//! `setpriv` tools. The participants must run as the invoking user and leave nothing owned
//! by root behind.
#![cfg(target_os = "linux")]

use std::net::{Ipv6Addr, SocketAddrV6, UdpSocket};
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

use sandbox::{Impairment, Options, Sandbox, Spawn, User};

/// The user to run as: the one that called sudo. Without one (real root without sudo) the
/// test cannot say anything about ownership and is skipped.
fn user() -> Option<User> {
    let user = User::from_sudo();
    if user.is_none() {
        eprintln!("SUDO_UID not set: skipping");
    }
    user
}

fn sandbox() -> Option<Sandbox> {
    user()?;
    Some(Sandbox::new(Options::default()).expect("sandbox"))
}

#[test]
fn artifacts_and_logs_belong_to_the_user_and_participants_run_as_that_user() {
    let Some(user) = user() else { return };
    // A nested artifacts root that does not exist yet: every directory made must be the user's.
    let root = std::env::temp_dir().join(format!("sandbox-test-{}/a/b", std::process::id()));
    let sb = Sandbox::new(Options { artifacts_root: Some(root.clone()), ..Options::default() }).unwrap();
    let mut p = sb.spawn_on_host(Spawn::new("id").arg("-u").label("id")).unwrap();
    assert!(p.wait_timeout(Duration::from_secs(5)).unwrap().unwrap().success());
    assert_eq!(p.log().trim(), user.uid.to_string());
    for path in [sb.artifacts(), p.log_path(), root.as_path(), root.parent().unwrap()] {
        assert_eq!(std::fs::metadata(path).unwrap().uid(), user.uid, "{path:?}");
    }
    // The participant can write next to its log, as the user.
    let mut q = sb.spawn_on_host(Spawn::new("sh").args(["-c", "touch made-by-participant"]).current_dir(sb.artifacts())).unwrap();
    assert!(q.wait_timeout(Duration::from_secs(5)).unwrap().unwrap().success());
    assert_eq!(std::fs::metadata(sb.artifacts().join("made-by-participant")).unwrap().uid(), user.uid);
    // Files written for the participants are the user's too, also in new subdirectories.
    let written = sb.write_artifact("pki/x/key.pem", b"k").unwrap();
    assert_eq!(std::fs::read(&written).unwrap(), b"k");
    for path in [written.as_path(), written.parent().unwrap(), written.parent().unwrap().parent().unwrap()] {
        assert_eq!(std::fs::metadata(path).unwrap().uid(), user.uid, "{path:?}");
    }
    let top = root.parent().unwrap().parent().unwrap().to_path_buf();
    drop((p, q, sb));
    std::fs::remove_dir_all(top).unwrap();
}

#[test]
fn nodes_are_joined_by_a_cable_with_usable_addresses() {
    let Some(mut sb) = sandbox() else { return };
    let (a, b) = (sb.node("car").unwrap(), sb.node("charger").unwrap());
    let (a, b) = sb.connect(&a, &b).unwrap();
    assert_eq!(a.address(), Some("fe80::1".parse().unwrap()));
    assert_eq!(b.address(), Some("fe80::2".parse().unwrap()));
    let mut show = sb.spawn(&b, Spawn::new("ip").args(["-6", "addr", "show", "dev", sandbox::INTERFACE]).label("addr")).unwrap();
    assert!(show.wait_timeout(Duration::from_secs(5)).unwrap().unwrap().success());
    let log = show.log();
    assert!(log.contains("fe80::2/64"), "{log}");
    assert!(!log.contains("tentative"), "{log}");
}

#[test]
fn processes_can_be_waited_for_by_their_log_and_killed_with_what_they_started() {
    let Some(sb) = sandbox() else { return };
    let mut p = sb.spawn_on_host(Spawn::new("sh").args(["-c", "echo ready; sleep 30 & sleep 30"]).label("sleeper")).unwrap();
    assert!(p.wait_for_log("ready", Duration::from_secs(5)).unwrap());
    assert!(!p.wait_for_log("never written", Duration::from_millis(100)).unwrap());
    assert!(p.wait_timeout(Duration::from_millis(100)).unwrap().is_none());
    p.kill();
    assert!(p.try_wait().unwrap().is_some());
    // A process that ends without writing what we wait for is reported, not waited out.
    let mut q = sb.spawn_on_host(Spawn::new("true").label("short")).unwrap();
    assert!(q.wait_for_log("x", Duration::from_secs(5)).is_err());
}

#[test]
fn dropping_the_sandbox_removes_its_namespaces() {
    let Some(mut sb) = sandbox() else { return };
    let (a, b) = (sb.node("a").unwrap(), sb.node("b").unwrap());
    sb.connect(&a, &b).unwrap();
    let names = sb.namespaces().unwrap();
    assert_eq!(names.len(), 2, "{names:?}");
    let prefix = names[0].rsplitn(2, '-').last().unwrap().to_string();
    drop(sb);
    let left: Vec<_> = std::fs::read_dir("/var/run/netns").map(|d| d.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect()).unwrap_or_default();
    assert!(left.iter().all(|n| !n.starts_with(&prefix)), "{left:?}");
}

#[test]
fn the_link_can_be_taken_down() {
    let Some(mut sb) = sandbox() else { return };
    let (a, b) = (sb.node("a").unwrap(), sb.node("b").unwrap());
    sb.connect(&a, &b).unwrap();
    sb.set_link(&a, false).unwrap();
    let mut show = sb.spawn(&b, Spawn::new("ip").args(["-br", "link", "show", sandbox::INTERFACE]).label("link")).unwrap();
    show.wait_timeout(Duration::from_secs(5)).unwrap();
    let state = show.log();
    assert!(!state.contains(" UP "), "the other end still reports UP: {state}");
    sb.set_link(&a, true).unwrap();
}

/// Round trip time of one UDP datagram from node `a` to an echo in node `b`.
fn round_trip(a: &sandbox::Node, b: &sandbox::Node) -> Duration {
    let index = |name: &str| {
        let c = std::ffi::CString::new(name).unwrap();
        unsafe { libc_if_nametoindex(c.as_ptr()) }
    };
    std::thread::scope(|s| {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let echo = s.spawn(move || {
            b.run_in(|| {
                let socket = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).unwrap();
                ready_tx.send(socket.local_addr().unwrap().port()).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut buf = [0u8; 16];
                let (n, from) = socket.recv_from(&mut buf).unwrap();
                socket.send_to(&buf[..n], from).unwrap();
            })
            .unwrap()
        });
        let port = ready_rx.recv().unwrap();
        let took = a
            .run_in(|| {
                let socket = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, 0)).unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let to = SocketAddrV6::new(b.address().unwrap(), port, 0, index(sandbox::INTERFACE));
                let start = Instant::now();
                socket.send_to(b"ping", to).unwrap();
                socket.recv(&mut [0u8; 16]).unwrap();
                start.elapsed()
            })
            .unwrap();
        echo.join().unwrap();
        took
    })
}

unsafe extern "C" {
    #[link_name = "if_nametoindex"]
    fn libc_if_nametoindex(name: *const std::ffi::c_char) -> u32;
}

#[test]
fn delay_can_be_added_to_and_removed_from_a_cable() {
    let Some(mut sb) = sandbox() else { return };
    let (a, b) = (sb.node("a").unwrap(), sb.node("b").unwrap());
    let (a, b) = sb.connect(&a, &b).unwrap();
    // The best of a few: the first datagram also pays for neighbour discovery, and other tests
    // run in parallel.
    let fast = || (0..3).map(|_| round_trip(&a, &b)).min().unwrap();
    assert!(fast() < Duration::from_millis(40));
    // 60 ms on what a sends and 60 ms on what b sends (the echo).
    sb.impair(&a, Impairment { delay_ms: Some(60), loss_percent: None }).unwrap();
    sb.impair(&b, Impairment { delay_ms: Some(60), loss_percent: None }).unwrap();
    let slow = round_trip(&a, &b);
    assert!(slow >= Duration::from_millis(110), "{slow:?}");
    sb.impair(&a, Impairment::default()).unwrap();
    sb.impair(&b, Impairment::default()).unwrap();
    assert!(fast() < Duration::from_millis(40));
}

#[test]
fn raw_sockets_need_asking_for_and_come_with_the_user_id_kept() {
    let Some(sb) = sandbox() else { return };
    let status = |spawn: Spawn| {
        let mut p = sb.spawn_on_host(spawn.label("caps")).unwrap();
        assert!(p.wait_timeout(Duration::from_secs(5)).unwrap().unwrap().success());
        p.log()
    };
    let show = || Spawn::new("sh").args(["-c", "id -u; grep CapAmb /proc/self/status"]);
    let plain = status(show());
    let raw = status(show().net_raw());
    let uid = user().unwrap().uid.to_string();
    assert!(plain.starts_with(&uid) && plain.contains("CapAmb:\t0000000000000000"), "{plain}");
    // CAP_NET_RAW is capability 13.
    assert!(raw.starts_with(&uid) && raw.contains("CapAmb:\t0000000000002000"), "{raw}");
}
