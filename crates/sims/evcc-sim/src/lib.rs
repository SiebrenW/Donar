//! A simulated vehicle for tests, scenarios and the `evcc-sim` command: a car with a 400 V battery that
//! closes its contactors when asked, reaches the pre-charge voltage, charges until told to
//! stop and ramps down at once.

use std::cell::Cell;
use std::io;
use std::rc::Rc;
use std::sync::Arc;

use evcc_core::ev::{DcConnector, DcLimits, Ev, ScheduleRequest};
use evcc_core::log::LogSink;
use evcc_core::session::{Buffers, Config, Event, Notice, Session};
use iso15118_20::common_types::RationalNumber;

use evcc_linux::{Outcome, Transport};

mod live;
mod settings;

pub use live::{Action, EvLive, Phase};
pub use settings::{CertificateChecks, Connector, Contract, Din, Iso2, Iso2Mode, Limits, LogLevel, Protocol, Settings, SlacValidation, Tls, TlsVersions};

pub struct SimulatedEv {
    connector: DcConnector,
    mac: [u8; 6],
    limits: Limits,
    live: Option<Arc<EvLive>>,
    contract: Option<Arc<dyn evcc_core::ev::Contract>>,
    reads: Cell<u32>,
    /// Set by the host when the contactors are open: the inlet voltage then drops to zero.
    open: Rc<Cell<bool>>,
}

impl SimulatedEv {
    pub fn new(open: Rc<Cell<bool>>, connector: DcConnector) -> Self {
        Self { connector, mac: Din::default().mac_address, limits: Limits::default(), live: None, contract: None, reads: Cell::new(0), open }
    }

    /// A car whose limits are `limits` and whose measurements the test platform sees and sets.
    pub fn with_live(mut self, live: Arc<EvLive>) -> Self {
        self.limits = live.limits();
        self.live = Some(live);
        self
    }

    /// A car with this MAC address (the EVCCID of DIN SPEC 70121).
    pub fn with_mac(mut self, mac: [u8; 6]) -> Self {
        self.mac = mac;
        self
    }

    /// A car with a contract for Plug & Charge.
    pub fn with_contract(mut self, contract: Arc<dyn evcc_core::ev::Contract>) -> Self {
        self.contract = Some(contract);
        self
    }

    /// The limits now: the live ones of a test platform (which it may change during the
    /// session), otherwise those the car was made with.
    fn limits(&self) -> Limits {
        self.live.as_ref().map_or(self.limits, |l| l.limits())
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }
}

impl SimulatedEv {
    /// A power per line for the lines the car has.
    fn per_line(&self, watts: u32) -> evcc_core::ev::Phases {
        let w = rn(watts);
        if self.limits().ac_phases >= 3 { evcc_core::ev::Phases::three(w) } else { evcc_core::ev::Phases::single(w) }
    }
}

fn rn(x: u32) -> RationalNumber {
    RationalNumber::from_f64(f64::from(x))
}

impl Ev for SimulatedEv {
    fn evcc_id(&self) -> &str {
        // Annex C.5: WMI, "V", the OEM's own id and the check digit; no I, O or Q. The example of C.5.2.
        "DE8VAA0000453C4D58Y2"
    }
    fn evcc_mac(&self) -> [u8; 6] {
        self.mac
    }
    fn schedule_request(&self) -> ScheduleRequest {
        let l = &self.limits();
        // With a battery model the energy requests follow the state of charge: what is needed
        // for the target and the minimum, and what fits before the battery is full
        // (8.3.5.3.13: `EVTargetEnergyRequest` is "the energy the EV needs to fulfil the target
        // SOC"). Without one they are the numbers of the settings.
        let (target, maximum, minimum) = match &self.live {
            Some(live) => {
                let soc = live.present_soc();
                let needed = |percent: f64| RationalNumber::from_f64(((percent - soc).max(0.0) / 100.0 * live::CAPACITY_WH).round());
                match l.discharge_to_soc.filter(|_| live.discharging()) {
                    // Discharging: a negative target is the energy to give back (a BPT session).
                    Some(floor) => {
                        let give = RationalNumber::from_f64((-(soc - f64::from(floor)).max(0.0) / 100.0 * live::CAPACITY_WH).round());
                        (give, needed(100.0), give)
                    }
                    None => (needed(f64::from(l.target_soc)), needed(100.0), needed(f64::from(l.min_soc.unwrap_or(0)))),
                }
            }
            None => (rn(l.target_energy_wh), rn(l.max_energy_wh), rn(l.min_energy_wh)),
        };
        ScheduleRequest {
            departure_time: l.departure_time_s.max(1),
            minimum_soc: l.min_soc,
            target_soc: Some(l.target_soc),
            target_energy: target,
            maximum_energy: maximum,
            minimum_energy: minimum,
        }
    }
    fn dc_discharge_limits(&self) -> evcc_core::ev::DischargeLimits {
        let l = &self.limits();
        evcc_core::ev::DischargeLimits {
            maximum_discharge_power: rn(l.max_discharge_power_w),
            minimum_discharge_power: rn(l.min_discharge_power_w),
            maximum_discharge_current: rn(l.max_discharge_current_a),
            minimum_discharge_current: rn(l.min_discharge_current_a),
        }
    }
    fn dc_limits(&self) -> DcLimits {
        let l = &self.limits();
        DcLimits {
            maximum_charge_power: rn(l.max_charge_power_w),
            minimum_charge_power: rn(l.min_charge_power_w),
            maximum_charge_current: rn(l.max_charge_current_a),
            minimum_charge_current: rn(l.min_charge_current_a),
            maximum_voltage: rn(l.max_voltage_v),
            minimum_voltage: rn(l.min_voltage_v),
            target_soc: Some(l.target_soc),
        }
    }
    fn battery_voltage(&self) -> RationalNumber {
        RationalNumber::from_f64(self.live.as_ref().map_or(400.0, |l| l.battery_voltage()))
    }
    /// The first reading is below the battery voltage, the later ones match it.
    fn inlet_voltage(&self) -> RationalNumber {
        let reads = self.reads.get();
        self.reads.set(reads + 1);
        let battery = self.live.as_ref().map_or(400.0, |l| l.battery_voltage());
        RationalNumber::from_f64(if self.open.get() { 0.0 } else if reads == 0 { battery * 0.75 } else { battery })
    }
    fn contract(&self) -> Option<&dyn evcc_core::ev::Contract> {
        self.contract.as_deref()
    }
    fn present_soc(&self) -> Option<u8> {
        self.live.as_ref().map(|l| l.present_soc().round_ties_even().clamp(0.0, 100.0) as u8)
    }
    fn display(&self) -> evcc_core::ev::Display {
        let l = self.limits();
        let soc = self.live.as_ref().map(|live| live.present_soc());
        evcc_core::ev::Display {
            maximum_soc: Some(100),
            // The XSD makes every element optional; the Python library insists on this one.
            charging_complete: Some(soc.is_some_and(|s| s >= f64::from(l.target_soc))),
            battery_energy_capacity: self.live.as_ref().map(|_| rn(live::CAPACITY_WH as u32)),
            ..Default::default()
        }
    }
    fn energy_offer(&self) -> Option<evcc_core::ev::EnergyOffer> {
        let l = self.limits();
        let watts = l.energy_offer_w?;
        // The fee in cents per kWh is a RationalNumber of euros.
        Some(evcc_core::ev::EnergyOffer::new("EUR").power(l.energy_offer_s.max(1), rn(watts)).price(l.energy_offer_s.max(1), RationalNumber { exponent: -2, value: l.energy_offer_fee_cents.min(i16::MAX as u32) as i16 }))
    }
    fn dc_connector(&self) -> DcConnector {
        self.connector
    }
    fn ac_connector(&self) -> evcc_core::ev::AcConnector {
        if self.limits().ac_phases >= 3 { evcc_core::ev::AcConnector::ThreePhase } else { evcc_core::ev::AcConnector::SinglePhase }
    }
    fn ac20_limits(&self) -> evcc_core::ev::AcPowerLimits {
        let l = &self.limits();
        evcc_core::ev::AcPowerLimits {
            maximum_charge_power: self.per_line(l.ac_max_charge_power_w),
            minimum_charge_power: self.per_line(l.ac_min_charge_power_w),
        }
    }
    fn ac20_discharge_limits(&self) -> evcc_core::ev::AcDischargePowerLimits {
        let l = &self.limits();
        evcc_core::ev::AcDischargePowerLimits {
            maximum_discharge_power: self.per_line(l.ac_max_discharge_power_w),
            minimum_discharge_power: self.per_line(l.ac_min_discharge_power_w),
        }
    }
    /// The on-board charger follows the charger's target; with no battery model it draws nothing.
    fn ac_present_power(&self) -> evcc_core::ev::AcPresentPower {
        let watts = self.live.as_ref().map_or([0.0; 3], |l| l.ac_present_power());
        let lines = self.limits().ac_phases.clamp(1, 3) as usize;
        let phase = |i: usize| (i < lines).then(|| RationalNumber::from_f64(watts[i]));
        let zero = RationalNumber::from_f64(0.0);
        evcc_core::ev::AcPresentPower {
            active: evcc_core::ev::Phases { l1: RationalNumber::from_f64(watts[0]), l2: phase(1), l3: phase(2) },
            reactive: evcc_core::ev::Phases { l1: zero, l2: phase(1).map(|_| zero), l3: phase(2).map(|_| zero) },
        }
    }
    fn now_micros(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_micros() as u64)
    }
}

/// What the simulated car does besides charging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Behaviour {
    pub connector: DcConnector,
    /// After this many charge loop answers the car asks to stop (`None`: it never does, the
    /// EVSE ends the charge).
    pub stop_after: Option<u32>,
    /// After this many charge loop answers the car asks for a service renegotiation, once.
    pub renegotiate_after: Option<u32>,
    /// After this many charge loop answers the car asks for a schedule renegotiation, once.
    pub renegotiate_schedule_after: Option<u32>,
    /// Ask to stop as soon as a schedule renegotiation has finished (for tests that must not
    /// depend on how long it takes).
    pub stop_when_rescheduled: bool,
    /// After this many charge loop answers the car pauses the session (`SessionStopReq` Pause).
    pub pause_after: Option<u32>,
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
    run_car(stream, secured, config, Behaviour { connector, stop_after, renegotiate_after: None, renegotiate_schedule_after: None, stop_when_rescheduled: false, pause_after: None }, log)
}

pub fn run_car(
    stream: &mut impl Transport,
    secured: bool,
    config: Config,
    behaviour: Behaviour,
    log: impl LogSink,
) -> io::Result<Outcome> {
    run_car_live(stream, secured, config, behaviour, log, None, Limits::default(), None)
}

/// The configuration of the session machine the car runs: the protocol it speaks.
#[derive(Debug, Clone, Copy)]
pub enum SessionConfig {
    Iso20(Config),
    /// DIN SPEC 70121, with the MAC address the car sends as its EVCCID.
    Din(evcc_core::din::Config, [u8; 6]),
    /// ISO 15118-2, with the MAC address the car sends as its EVCCID.
    Iso2(evcc_core::iso2::Config, [u8; 6]),
}

impl From<Config> for SessionConfig {
    fn from(config: Config) -> Self {
        SessionConfig::Iso20(config)
    }
}

impl From<evcc_core::din::Config> for SessionConfig {
    fn from(config: evcc_core::din::Config) -> Self {
        SessionConfig::Din(config, Din::default().mac_address)
    }
}

/// [`run_car`] with the live state of a test platform (held at the start of each phase, with
/// measurements to see and set) and the vehicle's limits.
#[allow(clippy::too_many_arguments)]
pub fn run_car_live(
    stream: &mut impl Transport,
    secured: bool,
    config: impl Into<SessionConfig>,
    behaviour: Behaviour,
    log: impl LogSink,
    live: Option<Arc<EvLive>>,
    limits: Limits,
    contract: Option<Arc<dyn evcc_core::ev::Contract>>,
) -> io::Result<Outcome> {
    let (mut rx, mut tx, mut arena) = (vec![0u8; 8192], vec![0u8; 8192], vec![0u8; 32768]);
    let open = Rc::new(Cell::new(false));
    let config = config.into();
    let mut car = SimulatedEv::new(open.clone(), behaviour.connector).with_limits(limits);
    if let SessionConfig::Din(_, mac) | SessionConfig::Iso2(_, mac) = config {
        car = car.with_mac(mac);
    }
    if let Some(live) = &live {
        car = car.with_live(live.clone());
    }
    if let Some(contract) = contract {
        car = car.with_contract(contract);
    }
    let buffers = Buffers { rx: &mut rx, tx: &mut tx, arena: &mut arena };
    let (mut loops, mut renegotiated, mut rescheduled) = (0, false, false);
    let mut gate = |state| live.as_ref().is_some_and(|l| l.gate(state));
    // What the car does after a charge loop answer, DC or AC; `reached` is the battery model's wish to stop.
    let mut on_loop = |reached: bool| {
        loops += 1;
        if reached || Some(loops) == behaviour.stop_after {
            Some(Event::StopRequested)
        } else if let Some(action) = live.as_ref().and_then(|l| l.take_action()) {
            // Asked for from the test platform (the page), once.
            Some(match action {
                Action::Pause => Event::PauseRequested,
                Action::RenegotiateServices => Event::RenegotiateServices,
                Action::RenegotiateSchedule => Event::RenegotiateSchedule,
            })
        } else if Some(loops) == behaviour.pause_after {
            Some(Event::PauseRequested)
        } else if Some(loops) == behaviour.renegotiate_after && !renegotiated {
            renegotiated = true;
            Some(Event::RenegotiateServices)
        } else if Some(loops) == behaviour.renegotiate_schedule_after && !rescheduled {
            rescheduled = true;
            Some(Event::RenegotiateSchedule)
        } else {
            None
        }
    };
    let mut host = |n: &Notice| match n {
        // The car closes its contactors and stops drawing current as soon as it is asked to.
        Notice::CloseContactors => Some(Event::ContactorsClosed),
        Notice::StopEnergyTransfer { .. } => Some(Event::EnergyTransferStopped),
        Notice::OpenContactors => {
            open.set(true);
            Some(Event::ContactorsOpened)
        }
        // The control pilot follows at once (DIN SPEC 70121).
        Notice::ControlPilot(_) => Some(Event::ControlPilotChanged),
        // The charger's output is on again for the next pre-charge.
        Notice::ServicesRenegotiating => {
            open.set(false);
            None
        }
        Notice::ScheduleRenegotiated => behaviour.stop_when_rescheduled.then_some(Event::StopRequested),
        Notice::ChargeLoop(status) => {
            let reached = live.as_ref().is_some_and(|l| l.charge_loop(status));
            on_loop(reached)
        }
        Notice::AcChargeParametersDiscovered(mode) => {
            // V2G20-1829: the ramp limit in percent per minute of the maximum power; 0 is none (V2G20-1830).
            let ramp = match mode {
                iso15118_20::ac::AcCpdResEnergyTransferModeChoice::Base(m) => m.evse_power_ramp_limitation,
                iso15118_20::ac::AcCpdResEnergyTransferModeChoice::Bpt(m) => m.evse_power_ramp_limitation,
            };
            if let Some(live) = &live {
                live.set_ramp_limit(ramp.map_or(0.0, |r| r.to_f64()));
            }
            None
        }
        Notice::AcChargeLoop(status) => {
            let reached = live.as_ref().is_some_and(|l| l.ac_charge_loop(status));
            on_loop(reached)
        }
        _ => None,
    };
    let outcome = match config {
        SessionConfig::Iso20(config) => {
            let mut session = Session::new(config, car, log, buffers);
            evcc_linux::run_gated(stream, &mut session, secured, &mut host, &mut gate)
        }
        SessionConfig::Din(config, _) => {
            let mut session = evcc_core::din::Session::new(config, car, log, buffers);
            evcc_linux::run_gated(stream, &mut session, secured, &mut host, &mut gate)
        }
        SessionConfig::Iso2(config, _) => {
            let mut session = evcc_core::iso2::Session::new(config, car, log, buffers);
            evcc_linux::run_gated(stream, &mut session, secured, &mut host, &mut gate)
        }
    };
    if let Some(live) = &live {
        live.finished(format!("{outcome:?}"));
    }
    outcome
}

/// Prints records of at most `level` to standard error.
pub struct StderrLog {
    pub level: evcc_core::log::Level,
}

/// Sends records to a log and to the live state of a test platform for its timeline.
pub struct LiveLog<L> {
    pub inner: L,
    pub live: Option<Arc<EvLive>>,
}

impl<L: LogSink> LogSink for LiveLog<L> {
    fn record(&mut self, record: &evcc_core::log::Record<'_>) {
        if let Some(live) = &self.live {
            live.record(record);
        }
        self.inner.record(record);
    }
}

impl LogSink for StderrLog {
    fn record(&mut self, record: &evcc_core::log::Record<'_>) {
        if record.level() <= self.level {
            eprintln!("[{:?}] {record:?}", record.level());
        }
    }
}

/// A simulated control pilot for scenarios: the toggles are timed as the standard asks, and
/// every completed toggle appends one octet to a file, where the simulated charger counts them.
#[cfg(target_os = "linux")]
pub struct FilePilot(pub std::path::PathBuf);

#[cfg(target_os = "linux")]
impl evcc_linux::slac::ControlPilot for FilePilot {
    fn toggle(&mut self, toggle: evcc_core::slac::Toggle) -> io::Result<()> {
        use std::io::Write;
        let pause = std::time::Duration::from_millis(toggle.state_ms.into());
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&self.0)?;
        std::thread::sleep(pause); // the initial state B
        for _ in 0..toggle.count {
            std::thread::sleep(pause); // C
            std::thread::sleep(pause); // B again
            file.write_all(&[1])?;
            file.flush()?;
        }
        Ok(())
    }
}
