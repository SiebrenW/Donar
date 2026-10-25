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
            if settings.disclose_secrets {
                credentials = credentials.disclose_secrets(evcc_linux::tls::Disclosure::on_interface(evcc_linux::sdp::interface_index(&args.interface)?)?);
            }
            Some(evcc_linux::tls::server_context(&credentials).map_err(io::Error::other)?)
        }
        None => None,
    };
    if args.slac || settings.slac_responder {
        start_slac(&args.interface, settings.slac.clone(), settings.slac_modem, settings.control_pilot_file.clone(), settings.control_pilot_wire.clone())?;
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
    // The control pilot: the oscillator is on (5 %) from the start, until a session ends in an emergency of the charger.
    let wire = match &settings.control_pilot_wire {
        Some(dir) => {
            let wire = evcc_linux::pilot::Wire::new(dir)?;
            wire.set_evse(control_pilot::Pwm::Duty(settings.nominal_duty_percent.unwrap_or(5)))?;
            if let Some(live) = &live {
                live.set_wire(wire.clone());
            }
            Some(wire)
        }
        None => None,
    };
    if let Some(duty) = settings.nominal_duty_percent {
        let wire = wire.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "nominal_duty_percent needs the control pilot wire"))?;
        return basic_charging(wire, duty);
    }
    let mut served = 0;
    let mut all_stopped = true;
    // After an emergency of its own the oscillator stays off until the next vehicle asks: the one that was connected needs
    // time to see it (a charger that turned it on again at once would look like one that only dropped the connection).
    let mut emergency = false;
    let sessions = args.sessions.unwrap_or(settings.sessions);
    while sessions == 0 || served < sessions {
        // An ACDP charger answers the wireless SDP `Ongoing` until its PPD reads the EVID of the vehicle, and pairs with that one.
        let mut session_config = settings.session.clone();
        let seen = match session_config.acdp.as_mut() {
            Some(acdp) => {
                acdp.paired_evid = evse_sim::answer_wireless_discovery(&udp, response, acdp)?;
                1
            }
            None => evse_sim::answer_discovery(&udp, response, 0)?,
        };
        eprintln!("sdp: answered after {seen} request(s)");
        if let (true, Some(wire)) = (std::mem::take(&mut emergency), &wire) {
            wire.set_evse(control_pilot::Pwm::Duty(5))?;
        }
        let state = match &context {
            Some(context) => {
                let (stream, _) = tcp.accept()?;
                // A connection that fails the handshake or the certificate checks is a lost
                // session, not the end of the charger.
                let session = || -> io::Result<State> {
                    let mut stream = evcc_linux::tls::accept(context, stream)?;
                    evcc_linux::tls::check_peer_certificates(&stream, evcc_linux::certificates::Peer::Vehicle, settings.certificate_checks.into(), |v| eprintln!("certificates: {v}"))?;
                    eprintln!("tls: {} {}", stream.ssl().version_str(), stream.ssl().current_cipher().map_or("?", |c| c.name()));
                    match (&wire, &live) {
                        (Some(wire), _) => evse_sim::serve_wired(&mut stream, session_config.clone(), live.as_ref(), wire),
                        (None, Some(live)) => evse_sim::serve_live(&mut stream, live),
                        (None, None) => evse_sim::serve(&mut stream, session_config.clone()),
                    }
                };
                session().unwrap_or_else(|e| {
                    eprintln!("tls: {e}");
                    State::Closed
                })
            }
            None => match (&wire, &live) {
                (Some(wire), _) => evse_sim::serve_wired(&mut tcp.accept()?.0, session_config.clone(), live.as_ref(), wire)?,
                (None, Some(live)) => evse_sim::serve_live(&mut tcp.accept()?.0, live)?,
                (None, None) => evse_sim::serve_one(&tcp, session_config.clone())?,
            },
        };
        if let Some(wire) = &wire {
            emergency = wire.evse() == Some(evcc_linux::pilot::Oscillator::Off);
            if !emergency {
                wire.set_evse(control_pilot::Pwm::Duty(5))?;
            }
        }
        if let Some(live) = &live {
            live.set_state("Waiting");
        }
        eprintln!("session: {state:?}");
        all_stopped &= matches!(state, State::Stopped | State::Paused);
        served += 1;
    }
    Ok(all_stopped)
}

/// A charger that offers a nominal duty cycle: basic charging by IEC 61851-1 alone. The contactor closes when the vehicle
/// holds C (or D) and opens when it leaves it; there is no SDP and no session.
#[cfg(target_os = "linux")]
fn basic_charging(wire: &evcc_linux::pilot::Wire, duty: u8) -> io::Result<bool> {
    use std::time::{Duration, Instant};
    let offered = control_pilot::Pwm::Duty(duty).offered_current_tenths().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "nominal_duty_percent is 10 to 96"))?;
    eprintln!("basic charging: {}.{} A offered by a duty cycle of {duty} %, no high level communication", offered / 10, offered % 10);
    let wait = |until: &dyn Fn(control_pilot::Reading) -> bool, limit: Duration| {
        let end = Instant::now() + limit;
        loop {
            if let Some(reading) = wire.reading().filter(|r| until(*r)) {
                return Some(reading);
            }
            if Instant::now() > end {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    if wait(&|r| r.is_ready(), Duration::from_secs(30)).is_none() {
        eprintln!("basic charging: the vehicle never asked for energy");
        return Ok(false);
    }
    eprintln!("basic charging: the vehicle asks for energy (C): contactor closed");
    let left = wait(&|r| !r.is_ready(), Duration::from_secs(120));
    match left {
        Some(r) => eprintln!("basic charging: the vehicle is done ({:?}): contactor open", r.state),
        None => {
            eprintln!("basic charging: the vehicle never stopped");
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(target_os = "linux")]
fn start_slac(interface: &str, config: evse_sim::slac::Config, modem: bool, pilot: Option<String>, wire: Option<String>) -> io::Result<()> {
    use evcc_linux::slac::{NoModem, PacketSocket, QcaModem};
    use evse_sim::slac::{Evse, Serve};
    let socket = PacketSocket::open(interface)?;
    let x1_ms = config.x1_ms;
    let config = evse_sim::slac::Config { mac: socket.mac(), ..config };
    // With a cable the charger watches the pilot itself: the matching starts with the plug-in, and the pilot does what the
    // machine asks for (state E or F to restart the connection, X1).
    let wire = wire.map(evcc_linux::pilot::Wire::new).transpose()?.map(|w| (w, x1_ms));
    let evse = if wire.is_some() { Evse::wired(config) } else { Evse::new(config) };
    if modem {
        let modem = QcaModem::open(interface)?.as_coordinator();
        eprintln!("slac: modem {}", modem.version()?);
        slac_loop(socket, Serve::new(evse, modem), pilot, wire);
    } else {
        slac_loop(socket, Serve::new(evse, NoModem), pilot, wire);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn slac_loop<M: evcc_linux::slac::Modem + Send + 'static>(
    socket: evcc_linux::slac::PacketSocket,
    mut serve: evse_sim::slac::Serve<M>,
    pilot: Option<String>,
    wire: Option<(evcc_linux::pilot::Wire, u32)>,
) {
    use evcc_linux::pilot::PilotSource;
    use std::time::Duration;
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        let mut matched = false;
        let start = std::time::Instant::now();
        let now_ms = move || start.elapsed().as_millis() as u64;
        let mut source = wire.as_ref().map(|(w, _)| w.source());
        let toggles = |pilot: &Option<String>| pilot.as_deref().and_then(|p| std::fs::metadata(p).ok()).map_or(0, |m| m.len());
        loop {
            // Wait for a frame, but not past the next timer of the machine (the modem's link is asked about every 100 ms).
            let longest = if source.is_some() { 5 } else { 100 };
            let wait = serve.next_deadline().map_or(longest, |d| d.saturating_sub(now_ms()).clamp(1, longest));
            let received = socket.recv(&mut buf, Some(Duration::from_millis(wait))).ok().flatten();
            // Toggles are counted from here on (the file may hold those of an earlier run).
            let before = toggles(&pilot);
            let mut frames = Vec::new();
            if let Some(reading) = source.as_mut().and_then(|s| s.poll()) {
                frames.extend(serve.pilot(now_ms(), reading));
            }
            frames.extend(serve.step(now_ms(), received.map(|n| &buf[..n])));
            for frame in frames {
                let _ = socket.send(&frame);
            }
            if let Some((wire, x1_ms)) = &wire {
                for request in serve.evse.pilot_requests.drain(..) {
                    eprintln!("slac: pilot {request:?}");
                    evse_sim::sequence::spawn(wire.clone(), request, *x1_ms);
                }
            }
            // The vehicle asked for the validation by BCB-Toggle: listen, count, answer.
            if let Some(ms) = serve.evse.validation_window_ms() {
                std::thread::sleep(Duration::from_millis(ms.into()));
                let counted = toggles(&pilot) - before;
                eprintln!("slac: validation, {counted} toggles counted");
                serve.evse.at(now_ms());
                for frame in serve.evse.finish_validation(counted.min(255) as u8) {
                    let _ = socket.send(&frame);
                }
            }
            for notice in serve.evse.notices.drain(..) {
                eprintln!("slac: {notice:?}");
            }
            for error in serve.errors.drain(..) {
                eprintln!("slac: {error}");
            }
            if !matched && matches!(serve.evse.state(), evse_sim::slac::State::Matched { .. }) {
                matched = true;
                eprintln!("slac: matched");
            }
        }
    });
}

#[cfg(not(target_os = "linux"))]
fn start_slac(_interface: &str, _config: evse_sim::slac::Config, _modem: bool, _pilot: Option<String>, _wire: Option<String>) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SLAC needs Linux",
    ))
}
