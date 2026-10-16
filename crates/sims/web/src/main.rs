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
}

type Shared = State<Arc<App>>;

fn fail(status: StatusCode, message: impl ToString) -> Response {
    (status, Json(json!({ "error": message.to_string() }))).into_response()
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let mut listen: SocketAddr = ([127, 0, 0, 1], 8080).into();
    let mut scenarios = PathBuf::from("crates/sims/scenarios/scenarios");
    let mut work = PathBuf::from("target/web");
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
    let runs = match Runs::new(work, sudo) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("evcc-web: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let app = Arc::new(App { scenarios, runs });
    let router = Router::new()
        .route("/", get(|| async { Html(include_str!("index.html")) }))
        .route("/api/schema", get(schema))
        .route("/api/scenarios", get(list))
        .route("/api/scenarios/{name}", get(show))
        .route("/api/runs", post(start))
        .route("/api/runs/{id}/events", get(events))
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
async fn schema() -> Json<Value> {
    let part = |schema: schemars::Schema, defaults: Value| json!({ "schema": schema, "defaults": defaults });
    Json(json!({
        "evcc": part(schemars::schema_for!(evcc_sim::Settings), serde_json::to_value(evcc_sim::Settings::default()).unwrap()),
        "secc": part(schemars::schema_for!(evse_sim::Settings), serde_json::to_value(evse_sim::Settings::default()).unwrap()),
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
