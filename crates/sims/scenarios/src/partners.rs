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

/// Files the vehicle needs to talk TLS to a partner (PEM).
pub struct VehicleTls {
    pub chain: PathBuf,
    pub key: PathBuf,
    pub key_password: Option<String>,
    pub trust: PathBuf,
}

/// A contract the partner's test PKI makes for the vehicle: the chain (contract certificate, then its
/// sub-CAs; PEM) and the key.
pub struct VehicleContract {
    pub chain: PathBuf,
    pub key: PathBuf,
    pub key_password: Option<String>,
}

/// A started partner. `vehicle_tls` is the vehicle side of its test PKI, when it runs TLS.
pub struct Partner {
    pub process: Process,
    pub vehicle_tls: Option<VehicleTls>,
    pub contract: Option<VehicleContract>,
}

/// The log line that says the partner listens.
pub const READY: &str = "UDP server socket ready";

/// Runs a helper on the host to completion; its output is in the sandbox's artifacts.
fn run_helper(sb: &Sandbox, spec: Spawn) -> io::Result<()> {
    let mut p = sb.spawn_on_host(spec)?;
    match p.wait_timeout(Duration::from_secs(60))? {
        Some(status) if status.success() => Ok(()),
        other => Err(io::Error::other(format!("helper failed ({other:?}):\n{}", p.log()))),
    }
}

/// The SECC of the `iso15118` library: `protocols` (its `PROTOCOLS` setting, `ISO_15118_20_DC` or
/// `DIN_SPEC_70121`) with EIM, plain TCP, or TLS 1.3 with
/// mutual authentication using the library's own test PKI (secp256r1, the ISO 15118-2
/// profile), which is created for the run.
pub fn iso15118_python(sb: &Sandbox, node: &Node, tls: bool, protocols: &str, auth_modes: &str) -> io::Result<Partner> {
    let mut spawn = Spawn::new(venv().join("bin/python"))
        .args(["-m", "iso15118.secc.main"])
        .env("NETWORK_INTERFACE", sandbox::INTERFACE)
        .env("PROTOCOLS", protocols)
        .env("AUTH_MODES", auth_modes)
        .env("LOG_LEVEL", "DEBUG")
        .current_dir(sb.artifacts())
        .label("secc");
    let mut vehicle_tls = None;
    let mut contract = None;
    if tls {
        // The library reads its PKI from the `iso15118_2` directory whatever the version.
        let pki = sb.artifacts().join("pki");
        run_helper(sb, Spawn::new("cp").arg("-r").arg(pki_scripts()?.join(".")).arg(&pki).label("pki-copy"))?;
        run_helper(sb, Spawn::new("bash").args(["create_certs.sh", "-v", "iso-2", "-p", "pass123"]).current_dir(&pki).label("pki"))?;
        let certs = pki.join("iso15118_2");
        spawn = spawn.env("SECC_ENFORCE_TLS", "true").env("ENABLE_TLS_1_3", "true").env("PKI_PATH", format!("{}/", pki.display()));
        // The test PKI also has a contract for the vehicle: the leaf and its two sub-CAs.
        let (certs_dir, keys_dir) = (certs.join("certs"), certs.join("private_keys"));
        let mut chain = Vec::new();
        for name in ["contractLeafCert.pem", "moSubCA2Cert.pem", "moSubCA1Cert.pem"] {
            chain.extend(std::fs::read(certs_dir.join(name))?);
        }
        let chain_path = certs_dir.join("contractChain.pem");
        std::fs::write(&chain_path, chain)?;
        contract = Some(VehicleContract { chain: chain_path, key: keys_dir.join("contractLeaf.key"), key_password: Some("pass123".into()) });
        vehicle_tls = Some(VehicleTls {
            chain: certs.join("certs/oemCertChain.pem"),
            key: certs.join("private_keys/oemLeaf.key"),
            key_password: Some("pass123".into()),
            trust: certs.join("certs/v2gRootCACert.pem"),
        });
    } else {
        spawn = spawn.env("SECC_ENFORCE_TLS", "false");
    }
    let mut process = sb.spawn(node, spawn)?;
    // The partner starts its EXI codec (a Java process) before it listens.
    if !process.wait_for_log(READY, Duration::from_secs(40))? {
        return Err(io::Error::other(format!("the partner did not start in time:\n{}", process.log())));
    }
    Ok(Partner { process, vehicle_tls, contract })
}

/// Path of a binary built by cargo: next to the running executable, or one level up when the
/// running one is a test (`target/debug/deps/x`). Cargo puts all binaries of a workspace
/// build in the same directory; `cargo test -p scenarios` alone does not build them.
pub fn sibling_binary(name: &str) -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir: &Path = exe.parent().ok_or_else(|| io::Error::other("no directory"))?;
    [dir.to_path_buf(), dir.parent().unwrap_or(dir).to_path_buf()]
        .into_iter()
        .map(|d| d.join(name))
        .find(|p| p.exists())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{name} not built next to {} (cargo build --workspace --bins)", exe.display())))
}
