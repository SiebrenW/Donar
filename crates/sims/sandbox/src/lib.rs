//! The environment scenarios run in: network namespaces ("nodes") joined by virtual Ethernet
//! cables, with the participants (simulators, external charger implementations, helper tools)
//! running as processes inside them.
//!
//! Linux only, and root only: creating namespaces and veth pairs needs privileges. Everything
//! else does not: participants are started through `ip netns exec` and `setpriv` and run as
//! the **invoking user**, so that nothing they write is owned by root. The invoking user is
//! taken from `SUDO_UID`/`SUDO_GID` (the test runner of the dev container uses `sudo -E`) or
//! given in [`Options`]. The logs of the participants and everything else a run leaves behind
//! go to a directory ([`Sandbox::artifacts`]) that is created for, and owned by, that user.
//!
//! The API hides how the isolation is done (`ip` commands, namespaces by name). A rootless
//! implementation with user namespaces could replace it without changing its users.
//!
//! ```no_run
//! # #[cfg(not(target_os = "linux"))]
//! # fn main() {}
//! # #[cfg(target_os = "linux")]
//! # fn main() -> std::io::Result<()> {
//! use sandbox::{Options, Sandbox, Spawn};
//! let mut sb = Sandbox::new(Options::default())?;
//! let (car, charger) = (sb.node("car")?, sb.node("charger")?);
//! sb.connect(&car, &charger)?; // veth0 in both, fe80::1 and fe80::2
//! let mut server = sb.spawn(&charger, Spawn::new("my-server"))?;
//! server.wait_for_log("listening", std::time::Duration::from_secs(5))?;
//! # Ok(()) }
//! ```
#![cfg(target_os = "linux")]

mod ns;
mod process;

use std::io::{self, ErrorKind};
use std::net::Ipv6Addr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

pub use process::{Process, Spawn};

/// The interface name of the cable in every node.
pub const INTERFACE: &str = "veth0";

/// A user and group, by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct User {
    pub uid: u32,
    pub gid: u32,
}

impl User {
    /// The user that started us through `sudo`, from `SUDO_UID` and `SUDO_GID`.
    pub fn from_sudo() -> Option<User> {
        let get = |name| std::env::var(name).ok()?.parse::<u32>().ok();
        Some(User { uid: get("SUDO_UID")?, gid: get("SUDO_GID")? })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Who the participants run as and who owns the artifacts. Default: the `sudo` user.
    pub user: Option<User>,
    /// Where the artifacts directory of this run is created: `<root>/<run id>`. Default:
    /// `$SANDBOX_ARTIFACTS`, else `$CARGO_TARGET_DIR/sandbox`, else `target/sandbox`.
    pub artifacts_root: Option<PathBuf>,
    /// Allow participants to run as root when there is no user to drop to. Off by default:
    /// a root process would leave root-owned files behind.
    pub allow_root: bool,
}

static NEXT_SANDBOX: AtomicU32 = AtomicU32::new(0);
/// Interface names are visible in the host namespace while a cable is being set up, so they
/// must differ between sandboxes of one process too.
static NEXT_CABLE: AtomicU32 = AtomicU32::new(0);

/// A set of nodes. Dropping it removes the namespaces (and with them the cables).
pub struct Sandbox {
    id: String,
    user: Option<User>,
    artifacts: PathBuf,
    nodes: u32,
}

/// A network namespace. Cheap to clone; it is only a handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    name: String,
    namespace: String,
    address: Option<Ipv6Addr>,
}

impl Node {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The link-local address of the cable's end in this node, once connected.
    pub fn address(&self) -> Option<Ipv6Addr> {
        self.address
    }

    /// Runs `f` on a new thread that lives in this node: sockets it creates belong here. The
    /// thread runs as root (entering a namespace needs it), so use this for quick in-process
    /// tests and not for anything that writes files.
    pub fn run_in<T: Send>(&self, f: impl FnOnce() -> T + Send) -> io::Result<T> {
        ns::run_in(&self.namespace, f)
    }
}

/// Network impairments of a node's cable (applied to what it sends).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Impairment {
    pub delay_ms: Option<u32>,
    pub loss_percent: Option<f32>,
}

fn ip(args: &[&str]) -> io::Result<()> {
    let out = Command::new("ip").args(args).output().map_err(|e| io::Error::new(e.kind(), format!("cannot run `ip`: {e}")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("ip {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim())))
    }
}

impl Sandbox {
    pub fn new(options: Options) -> io::Result<Sandbox> {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc::geteuid() } != 0 {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                "the sandbox needs root to create namespaces (run through sudo, e.g. `cargo xtask linux-test`)",
            ));
        }
        let user = options.user.or_else(User::from_sudo);
        if user.is_none() && !options.allow_root {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "no user to run participants as: set Options::user, run through sudo (SUDO_UID) or allow root explicitly",
            ));
        }
        let id = format!("{}-{}", std::process::id(), NEXT_SANDBOX.fetch_add(1, Ordering::Relaxed));
        let root = options.artifacts_root.unwrap_or_else(default_artifacts_root);
        let artifacts = root.join(&id);
        ns::create_dir_all_owned(&artifacts, user)?;
        Ok(Sandbox { id, user, artifacts, nodes: 0 })
    }

    /// The directory for logs and other files of this run; owned by the participants' user.
    pub fn artifacts(&self) -> &Path {
        &self.artifacts
    }

    /// Writes a file into the artifacts directory (`name` may have subdirectories) owned by the
    /// participants' user, and returns its path. For configuration and credentials that the
    /// participants read.
    pub fn write_artifact(&self, name: &str, contents: &[u8]) -> io::Result<PathBuf> {
        let path = self.artifacts.join(name);
        if let Some(dir) = path.parent() {
            ns::create_dir_all_owned(dir, self.user)?;
        }
        std::fs::write(&path, contents)?;
        ns::chown(&path, self.user)?;
        Ok(path)
    }

    /// Creates a node (a network namespace with its loopback up).
    pub fn node(&mut self, name: &str) -> io::Result<Node> {
        self.nodes += 1;
        let namespace = format!("sb{}-{}-{name}", self.id, self.nodes);
        ip(&["netns", "add", &namespace])?;
        let node = Node { name: name.to_string(), namespace, address: None };
        ip(&["-n", &node.namespace, "link", "set", "lo", "up"])?;
        Ok(node)
    }

    /// Joins two nodes with a virtual cable: [`INTERFACE`] in each, link-local addresses
    /// `fe80::1` (first node) and `fe80::2`, and no duplicate address detection, so the
    /// addresses (also the automatic ones) are usable at once. Returns once both ends have a carrier and their routes are up. Returns the two nodes with
    /// their addresses set; the originals stay valid handles.
    pub fn connect(&mut self, a: &Node, b: &Node) -> io::Result<(Node, Node)> {
        // Interface names are at most 15 characters.
        let n = NEXT_CABLE.fetch_add(1, Ordering::Relaxed);
        let (end_a, end_b) = (format!("c{}a{n}", std::process::id() % 100_000), format!("c{}b{n}", std::process::id() % 100_000));
        ip(&["link", "add", &end_a, "type", "veth", "peer", "name", &end_b])?;
        let a_address: Ipv6Addr = "fe80::1".parse().unwrap();
        let b_address: Ipv6Addr = "fe80::2".parse().unwrap();
        for (end, node, address) in [(&end_a, a, a_address), (&end_b, b, b_address)] {
            ip(&["link", "set", end, "netns", &node.namespace])?;
            ip(&["-n", &node.namespace, "link", "set", end, "name", INTERFACE])?;
            ns::sysctl(&node.namespace, &format!("net.ipv6.conf.{INTERFACE}.accept_dad=0"))?;
            ip(&["-n", &node.namespace, "addr", "add", &format!("{address}/64"), "dev", INTERFACE, "nodad"])?;
            ip(&["-n", &node.namespace, "link", "set", INTERFACE, "up"])?;
        }
        ns::wait_link_ready(&a.namespace)?;
        ns::wait_link_ready(&b.namespace)?;
        Ok((Node { address: Some(a_address), ..a.clone() }, Node { address: Some(b_address), ..b.clone() }))
    }

    /// Takes the cable's end in `node` down or up (the other side sees the link go away).
    pub fn set_link(&self, node: &Node, up: bool) -> io::Result<()> {
        ip(&["-n", &node.namespace, "link", "set", INTERFACE, if up { "up" } else { "down" }])
    }

    /// Applies delay and loss to what `node` sends (`tc netem`); the default value removes them.
    pub fn impair(&self, node: &Node, impairment: Impairment) -> io::Result<()> {
        ns::netem(&node.namespace, impairment)
    }

    /// Starts a participant in `node` as the sandbox's user. Its output goes to a log file in
    /// the artifacts directory.
    pub fn spawn(&self, node: &Node, spec: Spawn) -> io::Result<Process> {
        Process::start(Some(node), &self.artifacts, self.user, spec)
    }

    /// Starts a helper outside any node (for example a tool that prepares files), as the
    /// sandbox's user, with the same log handling.
    pub fn spawn_on_host(&self, spec: Spawn) -> io::Result<Process> {
        Process::start(None, &self.artifacts, self.user, spec)
    }

    /// Names of the namespaces that exist for this sandbox (for tests and diagnostics).
    pub fn namespaces(&self) -> io::Result<Vec<String>> {
        ns::list(&format!("sb{}-", self.id))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Removing a namespace removes the veth ends inside it, and with them the cable.
        if let Ok(names) = ns::list(&format!("sb{}-", self.id)) {
            for name in names {
                let _ = ip(&["netns", "del", &name]);
            }
        }
    }
}

fn default_artifacts_root() -> PathBuf {
    if let Some(p) = std::env::var_os("SANDBOX_ARTIFACTS") {
        return PathBuf::from(p);
    }
    let target = std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| PathBuf::from("target"), PathBuf::from);
    target.join("sandbox")
}
