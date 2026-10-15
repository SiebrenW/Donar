//! `evcc`: brings up a charging session with a simulated vehicle, for interoperability tests
//! against other chargers (SECCs). SLAC (optional), SECC discovery, TLS (optional) and a DC
//! session with the EIM authorization.
//!
//! ```text
//! evcc --interface eth1 [--slac] [--no-tls | --chain F --key F --trust F]
//!      [--stop-after N | --never-stop] [--connector core|extended|dual2|dual4]
//!      [--log error|info|debug|trace]
//! ```
//! Exit status 0 when the session ran to its end, 1 when it did not, 2 for bad arguments.

use std::io;
use std::process::ExitCode;

use evcc_core::ev::DcConnector;
use evcc_core::log::Level;
use evcc_core::session::{Config, EndReason};
use evcc_core::sdp::Outcome as SdpOutcome;
use evcc_linux::sim_ev::{StderrLog, run_simulated_car};
use evcc_linux::{Outcome, sdp, tls};
use v2gtp::sdp::Security;

struct Args {
    interface: String,
    slac: bool,
    no_tls: bool,
    chain: Option<String>,
    key: Option<String>,
    trust: Option<String>,
    stop_after: Option<u32>,
    connector: DcConnector,
    level: Level,
}

fn parse() -> Result<Args, String> {
    let mut a = Args { interface: String::new(), slac: false, no_tls: false, chain: None, key: None, trust: None, stop_after: Some(3), connector: DcConnector::Core, level: Level::Info };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--interface" => a.interface = value("--interface")?,
            "--slac" => a.slac = true,
            "--no-tls" => a.no_tls = true,
            "--chain" => a.chain = Some(value("--chain")?),
            "--key" => a.key = Some(value("--key")?),
            "--trust" => a.trust = Some(value("--trust")?),
            "--stop-after" => a.stop_after = Some(value("--stop-after")?.parse().map_err(|_| "--stop-after: not a number")?),
            "--never-stop" => a.stop_after = None,
            "--connector" => {
                a.connector = match value("--connector")?.as_str() {
                    "core" => DcConnector::Core,
                    "extended" => DcConnector::Extended,
                    "dual2" => DcConnector::Dual2,
                    "dual4" => DcConnector::Dual4,
                    other => return Err(format!("--connector: unknown inlet {other}")),
                }
            }
            "--log" => {
                a.level = match value("--log")?.as_str() {
                    "error" => Level::Error,
                    "info" => Level::Info,
                    "debug" => Level::Debug,
                    "trace" => Level::Trace,
                    other => return Err(format!("--log: unknown level {other}")),
                }
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if a.interface.is_empty() {
        return Err("--interface is required".into());
    }
    if !a.no_tls && (a.chain.is_none() || a.key.is_none() || a.trust.is_none()) {
        return Err("TLS needs --chain, --key and --trust (or use --no-tls)".into());
    }
    Ok(a)
}

fn main() -> ExitCode {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("evcc: {e}\nusage: evcc --interface IF [--slac] [--no-tls | --chain F --key F --trust F] [--stop-after N | --never-stop] [--connector INLET] [--log LEVEL]");
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("evcc: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> io::Result<bool> {
    let ifindex = sdp::interface_index(&args.interface)?;
    if args.slac && !run_slac(&args.interface)? {
        return Ok(false);
    }
    let security = if args.no_tls { Security::NoTls } else { Security::Tls };
    let socket = sdp::socket()?;
    let found = sdp::discover(&socket, sdp::multicast_destination(ifindex), evcc_core::sdp::Config::new(security))?;
    let SdpOutcome::Found(response) = found else {
        eprintln!("sdp: no charger found");
        return Ok(false);
    };
    eprintln!("sdp: {}", sdp::address(&response, ifindex));
    let tcp = sdp::connect(&response, ifindex)?;
    let config = Config { allow_unsecured: args.no_tls, ..Config::DC_20 };
    let log = StderrLog { level: args.level };
    let outcome = if args.no_tls {
        let mut stream = tcp;
        run_simulated_car(&mut stream, false, config, args.stop_after, args.connector, log)?
    } else {
        let read = |p: &Option<String>| std::fs::read(p.as_deref().unwrap());
        let (chain, key, trust) = (read(&args.chain)?, read(&args.key)?, read(&args.trust)?);
        let context = tls::client_context(&tls::Credentials { chain_pem: &chain, key_pem: &key, trust_pem: &trust }).map_err(io::Error::other)?;
        let mut stream = tls::connect(&context, tcp)?;
        run_simulated_car(&mut stream, true, config, args.stop_after, args.connector, log)?
    };
    eprintln!("session: {outcome:?}");
    Ok(outcome == Outcome::Ended(EndReason::Completed))
}

#[cfg(target_os = "linux")]
fn run_slac(interface: &str) -> io::Result<bool> {
    use evcc_core::slac::Outcome as SlacOutcome;
    let socket = evcc_linux::slac::PacketSocket::open(interface)?;
    let run_id = std::array::from_fn(|i| (std::process::id() as u8).wrapping_add(i as u8 * 37));
    let outcome = evcc_linux::slac::run(&socket, evcc_core::slac::Config::DEFAULT, run_id, &mut evcc_linux::slac::NoModem)?;
    eprintln!("slac: {outcome:?}");
    Ok(matches!(outcome, SlacOutcome::Matched { .. }))
}

#[cfg(not(target_os = "linux"))]
fn run_slac(_interface: &str) -> io::Result<bool> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "SLAC needs Linux"))
}
