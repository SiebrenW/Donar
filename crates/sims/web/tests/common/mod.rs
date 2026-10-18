//! Shared by the web tests: starts `evcc-web` and talks HTTP to it with plain sockets.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

pub struct Server {
    child: Child,
    pub addr: String,
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

pub fn start() -> Server {
    let scenarios = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../scenarios/scenarios");
    // One work directory per server: the tests run side by side, and a server clears its own.
    static SERVERS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let work = target_dir().join("web-test").join(format!("{}-{}", std::process::id(), SERVERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
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
#[allow(dead_code)]
pub fn http(server: &Server, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(&server.addr).unwrap();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response.split(' ').nth(1).unwrap().parse().unwrap();
    let body = response.split_once("\r\n\r\n").map_or("", |(_, b)| b).to_string();
    (status, body)
}

