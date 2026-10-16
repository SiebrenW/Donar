//! Shared by the end-to-end tests; each test crate uses only part of it.
#![allow(dead_code)]

pub mod pki;

use evcc_core::ev::DcConnector;
use evcc_core::log::NoLog;
use evcc_core::session::Config;
use evcc_sim::run_simulated_car;
use evcc_linux::{Outcome, Transport};

/// Runs the car over `stream`; after `stop_after` charge loop answers it asks to stop.
pub fn run_car(stream: &mut impl Transport, secured: bool, config: Config, stop_after: Option<u32>) -> std::io::Result<Outcome> {
    run_simulated_car(stream, secured, config, stop_after, DcConnector::Core, NoLog)
}
