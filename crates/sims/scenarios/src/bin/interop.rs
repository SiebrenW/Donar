//! `interop [PARTNER] [evcc-sim arguments]`: runs `evcc-sim` against an independent charger
//! implementation in two sandbox nodes. Needs root and a user to drop to (sudo); the
//! logs of both sides are in the artifacts directory, which is printed.

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("interop: needs Linux network namespaces (use `cargo xtask interop`)");
    std::process::ExitCode::from(2)
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(target_os = "linux")]
mod linux {
use std::process::ExitCode;
use std::time::Duration;

use sandbox::{Options, Sandbox, Spawn};
use scenarios::partners;

pub fn main() -> ExitCode {
    match run() {
        Ok(status) => ExitCode::from(status),
        Err(e) => {
            eprintln!("interop: {e}");
            ExitCode::from(2)
        }
    }
}

fn run() -> std::io::Result<u8> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let partner = if args.first().is_some_and(|a| !a.starts_with("--")) { args.remove(0) } else { "iso15118-python".to_string() };
    let evcc_sim = partners::sibling_binary("evcc-sim")?;
    let mut sb = Sandbox::new(Options::default())?;
    let (evcc, secc) = (sb.node("evcc")?, sb.node("secc")?);
    sb.connect(&evcc, &secc)?;
    let partner_process = partners::start(&partner, &sb, &secc)?;
    let mut car = sb.spawn(
        &evcc,
        Spawn::new(evcc_sim).args(["--interface", sandbox::INTERFACE]).args(partner_process.evcc_args.clone()).args(args).label("evcc-sim"),
    )?;
    let status = car.wait_timeout(Duration::from_secs(60))?;
    eprint!("{}", car.log());
    let code = match status {
        Some(s) => s.code().unwrap_or(1),
        None => {
            eprintln!("evcc-sim did not finish in 60 s");
            car.kill();
            1
        }
    };
    println!("evcc exit status: {code}");
    if code != 0 || std::env::var_os("INTEROP_SHOW_PARTNER_LOG").is_some() {
        println!("----- {partner} log -----");
        let log = partner_process.process.log();
        if std::env::var_os("INTEROP_SHOW_PARTNER_LOG").is_some() {
            println!("{log}");
        } else {
            let lines: Vec<&str> = log.lines().collect();
            println!("{}", lines[lines.len().saturating_sub(60)..].join("\n"));
        }
    }
    println!("artifacts: {}", sb.artifacts().display());
    Ok(code as u8)
}
}
