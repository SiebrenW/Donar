//! The web page in a real browser: loaded, clicked through, and watched for exceptions. A script error anywhere in
//! the page (a name that does not exist, a failed build of a view) shows up here and nowhere else.
//!
//! The browser is Chrome or Chromium, driven over the DevTools protocol on a WebSocket. Found through `CHROME`, or by
//! name on the path. Without one the tests say so and pass, unless
//! `EVCC_REQUIRE_BROWSER` is set (the dev container sets it).

mod common;

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::{Server, start};
use serde_json::{Value, json};
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

fn find_browser() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CHROME") {
        return Some(PathBuf::from(path));
    }
    let names = ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable", "chrome"];
    let mut on_path = std::env::var_os("PATH").into_iter().flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).flat_map(|dir| names.iter().map(move |n| dir.join(n)));
    on_path.find(|p| p.exists())
}

/// A headless browser with one page, and what it reported.
struct Browser {
    child: Child,
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    /// Exceptions and `console.error` calls seen so far.
    errors: Vec<String>,
    profile: PathBuf,
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

fn launch() -> Option<Browser> {
    let Some(program) = find_browser() else {
        assert!(std::env::var_os("EVCC_REQUIRE_BROWSER").is_none(), "EVCC_REQUIRE_BROWSER is set and there is no Chrome or Chromium (set CHROME)");
        eprintln!("no Chrome or Chromium found: skipping the browser test");
        return None;
    };
    static COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let profile = std::env::temp_dir().join(format!("evcc-browser-{}-{}", std::process::id(), COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let mut child = Command::new(program)
        .args(["--headless=new", "--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage", "--no-first-run", "--remote-debugging-port=0", "--window-size=1400,1000"])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("about:blank")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the browser");
    // "DevTools listening on ws://127.0.0.1:PORT/devtools/browser/ID"
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let port = loop {
        let line = lines.next().expect("the browser ended before it listened").unwrap();
        if let Some(rest) = line.split("ws://127.0.0.1:").nth(1) {
            break rest.split('/').next().unwrap().to_string();
        }
    };
    // Keep reading its output so that it never blocks on a full pipe.
    std::thread::spawn(move || lines.for_each(drop));
    // The page target appears a moment after the browser listens.
    let deadline = Instant::now() + Duration::from_secs(15);
    let url = loop {
        let (_, list) = http_get(&format!("127.0.0.1:{port}"), "/json/list");
        let targets: Vec<Value> = serde_json::from_str(&list).unwrap_or_default();
        if let Some(url) = targets.iter().find(|t| t["type"] == "page").and_then(|t| t["webSocketDebuggerUrl"].as_str()) {
            break url.to_string();
        }
        assert!(Instant::now() < deadline, "the browser has no page target: {list}");
        std::thread::sleep(Duration::from_millis(100));
    };
    let (socket, _) = tungstenite::connect(url.as_str()).expect("connect to the page");
    let mut browser = Browser { child, socket, next_id: 0, errors: Vec::new(), profile };
    browser.call("Runtime.enable", json!({}));
    browser.call("Page.enable", json!({}));
    Some(browser)
}

/// One GET; the browser's debugging server may keep the connection open, so the body is read by its length.
fn http_get(addr: &str, path: &str) -> (u16, String) {
    use std::io::{Read, Write};
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: {addr}\r\n\r\n").unwrap();
    let mut data = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).expect("the browser answers the HTTP request");
        data.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&data);
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let length = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
            if body.len() >= length || n == 0 {
                let status = head.split(' ').nth(1).unwrap().parse().unwrap();
                return (status, body.to_string());
            }
        }
        assert!(n > 0, "the browser closed the connection early");
    }
}

impl Browser {
    fn note(&mut self, message: &Value) {
        match message["method"].as_str() {
            Some("Runtime.exceptionThrown") => {
                let d = &message["params"]["exceptionDetails"];
                let text = d["exception"]["description"].as_str().or(d["text"].as_str()).unwrap_or("exception");
                self.errors.push(format!("exception: {text} (line {})", d["lineNumber"]));
            }
            Some("Runtime.consoleAPICalled") if message["params"]["type"] == "error" => {
                let args: Vec<String> = message["params"]["args"].as_array().into_iter().flatten().map(|a| a["value"].as_str().map_or_else(|| a["description"].to_string(), str::to_string)).collect();
                self.errors.push(format!("console.error: {}", args.join(" ")));
            }
            Some("Log.entryAdded") if message["params"]["entry"]["level"] == "error" => self.errors.push(format!("log: {}", message["params"]["entry"]["text"])),
            _ => {}
        }
    }

    /// One protocol call; events that arrive meanwhile are noted.
    fn call(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.socket.send(Message::text(json!({ "id": id, "method": method, "params": params }).to_string())).unwrap();
        loop {
            let Message::Text(text) = self.socket.read().expect("the browser answers") else { continue };
            let message: Value = serde_json::from_str(&text).unwrap();
            if message["id"] == id {
                assert!(message.get("error").is_none(), "{method}: {message}");
                return message["result"].clone();
            }
            self.note(&message);
        }
    }

    /// Evaluates an expression in the page (promises are awaited); the value as JSON.
    fn eval(&mut self, expression: &str) -> Value {
        let r = self.call("Runtime.evaluate", json!({ "expression": expression, "awaitPromise": true, "returnByValue": true }));
        if let Some(d) = r.get("exceptionDetails") {
            let text = d["exception"]["description"].as_str().unwrap_or("exception");
            self.errors.push(format!("exception in {expression:?}: {text}"));
            return Value::Null;
        }
        r["result"]["value"].clone()
    }

    fn goto(&mut self, url: &str) {
        self.call("Page.navigate", json!({ "url": url }));
    }

    /// Waits until the expression is truthy; panics with what the page reported otherwise.
    fn wait_for(&mut self, what: &str, expression: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if self.eval(expression) == json!(true) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}; page errors: {:?}", self.errors);
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Lets the page settle, then fails on anything it reported.
    fn assert_no_errors(&mut self, when: &str) {
        std::thread::sleep(Duration::from_millis(200));
        self.eval("1");
        assert!(self.errors.is_empty(), "the page reported errors {when}: {:#?}", self.errors);
    }
}

fn open(server: &Server, fragment: &str) -> Option<Browser> {
    let mut b = launch()?;
    b.goto(&format!("http://{}/{fragment}", server.addr));
    Some(b)
}

#[test]
fn the_simulation_page_builds_with_all_its_controls() {
    let server = start();
    let Some(mut b) = open(&server, "#simulation") else { return };
    b.wait_for("the Start button", "!!document.querySelector('#primary')");
    for id in ["sim-protocol", "sim-energy", "sim-bpt", "sim-scheduled", "sim-mobility-needs", "primary", "step-start", "pilot-panel", "pilot-reading", "pilot-guard-ev", "pilot-guard-evse"] {
        assert_eq!(b.eval(&format!("!!document.getElementById('{id}')")), json!(true), "no element {id}");
    }
    // Three protocols, DC and AC.
    assert_eq!(b.eval("document.getElementById('sim-protocol').children.length"), json!(3));
    assert_eq!(b.eval("document.getElementById('sim-energy').children.length"), json!(2));
    b.assert_no_errors("while building the simulation page");
}

#[test]
fn every_choice_of_the_simulation_page_can_be_made() {
    let server = start();
    let Some(mut b) = open(&server, "#simulation") else { return };
    b.wait_for("the Start button", "!!document.querySelector('#primary')");
    // Each protocol, then within ISO 15118-20 each energy kind and each checkbox, in both states.
    let protocols = b.eval("document.getElementById('sim-protocol').children.length").as_u64().unwrap();
    for i in 0..protocols {
        b.eval(&format!("document.getElementById('sim-protocol').children[{i}].click()"));
        b.assert_no_errors(&format!("after choosing protocol button {i}"));
        // The page is rebuilt on each choice: it still has its controls.
        assert_eq!(b.eval("!!document.getElementById('primary') && !!document.getElementById('sim-bpt')"), json!(true), "controls missing after protocol button {i}");
    }
    b.eval("document.getElementById('sim-protocol').children[0].click()");
    for energy in 0..2 {
        b.eval(&format!("document.getElementById('sim-energy').children[{energy}].click()"));
        for checkbox in ["sim-bpt", "sim-scheduled", "sim-mobility-needs"] {
            for _ in 0..2 {
                b.eval(&format!("document.getElementById('{checkbox}').click()"));
                b.assert_no_errors(&format!("after toggling {checkbox} (energy kind {energy})"));
            }
        }
    }
    // Presets, if the page has them as buttons in a row.
    let presets = b.eval("document.querySelectorAll('.presets button, #presets button').length").as_u64().unwrap_or(0);
    for i in 0..presets {
        b.eval(&format!("document.querySelectorAll('.presets button, #presets button')[{i}].click()"));
        b.assert_no_errors(&format!("after preset {i}"));
    }
}

#[test]
fn the_start_button_and_state_stay_in_view_on_every_tab_and_the_connection_follows_the_choice() {
    let server = start();
    let Some(mut b) = open(&server, "#simulation") else { return };
    b.wait_for("the Start button", "!!document.querySelector('#primary')");
    // The top (picture, readouts, status, Start) is outside the tab panes: every tab leaves it visible.
    let tabs = b.eval("document.querySelectorAll('.subtabs button').length").as_u64().unwrap();
    assert!(tabs >= 5, "{tabs} tabs");
    for i in 0..tabs {
        b.eval(&format!("document.querySelectorAll('.subtabs button')[{i}].click()"));
        assert_eq!(b.eval("document.querySelectorAll('.tabbody > [data-pane]:not([hidden])').length"), json!(1), "one pane shows on tab {i}");
        assert_eq!(b.eval("(() => { const r = document.getElementById('primary').getBoundingClientRect(); return r.width > 0 && r.top >= 0 && r.bottom <= innerHeight; })()"), json!(true), "Start not in view on tab {i}");
        assert_eq!(b.eval("document.getElementById('sim-status').getBoundingClientRect().width > 0"), json!(true), "no status on tab {i}");
        b.assert_no_errors(&format!("after tab {i}"));
    }
    // Plug is the connection the simulation runs; ACDP and OppCharge change the picture and the fields, and cannot be started yet.
    b.eval("document.getElementById('subtab-setup').click()");
    for (mode, protocol) in [("acdp", 0), ("opp", 1)] {
        b.eval(&format!("document.getElementById('mode-{mode}').click()"));
        assert_eq!(b.eval("document.getElementById('primary').disabled"), json!(true), "{mode} cannot be started");
        assert_eq!(b.eval("document.getElementById('sim-energy').hidden"), json!(true), "{mode} has no AC/DC choice");
        assert_eq!(b.eval(&format!("document.getElementById('sim-protocol').children[{protocol}].classList.contains('on')")), json!(true), "{mode} sets its protocol");
        assert_eq!(b.eval("!!document.querySelector('#sceneview svg.scene3d')"), json!(true), "{mode} has its picture");
        b.assert_no_errors(&format!("after choosing {mode}"));
    }
    b.eval("document.getElementById('mode-plug').click()");
    assert_eq!(b.eval("document.getElementById('primary').disabled"), json!(false));
    b.assert_no_errors("after going back to the plug");
}

#[test]
fn the_scenarios_page_lists_the_scenarios() {
    let server = start();
    let Some(mut b) = open(&server, "") else { return };
    b.wait_for("the scenario list", "(document.body && document.body.innerText.includes('dc-plain'))");
    b.assert_no_errors("while building the scenarios page");
    // The tabs switch without errors.
    b.eval("location.hash = '#simulation'");
    b.wait_for("the simulation view", "!!document.querySelector('#primary')");
    b.eval("location.hash = '#scenarios'");
    b.assert_no_errors("after switching the tabs");
}

#[test]
fn a_run_ends_with_its_verdict_and_the_run_button_works_again() {
    let server = start();
    let Some(mut b) = open(&server, "") else { return };
    b.wait_for("the scenario list", "(document.body && document.body.innerText.includes('ac-plain'))");
    b.eval("[...document.querySelectorAll('a')].find(a => a.textContent.includes('ac-plain')).click()");
    b.wait_for("the scenario page", "!!document.getElementById('run')");
    b.eval("document.getElementById('run').click()");
    b.wait_for("the verdict", "/^PASS/m.test(document.getElementById('verdict').textContent)");
    b.wait_for("the run button to work again", "!document.getElementById('run').disabled");
    // Nothing is delivered twice: every log pane holds each line of its file once, however long we look.
    std::thread::sleep(Duration::from_secs(8));
    let listening = b.eval("[...document.querySelectorAll('#logs .log')].map(d => [d.querySelector('h3').textContent, d.querySelector('pre').textContent.split('listening on veth0').length - 1])");
    assert_eq!(listening.as_array().unwrap().iter().filter(|p| p[0] == "evcc-tcpdump.log").map(|p| p[1].as_u64().unwrap()).collect::<Vec<_>>(), [1], "{listening}");
    b.assert_no_errors("after a run");
}
