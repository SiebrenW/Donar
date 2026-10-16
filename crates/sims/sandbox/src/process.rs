//! Participants: processes started in a node as the sandbox's user, with a log file.

use std::ffi::OsString;
use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::{Node, User, ns};

/// What to start: the program, its arguments and environment, and a label for the log file.
#[derive(Debug, Clone)]
pub struct Spawn {
    program: OsString,
    args: Vec<OsString>,
    env: Vec<(OsString, OsString)>,
    cwd: Option<PathBuf>,
    label: Option<String>,
}

impl Spawn {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self { program: program.into(), args: Vec::new(), env: Vec::new(), cwd: None, label: None }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I: IntoIterator<Item = S>, S: Into<OsString>>(mut self, args: I) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.cwd = Some(dir.into());
        self
    }

    /// Names the log file (`<node>-<label>.log`); default: the program's file name.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

/// A running (or finished) participant. Dropping it kills it and everything it started.
pub struct Process {
    child: Child,
    log: PathBuf,
}

impl Process {
    pub(crate) fn start(node: Option<&Node>, artifacts: &Path, user: Option<User>, spec: Spawn) -> io::Result<Process> {
        let label = spec.label.clone().unwrap_or_else(|| {
            Path::new(&spec.program).file_name().map_or_else(|| "process".into(), |n| n.to_string_lossy().into_owned())
        });
        let log = artifacts.join(format!("{}-{label}.log", node.map_or("host", Node::name)));
        let out = File::create(&log)?;
        ns::chown(&log, user)?;
        let err = out.try_clone()?;
        // `ip netns exec NS` enters the namespace (and gives the process the namespace's view
        // of /sys); `setpriv` then drops root before the program starts.
        let mut cmd;
        let tail = |cmd: &mut Command| {
            cmd.arg(&spec.program).args(&spec.args);
        };
        match (node, user) {
            (Some(n), Some(u)) => {
                cmd = Command::new("ip");
                cmd.args(["netns", "exec", &n.namespace, "setpriv", &format!("--reuid={}", u.uid), &format!("--regid={}", u.gid), "--clear-groups", "--"]);
                tail(&mut cmd);
            }
            (Some(n), None) => {
                cmd = Command::new("ip");
                cmd.args(["netns", "exec", &n.namespace]);
                tail(&mut cmd);
            }
            (None, Some(u)) => {
                cmd = Command::new("setpriv");
                cmd.args([&format!("--reuid={}", u.uid), &format!("--regid={}", u.gid), "--clear-groups", "--"]);
                tail(&mut cmd);
            }
            (None, None) => {
                cmd = Command::new(&spec.program);
                cmd.args(&spec.args);
            }
        }
        cmd.envs(spec.env.iter().map(|(k, v)| (k, v)));
        if let Some(dir) = &spec.cwd {
            cmd.current_dir(dir);
        }
        // Its own process group, so that killing it also kills what it started.
        cmd.stdin(Stdio::null()).stdout(out).stderr(err).process_group(0);
        let child = cmd.spawn().map_err(|e| io::Error::new(e.kind(), format!("cannot start {:?}: {e}", spec.program)))?;
        Ok(Process { child, log })
    }

    pub fn id(&self) -> u32 {
        self.child.id()
    }

    /// Path of the log file (standard output and error together).
    pub fn log_path(&self) -> &Path {
        &self.log
    }

    /// Everything the process has written so far.
    pub fn log(&self) -> String {
        String::from_utf8_lossy(&std::fs::read(&self.log).unwrap_or_default()).into_owned()
    }

    /// The exit status if the process has ended.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Waits for the process to end, for at most `timeout`; `None` if it is still running.
    pub fn wait_timeout(&mut self, timeout: Duration) -> io::Result<Option<ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits until the log contains `needle`; false on timeout. An error if the process ends
    /// before that without having written it.
    pub fn wait_for_log(&mut self, needle: &str, timeout: Duration) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.log().contains(needle) {
                return Ok(true);
            }
            if let Some(status) = self.child.try_wait()? {
                // One last look: the output may have arrived just before the exit.
                if self.log().contains(needle) {
                    return Ok(true);
                }
                return Err(io::Error::new(ErrorKind::UnexpectedEof, format!("the process ended ({status}) before logging {needle:?}")));
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Kills the process and its group.
    pub fn kill(&mut self) {
        // SAFETY: killing a process group we created; the pid is that of our own child.
        unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.kill();
        }
    }
}
