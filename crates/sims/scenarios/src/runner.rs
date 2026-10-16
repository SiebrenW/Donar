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
}

impl Report {
    pub fn passed(&self) -> bool {
        self.failures.is_empty()
    }

    /// The failures with both logs, for a test failure or the command line.
    pub fn describe(&self) -> String {
        format!(
            "{}\n----- secc log -----\n{}\n----- evcc log -----\n{}\nartifacts: {}",
            self.failures.join("\n"),
            tail(&self.secc_log, 60),
            tail(&self.evcc_log, 60),
            self.artifacts.display()
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
    let mut evcc_settings = scenario.evcc_settings();
    evcc_settings.control_pilot_file = evcc_settings.control_pilot_file.map(|p| sb.artifacts().join(p).display().to_string());
    let mut secc_args: Vec<String> = Vec::new();
    // Certificates: made for the run, or the partner's own.
    let mut partner_tls = None;
    let mut secc_process = match scenario.secc.kind {
        Kind::Iso15118Python => {
            let partner = partners::iso15118_python(&sb, &secc, scenario.secc.tls)?;
            partner_tls = partner.vehicle_tls;
            partner.process
        }
        Kind::EvseSim => {
            let mut settings = scenario.secc_settings();
            settings.control_pilot_file = settings.control_pilot_file.map(|p| sb.artifacts().join(p).display().to_string());
            let config = sb.write_artifact("secc.toml", toml::to_string(&settings).map_err(io::Error::other)?.as_bytes())?;
            secc_args.extend(["--config".into(), config.display().to_string()]);
            if scenario.uses_tls() {
                let files = generated_pki(&sb, scenario)?;
                if scenario.secc.tls {
                    secc_args.extend(["--chain".into(), files.secc_chain, "--key".into(), files.secc_key, "--trust".into(), files.secc_trust]);
                }
                // The scenario may set the options of the vehicle's TLS; the files are the run's.
                let own = evcc_settings.tls.take().unwrap_or_default();
                evcc_settings.tls = scenario.evcc.tls.then_some(evcc_sim::Tls { backwards_compatible: own.backwards_compatible, versions: own.versions, ..files.evcc_tls });
            }
            let program = partners::sibling_binary("evse-sim")?;
            let mut spawn = Spawn::new(program).args(["--interface", sandbox::INTERFACE, "--address", "fe80::2"]).args(&secc_args).label("evse-sim");
            if settings.slac_responder {
                spawn = spawn.net_raw();
            }
            sb.spawn(&secc, spawn)?
        }
        Kind::EvccSim => unreachable!("rejected on load"),
    };
    if let Some(tls) = partner_tls.filter(|_| scenario.evcc.tls) {
        let versions = evcc_settings.tls.take().map_or_else(Default::default, |t| t.versions);
        evcc_settings.tls = Some(evcc_sim::Tls {
            versions,
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
        _ => partners::READY,
    };
    // An error here is a charger that ended before it was ready.
    if !secc_process.wait_for_log(ready, Duration::from_secs(40)).unwrap_or(false) {
        return Ok(finish(scenario, &sb, vec!["the charger did not become ready".into()], &mut secc_process, None));
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
    if let Some(code) = evcc_exit {
        if code != scenario.expect.evcc_exit {
            failures.push(format!("evcc exited with {code}, expected {}", scenario.expect.evcc_exit));
        }
    }
    if let Some(expected) = scenario.expect.secc_exit {
        if secc_exit != Some(expected) {
            failures.push(format!("secc exit status {secc_exit:?}, expected {expected}"));
        }
    }
    Ok(finish(scenario, &sb, failures, &mut secc_process, Some(&mut car)))
}

fn finish(scenario: &Scenario, sb: &Sandbox, mut failures: Vec<String>, secc: &mut Process, car: Option<&mut Process>) -> Report {
    let secc_log = secc.log();
    let evcc_log = car.as_ref().map(|c| c.log()).unwrap_or_default();
    secc.kill();
    for (who, log, wanted) in [("evcc", &evcc_log, &scenario.expect.evcc_log), ("secc", &secc_log, &scenario.expect.secc_log)] {
        for line in wanted {
            if !log.contains(line.as_str()) {
                failures.push(format!("the {who} log has no line with {line:?}"));
            }
        }
    }
    Report { failures, artifacts: sb.artifacts().to_path_buf(), evcc_log, secc_log }
}

/// Certificates of a run, as paths in the artifacts directory.
pub struct Files {
    pub secc_chain: String,
    pub secc_key: String,
    pub secc_trust: String,
    pub evcc_tls: evcc_sim::Tls,
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
        evcc_tls: evcc_sim::Tls {
            chain: put("evcc-chain.pem", &m.evcc.chain)?,
            key: put("evcc-key.pem", &m.evcc.key)?,
            trust: put("evcc-trust.pem", &m.evcc_trust)?,
            key_password: None,
            backwards_compatible: false,
            versions: Default::default(),
        },
    })
}
