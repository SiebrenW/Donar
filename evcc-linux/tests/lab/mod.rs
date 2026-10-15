//! Two network namespaces joined by a veth pair, for the Linux-only tests. Needs root and the
//! `ip` tool (see `cargo xtask linux-test`).

use std::fs::File;
use std::os::fd::AsRawFd;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

/// The interface name in both namespaces.
pub const IFACE: &str = "veth0";
/// Link-local addresses of the EVCC and SECC ends.
pub const EVCC_ADDR: &str = "fe80::1";
pub const SECC_ADDR: &str = "fe80::2";

pub struct Lab {
    pub evcc: String,
    pub secc: String,
}

fn ip(args: &[&str]) {
    let out = Command::new("ip").args(args).output().expect("`ip` is installed");
    assert!(out.status.success(), "ip {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

impl Lab {
    pub fn new() -> Lab {
        let id = format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
        let lab = Lab { evcc: format!("evcc{id}"), secc: format!("secc{id}") };
        let (a, b) = (format!("a{id}"), format!("b{id}"));
        ip(&["netns", "add", &lab.evcc]);
        ip(&["netns", "add", &lab.secc]);
        ip(&["link", "add", &a, "type", "veth", "peer", "name", &b]);
        for (end, ns, addr) in [(&a, &lab.evcc, EVCC_ADDR), (&b, &lab.secc, SECC_ADDR)] {
            ip(&["link", "set", end, "netns", ns]);
            ip(&["-n", ns, "link", "set", end, "name", IFACE]);
            // No duplicate address detection: the address is usable immediately.
            ip(&["-n", ns, "addr", "add", &format!("{addr}/64"), "dev", IFACE, "nodad"]);
            ip(&["-n", ns, "link", "set", IFACE, "up"]);
        }
        lab
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        // Deleting the namespaces also removes the veth pair.
        for ns in [&self.evcc, &self.secc] {
            let _ = Command::new("ip").args(["netns", "del", ns]).status();
        }
    }
}

/// Runs `f` on a new thread that lives in namespace `ns`; sockets it creates belong there.
pub fn in_ns<T: Send>(ns: &str, f: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|s| {
        s.spawn(|| {
            let file = File::open(format!("/var/run/netns/{ns}")).expect("namespace exists");
            // SAFETY: `file` is an open namespace descriptor; setns only affects this thread.
            let rc = unsafe { libc::setns(file.as_raw_fd(), libc::CLONE_NEWNET) };
            assert_eq!(rc, 0, "setns: {}", std::io::Error::last_os_error());
            f()
        })
        .join()
        .unwrap()
    })
}
