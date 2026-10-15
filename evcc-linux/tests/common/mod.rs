//! Shared by the TCP and TLS end-to-end tests.

use evcc_core::ev::{DcConnector, DcLimits, Ev, ScheduleRequest};
use evcc_core::log::NoLog;
use evcc_core::session::{Buffers, Config, Event, Notice, Session};
use evcc_linux::{Outcome, Transport, run};
use iso15118_20::common_types::RationalNumber;

pub struct Car {
    pub reads: std::cell::Cell<u32>,
    /// Set by the host when the contactors are open: the inlet voltage then drops to zero.
    pub open: std::rc::Rc<std::cell::Cell<bool>>,
}
impl Ev for Car {
    fn evcc_id(&self) -> &str {
        "WMIV1234567890ABCDEX"
    }
    fn schedule_request(&self) -> ScheduleRequest {
        let rn = |exponent, value| RationalNumber { exponent, value };
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
        let rn = |exponent, value| RationalNumber { exponent, value };
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
        RationalNumber { exponent: 0, value: 400 }
    }
    fn inlet_voltage(&self) -> RationalNumber {
        let reads = self.reads.get();
        self.reads.set(reads + 1);
        let value = if self.open.get() { 0 } else if reads == 0 { 300 } else { 400 };
        RationalNumber { exponent: 0, value }
    }
    fn dc_connector(&self) -> DcConnector {
        DcConnector::Core
    }
    fn now_micros(&self) -> u64 {
        1_700_000_000
    }
}

/// Runs the car over `stream`; after `stop_after` charge loop answers it asks to stop.
pub fn run_car(stream: &mut impl Transport, secured: bool, config: Config, stop_after: Option<u32>) -> std::io::Result<Outcome> {
    let (mut rx, mut tx, mut arena) = ([0u8; 1024], [0u8; 1024], [0u8; 4096]);
    let open = std::rc::Rc::new(std::cell::Cell::new(false));
    let car = Car { reads: Default::default(), open: open.clone() };
    let mut session = Session::new(config, car, NoLog, Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena });
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
