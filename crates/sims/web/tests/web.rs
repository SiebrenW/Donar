//! The server end to end: starts `evcc-web`, talks HTTP to it with plain sockets. The run
//! itself needs Linux network namespaces (and the privileges the other scenario tests get).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

struct Server {
    child: Child,
    addr: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn target_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_evcc-web")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn start() -> Server {
    let scenarios = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../scenarios/scenarios");
    let work = target_dir().join("web-test");
    let (program, mut args) = (env!("CARGO_BIN_EXE_evcc-web").to_string(), Vec::<String>::new());
    // The test may run as root (the Linux runner); the server, like a person, does not, and
    // everything it makes must not need root to remove.
    let mut cmd = match std::env::var("SUDO_USER") {
        Ok(user) if unsafe { libc::geteuid() } == 0 => {
            let mut c = Command::new("sudo");
            args.splice(0..0, ["-u".to_string(), user, "-E".to_string(), program]);
            c.args(&args);
            c
        }
        _ => Command::new(program),
    };
    let mut child = cmd
        .args(["--listen", "127.0.0.1:0", "--scenarios"])
        .arg(scenarios)
        .arg("--work")
        .arg(work)
        .stderr(Stdio::piped())
        .spawn()
        .expect("start evcc-web");
    let mut line = String::new();
    BufReader::new(child.stderr.take().unwrap()).read_line(&mut line).unwrap();
    let addr = line.trim().strip_prefix("listening on http://").and_then(|r| r.strip_suffix('/')).unwrap_or_else(|| panic!("unexpected output {line:?}")).to_string();
    Server { child, addr }
}

/// One request, the whole response (the server closes the connection at the end).
fn http(server: &Server, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(&server.addr).unwrap();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response.split(' ').nth(1).unwrap().parse().unwrap();
    let body = response.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_string();
    (status, body)
}

#[test]
fn lists_scenarios_and_serves_the_schemas() {
    let server = start();
    let (status, body) = http(&server, "GET", "/api/scenarios", "");
    assert_eq!(status, 200);
    assert!(body.contains("\"file\":\"dc-plain\""), "{body}");
    assert!(body.contains("\"file\":\"interop/iso15118-python\""), "{body}");
    let (status, body) = http(&server, "GET", "/api/scenarios/standby", "");
    assert_eq!(status, 200);
    assert!(body.contains("\"pause_after_loops\":2"), "{body}");
    assert_eq!(http(&server, "GET", "/api/scenarios/nothing", "").0, 404);
    assert_eq!(http(&server, "GET", "/api/scenarios/..%2F..%2FCargo", "").0, 404);
    let (_, schema) = http(&server, "GET", "/api/schema", "");
    for key in ["stop_after", "terminate_after_loops", "standby_allowed"] {
        assert!(schema.contains(key), "{key} missing from the schemas");
    }
    let (status, page) = http(&server, "GET", "/", "");
    assert_eq!(status, 200);
    assert!(page.contains("<title>evcc-rs scenarios</title>"));
}

#[test]
fn refuses_a_bad_scenario() {
    let server = start();
    let (status, body) = http(&server, "POST", "/api/runs", r#"{"name":"x"}"#);
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("secc"), "{body}");
    // A key the settings do not know is as much an error as on the command line.
    let (_, scenario) = http(&server, "GET", "/api/scenarios/dc-plain", "");
    let broken = scenario.replacen("\"settings\":{", "\"settings\":{\"no_such_knob\":1,", 1);
    let (status, _) = http(&server, "POST", "/api/runs", if broken == scenario { r#"{"name":"x","secc":{"kind":"evse-sim","settings":{"no_such_knob":1}},"evcc":{"kind":"evcc-sim"}}"# } else { &broken });
    assert_eq!(status, 400);
}

/// A scenario started over HTTP, its logs followed over server-sent events, and its verdict.
#[cfg(target_os = "linux")]
#[test]
fn runs_a_scenario_and_streams_its_logs() {
    let server = start();
    let (_, scenario) = http(&server, "GET", "/api/scenarios/dc-plain", "");
    let (status, body) = http(&server, "POST", "/api/runs", &scenario);
    assert_eq!(status, 200, "{body}");
    let id = body.split("\"id\":").nth(1).unwrap().trim_end_matches('}').trim();
    let (status, events) = http(&server, "GET", &format!("/api/runs/{id}/events"), "");
    assert_eq!(status, 200);
    assert!(events.contains("event: log"), "{events}");
    assert!(events.contains("evse-sim.log"), "{events}");
    assert!(events.contains("evcc-sim.log"), "{events}");
    assert!(events.contains("event: result"), "{events}");
    assert!(events.contains("PASS dc-plain"), "{events}");
    assert!(events.contains("event: end"), "{events}");
}

#[cfg(target_os = "linux")]
mod simulation {
    use super::*;
    use serde_json::{Value, json};
    use std::time::{Duration, Instant};

    fn view(server: &Server) -> Value {
        serde_json::from_str(&http(server, "GET", "/api/sim", "").1).unwrap()
    }

    fn post(server: &Server, path: &str, body: Value) -> Value {
        let (status, text) = http(server, "POST", path, &body.to_string());
        assert_eq!(status, 200, "{path}: {text}");
        serde_json::from_str(&text).unwrap()
    }

    fn wait(server: &Server, what: &str, until: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let v = view(server);
            if until(&v) {
                return v;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}; last view: {v}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn held_at(phase: &'static str) -> impl Fn(&Value) -> bool {
        move |v| v["ev"]["phase"] == phase && v["ev"]["held"] == true
    }

    fn advance(server: &Server, to: &str) {
        post(server, "/api/sim/cmd", json!({ "target": "ev", "cmd": { "cmd": "advance", "to": to } }));
    }

    /// Two sessions in one open simulation, stepped: held at the first phase, advanced to the
    /// cable check, a charger measurement set by hand showing up at the vehicle, the stop held
    /// and released, and the next session started with other values.
    #[test]
    fn steps_a_session_and_starts_another() {
        let server = start();
        post(&server, "/api/sim/open", json!({}));
        wait(&server, "the simulation to open", |v| v["open"] == true && v["evse"].is_object());
        let vehicle = json!({ "charge_loop_interval_ms": 40, "poll_interval_ms": 10, "stop_after": 0, "time_scale": 3600, "stop_at_target_soc": true, "limits": { "target_soc": 30, "max_charge_current_a": 150 } });
        post(&server, "/api/sim/session", json!({ "evcc": vehicle, "secc": { "limits": { "max_charge_current_a": 120 } } }));

        let v = wait(&server, "the hold before setup", held_at("setup"));
        assert_eq!(v["ev"]["limits"]["max_charge_current_a"], 150);
        advance(&server, "cable_check");
        wait(&server, "the hold before the cable check", held_at("cable_check"));
        let v = view(&server);
        assert_eq!(v["evse"]["state"], "AwaitCableCheckReq", "{v}");
        assert_eq!(v["evse"]["limits"]["max_charge_current_a"], 120, "the charger's limits of this session");

        // A hand-set present current reaches the vehicle in the charge loop.
        post(&server, "/api/sim/cmd", json!({ "target": "evse", "cmd": { "cmd": "measure", "key": "current", "value": 77 } }));
        advance(&server, "charge_loop");
        wait(&server, "the hold before the charge loop", held_at("charge_loop"));
        advance(&server, "stop");
        let v = wait(&server, "the hold before the stop", held_at("stop"));
        assert_eq!(v["ev"]["evse_reports"]["current"], 77.0, "{v}");
        assert!(v["ev"]["loops"].as_u64().unwrap() >= 1, "{v}");

        advance(&server, "ended");
        let v = wait(&server, "the session to end", |v| v["running"] == false && v["session"] == 1);
        assert_eq!(v["exit_code"], 0, "{v}");

        // The next session: the charger's measurement edit stays, its settings are new.
        post(&server, "/api/sim/session", json!({ "evcc": vehicle, "secc": { "limits": { "max_charge_current_a": 90 } } }));
        wait(&server, "the second session's hold", |v| v["session"] == 2 && v["ev"]["phase"] == "setup" && v["ev"]["held"] == true);
        advance(&server, "cable_check");
        let v = wait(&server, "the hold before the cable check", held_at("cable_check"));
        assert_eq!(v["evse"]["limits"]["max_charge_current_a"], 90, "{v}");
        post(&server, "/api/sim/close", json!({}));
        wait(&server, "the simulation to close", |v| v["open"] == false);
    }

    /// Staying in the charge loop: the messages keep flowing and the limits of both sides change
    /// from the next message on.
    #[test]
    fn stays_in_the_charge_loop_and_changes_limits_live() {
        let server = start();
        post(&server, "/api/sim/open", json!({}));
        wait(&server, "the simulation to open", |v| v["open"] == true && v["evse"].is_object());
        let vehicle = json!({ "charge_loop_interval_ms": 30, "poll_interval_ms": 10, "stop_after": 0, "time_scale": 1, "stop_at_target_soc": false });
        post(&server, "/api/sim/session", json!({ "evcc": vehicle, "secc": {} }));
        wait(&server, "the hold before setup", held_at("setup"));
        post(&server, "/api/sim/cmd", json!({ "target": "ev", "cmd": { "cmd": "advance", "to": "charge_loop", "stay": true } }));
        // Into the charge loop and staying: not held, the loop runs.
        let v = wait(&server, "the charge loop to run", |v| v["ev"]["phase"] == "charge_loop" && v["ev"]["held"] == false && v["ev"]["loops"].as_u64().unwrap_or(0) >= 3);
        assert_eq!(v["ev"]["stay"], true, "{v}");
        let current = v["ev"]["evse_reports"]["current"].as_f64().unwrap();
        assert!(current > 10.0, "{v}");
        // The charger lowers its current limit: the vehicle sees less.
        post(&server, "/api/sim/cmd", json!({ "target": "evse", "cmd": { "cmd": "limits", "key": "max_charge_current_a", "value": 10 } }));
        let v = wait(&server, "the lower current limit", |v| v["ev"]["evse_reports"]["current"].as_f64() == Some(10.0));
        assert_eq!(v["evse"]["limits"]["max_charge_current_a"], 10, "{v}");
        // The vehicle lowers what it asks for: the charger sees it, and the current follows.
        post(&server, "/api/sim/cmd", json!({ "target": "ev", "cmd": { "cmd": "limits", "key": "max_charge_current_a", "value": 4 } }));
        let v = wait(&server, "the vehicle's lower limit", |v| v["evse"]["ev_reports"]["max_current"].as_f64() == Some(4.0) && v["ev"]["evse_reports"]["current"].as_f64() == Some(4.0));
        assert_eq!(v["ev"]["limits"]["max_charge_current_a"], 4, "{v}");
        // Still in the charge loop: not held, still going.
        let loops = v["ev"]["loops"].as_u64().unwrap();
        wait(&server, "more loops", |v| v["ev"]["loops"].as_u64().unwrap() > loops + 2 && v["ev"]["held"] == false);
        // Advancing to the stop ends it.
        post(&server, "/api/sim/cmd", json!({ "target": "ev", "cmd": { "cmd": "advance", "to": "ended" } }));
        let v = wait(&server, "the session to end", |v| v["running"] == false);
        assert_eq!(v["exit_code"], 0, "{v}");
        post(&server, "/api/sim/close", json!({}));
        wait(&server, "the simulation to close", |v| v["open"] == false);
    }
}
