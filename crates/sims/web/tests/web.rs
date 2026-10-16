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
