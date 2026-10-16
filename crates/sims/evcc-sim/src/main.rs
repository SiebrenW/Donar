//! `evcc-sim`: brings up a charging session with a simulated vehicle, for scenarios and
//! interoperability tests against other chargers (SECCs). SLAC (optional), SECC discovery,
//! TLS (optional) and a DC session with the EIM authorization.
//!
//! ```text
//! evcc-sim --interface eth1 [--config FILE] [--slac] [--no-tls | --chain F --key F --trust F
//!                        [--key-password P] [--backwards-compatible]]
//!      [--stop-after N | --never-stop] [--connector core|extended|dual2|dual4]
//!      [--log error|info|debug|trace]
//! ```
//! `--config` names a TOML file with `evcc_sim::Settings`; the other options override it.
//! Exit status 0 when the session ran to its end, 1 when it did not, 2 for bad arguments.

use std::io;
use std::process::ExitCode;

use evcc_core::sdp::Outcome as SdpOutcome;
use evcc_core::session::EndReason;
use evcc_linux::{Outcome, sdp, tls};
use evcc_sim::{Behaviour, Connector, LogLevel, Settings, StderrLog, Tls, run_car};
use v2gtp::sdp::Security;

const USAGE: &str = "usage: evcc-sim --interface IF [--config FILE] [--slac] [--no-tls | --chain F --key F --trust F [--key-password P] [--backwards-compatible]] [--stop-after N | --never-stop] [--connector INLET] [--log LEVEL]";

fn parse() -> Result<(String, Settings), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut settings = Settings::default();
    // The file first, so that the options override it wherever they stand.
    if let Some(i) = args.iter().position(|a| a == "--config") {
        let path = args.get(i + 1).ok_or("--config needs a value")?;
        let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        settings = toml::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    }
    let mut interface = String::new();
    let (mut chain, mut key, mut trust, mut key_password, mut backwards_compatible) =
        (None, None, None, None, false);
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--config" => {
                value("--config")?;
            }
            "--interface" => interface = value("--interface")?,
            "--slac" => settings.slac = true,
            "--no-tls" => settings.tls = None,
            "--chain" => chain = Some(value("--chain")?),
            "--key" => key = Some(value("--key")?),
            "--trust" => trust = Some(value("--trust")?),
            "--key-password" => key_password = Some(value("--key-password")?),
            "--backwards-compatible" => backwards_compatible = true,
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
        if tls.chain.is_empty() || tls.key.is_empty() || tls.trust.is_empty() {
            return Err("TLS needs a chain, a key and a trust file (or use --no-tls)".into());
        }
    } else if backwards_compatible || key_password.is_some() {
        return Err("--backwards-compatible and --key-password belong to TLS".into());
    }
    if interface.is_empty() {
        return Err("--interface is required".into());
    }
    Ok((interface, settings))
}

fn main() -> ExitCode {
    let (interface, settings) = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("evcc-sim: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(&interface, &settings) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("evcc-sim: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(interface: &str, settings: &Settings) -> io::Result<bool> {
    let ifindex = sdp::interface_index(interface)?;
    if settings.slac && !run_slac(interface, settings)? {
        return Ok(false);
    }
    let security = if settings.tls.is_some() {
        Security::Tls
    } else {
        Security::NoTls
    };
    let socket = sdp::socket()?;
    let found = sdp::discover(
        &socket,
        sdp::multicast_destination(ifindex),
        evcc_core::sdp::Config::new(security),
    )?;
    let SdpOutcome::Found(response) = found else {
        eprintln!("sdp: no charger found");
        return Ok(false);
    };
    eprintln!("sdp: {}", sdp::address(&response, ifindex));
    let tcp = sdp::connect(&response, ifindex)?;
    let config = settings.session();
    let log = StderrLog {
        level: settings.log.into(),
    };
    let behaviour = Behaviour { connector: settings.connector.into(), stop_after: settings.stop_after(), renegotiate_after: settings.renegotiate_after, renegotiate_schedule_after: settings.renegotiate_schedule_after, stop_when_rescheduled: settings.stop_when_rescheduled };
    let outcome = match &settings.tls {
        None => {
            let mut stream = tcp;
            run_car(&mut stream, false, config, behaviour, log)?
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
            let context = tls::client_context(&credentials).map_err(io::Error::other)?;
            let mut stream = tls::connect(&context, tcp)?;
            eprintln!(
                "tls: {} {}",
                stream.ssl().version_str(),
                stream.ssl().current_cipher().map_or("?", |c| c.name())
            );
            run_car(&mut stream, true, config, behaviour, log)?
        }
    };
    eprintln!("session: {outcome:?}");
    Ok(outcome == Outcome::Ended(EndReason::Completed))
}

#[cfg(target_os = "linux")]
fn run_slac(interface: &str, settings: &Settings) -> io::Result<bool> {
    use evcc_core::slac::Outcome as SlacOutcome;
    let socket = evcc_linux::slac::PacketSocket::open(interface)?;
    let run_id = std::array::from_fn(|i| (std::process::id() as u8).wrapping_add((i as u8).wrapping_mul(37)));
    let outcome = evcc_linux::slac::run(
        &socket,
        settings.slac_config(),
        run_id,
        &mut evcc_linux::slac::NoModem,
    )?;
    eprintln!("slac: {outcome:?}");
    Ok(matches!(outcome, SlacOutcome::Matched { .. }))
}

#[cfg(not(target_os = "linux"))]
fn run_slac(_interface: &str, _settings: &Settings) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "SLAC needs Linux",
    ))
}
