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

/// Collects the debug text of every record.
#[derive(Clone, Default)]
pub struct Recording(pub std::rc::Rc<std::cell::RefCell<Vec<String>>>);

impl evcc_core::log::LogSink for Recording {
    fn record(&mut self, record: &evcc_core::log::Record<'_>) {
        self.0.borrow_mut().push(format!("{record:?}"));
    }
}

impl Recording {
    /// How many records contain `needle`.
    pub fn count(&self, needle: &str) -> usize {
        self.0.borrow().iter().filter(|r| r.contains(needle)).count()
    }
}

/// Like [`run_car`], recording what the session logs.
pub fn run_car_recorded(stream: &mut impl Transport, config: Config, stop_after: Option<u32>) -> (Outcome, Recording) {
    let recording = Recording::default();
    let outcome = run_simulated_car(stream, false, config, stop_after, DcConnector::Core, recording.clone()).unwrap();
    (outcome, recording)
}

/// Like [`run_car_recorded`] with the car's whole behaviour.
pub fn run_behaving_car(stream: &mut impl Transport, config: Config, behaviour: evcc_sim::Behaviour) -> (Outcome, Recording) {
    let recording = Recording::default();
    let outcome = evcc_sim::run_car(stream, false, config, behaviour, recording.clone()).unwrap();
    (outcome, recording)
}
