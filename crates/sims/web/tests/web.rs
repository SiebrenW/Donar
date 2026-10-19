//! The server end to end: starts `evcc-web`, talks HTTP to it with plain sockets. The run
//! itself needs Linux network namespaces (and the privileges the other scenario tests get).

mod common;

#[allow(unused_imports)]
use std::io::{BufRead, BufReader, Read, Write};
#[allow(unused_imports)]
use std::net::TcpStream;
#[allow(unused_imports)]
use std::path::PathBuf;

#[allow(unused_imports)]
use common::{Server, http, start};

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
        // The charge is over: the charger delivers nothing any more.
        let v = wait(&server, "the charger to be idle", |v| v["evse"]["state"] == "Waiting");
        for key in ["voltage", "current", "max_power"] {
            assert_eq!(v["evse"]["measurements"][key]["effective"], 0.0, "{key}: {v}");
        }

        // The next session: the charger's measurement edit stays, its settings are new.
        post(&server, "/api/sim/session", json!({ "evcc": vehicle, "secc": { "limits": { "max_charge_current_a": 90 } } }));
        wait(&server, "the second session's hold", |v| v["session"] == 2 && v["ev"]["phase"] == "setup" && v["ev"]["held"] == true);
        advance(&server, "cable_check");
        let v = wait(&server, "the hold before the cable check", held_at("cable_check"));
        assert_eq!(v["evse"]["limits"]["max_charge_current_a"], 90, "{v}");
        post(&server, "/api/sim/close", json!({}));
        wait(&server, "the simulation to close", |v| v["open"] == false);
    }

    /// An ISO 15118-2 session runs through the page's simulation to the end.
    #[test]
    fn runs_an_iso2_session() {
        let server = start();
        post(&server, "/api/sim/open", json!({}));
        wait(&server, "the simulation to open", |v| v["open"] == true && v["evse"].is_object());
        let vehicle = json!({ "protocol": "iso15118-2", "connector": "dual2", "charge_loop_interval_ms": 20, "poll_interval_ms": 10, "stop_after": 0, "time_scale": 3600, "stop_at_target_soc": true, "limits": { "target_soc": 30 } });
        post(&server, "/api/sim/session", json!({ "evcc": vehicle, "secc": {} }));
        wait(&server, "the hold before setup", held_at("setup"));
        advance(&server, "ended");
        let v = wait(&server, "the session to end", |v| v["running"] == false && v["session"] == 1);
        assert_eq!(v["exit_code"], 0, "{v}");
        post(&server, "/api/sim/close", json!({}));
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

    /// A BPT session discharges the battery down to a floor, then charges to the target: the
    /// charger reports a negative current first, a positive one after.
    #[test]
    fn a_bpt_session_discharges_then_charges() {
        let server = start();
        post(&server, "/api/sim/open", json!({}));
        wait(&server, "the simulation to open", |v| v["open"] == true && v["evse"].is_object());
        let vehicle = json!({ "bpt": true, "charge_loop_interval_ms": 30, "poll_interval_ms": 10, "stop_after": 0, "time_scale": 600, "start_soc": 60,
            "stop_at_target_soc": true, "limits": { "target_soc": 62, "discharge_to_soc": 58 } });
        post(&server, "/api/sim/session", json!({ "evcc": vehicle, "secc": {} }));
        wait(&server, "the hold before setup", held_at("setup"));
        advance(&server, "charge_loop");
        wait(&server, "the hold before the charge loop", held_at("charge_loop"));
        post(&server, "/api/sim/cmd", json!({ "target": "ev", "cmd": { "cmd": "advance", "to": "charge_loop", "stay": true } }));
        let v = wait(&server, "a discharge", |v| v["ev"]["evse_reports"]["current"].as_f64().is_some_and(|c| c < -1.0));
        assert!(v["ev"]["model_soc"].as_f64().unwrap() <= 60.0, "{v}");
        // The floor is reached, then the car charges until its target and the session ends.
        wait(&server, "the charge after the discharge", |v| v["ev"]["evse_reports"]["current"].as_f64().is_some_and(|c| c > 1.0));
        let _v = wait(&server, "the vehicle to stop at its target", |v| v["ev"]["phase"] == "stop");
        advance(&server, "ended");
        let v = wait(&server, "the session to end", |v| v["running"] == false);
        assert_eq!(v["exit_code"], 0, "{v}");
        assert!(v["ev"]["model_soc"].as_f64().unwrap() >= 62.0, "{v}");
        post(&server, "/api/sim/close", json!({}));
    }

    /// An AC BPT session discharges the battery down to a floor, then charges to the target: the
    /// charger reports a negative current first, a positive one after.
    #[test]
    fn an_ac_bpt_session_discharges_then_charges() {
        let server = start();
        post(&server, "/api/sim/open", json!({}));
        wait(&server, "the simulation to open", |v| v["open"] == true && v["evse"].is_object());
        let vehicle = json!({ "ac": true, "bpt": true, "charge_loop_interval_ms": 30, "poll_interval_ms": 10, "stop_after": 0, "time_scale": 3000, "start_soc": 60,
            "stop_at_target_soc": true, "limits": { "target_soc": 62, "discharge_to_soc": 58 } });
        post(&server, "/api/sim/session", json!({ "evcc": vehicle, "secc": {} }));
        wait(&server, "the hold before setup", held_at("setup"));
        advance(&server, "charge_loop");
        wait(&server, "the hold before the charge loop", held_at("charge_loop"));
        post(&server, "/api/sim/cmd", json!({ "target": "ev", "cmd": { "cmd": "advance", "to": "charge_loop", "stay": true } }));
        let v = wait(&server, "a discharge", |v| v["ev"]["evse_reports"]["current"].as_f64().is_some_and(|c| c < -1.0));
        assert!(v["ev"]["model_soc"].as_f64().unwrap() <= 60.0, "{v}");
        // The floor is reached, then the car charges until its target and the session ends.
        wait(&server, "the charge after the discharge", |v| v["ev"]["evse_reports"]["current"].as_f64().is_some_and(|c| c > 1.0));
        let _v = wait(&server, "the vehicle to stop at its target", |v| v["ev"]["phase"] == "stop");
        advance(&server, "ended");
        let v = wait(&server, "the session to end", |v| v["running"] == false);
        assert_eq!(v["exit_code"], 0, "{v}");
        assert!(v["ev"]["model_soc"].as_f64().unwrap() >= 62.0, "{v}");
        post(&server, "/api/sim/close", json!({}));
    }

    /// An AC session in Scheduled control mode charges with the power of the selected schedule.
    #[test]
    fn a_scheduled_ac_session_charges() {
        let server = start();
        post(&server, "/api/sim/open", json!({}));
        wait(&server, "the simulation to open", |v| v["open"] == true && v["evse"].is_object());
        let vehicle = json!({ "ac": true, "scheduled": true, "charge_loop_interval_ms": 30, "poll_interval_ms": 10, "stop_after": 0, "time_scale": 600, "start_soc": 60, "stop_at_target_soc": true, "limits": { "target_soc": 61 } });
        post(&server, "/api/sim/session", json!({ "evcc": vehicle, "secc": {} }));
        wait(&server, "the hold before setup", held_at("setup"));
        post(&server, "/api/sim/cmd", json!({ "target": "ev", "cmd": { "cmd": "advance", "to": "charge_loop", "stay": true } }));
        // Tuple 1 starts at the charger's full power: 3 lines of 3680 W at 400 V.
        wait(&server, "the scheduled charge", |v| v["ev"]["evse_reports"]["current"].as_f64().is_some_and(|c| (c - 27.6).abs() < 0.5));
        advance(&server, "ended");
        let v = wait(&server, "the session to end", |v| v["running"] == false);
        assert_eq!(v["exit_code"], 0, "{v}");
        post(&server, "/api/sim/close", json!({}));
    }

    /// Reads the event stream for a while and returns what came.
    fn listen(server: &Server, milliseconds: u64) -> String {
        let mut stream = TcpStream::connect(&server.addr).unwrap();
        write!(stream, "GET /api/sim/events HTTP/1.1\r\nHost: test\r\nAccept: text/event-stream\r\n\r\n").unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let (mut text, deadline) = (Vec::new(), Instant::now() + Duration::from_millis(milliseconds));
        let mut chunk = [0u8; 8192];
        while Instant::now() < deadline {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => text.extend_from_slice(&chunk[..n]),
                Err(_) => {}
            }
        }
        String::from_utf8_lossy(&text).into_owned()
    }

    /// The timeline's messages and the faults injected during a session.
    #[test]
    fn shows_the_control_pilot_and_takes_emergencies_from_either_side() {
        let server = start();
        post(&server, "/api/sim/open", json!({}));
        wait(&server, "the simulation to open", |v| v["open"] == true && v["evse"].is_object());
        let vehicle = json!({ "charge_loop_interval_ms": 30, "poll_interval_ms": 10, "stop_after": 0, "time_scale": 1, "stop_at_target_soc": false });
        post(&server, "/api/sim/session", json!({ "evcc": vehicle, "secc": {} }));
        wait(&server, "the hold before setup", held_at("setup"));
        post(&server, "/api/sim/cmd", json!({ "target": "ev", "cmd": { "cmd": "advance", "to": "charge_loop", "stay": true } }));
        // The cable shows state C and the 5 % oscillator, and both machines have told their hosts what to watch.
        let v = wait(&server, "the charge loop with the pilot at C", |v| v["ev"]["loops"].as_u64().unwrap_or(0) >= 3 && v["ev"]["pilot"]["reading"]["state"] == "C");
        assert_eq!(v["ev"]["pilot"]["reading"]["pwm"], 5, "{v}");
        assert_eq!(v["ev"]["pilot"]["guard"]["hold_ready"], true, "{v}");
        assert_eq!(v["ev"]["pilot"]["guard"]["hold_five_percent"], true, "{v}");
        let v = wait(&server, "the charger's guard", |v| v["evse"]["pilot"]["guard"]["hold_ready"] == true);
        assert_eq!(v["evse"]["pilot"]["reading"]["state"], "C", "{v}");
        // The charger turns its oscillator to 50 %: in a DC session that is an emergency for the vehicle, which ends the session.
        post(&server, "/api/sim/cmd", json!({ "target": "evse", "cmd": { "cmd": "pilot", "oscillator": 50 } }));
        let v = wait(&server, "the vehicle to end the session", |v| v["running"] == false);
        assert_eq!(v["exit_code"], 1, "{v}");
        // A second session: the vehicle's own emergency (its pilot leaves C, the connection is dropped).
        post(&server, "/api/sim/session", json!({ "evcc": vehicle, "secc": {} }));
        wait(&server, "the hold before setup", held_at("setup"));
        post(&server, "/api/sim/cmd", json!({ "target": "ev", "cmd": { "cmd": "advance", "to": "charge_loop", "stay": true } }));
        wait(&server, "the charge loop", |v| v["session"] == 2 && v["ev"]["loops"].as_u64().unwrap_or(0) >= 3);
        post(&server, "/api/sim/cmd", json!({ "target": "ev", "cmd": { "cmd": "event", "name": "pilot_emergency" } }));
        let v = wait(&server, "the vehicle to end its session", |v| v["running"] == false && v["session"] == 2);
        assert_eq!(v["exit_code"], 1, "{v}");
        post(&server, "/api/sim/close", json!({}));
        wait(&server, "the simulation to close", |v| v["open"] == false);
    }

    #[test]
    fn reports_messages_and_takes_injected_faults() {
        let server = start();
        post(&server, "/api/sim/open", json!({}));
        wait(&server, "the simulation to open", |v| v["open"] == true && v["evse"].is_object());
        let vehicle = json!({ "charge_loop_interval_ms": 30, "poll_interval_ms": 10, "stop_after": 0, "time_scale": 1, "stop_at_target_soc": false });
        post(&server, "/api/sim/session", json!({ "evcc": vehicle, "secc": {} }));
        wait(&server, "the hold before setup", held_at("setup"));
        post(&server, "/api/sim/cmd", json!({ "target": "ev", "cmd": { "cmd": "advance", "to": "charge_loop", "stay": true } }));
        wait(&server, "the charge loop to run", |v| v["ev"]["phase"] == "charge_loop" && v["ev"]["loops"].as_u64().unwrap_or(0) >= 3);

        // The vehicle's messages and the charger's state changes reach the page as events.
        let stream = listen(&server, 800);
        for wanted in ["event: msg", r#""msg":"DcChargeLoopReq""#, r#""kind":"tx""#, r#""kind":"response""#, r#""code":"OK""#, r#""src":"evse""#, r#""kind":"state""#] {
            assert!(stream.contains(wanted), "{wanted} missing from the events: {}", &stream[..stream.len().min(1500)]);
        }

        // A charger setting changed during the session, and the charger asking to terminate.
        post(&server, "/api/sim/cmd", json!({ "target": "evse", "cmd": { "cmd": "patch", "values": { "terminate_after_loops": 1 } } }));
        wait(&server, "the charger's setting to show", |v| v["evse"]["config"]["terminate_after_loops"] == 1);
        let v = wait(&server, "the stop after the charger's request", held_at("stop"));
        assert_eq!(v["evse"]["state"], "ChargeLoop", "{v}");
        post(&server, "/api/sim/cmd", json!({ "target": "ev", "cmd": { "cmd": "advance", "to": "ended" } }));
        let v = wait(&server, "the session to end", |v| v["running"] == false);
        assert_eq!(v["exit_code"], 0, "{v}");

        // A bad setting is refused; a silent charger makes the vehicle time out.
        let (status, body) = http(&server, "POST", "/api/sim/cmd", &json!({ "target": "evse", "cmd": { "cmd": "patch", "values": { "no_such": 1 } } }).to_string());
        assert_eq!(status, 200, "{body}");
        post(&server, "/api/sim/session", json!({ "evcc": vehicle, "secc": { "silent": true } }));
        wait(&server, "the hold before setup", held_at("setup"));
        post(&server, "/api/sim/cmd", json!({ "target": "ev", "cmd": { "cmd": "advance", "to": "ended" } }));
        let v = wait(&server, "the vehicle to give up", |v| v["running"] == false && v["session"] == 2);
        assert_ne!(v["exit_code"], 0, "{v}");
        post(&server, "/api/sim/close", json!({}));
        wait(&server, "the simulation to close", |v| v["open"] == false);
    }
}
