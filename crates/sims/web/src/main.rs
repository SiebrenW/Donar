//! `evcc-web`: a small web UI over the scenario runner. Pick a scenario, change the settings of
//! the vehicle and the charger in forms made from their JSON Schemas, start it and follow the
//! logs of both live.
//!
//! ```text
//! evcc-web [--listen ADDR] [--scenarios DIR] [--work DIR] [--no-sudo]
//! ```
//! The server runs as you. A run is the `scenario` command (next to this binary) started
//! through `sudo` unless you are root or pass `--no-sudo`; only that process is privileged
//! (it creates the namespaces), and everything it leaves in the work directory belongs to you.
//! One run at a time. The page is `index.html`, plain JavaScript, no build step.

mod runs;
mod sim;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio_stream::wrappers::ReceiverStream;

use runs::Runs;

const USAGE: &str = "usage: evcc-web [--listen ADDR] [--scenarios DIR] [--work DIR] [--no-sudo]";

struct App {
    scenarios: PathBuf,
    runs: Runs,
    sim: Arc<sim::Sim>,
}

type Shared = State<Arc<App>>;

fn fail(status: StatusCode, message: impl ToString) -> Response {
    (status, Json(json!({ "error": message.to_string() }))).into_response()
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let mut listen: SocketAddr = ([127, 0, 0, 1], 8080).into();
    let mut scenarios = PathBuf::from("crates/sims/scenarios/scenarios");
    // Below the artifacts root, next to the runs of the tests: one place to look in, on the host too.
    let mut work = sandbox::artifacts_root().join("web");
    let mut sudo = unsafe { libc::geteuid() } != 0;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        let parsed = match arg.as_str() {
            "--listen" => value("--listen").and_then(|v| v.parse().map(|a| listen = a).map_err(|e| format!("--listen: {e}"))),
            "--scenarios" => value("--scenarios").map(|v| scenarios = v.into()),
            "--work" => value("--work").map(|v| work = v.into()),
            "--no-sudo" => {
                sudo = false;
                Ok(())
            }
            other => Err(format!("unknown argument {other}")),
        };
        if let Err(e) = parsed {
            eprintln!("evcc-web: {e}\n{USAGE}");
            return std::process::ExitCode::from(2);
        }
    }
    let sim = sim::Sim::new(work.join("sim"), sudo);
    let runs = match Runs::new(work, sudo) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("evcc-web: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let app = Arc::new(App { scenarios, runs, sim });
    let router = Router::new()
        .route("/", get(|| async { Html(include_str!("index.html")) }))
        .route("/api/schema", get(schema))
        .route("/api/scenarios", get(list))
        .route("/api/scenarios/{name}", get(show))
        .route("/api/runs", post(start))
        .route("/api/runs/{id}/events", get(events))
        .route("/api/sim", get(sim_view))
        .route("/api/sim/open", post(sim_open))
        .route("/api/sim/session", post(sim_session))
        .route("/api/sim/cmd", post(sim_command))
        .route("/api/sim/close", post(sim_close))
        .route("/api/sim/events", get(sim_events))
        .with_state(app);
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("evcc-web: {listen}: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    eprintln!("listening on http://{}/", listener.local_addr().unwrap_or(listen));
    if let Err(e) = axum::serve(listener, router).await {
        eprintln!("evcc-web: {e}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

/// The JSON Schemas of the two settings files with their defaults, for the forms.
/// The response code settings of the charger and the codes the message each decides may carry
/// (Tables 224 and 226), for the page's choice of a code.
fn response_codes() -> Value {
    use iso15118_20::ResponseMessage as M;
    let settings = [
        ("setup_code", M::SessionSetup),
        ("authorization_code", M::Authorization),
        ("selection_code", M::ServiceSelection),
        ("cpd_code", M::DcChargeParameterDiscovery),
        ("schedule_code", M::ScheduleExchange),
        ("cable_check_code", M::DcCableCheck),
        ("power_delivery_code", M::PowerDelivery),
        ("welding_code", M::DcWeldingDetection),
    ];
    let all: Vec<&str> = iso15118_20::common_types::ResponseCode::ALL.iter().map(|c| c.xml_name()).collect();
    let allowed: serde_json::Map<String, Value> = settings
        .iter()
        .map(|(setting, message)| {
            let codes: Vec<&str> = iso15118_20::common_types::ResponseCode::ALL.iter().filter(|c| message.allows(**c)).map(|c| c.xml_name()).collect();
            (setting.to_string(), json!({ "message": message.name(), "allowed": codes }))
        })
        .collect();
    json!({ "all": all, "settings": allowed })
}

async fn schema() -> Json<Value> {
    let part = |schema: schemars::Schema, defaults: Value| json!({ "schema": schema, "defaults": defaults });
    Json(json!({
        "evcc": part(schemars::schema_for!(evcc_sim::Settings), serde_json::to_value(evcc_sim::Settings::default()).unwrap()),
        "secc": part(schemars::schema_for!(evse_sim::Settings), serde_json::to_value(evse_sim::Settings::default()).unwrap()),
        "response_codes": response_codes(),
    }))
}

/// Scenario files below the scenarios directory, by path without `.toml` (`interop/x`).
fn scenario_files(dir: &std::path::Path) -> Vec<String> {
    fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if let (true, Ok(rel)) = (path.extension().is_some_and(|e| e == "toml"), path.strip_prefix(root)) {
                out.push(rel.with_extension("").to_string_lossy().into_owned());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

async fn list(State(app): Shared) -> Json<Value> {
    let entries: Vec<Value> = scenario_files(&app.scenarios)
        .into_iter()
        .map(|file| {
            let loaded = scenarios::Scenario::load(&app.scenarios.join(format!("{file}.toml")));
            match loaded {
                Ok(s) => json!({ "file": file, "name": s.name, "description": s.description }),
                Err(e) => json!({ "file": file, "name": file, "description": format!("does not load: {e}") }),
            }
        })
        .collect();
    Json(json!(entries))
}

/// A scenario file as JSON (the page edits JSON; there is no TOML parser in it).
async fn show(State(app): Shared, Path(name): Path<String>) -> Response {
    if !scenario_files(&app.scenarios).contains(&name) {
        return fail(StatusCode::NOT_FOUND, "no such scenario");
    }
    let text = match std::fs::read_to_string(app.scenarios.join(format!("{name}.toml"))) {
        Ok(t) => t,
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    match toml::from_str::<Value>(&text) {
        Ok(v) => Json(v).into_response(),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// Starts a run of the scenario in the body (JSON, the structure of the TOML file).
async fn start(State(app): Shared, Json(body): Json<Value>) -> Response {
    let text = match toml::Value::try_from(&body).map_err(|e| e.to_string()).and_then(|v| toml::to_string(&v).map_err(|e| e.to_string())) {
        Ok(t) => t,
        Err(e) => return fail(StatusCode::BAD_REQUEST, e),
    };
    if let Err(e) = scenarios::Scenario::parse(&text) {
        return fail(StatusCode::BAD_REQUEST, e);
    }
    match app.runs.start(&text) {
        Ok(id) => Json(json!({ "id": id })).into_response(),
        Err(runs::StartError::Busy) => fail(StatusCode::CONFLICT, "a run is in progress"),
        Err(runs::StartError::Io(e)) => fail(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

/// The logs of a run as they grow (`log`: `{file, text}`), then its verdict (`result`) and `end`.
async fn events(State(app): Shared, Path(id): Path<u32>) -> Response {
    let Some(follower) = app.runs.follow(id) else {
        return fail(StatusCode::NOT_FOUND, "no such run");
    };
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
    tokio::spawn(async move {
        let mut follower = follower;
        loop {
            let (chunks, verdict) = follower.poll().await;
            for (file, text) in chunks {
                let event = Event::default().event("log").data(json!({ "file": file, "text": text }).to_string());
                if tx.send(Ok(event)).await.is_err() {
                    return;
                }
            }
            if let Some(verdict) = verdict {
                let _ = tx.send(Ok(Event::default().event("result").data(verdict))).await;
                let _ = tx.send(Ok(Event::default().event("end").data(""))).await;
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()).into_response()
}

fn sim_failure(f: sim::Failure) -> Response {
    match f {
        sim::Failure::Bad(m) => fail(StatusCode::BAD_REQUEST, m),
        sim::Failure::Conflict(m) => fail(StatusCode::CONFLICT, m),
    }
}

fn done(result: Result<(), sim::Failure>) -> Response {
    match result {
        Ok(()) => Json(json!({})).into_response(),
        Err(f) => sim_failure(f),
    }
}

/// The state of the simulation now.
async fn sim_view(State(app): Shared) -> Json<sim::View> {
    Json(app.sim.view())
}

/// Opens the simulation sandbox: `{tls, pki, slac}`.
async fn sim_open(State(app): Shared, Json(body): Json<Value>) -> Response {
    done(app.sim.open(body))
}

/// Starts a session: `{evcc: <evcc_sim::Settings>, secc: <evse_sim::Config>}`.
async fn sim_session(State(app): Shared, Json(body): Json<Value>) -> Response {
    match app.sim.start_session(body["evcc"].clone(), body["secc"].clone()) {
        Ok(n) => Json(json!({ "session": n })).into_response(),
        Err(f) => sim_failure(f),
    }
}

/// A command for a simulator: `{target: "ev" | "evse", cmd: {...}}`.
async fn sim_command(State(app): Shared, Json(body): Json<Value>) -> Response {
    let target = match body["target"].as_str() {
        Some("ev") => sim::Target::Ev,
        Some("evse") => sim::Target::Evse,
        _ => return fail(StatusCode::BAD_REQUEST, "target: ev or evse"),
    };
    done(app.sim.command(target, &body["cmd"]))
}

async fn sim_close(State(app): Shared) -> Response {
    done(app.sim.close())
}

/// The state of the simulation whenever it changes (`state`) and the logs as they grow (`log`).
async fn sim_events(State(app): Shared) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, std::convert::Infallible>>(64);
    tokio::spawn(async move {
        let (mut seen, mut follower, mut watching, mut cursor) = (0, None, None, 0);
        loop {
            let version = app.sim.version();
            if version != seen {
                seen = version;
                let view = app.sim.view();
                // New logs for a new simulation.
                let dir = view.artifacts.clone();
                if dir != watching {
                    follower = dir.as_deref().and_then(|d| d.parent()).map(|p| runs::Follower::logs(p.to_path_buf()));
                    watching = dir;
                }
                let event = Event::default().event("state").data(serde_json::to_string(&view).unwrap_or_default());
                if tx.send(Ok(event)).await.is_err() {
                    return;
                }
            }
            let (events, next) = app.sim.events_since(cursor);
            cursor = next;
            for event in events {
                if tx.send(Ok(Event::default().event("msg").data(event.to_string()))).await.is_err() {
                    return;
                }
            }
            if let Some(follower) = &mut follower {
                for (file, text) in follower.poll().await.0 {
                    let event = Event::default().event("log").data(json!({ "file": file, "text": text }).to_string());
                    if tx.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    });
    Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default()).into_response()
}
