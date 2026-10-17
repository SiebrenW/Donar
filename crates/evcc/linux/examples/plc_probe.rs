//! Looks for a Qualcomm QCA7000 family powerline modem behind a network interface and prints its
//! software version. Reads only; needs `CAP_NET_RAW` (root).
//!
//!     plc_probe eth1
//!
//! With `--set-key NMK_HEX` it also sends the modem a network key (CM_SET_KEY.REQ) and prints its
//! answer. That changes the key of the modem until its next reset.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("plc_probe needs Linux");
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    probe::run()
}

#[cfg(target_os = "linux")]
mod probe {
use std::process::ExitCode;

use evcc_linux::slac::{Modem, QcaModem};

pub fn run() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(interface) = args.first() else {
        eprintln!("usage: plc_probe INTERFACE [--set-key NMK_HEX]");
        return ExitCode::from(2);
    };
    let mut modem = match QcaModem::open(interface) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{interface}: {e}");
            return ExitCode::FAILURE;
        }
    };
    match modem.version() {
        Ok(v) => println!("{interface}: modem {v}"),
        Err(e) => {
            eprintln!("{interface}: {e}");
            return ExitCode::FAILURE;
        }
    }
    if let (Some("--set-key"), Some(hex)) = (args.get(1).map(String::as_str), args.get(2)) {
        let Some(nmk) = parse_key(hex) else {
            eprintln!("the key is 32 hexadecimal digits");
            return ExitCode::from(2);
        };
        let nid = homeplug::modem::network_id(&nmk);
        match modem.set_key(nid, nmk) {
            Ok(()) => println!("{interface}: key accepted, network id {}", nid.iter().map(|b| format!("{b:02x}")).collect::<String>()),
            Err(e) => {
                eprintln!("{interface}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::SUCCESS
}

fn parse_key(hex: &str) -> Option<[u8; 16]> {
    if hex.len() != 32 {
        return None;
    }
    let mut key = [0u8; 16];
    for (i, b) in key.iter_mut().enumerate() {
        *b = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(key)
}
}
