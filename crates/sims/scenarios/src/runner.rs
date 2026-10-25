//! Runs a [`Scenario`] in the sandbox: two nodes joined by a cable, the charger started first,
//! then the vehicle, and the outcome checked against the scenario's expectations.
//!
//! Needs root and a user to drop to, like the sandbox. Settings and certificates are written
//! to the artifacts directory (owned by that user) and passed to the participants by path;
//! `evse-sim` and `evcc-sim` must be built (`cargo build --workspace --bins`).

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use sandbox::{Impairment, Options, Process, Sandbox, Spawn};

use crate::partners;
use crate::pki;
use crate::scenario::{Kind, Scenario};

/// What a run found.
#[derive(Debug)]
pub struct Report {
    /// Empty when the scenario passed; otherwise one line per expectation that did not hold.
    pub failures: Vec<String>,
    pub artifacts: PathBuf,
    pub evcc_log: String,
    pub secc_log: String,
    /// The capture of the cable (`capture.pcap` in the artifacts), when the run could make one.
    /// If the scenario uses TLS the vehicle discloses the session secrets on the cable (a UDP
    /// packet per key log line), which the dSPACE V2Gshark plugin uses to decrypt it.
    pub pcap: Option<PathBuf>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    /// The failures with both logs, for a test failure or the command line.
    pub fn describe(&self) -> String {
        format!(
            "{}\n----- secc log -----\n{}\n----- evcc log -----\n{}\nartifacts: {}{}",
            self.failures.join("\n"),
            tail(&self.secc_log, 60),
            tail(&self.evcc_log, 60),
            self.artifacts.display(),
            self.pcap.as_ref().map_or(String::new(), |p| format!("\ncapture: {}", p.display()))
        )
    }
}

fn tail(log: &str, lines: usize) -> String {
    let all: Vec<&str> = log.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

pub fn run(scenario: &Scenario) -> io::Result<Report> {
    run_with(scenario, Options::default())
}

pub fn run_with(scenario: &Scenario, options: Options) -> io::Result<Report> {
    let mut sb = Sandbox::new(options)?;
    let (evcc, secc) = (sb.node("evcc")?, sb.node("secc")?);
    let (evcc, secc) = sb.connect(&evcc, &secc)?;
    if let Some(i) = scenario.impairment {
        let impairment = Impairment { delay_ms: i.delay_ms, loss_percent: i.loss_percent };
        sb.impair(&evcc, impairment)?;
        sb.impair(&secc, impairment)?;
    }
    // A capture of the cable, from before the first packet to the end; without tcpdump (or the
    // right to capture) the run goes on without one.
    let mut capture = start_capture(&sb, &evcc);
    let mut evcc_settings = scenario.evcc_settings();
    // ACDP: where the vehicle is, shared by the vehicle and the charger's pairing and positioning device.
    let acdp_world = sb.artifacts().join("acdp-world").display().to_string();
    if let Some(acdp) = evcc_settings.acdp.as_mut() {
        acdp.world.get_or_insert(acdp_world.clone());
    }
    evcc_settings.control_pilot_file = evcc_settings.control_pilot_file.map(|p| sb.artifacts().join(p).display().to_string());
    let mut secc_args: Vec<String> = Vec::new();
    // Plug & Charge: a contract chain and key for the vehicle, and its root for the charger.
    let mut pnc_trust = None;
    if scenario.evcc.pnc && scenario.secc.kind == Kind::EvseSim {
        // ISO 15118-2 contracts are secp256r1; the -20 ones P-521 or Ed448.
        let iso2 = scenario.evcc.settings.get("protocol").and_then(toml::Value::as_str) == Some("iso15118-2");
        let curve = if iso2 { pki::Curve::P256 } else { scenario.pki().curve().filter(|c| *c != pki::Curve::P256).unwrap_or(pki::Curve::P521) };
        let (root, identity) = crate::profile_pki::profile_pki(curve, evcc_linux::certificates::Peer::Contract, crate::profile_pki::Flaw::None);
        let put = |name: &str, bytes: &[u8]| sb.write_artifact(&format!("pki/{name}"), bytes).map(|p| p.display().to_string());
        evcc_settings.pnc = Some(evcc_sim::Contract { chain: put("contract-chain.pem", &identity.chain)?, key: put("contract-key.pem", &identity.key)?, key_password: None });
        pnc_trust = Some(put("contract-trust.pem", &root.pem)?);
    }
    // Certificates: made for the run, or the partner's own.
    let mut partner_tls = None;
    let mut secc_process = match scenario.secc.kind {
        Kind::Iso15118Python => {
            let protocols = scenario.secc.protocols.as_deref().unwrap_or("ISO_15118_20_DC");
            let auth_modes = scenario.secc.auth_modes.as_deref().unwrap_or("EIM");
            let partner = partners::iso15118_python(&sb, &secc, scenario.secc.tls, protocols, auth_modes)?;
            partner_tls = partner.vehicle_tls;
            // The vehicle's contract, when it is to use one: the partner's own.
            if let (true, Some(c)) = (scenario.evcc.pnc, partner.contract) {
                evcc_settings.pnc = Some(evcc_sim::Contract { chain: c.chain.display().to_string(), key: c.key.display().to_string(), key_password: c.key_password });
            }
            partner.process
        }
        Kind::EvseSim => {
            let mut settings = scenario.secc_settings();
            if let Some(acdp) = settings.session.acdp.as_mut() {
                acdp.world.get_or_insert(acdp_world.clone());
            }
            // The control pilot between the two simulators: a directory both read and write.
            let wire = sb.artifacts().join("control-pilot").display().to_string();
            let wire = settings.control_pilot_wire.get_or_insert(wire).clone();
            evcc_settings.control_pilot_wire.get_or_insert(wire);
            settings.control_pilot_file = settings.control_pilot_file.map(|p| sb.artifacts().join(p).display().to_string());
            let config = sb.write_artifact("secc.toml", toml::to_string(&settings).map_err(io::Error::other)?.as_bytes())?;
            secc_args.extend(["--config".into(), config.display().to_string()]);
            if let Some(trust) = &pnc_trust {
                secc_args.extend(["--pnc-trust".into(), trust.clone()]);
            }
            if scenario.uses_tls() {
                let files = generated_pki(&sb, scenario)?;
                if scenario.secc.tls {
                    secc_args.extend(["--chain".into(), files.secc_chain, "--key".into(), files.secc_key, "--trust".into(), files.secc_trust]);
                    if let Some(ocsp) = files.secc_ocsp {
                        secc_args.extend(["--ocsp".into(), ocsp]);
                    }
                }
                // The scenario may set the options of the vehicle's TLS; the files are the run's.
                let own = evcc_settings.tls.take().unwrap_or_default();
                evcc_settings.tls = scenario.evcc.tls.then_some(evcc_sim::Tls { backwards_compatible: own.backwards_compatible, versions: own.versions, certificate_checks: own.certificate_checks, ocsp_checks: own.ocsp_checks, ocsp_require_all: own.ocsp_require_all, ..files.evcc_tls });
            }
            let program = partners::sibling_binary("evse-sim")?;
            let mut spawn = Spawn::new(program).args(["--interface", sandbox::INTERFACE, "--address", "fe80::2"]).args(&secc_args).label("evse-sim");
            if settings.slac_responder {
                spawn = spawn.net_raw();
            }
            sb.spawn(&secc, spawn)?
        }
        Kind::PySlac => partners::pyslac(&sb, &secc)?,
        Kind::EvccSim => unreachable!("rejected on load"),
    };
    if let Some(tls) = partner_tls.filter(|_| scenario.evcc.tls) {
        let own = evcc_settings.tls.take().unwrap_or_default();
        evcc_settings.tls = Some(evcc_sim::Tls {
            versions: own.versions,
            certificate_checks: own.certificate_checks,
            ocsp_checks: own.ocsp_checks,
            ocsp_require_all: own.ocsp_require_all,
            disclose_secrets: false,
            chain: tls.chain.display().to_string(),
            key: tls.key.display().to_string(),
            trust: tls.trust.display().to_string(),
            key_password: tls.key_password,
            // The partner's PKI is the one of ISO 15118-2.
            backwards_compatible: true,
        });
    }
    let ready = match scenario.secc.kind {
        Kind::EvseSim => "ready:",
        Kind::PySlac => partners::PYSLAC_READY,
        _ => partners::READY,
    };
    // An error here is a charger that ended before it was ready.
    if !secc_process.wait_for_log(ready, Duration::from_secs(40)).unwrap_or(false) {
        return Ok(finish(scenario, &sb, vec!["the charger did not become ready".into()], &mut secc_process, None, &mut capture));
    }
    // The vehicle is ours in every scenario and is the TLS client, so its secrets decrypt the capture.
    if let Some(tls) = evcc_settings.tls.as_mut() {
        tls.disclose_secrets = capture.is_some();
    }
    let evcc_config = sb.write_artifact("evcc.toml", toml::to_string(&evcc_settings).map_err(io::Error::other)?.as_bytes())?;
    let mut spawn = Spawn::new(partners::sibling_binary("evcc-sim")?)
        .args(["--interface", sandbox::INTERFACE, "--config"])
        .arg(evcc_config)
        .label("evcc-sim");
    if evcc_settings.slac {
        spawn = spawn.net_raw();
    }
    let mut car = sb.spawn(&evcc, spawn)?;
    let mut failures = Vec::new();
    let evcc_exit = match car.wait_timeout(Duration::from_secs(scenario.timeout_s))? {
        Some(status) => status.code(),
        None => {
            failures.push(format!("the vehicle did not finish in {} s", scenario.timeout_s));
            car.kill();
            None
        }
    };
    // A charger that served its session exits by itself; give it a moment to say so.
    let secc_exit = match scenario.expect.secc_exit {
        Some(_) if scenario.secc.kind == Kind::EvseSim => secc_process.wait_timeout(Duration::from_secs(5))?.and_then(|s| s.code()),
        _ => None,
    };
    if let Some(code) = evcc_exit
        && code != scenario.expect.evcc_exit {
            failures.push(format!("evcc exited with {code}, expected {}", scenario.expect.evcc_exit));
        }
    if let Some(expected) = scenario.expect.secc_exit
        && secc_exit != Some(expected) {
            failures.push(format!("secc exit status {secc_exit:?}, expected {expected}"));
        }
    Ok(finish(scenario, &sb, failures, &mut secc_process, Some(&mut car), &mut capture))
}

/// Name of the capture in the artifacts directory.
pub const CAPTURE: &str = "capture.pcap";

/// Starts `tcpdump` on the vehicle's end of the cable and waits until it listens.
fn start_capture(sb: &Sandbox, node: &sandbox::Node) -> Option<Process> {
    let path = sb.artifacts().join(CAPTURE);
    // `-U --immediate-mode`: every packet is delivered and written at once, so the file is complete when the process is killed.
    let spawn = Spawn::new("tcpdump").args(["-i", sandbox::INTERFACE, "-n", "-U", "--immediate-mode", "-w"]).arg(&path).net_raw().label("tcpdump");
    let mut process = match sb.spawn(node, spawn) {
        Ok(process) => process,
        Err(e) => {
            eprintln!("capture: tcpdump could not be started ({e}); the run goes on without {CAPTURE}");
            return None;
        }
    };
    if process.wait_for_log("listening on", Duration::from_secs(5)).unwrap_or(false) {
        return Some(process);
    }
    eprintln!("capture: tcpdump did not start ({}); the run goes on without {CAPTURE}. Is tcpdump installed (a dev container made before it was added to the Dockerfile has to be rebuilt)?", process.log().trim());
    process.kill();
    None
}

fn finish(scenario: &Scenario, sb: &Sandbox, mut failures: Vec<String>, secc: &mut Process, car: Option<&mut Process>, capture: &mut Option<Process>) -> Report {
    let secc_log = secc.log();
    let evcc_log = car.as_ref().map(|c| c.log()).unwrap_or_default();
    secc.kill();
    // The last packets of the session are still on their way to the capture.
    let pcap = capture.as_mut().map(|c| {
        std::thread::sleep(Duration::from_millis(300));
        c.kill();
        sb.artifacts().join(CAPTURE)
    });
    for (who, log, wanted) in [("evcc", &evcc_log, &scenario.expect.evcc_log), ("secc", &secc_log, &scenario.expect.secc_log)] {
        for line in wanted {
            if !log.contains(line.as_str()) {
                failures.push(format!("the {who} log has no line with {line:?}"));
            }
        }
    }
    Report { failures, artifacts: sb.artifacts().to_path_buf(), evcc_log, secc_log, pcap }
}

/// Certificates of a run, as paths in the artifacts directory.
pub struct Files {
    pub secc_chain: String,
    pub secc_key: String,
    pub secc_trust: String,
    pub evcc_tls: evcc_sim::Tls,
    /// A DER OCSP response for the SECC's certificate, when the PKI has one.
    pub secc_ocsp: Option<String>,
}

fn generated_pki(sb: &Sandbox, scenario: &Scenario) -> io::Result<Files> {
    let curve = scenario
        .pki()
        .curve()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "pki = \"partner\" needs an external charger"))?;
    write_pki(sb, curve)
}

/// Makes certificates for both sides and writes them into the artifacts directory.
pub fn write_pki(sb: &Sandbox, curve: pki::Curve) -> io::Result<Files> {
    let m = pki::material(curve);
    let put = |name: &str, bytes: &[u8]| sb.write_artifact(&format!("pki/{name}"), bytes).map(|p| p.display().to_string());
    Ok(Files {
        secc_chain: put("secc-chain.pem", &m.secc.chain)?,
        secc_key: put("secc-key.pem", &m.secc.key)?,
        secc_trust: put("secc-trust.pem", &m.secc_trust)?,
        secc_ocsp: m.secc_ocsp.as_deref().map(|r| put("secc-ocsp.der", r)).transpose()?,
        evcc_tls: evcc_sim::Tls {
            chain: put("evcc-chain.pem", &m.evcc.chain)?,
            key: put("evcc-key.pem", &m.evcc.key)?,
            trust: put("evcc-trust.pem", &m.evcc_trust)?,
            key_password: None,
            backwards_compatible: false,
            versions: Default::default(),
            certificate_checks: Default::default(),
            ocsp_checks: Default::default(),
            ocsp_require_all: false,
            disclose_secrets: false,
        },
    })
}
