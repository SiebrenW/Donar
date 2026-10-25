//! `evcc-sim`: brings up a charging session with a simulated vehicle, for scenarios and
//! interoperability tests against other chargers (SECCs). SLAC (optional), SECC discovery,
//! TLS (optional) and a DC session with the EIM authorization.
//!
//! ```text
//! evcc-sim --interface eth1 [--config FILE] [--slac] [--no-tls | --chain F --key F --trust F
//!                        [--key-password P] [--backwards-compatible] [--tls-versions V]]
//!      [--stop-after N | --never-stop] [--connector core|extended|dual2|dual4]
//!      [--log error|info|debug|trace] [--control SOCKET]
//! ```
//! `--config` names a TOML file with `evcc_sim::Settings`; the other options override it.
//! `--control` serves the live state and commands of a test platform on a Unix socket and holds the
//! session before each phase until it is told to advance (`sim_control`, the web UI).
//! Exit status 0 when the session ran to its end, 1 when it did not, 2 for bad arguments.

use std::io;
use std::process::ExitCode;

use evcc_core::sdp::Outcome as SdpOutcome;
use evcc_core::session::EndReason;
use evcc_linux::{Outcome, sdp, tls};
use evcc_sim::{Behaviour, Connector, EvLive, LogLevel, PilotHost, Protocol, SessionConfig, Settings, StderrLog, Tls, run_car_pilot};
use v2gtp::sdp::Security;

const USAGE: &str = "usage: evcc-sim --interface IF [--config FILE] [--slac] [--no-tls | --chain F --key F --trust F [--key-password P] [--backwards-compatible] [--tls-versions V]] [--stop-after N | --never-stop] [--connector INLET] [--log LEVEL] [--control SOCKET]";

fn parse() -> Result<(String, Settings, Option<String>), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut settings = Settings::default();
    // The file first, so that the options override it wherever they stand.
    if let Some(i) = args.iter().position(|a| a == "--config") {
        let path = args.get(i + 1).ok_or("--config needs a value")?;
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        settings = toml::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    }
    let mut interface = String::new();
    let mut control = None;
    let (mut chain, mut key, mut trust, mut key_password, mut backwards_compatible) =
        (None, None, None, None, false);
    let mut versions = None;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--config" => {
                value("--config")?;
            }
            "--interface" => interface = value("--interface")?,
            "--control" => control = Some(value("--control")?),
            "--slac" => settings.slac = true,
            "--no-tls" => settings.tls = None,
            "--chain" => chain = Some(value("--chain")?),
            "--key" => key = Some(value("--key")?),
            "--trust" => trust = Some(value("--trust")?),
            "--key-password" => key_password = Some(value("--key-password")?),
            "--backwards-compatible" => backwards_compatible = true,
            "--tls-versions" => versions = Some(parse_versions(&value("--tls-versions")?)?),
            "--stop-after" => {
                settings.stop_after = value("--stop-after")?
                    .parse()
                    .map_err(|_| "--stop-after: not a number")?
            }
            "--never-stop" => settings.stop_after = 0,
            "--connector" => {
                settings.connector = match value("--connector")?.as_str() {
                    "core" => Connector::Core,
                    "extended" => Connector::Extended,
                    "dual2" => Connector::Dual2,
                    "dual4" => Connector::Dual4,
                    other => return Err(format!("--connector: unknown inlet {other}")),
                }
            }
            "--log" => {
                settings.log = match value("--log")?.as_str() {
                    "error" => LogLevel::Error,
                    "info" => LogLevel::Info,
                    "debug" => LogLevel::Debug,
                    "trace" => LogLevel::Trace,
                    other => return Err(format!("--log: unknown level {other}")),
                }
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if chain.is_some() || key.is_some() || trust.is_some() {
        let tls = settings.tls.get_or_insert_with(Tls::default);
        if let Some(chain) = chain {
            tls.chain = chain
        }
        if let Some(key) = key {
            tls.key = key
        }
        if let Some(trust) = trust {
            tls.trust = trust
        }
    }
    if let Some(tls) = &mut settings.tls {
        if key_password.is_some() {
            tls.key_password = key_password
        }
        tls.backwards_compatible |= backwards_compatible;
        if let Some(v) = versions {
            tls.versions = v;
        }
        if tls.chain.is_empty() || tls.key.is_empty() || tls.trust.is_empty() {
            return Err("TLS needs a chain, a key and a trust file (or use --no-tls)".into());
        }
    } else if backwards_compatible || key_password.is_some() || versions.is_some() {
        return Err("--backwards-compatible, --tls-versions and --key-password belong to TLS".into());
    }
    if interface.is_empty() {
        return Err("--interface is required".into());
    }
    Ok((interface, settings, control))
}

fn parse_versions(text: &str) -> Result<evcc_sim::TlsVersions, String> {
    match text {
        "1.3" => Ok(evcc_sim::TlsVersions::Tls13),
        "1.3+1.2" => Ok(evcc_sim::TlsVersions::Tls13And12),
        "1.2" => Ok(evcc_sim::TlsVersions::Tls12),
        other => Err(format!("--tls-versions: {other}, expected 1.3, 1.3+1.2 or 1.2")),
    }
}

fn main() -> ExitCode {
    let (interface, settings, control) = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("evcc-sim: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&interface, &settings, control.as_deref()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("evcc-sim: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(interface: &str, settings: &Settings, control: Option<&str>) -> io::Result<bool> {
    let live = match control {
        Some(path) => {
            let live = EvLive::new(settings.limits, settings.start_soc, settings.time_scale, settings.stop_at_target_soc);
            sim_control::serve(std::path::Path::new(path), live.clone())?;
            Some(live)
        }
        None => None,
    };
    // Plugged in: the pilot is at B before anything else happens.
    if let Some(dir) = &settings.control_pilot_wire {
        evcc_linux::pilot::Wire::new(dir)?.set_ev(control_pilot::State::B)?;
    }
    let ifindex = sdp::interface_index(interface)?;
    if settings.slac {
        match run_slac(interface, settings)? {
            evcc_core::slac::Outcome::Matched { .. } => {}
            // A nominal duty cycle: the charger offers basic charging only, and there is nothing to match.
            evcc_core::slac::Outcome::BasicCharging => return basic_charging(settings),
            _ => return Ok(false),
        }
    }
    if settings.slac && settings.slac_only {
        return Ok(true);
    }
    // A bus: the approach to the pantograph starts now and goes on while the discovery waits for the charger's PPD to read the EVID.
    let driver = settings.acdp.as_ref().map(evcc_sim::Driver::start).transpose()?;
    let mut outcome = attempt(ifindex, settings, None, settings.pause_after, &live, &driver)?;
    // A paused session is resumed on a new connection (V2G20-2073); the data link stays up.
    while let (Outcome::Ended(EndReason::Paused(resume)), Some(ms)) = (outcome, settings.resume_after_ms) {
        eprintln!("paused; resuming session {:02x?} in {ms} ms", resume.session_id);
        std::thread::sleep(std::time::Duration::from_millis(ms.into()));
        outcome = attempt(ifindex, settings, Some(resume), None, &live, &driver)?;
    }
    if live.is_some() {
        // Time for the control socket to deliver the last state.
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    Ok(outcome == Outcome::Ended(EndReason::Completed))
}

/// One connection: SDP, TCP, TLS if wanted, and a session on it.
fn attempt(ifindex: u32, settings: &Settings, resume: Option<evcc_core::session::Resume>, pause_after: Option<u32>, live: &Option<std::sync::Arc<EvLive>>, driver: &Option<std::sync::Arc<evcc_sim::Driver>>) -> io::Result<Outcome> {
    let security = if settings.tls.is_some() {
        Security::Tls
    } else {
        Security::NoTls
    };
    let socket = sdp::socket()?;
    // An ACDP vehicle discovers wirelessly (ISO 15118-8): its EVID, with the PPD on the infrastructure and the in line contacts of
    // EN 50696 Annex A.3 (V2G20-4003, 4100).
    let mut sdp_config = evcc_core::sdp::Config::new(security);
    if let Some(acdp) = &settings.acdp {
        use v2gtp::sdp::{EvId, EvseId, coupling, p2ps};
        let evid = EvId::new(&acdp.evid).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "acdp.evid: at most 20 letters and digits"))?;
        sdp_config.wireless = Some(evcc_core::sdp::Wireless { p2ps_ppd: [p2ps::PPD_ON_INFRASTRUCTURE, 0], coupling_type: coupling::PANTOGRAPH_IN_LINE, evid, evse_id: EvseId::EMPTY });
    }
    let found = sdp::discover(&socket, sdp::multicast_destination(ifindex), sdp_config)?;
    let SdpOutcome::Found(response) = found else {
        return Err(io::Error::new(io::ErrorKind::NotFound, "sdp: no charger found"));
    };
    eprintln!("sdp: {}", sdp::address(&response, ifindex));
    let tcp = sdp::connect(&response, ifindex)?;
    let config = match settings.protocol {
        Protocol::Iso20 => SessionConfig::Iso20(evcc_core::session::Config { resume, ..settings.session() }),
        Protocol::Din => SessionConfig::Din(settings.din_session(), settings.din.mac_address),
        Protocol::Iso2 => SessionConfig::Iso2(evcc_core::iso2::Config { resume, ..settings.iso2_session() }, settings.iso2.mac_address),
    };
    let log = evcc_sim::LiveLog {
        inner: StderrLog { level: settings.log.into() },
        live: live.clone(),
    };
    let contract: Option<std::sync::Arc<dyn evcc_core::ev::Contract>> = match &settings.pnc {
        Some(c) => {
            let (chain, key) = (std::fs::read(&c.chain)?, std::fs::read(&c.key)?);
            let loaded = evcc_linux::pnc::PemContract::new(&chain, &key, c.key_password.as_deref().map(str::as_bytes)).map_err(io::Error::other)?;
            Some(std::sync::Arc::new(loaded))
        }
        None => None,
    };
    let behaviour = Behaviour { connector: settings.connector.into(), stop_after: settings.stop_after(), renegotiate_after: settings.renegotiate_after, renegotiate_schedule_after: settings.renegotiate_schedule_after, stop_when_rescheduled: settings.stop_when_rescheduled, pause_after };
    // The control pilot: the cable shared with the simulated charger.
    let pilot = match &settings.control_pilot_wire {
        Some(dir) => Some(PilotHost { wire: evcc_linux::pilot::Wire::new(dir)?, emergency_after: settings.pilot_emergency_after }),
        None => None,
    };
    let outcome = match &settings.tls {
        None => {
            let mut stream = tcp;
            run_car_pilot(&mut stream, false, config, behaviour, log, live.clone(), settings.limits, contract, pilot, driver.clone())?
        }
        Some(t) => {
            let (chain, key, trust) = (
                std::fs::read(&t.chain)?,
                std::fs::read(&t.key)?,
                std::fs::read(&t.trust)?,
            );
            let mut credentials = tls::Credentials::new(&chain, &key, &trust);
            if let Some(password) = &t.key_password {
                credentials = credentials.key_password(password.as_bytes());
            }
            if t.backwards_compatible {
                credentials = credentials.backwards_compatible();
            }
            credentials = credentials.versions(t.versions.into());
            if t.disclose_secrets {
                credentials = credentials.disclose_secrets(tls::Disclosure::on_interface(ifindex)?);
            }
            let context = tls::client_context(&credentials).map_err(io::Error::other)?;
            let mut stream = tls::connect(&context, tcp)?;
            tls::check_peer_certificates(&stream, evcc_linux::certificates::Peer::Secc, t.certificate_checks.into(), |v| eprintln!("certificates: {v}"))?;
            tls::check_peer_ocsp(&stream, &trust, &[], t.ocsp_checks.into(), t.ocsp_require_all, |v| eprintln!("ocsp: {v}"))?;
            eprintln!(
                "tls: {} {}",
                stream.ssl().version_str(),
                stream.ssl().current_cipher().map_or("?", |c| c.name())
            );
            run_car_pilot(&mut stream, true, config, behaviour, log, live.clone(), settings.limits, contract, pilot, driver.clone())?
        }
    };
    eprintln!("session: {outcome:?}");
    Ok(outcome)
}

#[cfg(target_os = "linux")]
fn run_slac(interface: &str, settings: &Settings) -> io::Result<evcc_core::slac::Outcome> {
    let socket = evcc_linux::slac::PacketSocket::open(interface)?;
    let outcome = if settings.qca_modem {
        let mut modem = evcc_linux::slac::QcaModem::open(interface)?;
        eprintln!("modem: {}", modem.version()?);
        slac_with(&socket, settings, &mut modem)?
    } else {
        slac_with(&socket, settings, &mut evcc_linux::slac::NoModem)?
    };
    eprintln!("slac: {outcome:?}");
    Ok(outcome)
}

/// Charging by IEC 61851-1 alone, as the charger's nominal duty cycle offers: the vehicle asks for energy with state C,
/// takes it for `basic_charge_ms` and stops with state B. It ends early when the charger takes the oscillator away or
/// signals an error.
#[cfg(target_os = "linux")]
fn basic_charging(settings: &Settings) -> io::Result<bool> {
    use control_pilot::{Pwm, State};
    use std::time::{Duration, Instant};
    let dir = settings.control_pilot_wire.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "basic charging needs the control pilot wire"))?;
    let wire = evcc_linux::pilot::Wire::new(dir)?;
    let offered = wire.reading().and_then(|r| r.pwm.offered_current_tenths()).ok_or_else(|| io::Error::other("no nominal duty cycle on the pilot"))?;
    eprintln!("basic charging: {}.{} A offered, no high level communication", offered / 10, offered % 10);
    wire.set_ev(State::C)?;
    eprintln!("basic charging: asking for energy (C) for {} ms", settings.basic_charge_ms);
    let end = Instant::now() + Duration::from_millis(settings.basic_charge_ms.into());
    while Instant::now() < end {
        if let Some(r) = wire.reading().filter(|r| r.pwm == Pwm::Off || matches!(r.state, State::A | State::E | State::F)) {
            eprintln!("basic charging: the charger ended it ({r:?})");
            wire.set_ev(State::B)?;
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    wire.set_ev(State::B)?;
    eprintln!("basic charging: done (B)");
    Ok(true)
}

#[cfg(target_os = "linux")]
fn slac_with(socket: &evcc_linux::slac::PacketSocket, settings: &Settings, modem: &mut impl evcc_linux::slac::Modem) -> io::Result<evcc_core::slac::Outcome> {
    let run_id = std::array::from_fn(|i| (std::process::id() as u8).wrapping_add((i as u8).wrapping_mul(37)));
    // With the wire the simulated car watches the cable like a real one and SLAC waits for the charger's 5 %; without it
    // (a bench with a veth cable) there is no pilot and it begins at once.
    let wire = match &settings.control_pilot_wire {
        Some(dir) => Some(evcc_linux::pilot::Wire::new(dir)?.source()),
        None => None,
    };
    let config = evcc_core::slac::Config { pilot_gate: wire.is_some(), ..settings.slac_config() };
    let mut pilot = evcc_sim::SimPilot { toggles: settings.control_pilot_file.as_ref().map(Into::into), wire };
    // The charger restarts the connection through state E or F (V2G3-M06-05, 07): the matching is aborted and the
    // car starts over when it sees 5 % again.
    loop {
        match evcc_linux::slac::run_with_pilot(socket, config, run_id, modem, &mut pilot)? {
            evcc_core::slac::Outcome::Aborted(state) => eprintln!("slac: aborted by pilot state {state:?}, starting over"),
            outcome => return Ok(outcome),
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn run_slac(_interface: &str, _settings: &Settings) -> io::Result<evcc_core::slac::Outcome> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SLAC needs Linux",
    ))
}
