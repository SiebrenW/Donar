//! Runs: the `scenario` command as a child process, its output and the log files it makes.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

pub enum StartError {
    Busy,
    Io(io::Error),
}

impl From<io::Error> for StartError {
    fn from(e: io::Error) -> Self {
        StartError::Io(e)
    }
}

/// What the child said (standard output and error) and whether it has ended.
#[derive(Default)]
struct Outcome {
    output: String,
    done: bool,
}

struct Run {
    dir: PathBuf,
    outcome: Arc<Mutex<Outcome>>,
}

pub struct Runs {
    work: PathBuf,
    sudo: bool,
    all: Mutex<HashMap<u32, Run>>,
}

impl Runs {
    pub fn new(work: PathBuf, sudo: bool) -> io::Result<Runs> {
        std::fs::create_dir_all(&work)?;
        Ok(Runs { work: work.canonicalize()?, sudo, all: Mutex::default() })
    }

    /// Starts the scenario in `text` unless a run is going on. Everything lands in
    /// `<work>/run-<id>/`: the scenario file and, below `artifacts/`, the logs.
    pub fn start(&self, text: &str) -> Result<u32, StartError> {
        let mut all = self.all.lock().unwrap();
        if all.values().any(|r| !r.outcome.lock().unwrap().done) {
            return Err(StartError::Busy);
        }
        let id = all.keys().max().map_or(1, |m| m + 1);
        let dir = self.work.join(format!("run-{id}"));
        std::fs::create_dir_all(&dir)?;
        let file = dir.join("scenario.toml");
        std::fs::write(&file, text)?;
        let program = std::env::current_exe()?.with_file_name("scenario");
        let mut cmd = if self.sudo {
            let mut c = tokio::process::Command::new("sudo");
            // `-E`: the sandbox finds the user to drop to in the environment sudo sets.
            c.arg("-E").arg(&program);
            c
        } else {
            tokio::process::Command::new(&program)
        };
        cmd.arg("--artifacts-root").arg(dir.join("artifacts")).arg(&file);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(false);
        let mut child = cmd.spawn()?;
        let outcome = Arc::new(Mutex::new(Outcome::default()));
        let (out, err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
        let shared = outcome.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let collect = |reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>, shared: Arc<Mutex<Outcome>>| async move {
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut o = shared.lock().unwrap();
                    o.output.push_str(&line);
                    o.output.push('\n');
                }
            };
            let (a, b) = (tokio::spawn(collect(Box::new(out), shared.clone())), tokio::spawn(collect(Box::new(err), shared.clone())));
            let status = child.wait().await;
            let _ = a.await;
            let _ = b.await;
            let mut o = shared.lock().unwrap();
            match status {
                Ok(s) if !s.success() && !o.output.contains("FAIL") => o.output.push_str(&format!("scenario exited with {s}\n")),
                Err(e) => o.output.push_str(&format!("scenario: {e}\n")),
                _ => {}
            }
            o.done = true;
        });
        all.insert(id, Run { dir, outcome });
        Ok(id)
    }

    pub fn follow(&self, id: u32) -> Option<Follower> {
        let all = self.all.lock().unwrap();
        let run = all.get(&id)?;
        Some(Follower { artifacts: run.dir.join("artifacts"), outcome: Some(run.outcome.clone()), offsets: HashMap::new() })
    }
}

/// One client's position in the logs of a run.
pub struct Follower {
    artifacts: PathBuf,
    /// Absent for logs that have no end (a simulation).
    outcome: Option<Arc<Mutex<Outcome>>>,
    offsets: HashMap<PathBuf, u64>,
}

impl Follower {
    /// Follows the logs below `artifacts` (a directory with one directory per run) without
    /// a verdict.
    pub fn logs(artifacts: PathBuf) -> Follower {
        Follower { artifacts, outcome: None, offsets: HashMap::new() }
    }

    /// New log text since the last call, by file name, and the verdict once the run has ended
    /// and everything was delivered.
    pub async fn poll(&mut self) -> (Vec<(String, String)>, Option<String>) {
        // Read `done` before the files, so that nothing written before the end is missed.
        let (done, output) = match &self.outcome {
            Some(o) => {
                let o = o.lock().unwrap();
                (o.done, o.output.clone())
            }
            None => (false, String::new()),
        };
        let mut chunks = Vec::new();
        for path in log_files(&self.artifacts) {
            let offset = self.offsets.entry(path.clone()).or_insert(0);
            let Ok(bytes) = tokio::fs::read(&path).await else { continue };
            if (bytes.len() as u64) > *offset {
                // Only whole lines, so a line is never split between two events.
                let new = &bytes[*offset as usize..];
                let end = if done { new.len() } else { new.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1) };
                if end > 0 {
                    *offset += end as u64;
                    let name = path.file_name().unwrap().to_string_lossy().into_owned();
                    chunks.push((name, String::from_utf8_lossy(&new[..end]).into_owned()));
                }
            }
        }
        (chunks, done.then_some(output))
    }
}

/// `<artifacts>/<run id>/*.log`, in name order.
fn log_files(artifacts: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(runs) = std::fs::read_dir(artifacts) else { return files };
    for run in runs.flatten() {
        if let Ok(entries) = std::fs::read_dir(run.path()) {
            files.extend(entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "log")));
        }
    }
    files.sort();
    files
}
