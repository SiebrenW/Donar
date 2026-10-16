//! A simulated vehicle for tests and the `evcc` command: a car with a 400 V battery that
//! closes its contactors when asked, reaches the pre-charge voltage, charges until told to
//! stop and ramps down at once.

use std::cell::Cell;
use std::io;
use std::rc::Rc;

use evcc_core::ev::{DcConnector, DcLimits, Ev, ScheduleRequest};
use evcc_core::log::LogSink;
use evcc_core::session::{Buffers, Config, Event, Notice, Session};
use iso15118_20::common_types::RationalNumber;

use crate::{Outcome, Transport, run};

pub struct SimulatedEv {
    connector: DcConnector,
    reads: Cell<u32>,
    /// Set by the host when the contactors are open: the inlet voltage then drops to zero.
    open: Rc<Cell<bool>>,
}

impl SimulatedEv {
    pub fn new(open: Rc<Cell<bool>>, connector: DcConnector) -> Self {
        Self { connector, reads: Cell::new(0), open }
    }
}

fn rn(exponent: i8, value: i16) -> RationalNumber {
    RationalNumber { exponent, value }
}

impl Ev for SimulatedEv {
    fn evcc_id(&self) -> &str {
        "WMIV1234567890ABCDEX"
    }
    fn schedule_request(&self) -> ScheduleRequest {
        ScheduleRequest {
            departure_time: 3600,
            minimum_soc: None,
            target_soc: Some(80),
            target_energy: rn(3, 40),
            maximum_energy: rn(3, 60),
            minimum_energy: rn(0, 0),
        }
    }
    fn dc_limits(&self) -> DcLimits {
        DcLimits {
            maximum_charge_power: rn(3, 100),
            minimum_charge_power: rn(0, 100),
            maximum_charge_current: rn(0, 200),
            minimum_charge_current: rn(0, 1),
            maximum_voltage: rn(0, 450),
            minimum_voltage: rn(0, 200),
            target_soc: Some(80),
        }
    }
    fn battery_voltage(&self) -> RationalNumber {
        rn(0, 400)
    }
    /// The first reading is below the battery voltage, the later ones match it.
    fn inlet_voltage(&self) -> RationalNumber {
        let reads = self.reads.get();
        self.reads.set(reads + 1);
        rn(0, if self.open.get() { 0 } else if reads == 0 { 300 } else { 400 })
    }
    fn dc_connector(&self) -> DcConnector {
        self.connector
    }
    fn now_micros(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_micros() as u64)
    }
}

/// Runs the simulated car over `stream`; after `stop_after` charge loop answers it asks to
/// stop (`None`: it never does, the EVSE ends the charge).
pub fn run_simulated_car(
    stream: &mut impl Transport,
    secured: bool,
    config: Config,
    stop_after: Option<u32>,
    connector: DcConnector,
    log: impl LogSink,
) -> io::Result<Outcome> {
    let (mut rx, mut tx, mut arena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let open = Rc::new(Cell::new(false));
    let car = SimulatedEv::new(open.clone(), connector);
    let mut session = Session::new(config, car, log, Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
    let mut loops = 0;
    run(stream, &mut session, secured, &mut |n| match n {
        // The car closes its contactors and stops drawing current as soon as it is asked to.
        Notice::CloseContactors => Some(Event::ContactorsClosed),
        Notice::StopEnergyTransfer { .. } => Some(Event::EnergyTransferStopped),
        Notice::OpenContactors => {
            open.set(true);
            Some(Event::ContactorsOpened)
        }
        Notice::ChargeLoop(_) => {
            loops += 1;
            (Some(loops) == stop_after).then_some(Event::StopRequested)
        }
        _ => None,
    })
}

/// Prints records of at most `level` to standard error.
pub struct StderrLog {
    pub level: evcc_core::log::Level,
}

impl LogSink for StderrLog {
    fn record(&mut self, record: &evcc_core::log::Record<'_>) {
        if record.level() <= self.level {
            eprintln!("[{:?}] {record:?}", record.level());
        }
    }
}
