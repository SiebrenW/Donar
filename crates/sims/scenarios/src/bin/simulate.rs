//! `simulate [--artifacts-root DIR]`: the privileged half of the web UI's simulation page. Owns
//! a sandbox (a charger and a vehicle node joined by a cable) for as long as the simulation is
//! open, runs a charger in it and a vehicle per session. Reads commands as JSON lines on
//! standard input and reports events as JSON lines on standard output; the simulators
//! themselves are controlled over their own sockets in the artifacts directory (`sim_control`).
//!
//! ```text
//! {"cmd":"open","tls":true,"pki":"p521","slac":false}   -> {"event":"opened","artifacts":DIR,"secc_socket":PATH}
//! {"cmd":"session","n":1,"settings":{...}}              -> {"event":"session_started","n":1,"socket":PATH}
//!                                                          {"event":"session_ended","n":1,"code":0}
//! {"cmd":"close"}                                       (or end of input)
//! ```
//! `settings` are an `evcc_sim::Settings`; TLS, SLAC and the control pilot are set from `open`.
//! Needs root and a user to drop to (sudo), like `scenario`.

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("simulate: needs Linux network namespaces (use `cargo xtask web`)");
    std::process::ExitCode::from(2)
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use std::io::BufRead;

    let mut args = std::env::args().skip(1);
    let mut options = sandbox::Options::default();
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--artifacts-root", Some(dir)) => options.artifacts_root = Some(dir.into()),
            _ => {
                eprintln!("usage: simulate [--artifacts-root DIR]");
                return std::process::ExitCode::from(2);
            }
        }
    }
    let mut simulation: Option<linux::Simulation> = None;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let reply = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(command) if command["cmd"] == "close" => break,
            Ok(command) => linux::handle(&mut simulation, &options, &command),
            Err(e) => Err(format!("not JSON: {e}")),
        };
        match reply {
            Ok(Some(event)) => linux::emit(event),
            Ok(None) => {}
            Err(message) => linux::emit(serde_json::json!({ "event": "error", "message": message })),
        }
    }
    // Dropping the simulation kills the processes and removes the namespaces.
    drop(simulation);
    std::process::ExitCode::SUCCESS
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use sandbox::{Node, Options, Process, Sandbox, Spawn};
    use scenarios::{partners, pki, runner};
    use serde_json::{Value, json};

    pub fn emit(event: Value) {
        println!("{event}");
    }

    pub struct Simulation {
        sandbox: Sandbox,
        evcc: Node,
        tls: Option<evcc_sim::Tls>,
        slac: bool,
        /// Kept so that dropping the simulation ends it.
        _secc: Process,
        car: Arc<Mutex<Option<Process>>>,
    }

    impl Drop for Simulation {
        fn drop(&mut self) {
            if let Some(mut car) = self.car.lock().unwrap().take() {
                car.kill();
            }
        }
    }

    pub fn handle(simulation: &mut Option<Simulation>, options: &Options, command: &Value) -> Result<Option<Value>, String> {
        match command["cmd"].as_str() {
            Some("open") => {
                if simulation.is_some() {
                    return Err("already open".into());
                }
                let (sim, event) = open(options, command).map_err(|e| e.to_string())?;
                *simulation = Some(sim);
                Ok(Some(event))
            }
            Some("session") => {
                let sim = simulation.as_mut().ok_or("not open")?;
                session(sim, command).map_err(|e| e.to_string()).map(Some)
            }
            other => Err(format!("unknown command {other:?}")),
        }
    }

    fn open(options: &Options, command: &Value) -> std::io::Result<(Simulation, Value)> {
        let bad = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, m);
        let (tls, slac) = (command["tls"].as_bool().unwrap_or(false), command["slac"].as_bool().unwrap_or(false));
        let curve = match command["pki"].as_str().unwrap_or("p521") {
            "p521" => pki::Curve::P521,
            "ed448" => pki::Curve::Ed448,
            "p256" => pki::Curve::P256,
            other => return Err(bad(format!("pki: unknown curve {other}"))),
        };
        let mut sandbox = Sandbox::new(options.clone())?;
        let (evcc, secc) = (sandbox.node("evcc")?, sandbox.node("secc")?);
        let (evcc, secc) = sandbox.connect(&evcc, &secc)?;
        let socket = sandbox.artifacts().join("secc.sock");
        let settings = evse_sim::Settings { slac_responder: slac, control_pilot_file: slac.then(|| sandbox.artifacts().join("pilot").display().to_string()), ..Default::default() };
        let config = sandbox.write_artifact("secc.toml", toml::to_string(&settings).map_err(std::io::Error::other)?.as_bytes())?;
        // The charger serves sessions for as long as the simulation is open.
        let mut args = vec!["--config".to_string(), config.display().to_string(), "--control".into(), socket.display().to_string(), "--sessions".into(), "0".into()];
        let mut evcc_tls = None;
        if tls {
            let files = runner::write_pki(&sandbox, curve)?;
            args.extend(["--chain".into(), files.secc_chain, "--key".into(), files.secc_key, "--trust".into(), files.secc_trust]);
            evcc_tls = Some(files.evcc_tls);
        }
        let mut spawn = Spawn::new(partners::sibling_binary("evse-sim")?).args(["--interface", sandbox::INTERFACE, "--address", "fe80::2"]).args(&args).label("evse-sim");
        if slac {
            spawn = spawn.net_raw();
        }
        let mut secc_process = sandbox.spawn(&secc, spawn)?;
        if !secc_process.wait_for_log("ready:", Duration::from_secs(40))? {
            return Err(std::io::Error::other("the charger did not become ready"));
        }
        let event = json!({ "event": "opened", "artifacts": sandbox.artifacts(), "secc_socket": socket });
        Ok((Simulation { sandbox, evcc, tls: evcc_tls, slac, _secc: secc_process, car: Arc::default() }, event))
    }

    fn session(sim: &mut Simulation, command: &Value) -> std::io::Result<Value> {
        let invalid = |m: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, m);
        if sim.car.lock().unwrap().as_mut().is_some_and(|c| matches!(c.try_wait(), Ok(None))) {
            return Err(invalid("a session is running".into()));
        }
        let n = command["n"].as_u64().ok_or_else(|| invalid("n: a number".into()))?;
        let mut settings: evcc_sim::Settings = serde_json::from_value(command["settings"].clone()).map_err(|e| invalid(format!("settings: {e}")))?;
        settings.tls = sim.tls.clone();
        settings.slac = sim.slac;
        settings.control_pilot_file = sim.slac.then(|| sim.sandbox.artifacts().join("pilot").display().to_string());
        let config = sim.sandbox.write_artifact(&format!("evcc-{n}.toml"), toml::to_string(&settings).map_err(std::io::Error::other)?.as_bytes())?;
        let socket: PathBuf = sim.sandbox.artifacts().join(format!("evcc-{n}.sock"));
        let mut spawn = Spawn::new(partners::sibling_binary("evcc-sim")?)
            .args(["--interface", sandbox::INTERFACE, "--config"])
            .arg(config)
            .arg("--control")
            .arg(&socket)
            .label(format!("evcc-sim-{n}"));
        if sim.slac {
            spawn = spawn.net_raw();
        }
        let process = sim.sandbox.spawn(&sim.evcc, spawn)?;
        *sim.car.lock().unwrap() = Some(process);
        let car = sim.car.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let mut guard = car.lock().unwrap();
                let Some(process) = guard.as_mut() else { return };
                if let Ok(Some(status)) = process.try_wait() {
                    emit(json!({ "event": "session_ended", "n": n, "code": status.code() }));
                    return;
                }
            }
        });
        Ok(json!({ "event": "session_started", "n": n, "socket": socket }))
    }
}
