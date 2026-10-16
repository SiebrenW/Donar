//! Namespace plumbing: entering, listing, files owned by the right user.

use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Command;

use crate::{Impairment, User};

/// Runs `f` on a scoped thread that has entered the named namespace.
pub(crate) fn run_in<T: Send>(namespace: &str, f: impl FnOnce() -> T + Send) -> io::Result<T> {
    let file = File::open(format!("/var/run/netns/{namespace}"))?;
    std::thread::scope(|s| {
        s.spawn(|| {
            // SAFETY: `file` is an open namespace descriptor; setns only affects this thread.
            if unsafe { libc::setns(file.as_raw_fd(), libc::CLONE_NEWNET) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(f())
        })
        .join()
        .map_err(|_| io::Error::other("a thread in the namespace panicked"))?
    })
}

/// Names of the namespaces that start with `prefix`.
pub(crate) fn list(prefix: &str) -> io::Result<Vec<String>> {
    match std::fs::read_dir("/var/run/netns") {
        Ok(dir) => Ok(dir
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter(|n| n.starts_with(prefix))
            .collect()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Runs a command inside the namespace (as root: the callers are setup steps).
fn exec_in(namespace: &str, program: &str, args: &[&str]) -> io::Result<()> {
    let out = Command::new("ip").args(["netns", "exec", namespace, program]).args(args).output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{program} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim())))
    }
}

pub(crate) fn sysctl(namespace: &str, assignment: &str) -> io::Result<()> {
    exec_in(namespace, "sysctl", &["-q", "-w", assignment])
}

pub(crate) fn netem(namespace: &str, i: Impairment) -> io::Result<()> {
    let dev = crate::INTERFACE;
    if i == Impairment::default() {
        // Nothing to remove is not an error.
        let _ = exec_in(namespace, "tc", &["qdisc", "del", "dev", dev, "root"]);
        return Ok(());
    }
    let mut args = vec!["qdisc".to_string(), "replace".into(), "dev".into(), dev.into(), "root".into(), "netem".into()];
    if let Some(ms) = i.delay_ms {
        args.extend(["delay".to_string(), format!("{ms}ms")]);
    }
    if let Some(p) = i.loss_percent {
        args.extend(["loss".to_string(), format!("{p}%")]);
    }
    exec_in(namespace, "tc", &args.iter().map(String::as_str).collect::<Vec<_>>())
}

/// Output of a command run inside the namespace.
fn output_in(namespace: &str, program: &str, args: &[&str]) -> io::Result<String> {
    let out = Command::new("ip").args(["netns", "exec", namespace, program]).args(args).output()?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Waits until the cable's end in the namespace can be used: the link has a carrier (the
/// other end is up too) and the IPv6 routes of the interface are no longer marked down. Both
/// follow `ip link set up` asynchronously; without the wait the first datagram can fail with
/// "network unreachable".
pub(crate) fn wait_link_ready(namespace: &str) -> io::Result<()> {
    let dev = crate::INTERFACE;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let link = output_in(namespace, "ip", &["link", "show", dev])?;
        let routes = output_in(namespace, "ip", &["-6", "route", "show", "dev", dev])?;
        if link.contains("LOWER_UP") && routes.contains("fe80::/64") && !routes.contains("linkdown") {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(ErrorKind::TimedOut, format!("the link in {namespace} did not come up: {link} {routes}")));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Creates `path` and the missing directories above it, each owned by `user` (when given): a
/// plain `create_dir_all` as root would leave root-owned directories behind.
pub(crate) fn create_dir_all_owned(path: &Path, user: Option<User>) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut at = Some(path);
    while let Some(p) = at {
        if p.exists() || p.as_os_str().is_empty() {
            break;
        }
        missing.push(p);
        at = p.parent();
    }
    for dir in missing.into_iter().rev() {
        std::fs::create_dir(dir)?;
        chown(dir, user)?;
    }
    Ok(())
}

pub(crate) fn chown(path: &Path, user: Option<User>) -> io::Result<()> {
    match user {
        Some(u) => std::os::unix::fs::chown(path, Some(u.uid), Some(u.gid)),
        None => Ok(()),
    }
}
