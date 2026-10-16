//! External charger implementations to test the simulated vehicle against.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sandbox::{Node, Process, Sandbox, Spawn};

/// Where the open-source `iso15118` Python library is installed (a virtual environment made
/// by the dev container's Dockerfile); overridable with `ISO15118_PYTHON_VENV`.
fn venv() -> PathBuf {
    std::env::var_os("ISO15118_PYTHON_VENV").map_or_else(|| PathBuf::from("/opt/iso15118"), PathBuf::from)
}

/// The library's test PKI scripts, inside its installation.
fn pki_scripts() -> io::Result<PathBuf> {
    let lib = venv().join("lib");
    for entry in std::fs::read_dir(&lib)? {
        let candidate = entry?.path().join("site-packages/iso15118/shared/pki");
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, format!("no iso15118 installation under {}", lib.display())))
}

/// A started partner and the arguments `evcc-sim` needs to talk to it.
pub struct Partner {
    pub process: Process,
    pub evcc_args: Vec<String>,
}

/// The names `start` knows.
pub const NAMES: &[&str] = &["iso15118-python", "iso15118-python-tls"];

pub fn start(name: &str, sb: &Sandbox, node: &Node) -> io::Result<Partner> {
    match name {
        "iso15118-python" => iso15118_python(sb, node, false),
        "iso15118-python-tls" => iso15118_python(sb, node, true),
        other => Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unknown partner {other}; known: {NAMES:?}"))),
    }
}

/// Runs a helper on the host to completion; its output is in the sandbox's artifacts.
fn run_helper(sb: &Sandbox, spec: Spawn) -> io::Result<()> {
    let mut p = sb.spawn_on_host(spec)?;
    match p.wait_timeout(Duration::from_secs(60))? {
        Some(status) if status.success() => Ok(()),
        other => Err(io::Error::other(format!("helper failed ({other:?}):\n{}", p.log()))),
    }
}

/// The SECC of the `iso15118` library: ISO 15118-20 DC with EIM, plain TCP, or TLS 1.3 with
/// mutual authentication using the library's own test PKI (secp256r1, the ISO 15118-2
/// profile), which is created for the run.
fn iso15118_python(sb: &Sandbox, node: &Node, tls: bool) -> io::Result<Partner> {
    let mut spawn = Spawn::new(venv().join("bin/python"))
        .args(["-m", "iso15118.secc.main"])
        .env("NETWORK_INTERFACE", sandbox::INTERFACE)
        .env("PROTOCOLS", "ISO_15118_20_DC")
        .env("AUTH_MODES", "EIM")
        .env("LOG_LEVEL", "DEBUG")
        .current_dir(sb.artifacts())
        .label("secc");
    let mut evcc_args: Vec<String> = ["--connector", "extended"].map(String::from).to_vec();
    if tls {
        // The library reads its PKI from the `iso15118_2` directory whatever the version.
        let pki = sb.artifacts().join("pki");
        run_helper(sb, Spawn::new("cp").arg("-r").arg(pki_scripts()?.join(".")).arg(&pki).label("pki-copy"))?;
        run_helper(sb, Spawn::new("bash").args(["create_certs.sh", "-v", "iso-2", "-p", "pass123"]).current_dir(&pki).label("pki"))?;
        let certs = pki.join("iso15118_2");
        let file = |dir: &str, name: &str| certs.join(dir).join(name).to_string_lossy().into_owned();
        spawn = spawn.env("SECC_ENFORCE_TLS", "true").env("ENABLE_TLS_1_3", "true").env("PKI_PATH", format!("{}/", pki.display()));
        evcc_args.extend([
            "--chain".into(), file("certs", "oemCertChain.pem"),
            "--key".into(), file("private_keys", "oemLeaf.key"),
            "--key-password".into(), "pass123".into(),
            "--trust".into(), file("certs", "v2gRootCACert.pem"),
            "--backwards-compatible".into(),
        ]);
    } else {
        spawn = spawn.env("SECC_ENFORCE_TLS", "false");
        evcc_args.insert(0, "--no-tls".into());
    }
    let mut process = sb.spawn(node, spawn)?;
    // The partner starts its EXI codec (a Java process) before it listens.
    if !process.wait_for_log("UDP server socket ready", Duration::from_secs(40))? {
        return Err(io::Error::other(format!("the partner did not start in time:\n{}", process.log())));
    }
    Ok(Partner { process, evcc_args })
}

/// Path of a binary built next to the running one (cargo puts all of a workspace's binaries
/// in one directory).
pub fn sibling_binary(name: &str) -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let path: &Path = exe.parent().ok_or_else(|| io::Error::other("no directory"))?;
    let candidate = path.join(name);
    if candidate.exists() {
        Ok(candidate)
    } else {
        Err(io::Error::new(io::ErrorKind::NotFound, format!("{} not built (cargo build -p evcc-sim)", candidate.display())))
    }
}
