//! The simulation: a charger and a vehicle in a sandbox that stays open, a session at a time.
//! The privileged part is the `simulate` command (a child process speaking JSON lines); the
//! simulators are reached over their control sockets (`sim_control`). Everything the page
//! shows is the [`View`], rebuilt from what those three report.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

#[derive(Default, Serialize, Clone)]
pub struct View {
    pub open: bool,
    pub opening: bool,
    pub artifacts: Option<PathBuf>,
    /// The number of the current (or last) session; 0 before the first.
    pub session: u32,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub ev: Option<Value>,
    pub evse: Option<Value>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Target {
    Ev,
    Evse,
}

struct Helper {
    child: Child,
    stdin: ChildStdin,
}

#[derive(Default)]
struct Inner {
    helper: Option<Helper>,
    view: View,
    /// Writing halves of the control sockets.
    sockets: HashMap<Target, UnixStream>,
    /// Counts the simulations (the work directory name).
    opened: u32,
}

pub struct Sim {
    work: PathBuf,
    sudo: bool,
    inner: Mutex<Inner>,
    version: AtomicU64,
}

pub enum Failure {
    Bad(String),
    Conflict(String),
}

impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        Failure::Bad(e.to_string())
    }
}

impl Sim {
    pub fn new(work: PathBuf, sudo: bool) -> Arc<Sim> {
        Arc::new(Sim { work, sudo, inner: Mutex::default(), version: AtomicU64::new(1) })
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    pub fn view(&self) -> View {
        self.inner.lock().unwrap().view.clone()
    }

    fn update(&self, f: impl FnOnce(&mut Inner)) {
        f(&mut self.inner.lock().unwrap());
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// Opens the sandbox with its charger. `options` are `{tls, pki, slac}`.
    pub fn open(self: &Arc<Self>, options: Value) -> Result<(), Failure> {
        let mut inner = self.inner.lock().unwrap();
        if inner.helper.is_some() {
            return Err(Failure::Conflict("a simulation is open".into()));
        }
        inner.opened += 1;
        let dir = self.work.join(format!("sim-{}", inner.opened));
        std::fs::create_dir_all(&dir)?;
        let program = std::env::current_exe()?.with_file_name("simulate");
        let mut cmd = if self.sudo {
            let mut c = Command::new("sudo");
            c.arg("-E").arg(&program);
            c
        } else {
            Command::new(&program)
        };
        cmd.arg("--artifacts-root").arg(dir.join("artifacts"));
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(std::fs::File::create(dir.join("simulate.log"))?).spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut line = json!({ "cmd": "open" });
        for key in ["tls", "pki", "slac"] {
            if let Some(v) = options.get(key) {
                line[key] = v.clone();
            }
        }
        writeln!(stdin, "{line}")?;
        inner.helper = Some(Helper { child, stdin });
        inner.view = View { opening: true, ..View::default() };
        inner.sockets.clear();
        drop(inner);
        self.version.fetch_add(1, Ordering::Relaxed);
        let this = self.clone();
        std::thread::spawn(move || this.read_helper(stdout));
        Ok(())
    }

    /// What the helper reports, until it ends.
    fn read_helper(self: Arc<Self>, stdout: impl Read) {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Ok(event) = serde_json::from_str::<Value>(&line) else { continue };
            match event["event"].as_str() {
                Some("opened") => {
                    let artifacts = event["artifacts"].as_str().map(PathBuf::from);
                    self.update(|i| {
                        i.view.opening = false;
                        i.view.open = true;
                        i.view.artifacts = artifacts;
                    });
                    if let Some(socket) = event["secc_socket"].as_str() {
                        self.connect(Target::Evse, PathBuf::from(socket));
                    }
                }
                Some("session_started") => {
                    if let Some(socket) = event["socket"].as_str() {
                        self.connect(Target::Ev, PathBuf::from(socket));
                    }
                }
                Some("session_ended") => {
                    let code = event["code"].as_i64().map(|c| c as i32);
                    self.update(|i| {
                        i.view.running = false;
                        i.view.exit_code = code;
                        i.sockets.remove(&Target::Ev);
                    });
                }
                Some("error") => {
                    let message = event["message"].as_str().unwrap_or("error").to_string();
                    self.update(|i| {
                        i.view.error = Some(message);
                        i.view.opening = false;
                    });
                }
                _ => {}
            }
        }
        // The helper is gone: closed, or it failed.
        self.update(|i| {
            if let Some(mut h) = i.helper.take() {
                let _ = h.child.wait();
            }
            let error = i.view.error.take();
            i.view = View { error, ..View::default() };
            i.sockets.clear();
        });
    }

    /// Connects to a simulator's control socket (it appears a moment after the process
    /// starts) and keeps its state in the view.
    fn connect(self: &Arc<Self>, target: Target, path: PathBuf) {
        let this = self.clone();
        std::thread::spawn(move || {
            let started = Instant::now();
            let stream = loop {
                match UnixStream::connect(&path) {
                    Ok(s) => break s,
                    Err(_) if started.elapsed() < Duration::from_secs(15) => std::thread::sleep(Duration::from_millis(50)),
                    Err(_) => return,
                }
            };
            let Ok(writer) = stream.try_clone() else { return };
            this.update(|i| {
                i.sockets.insert(target, writer);
            });
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(state) = message.get("state") {
                    let state = state.clone();
                    this.update(|i| match target {
                        Target::Ev => i.view.ev = Some(state),
                        Target::Evse => i.view.evse = Some(state),
                    });
                } else if let Some(error) = message["error"].as_str() {
                    let error = error.to_string();
                    this.update(|i| i.view.error = Some(error));
                }
            }
        });
    }

    /// Sends a command to a simulator.
    pub fn command(&self, target: Target, cmd: &Value) -> Result<(), Failure> {
        let mut inner = self.inner.lock().unwrap();
        let socket = inner.sockets.get_mut(&target).ok_or_else(|| Failure::Conflict(format!("the {target:?} simulator is not connected")))?;
        writeln!(socket, "{cmd}")?;
        Ok(())
    }

    /// Starts a session: the charger's settings for it, then a vehicle with its own.
    pub fn start_session(&self, evcc: Value, secc: Value) -> Result<u32, Failure> {
        serde_json::from_value::<evcc_sim::Settings>(evcc.clone()).map_err(|e| Failure::Bad(format!("vehicle settings: {e}")))?;
        serde_json::from_value::<evse_sim::Config>(secc.clone()).map_err(|e| Failure::Bad(format!("charger settings: {e}")))?;
        let mut inner = self.inner.lock().unwrap();
        if !inner.view.open {
            return Err(Failure::Conflict("no simulation is open".into()));
        }
        if inner.view.running {
            return Err(Failure::Conflict("a session is running".into()));
        }
        // The charger takes its settings for the next session over its socket; a bad one is
        // answered there.
        let evse = inner.sockets.get_mut(&Target::Evse).ok_or_else(|| Failure::Conflict("the charger is not connected".into()))?;
        writeln!(evse, "{}", json!({ "cmd": "config", "session": secc }))?;
        let n = inner.view.session + 1;
        let helper = inner.helper.as_mut().ok_or_else(|| Failure::Conflict("no simulation is open".into()))?;
        writeln!(helper.stdin, "{}", json!({ "cmd": "session", "n": n, "settings": evcc }))?;
        inner.view.session = n;
        inner.view.running = true;
        inner.view.exit_code = None;
        inner.view.ev = None;
        inner.view.error = None;
        drop(inner);
        self.version.fetch_add(1, Ordering::Relaxed);
        Ok(n)
    }

    /// Ends the simulation: the helper removes the sandbox when its input ends.
    pub fn close(&self) -> Result<(), Failure> {
        let mut inner = self.inner.lock().unwrap();
        let helper = inner.helper.as_mut().ok_or_else(|| Failure::Conflict("no simulation is open".into()))?;
        // A held vehicle would keep the helper waiting for nothing; the kill is the helper's.
        writeln!(helper.stdin, "{}", json!({ "cmd": "close" }))?;
        Ok(())
    }
}
