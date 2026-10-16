//! The control channel of a running simulator (`--control SOCKET`): a Unix socket speaking JSON
//! lines. The simulator pushes its whole live state whenever it changes (`{"state": ...}`), a
//! client sends commands (`{"cmd": ...}`) and gets `{"error": "..."}` for one that failed.
//! Used by the web UI; the socket sits in the artifacts directory, owned by the simulator's user.

use std::io::{self, ErrorKind, Read, Write};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a simulator offers on its socket.
pub trait Endpoint: Send + Sync + 'static {
    /// The whole live state; sent again whenever it differs from what was sent last.
    fn snapshot(&self) -> Value;
    /// One command.
    fn command(&self, cmd: Value) -> Result<(), String>;
}

/// Listens on `path` (replacing a socket left by an earlier run) in a thread of its own; every
/// client gets a thread too.
pub fn serve(path: &Path, endpoint: Arc<dyn Endpoint>) -> io::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let endpoint = endpoint.clone();
            std::thread::spawn(move || {
                let _ = client(stream, &*endpoint);
            });
        }
    });
    Ok(())
}

const TICK: Duration = Duration::from_millis(80);

fn client(mut stream: std::os::unix::net::UnixStream, endpoint: &dyn Endpoint) -> io::Result<()> {
    stream.set_read_timeout(Some(TICK))?;
    let (mut pending, mut last) = (Vec::new(), String::new());
    let mut chunk = [0u8; 1024];
    loop {
        let line = serde_json::json!({ "state": endpoint.snapshot() }).to_string();
        if line != last {
            writeln!(stream, "{line}")?;
            last = line;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(n) => pending.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(e) => return Err(e),
        }
        while let Some(end) = pending.iter().position(|&b| b == b'\n') {
            let text: Vec<u8> = pending.drain(..=end).collect();
            let result = serde_json::from_slice::<Value>(&text).map_err(|e| e.to_string()).and_then(|v| endpoint.command(v));
            if let Err(message) = result {
                writeln!(stream, "{}", serde_json::json!({ "error": message }))?;
            }
        }
    }
}

/// A live value that is either modelled (auto) or held at a value you set.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Metric {
    pub auto: bool,
    /// The value used while `auto` is off.
    pub manual: f64,
    /// What was last sent (the model's value, or the manual one): what the peer saw.
    pub effective: f64,
}

impl Metric {
    pub fn new(manual: f64) -> Self {
        Self { auto: true, manual, effective: manual }
    }

    /// The value to use now: the manual one, or `modelled`; remembered as the effective one.
    pub fn resolve(&mut self, modelled: f64) -> f64 {
        self.effective = if self.auto { modelled } else { self.manual };
        self.effective
    }

    /// Applies `{"auto": bool, "value": number}`; either part may be missing.
    pub fn apply(&mut self, cmd: &Value) {
        if let Some(auto) = cmd.get("auto").and_then(Value::as_bool) {
            self.auto = auto;
            if !auto && cmd.get("value").is_none() {
                // Going manual keeps the value the peer sees now.
                self.manual = self.effective;
            }
        }
        if let Some(v) = cmd.get("value").and_then(Value::as_f64) {
            self.manual = v;
            self.auto = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    use std::sync::Mutex;

    struct Counter(Mutex<i64>);
    impl Endpoint for Counter {
        fn snapshot(&self) -> Value {
            json!({ "n": *self.0.lock().unwrap() })
        }
        fn command(&self, cmd: Value) -> Result<(), String> {
            match cmd.get("add").and_then(Value::as_i64) {
                Some(n) => {
                    *self.0.lock().unwrap() += n;
                    Ok(())
                }
                None => Err("no add".into()),
            }
        }
    }

    #[test]
    fn state_is_pushed_on_change_and_commands_are_applied() {
        let dir = std::env::temp_dir().join(format!("sim-control-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.sock");
        serve(&path, Arc::new(Counter(Mutex::new(0)))).unwrap();
        let stream = UnixStream::connect(&path).unwrap();
        let mut writer = stream.try_clone().unwrap();
        let mut lines = BufReader::new(stream).lines();
        assert_eq!(lines.next().unwrap().unwrap(), r#"{"state":{"n":0}}"#);
        writeln!(writer, r#"{{"add":5}}"#).unwrap();
        assert_eq!(lines.next().unwrap().unwrap(), r#"{"state":{"n":5}}"#);
        writeln!(writer, r#"{{"nothing":1}}"#).unwrap();
        assert_eq!(lines.next().unwrap().unwrap(), r#"{"error":"no add"}"#);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_metric_follows_the_model_until_set() {
        let mut m = Metric::new(10.0);
        assert_eq!(m.resolve(42.0), 42.0);
        m.apply(&json!({ "auto": false }));
        assert_eq!(m.resolve(50.0), 42.0, "going manual keeps what the peer saw");
        m.apply(&json!({ "value": 7 }));
        assert_eq!((m.auto, m.resolve(50.0)), (false, 7.0));
        m.apply(&json!({ "auto": true }));
        assert_eq!(m.resolve(50.0), 50.0);
    }
}
