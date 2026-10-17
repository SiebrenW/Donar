//! `evse-sim`: a simulated charger (SECC) as a process: SLAC responder (optional), SDP answers
//! on the interface, TLS (optional) and a DC session per connection with the knobs of the
//! settings file.
//!
//! ```text
//! evse-sim --interface eth1 --address fe80::2 [--config FILE] [--slac]
//!          [--chain F --key F --trust F [--key-password P] [--backwards-compatible]]
//!          [--sessions N] [--control SOCKET] [--ocsp FILE] [--pnc-trust FILE]
//! ```
//! `--control` serves the live state and commands of a test platform on a Unix socket
//! (`sim_control`): measurements to set, and the settings of the next session.
//! `--ocsp` names a DER OCSP response for the charger's certificate, stapled when the vehicle asks.
//! `--pnc-trust` names the PEM trust anchors (eMSP roots) for the contract chains of Plug & Charge.
//! TLS is on when `--chain` is given. `--sessions N` serves N sessions one after the other
//! (default: the settings file's `sessions`, 1; 0 for ever). The settings file is `evse_sim::Settings` in TOML. Exit status 0
//! when every session ended with `SessionStop`, 1 when one did not, 2 for bad arguments.

use std::io;
use std::net::{Ipv6Addr, TcpListener, UdpSocket};
use std::process::ExitCode;

use evse_sim::{Settings, State};
use v2gtp::sdp::{Response, Security, Transport};

struct Args {
    interface: String,
    address: Ipv6Addr,
    config: Option<String>,
    slac: bool,
    chain: Option<String>,
    key: Option<String>,
    trust: Option<String>,
    key_password: Option<String>,
    backwards_compatible: bool,
    sessions: Option<u32>,
    control: Option<String>,
    ocsp: Option<String>,
    pnc_trust: Option<String>,
}

const USAGE: &str = "usage: evse-sim --interface IF --address ADDR [--config FILE] [--slac] [--chain F --key F --trust F [--key-password P] [--backwards-compatible]] [--sessions N] [--control SOCKET] [--ocsp FILE] [--pnc-trust FILE]";

fn parse() -> Result<Args, String> {
    let mut a = Args {
        interface: String::new(),
        address: Ipv6Addr::UNSPECIFIED,
        config: None,
        slac: false,
        chain: None,
        key: None,
        trust: None,
        key_password: None,
        backwards_compatible: false,
        sessions: None,
        control: None,
        ocsp: None,
        pnc_trust: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--interface" => a.interface = value("--interface")?,
            "--address" => {
                a.address = value("--address")?
                    .parse()
                    .map_err(|_| "--address: not an IPv6 address")?
            }
            "--config" => a.config = Some(value("--config")?),
            "--control" => a.control = Some(value("--control")?),
            "--ocsp" => a.ocsp = Some(value("--ocsp")?),
            "--pnc-trust" => a.pnc_trust = Some(value("--pnc-trust")?),
            "--slac" => a.slac = true,
            "--chain" => a.chain = Some(value("--chain")?),
            "--key" => a.key = Some(value("--key")?),
            "--trust" => a.trust = Some(value("--trust")?),
            "--key-password" => a.key_password = Some(value("--key-password")?),
            "--backwards-compatible" => a.backwards_compatible = true,
            "--sessions" => {
                a.sessions = Some(
                    value("--sessions")?
                        .parse()
                        .map_err(|_| "--sessions: not a number")?,
                )
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if a.interface.is_empty() || a.address.is_unspecified() {
        return Err("--interface and --address are required".into());
    }
    if a.chain.is_some() && (a.key.is_none() || a.trust.is_none()) {
        return Err("TLS needs --chain, --key and --trust".into());
    }
    Ok(a)
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("evse-sim: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("evse-sim: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> io::Result<bool> {
    let mut settings: Settings = match &args.config {
        Some(path) => toml::from_str(&std::fs::read_to_string(path)?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{path}: {e}")))?,
        None => Settings::default(),
    };
    if let Some(path) = &args.pnc_trust {
        settings.session.pnc.trust_pem = Some(std::fs::read_to_string(path)?);
    }
    let context = match &args.chain {
        Some(_) => {
            let read = |p: &Option<String>| std::fs::read(p.as_deref().unwrap());
            let (chain, key, trust) = (read(&args.chain)?, read(&args.key)?, read(&args.trust)?);
            let ocsp = args.ocsp.as_deref().map(std::fs::read).transpose()?;
            let mut credentials = evcc_linux::tls::Credentials::new(&chain, &key, &trust);
            if let Some(ocsp) = &ocsp {
                credentials = credentials.ocsp_response(ocsp);
            }
            if let Some(password) = &args.key_password {
                credentials = credentials.key_password(password.as_bytes());
            }
            if args.backwards_compatible || settings.backwards_compatible {
                credentials = credentials.backwards_compatible();
            }
            credentials = credentials.versions(settings.tls_versions.into());
            Some(evcc_linux::tls::server_context(&credentials).map_err(io::Error::other)?)
        }
        None => None,
    };
    if args.slac || settings.slac_responder {
        start_slac(&args.interface, settings.slac.clone(), settings.control_pilot_file.clone())?;
    }
    let ifindex = evcc_linux::sdp::interface_index(&args.interface)?;
    let udp = UdpSocket::bind((Ipv6Addr::UNSPECIFIED, v2gtp::sdp::SDP_PORT))?;
    let tcp = TcpListener::bind((Ipv6Addr::UNSPECIFIED, 0))?;
    let response = Response {
        address: args.address.octets(),
        port: tcp.local_addr()?.port(),
        security: if context.is_some() {
            Security::Tls
        } else {
            Security::NoTls
        },
        transport: Transport::Tcp,
    };
    eprintln!("ready: {}", evcc_linux::sdp::address(&response, ifindex));
    let live = match &args.control {
        Some(path) => {
            let live = evse_sim::EvseLive::new(settings.session.clone());
            sim_control::serve(std::path::Path::new(path), live.clone())?;
            Some(live)
        }
        None => None,
    };
    let mut served = 0;
    let mut all_stopped = true;
    let sessions = args.sessions.unwrap_or(settings.sessions);
    while sessions == 0 || served < sessions {
        let seen = evse_sim::answer_discovery(&udp, response, 0)?;
        eprintln!("sdp: answered after {seen} request(s)");
        let state = match &context {
            Some(context) => {
                let (stream, _) = tcp.accept()?;
                // A connection that fails the handshake or the certificate checks is a lost
                // session, not the end of the charger.
                let session = || -> io::Result<State> {
                    let mut stream = evcc_linux::tls::accept(context, stream)?;
                    evcc_linux::tls::check_peer_certificates(&stream, evcc_linux::certificates::Peer::Vehicle, settings.certificate_checks.into(), |v| eprintln!("certificates: {v}"))?;
                    eprintln!("tls: {} {}", stream.ssl().version_str(), stream.ssl().current_cipher().map_or("?", |c| c.name()));
                    match &live {
                        Some(live) => evse_sim::serve_live(&mut stream, live),
                        None => evse_sim::serve(&mut stream, settings.session.clone()),
                    }
                };
                session().unwrap_or_else(|e| {
                    eprintln!("tls: {e}");
                    State::Closed
                })
            }
            None => match &live {
                Some(live) => evse_sim::serve_live(&mut tcp.accept()?.0, live)?,
                None => evse_sim::serve_one(&tcp, settings.session.clone())?,
            },
        };
        if let Some(live) = &live {
            live.set_state("Waiting");
        }
        eprintln!("session: {state:?}");
        all_stopped &= matches!(state, State::Stopped | State::Paused);
        served += 1;
    }
    Ok(all_stopped)
}

#[cfg(target_os = "linux")]
fn start_slac(interface: &str, config: evse_sim::slac::Config, pilot: Option<String>) -> io::Result<()> {
    use evcc_linux::slac::PacketSocket;
    let socket = PacketSocket::open(interface)?;
    let mut evse = evse_sim::slac::Evse::new(evse_sim::slac::Config {
        mac: socket.mac(),
        ..config
    });
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        let mut matched = false;
        loop {
            let Ok(Some(n)) = socket.recv(&mut buf, Some(std::time::Duration::from_millis(100)))
            else {
                continue;
            };
            // Toggles are counted from here on (the file may hold those of an earlier run).
            let before = pilot.as_deref().and_then(|p| std::fs::metadata(p).ok()).map_or(0, |m| m.len());
            for frame in evse.handle(&buf[..n]) {
                let _ = socket.send(&frame);
            }
            // The vehicle asked for the validation by BCB-Toggle: listen, count, answer.
            if let Some(ms) = evse.validation_window_ms() {
                std::thread::sleep(std::time::Duration::from_millis(ms.into()));
                let after = pilot.as_deref().and_then(|p| std::fs::metadata(p).ok()).map_or(0, |m| m.len());
                eprintln!("slac: validation, {} toggles counted", after - before);
                for frame in evse.finish_validation((after - before).min(255) as u8) {
                    let _ = socket.send(&frame);
                }
            }
            if !matched && matches!(evse.state(), evse_sim::slac::State::Matched { .. }) {
                matched = true;
                eprintln!("slac: matched");
            }
        }
    });
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn start_slac(_interface: &str, _config: evse_sim::slac::Config, _pilot: Option<String>) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SLAC needs Linux",
    ))
}
