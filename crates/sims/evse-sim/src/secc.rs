//! The simulated charger as a `Secc` for `evse-core`: what it offers, what it delivers, and the faults
//! its settings inject. The session machine of ISO 15118-20 is the core's; everything here is the
//! charger's side of it.

use std::cell::Cell;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use evse_core::secc::{AcLoopAnswer, Action, Authorize, DcLoopAnswer, Ended, Progress, Request, Secc, Signed};
use iso15118_20::ResponseMessage;
use iso15118_20::ac;
use iso15118_20::common::{
    self, Authorization, ChargingSchedule, ChargingSession, EvpptControlModeChoice, MeteringConfirmationReq, Parameter, ParameterChoice, ParameterSet, PowerDeliveryReq,
    PowerSchedule, PowerScheduleEntry, PowerScheduleEntryList, ScheduleTuple, SelectedService, Service, ServiceSelectionReq,
};
use iso15118_20::common_types::{DetailedCost, EvseNotification, EvseStatus, MeterInfo, RationalNumber, Receipt, ResponseCode};
use iso15118_20::dc;
use openssl::x509::X509;
use v2gtp::PayloadType;

use crate::config::{Config, Limits, OfferedSet};
use crate::live::{Discharge, EvseLive, modelled_current};

/// Service ids of Table 204.
const SERVICE_AC: u16 = 1;
const SERVICE_DC: u16 = 2;
const SERVICE_AC_BPT: u16 = 5;
const SERVICE_DC_BPT: u16 = 6;

/// The schedule tuples a scheduled session offers: 1 is the charger's full power for an entry
/// (`schedule_entry_s`) and then half of it, 2 half of it throughout.
const TUPLES: [u32; 2] = [1, 2];

/// A power per line, in watts; a line the charger or the vehicle does not use is `None`.
type Lines = [Option<f64>; 3];

fn rational(x: Option<f64>) -> Option<RationalNumber> {
    x.map(RationalNumber::from_f64)
}

fn leak<T>(v: Vec<T>) -> &'static [T] {
    Box::leak(v.into_boxed_slice())
}

fn leak_str(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

/// The sets of a config as the codec wants them (the names and lists live for the rest of the process:
/// a few hundred bytes per session of the simulator).
fn build_sets(sets: &[OfferedSet], extra: &[(&str, i32)]) -> Vec<ParameterSet<'static>> {
    sets.iter()
        .map(|set| {
            let parameters: Vec<Parameter<'static>> =
                set.values.iter().map(|(n, v)| (n.as_str(), *v)).chain(extra.iter().copied()).map(|(name, v)| Parameter { name: leak_str(name), choice: ParameterChoice::IntValue(v) }).collect();
            ParameterSet { parameter_set_id: set.id, parameter: leak(parameters) }
        })
        .collect()
}

fn parameter(set: &OfferedSet, name: &str) -> Option<i32> {
    set.values.get(name).copied()
}

fn is_ok(code: ResponseCode) -> bool {
    evse_core::session::is_ok(code)
}

/// What a vehicle announced in its charge parameter discovery (V2G20-2198, 2199 hold the charge
/// loop to it).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Announced {
    max_charge: Lines,
    min_charge: Lines,
}

pub(crate) struct SimSecc {
    pub(crate) config: Config,
    pub(crate) live: Option<Arc<EvseLive>>,
    /// What the charger did, one line each (the log of the simulator).
    pub(crate) log: Vec<String>,
    services: Vec<Service>,
    dc_sets: Vec<ParameterSet<'static>>,
    dc_bpt_sets: Vec<ParameterSet<'static>>,
    ac_sets: Vec<ParameterSet<'static>>,
    tuples: Vec<ScheduleTuple<'static>>,
    // The selected service.
    service: u16,
    bpt: bool,
    ac: bool,
    scheduled: bool,
    ac_three: bool,
    ac_asym: bool,
    /// The `EVSENominalVoltage` of the selected AC parameter set, which the live measurements show (the
    /// AC power over this voltage is the current shown).
    ac_voltage: f64,
    ev_announced: Option<dc::DcCpdReqEnergyTransferMode>,
    ac_announced: Option<Announced>,
    // Counters of the knobs.
    authorization_reqs_seen: u32,
    schedule_reqs_seen: u32,
    cable_reqs_seen: u32,
    precharge_reqs_seen: u32,
    loops_seen: u32,
    /// A standby period is on, and how many charge loop answers it has had.
    standby: Option<u32>,
    pause_answered: bool,
    renegotiation_requested: bool,
    renegotiated: bool,
    side_renegotiated: bool,
    selected_tuple: u32,
    schedule_started: Option<Instant>,
    refused_resume: bool,
    // Plug & Charge and the meter.
    pnc: bool,
    contract: Option<X509>,
    energy_charged_wh: f64,
    energy_discharged_wh: f64,
    meter_updated: Option<Instant>,
    confirmation_outstanding: bool,
    /// The reading of the last `MeterInfo` sent, which a confirmation has to repeat.
    last_reading: Cell<Option<u64>>,
    receipt_due: Cell<bool>,
}

impl SimSecc {
    pub(crate) fn new(config: Config, live: Option<Arc<EvseLive>>) -> Self {
        let mut s = Self {
            config: config.clone(),
            live,
            log: Vec::new(),
            services: Vec::new(),
            dc_sets: Vec::new(),
            dc_bpt_sets: Vec::new(),
            ac_sets: Vec::new(),
            tuples: Vec::new(),
            service: 0,
            bpt: false,
            ac: false,
            scheduled: false,
            ac_three: false,
            ac_asym: false,
            ac_voltage: 400.0,
            ev_announced: None,
            ac_announced: None,
            authorization_reqs_seen: 0,
            schedule_reqs_seen: 0,
            cable_reqs_seen: 0,
            precharge_reqs_seen: 0,
            loops_seen: 0,
            standby: None,
            pause_answered: false,
            renegotiation_requested: false,
            renegotiated: false,
            side_renegotiated: false,
            selected_tuple: 0,
            schedule_started: None,
            refused_resume: false,
            pnc: false,
            contract: None,
            energy_charged_wh: 0.0,
            energy_discharged_wh: 0.0,
            meter_updated: None,
            confirmation_outstanding: false,
            last_reading: Cell::new(None),
            receipt_due: Cell::new(false),
        };
        s.set_config(config);
        s
    }

    /// New settings (a test platform changed them during the session).
    pub(crate) fn set_config(&mut self, config: Config) {
        self.services = config.energy_services.iter().map(|&service_id| Service { service_id, free_service: false }).collect();
        // Table 207: DC_BPT adds the channel (1 unified) and the generator mode (1 grid following).
        self.dc_sets = build_sets(&config.parameter_sets, &[]);
        self.dc_bpt_sets = build_sets(&config.parameter_sets, &[("BPTChannel", 1), ("GeneratorMode", 1)]);
        self.ac_sets = build_sets(&config.ac_parameter_sets, &[]);
        self.config = config;
    }

    fn limits(&self) -> Limits {
        self.live.as_ref().map_or(self.config.limits, |l| l.limits())
    }

    fn note(&mut self, line: impl Into<String>) {
        self.log.push(line.into());
    }

    fn set_for(&self, service_id: u16, id: u16) -> Option<&OfferedSet> {
        let sets = if matches!(service_id, SERVICE_AC | SERVICE_AC_BPT) { &self.config.ac_parameter_sets } else { &self.config.parameter_sets };
        sets.iter().find(|s| s.id == id)
    }

    /// The notification the next charge loop answer carries, if the config calls for one.
    fn loop_notification(&mut self) -> Option<(EvseNotification, u16)> {
        let c = &self.config;
        let terminate = c.terminate_after_loops.is_some_and(|n| self.loops_seen >= n);
        let confirmation = self.pnc
            && !self.confirmation_outstanding
            && c.metering_confirmation_every_loops.is_some_and(|n| n > 0 && self.loops_seen > 0 && self.loops_seen % n == 0);
        if terminate {
            return Some((EvseNotification::Terminate, 5));
        }
        if !self.side_renegotiated && c.schedule_renegotiation_after_loops.is_some_and(|n| self.loops_seen >= n) {
            return Some((EvseNotification::ScheduleRenegotiation, 10));
        }
        if !self.renegotiated && c.service_renegotiation_after_loops.is_some_and(|n| self.loops_seen >= n) {
            self.renegotiation_requested = true;
            return Some((EvseNotification::ServiceRenegotiation, 10));
        }
        if let Some(rounds) = &mut self.standby {
            *rounds += 1;
            return (*rounds > c.standby_loops).then_some((EvseNotification::ExitStandby, 60));
        }
        if !self.pause_answered && c.pause_after_loops.is_some_and(|n| self.loops_seen >= n) {
            return Some((EvseNotification::Pause, 60));
        }
        if confirmation {
            // V2G20-1083, V2G20-2105: one at a time, with the seconds the vehicle has.
            self.confirmation_outstanding = true;
            return Some((EvseNotification::MeteringConfirmation, 10));
        }
        None
    }

    /// The meter takes `watts` for the time since the last answer (positive: charged, negative: given back).
    fn account(&mut self, watts: f64) {
        let now = Instant::now();
        if let Some(before) = self.meter_updated.replace(now) {
            let wh = watts * now.duration_since(before).as_secs_f64() / 3600.0;
            if wh >= 0.0 {
                self.energy_charged_wh += wh;
            } else {
                self.energy_discharged_wh -= wh;
            }
        }
        let every = self.config.receipt_every_loops;
        self.receipt_due.set(every.is_some_and(|n| n > 0 && self.loops_seen % n == 0));
    }

    // ---- DC

    fn check_ev_loop_limits(&mut self, m: &dc::DynamicDcClReqControlMode) -> bool {
        let Some(a) = self.ev_announced else { return true };
        let checks = [
            ("V2G20-2198", "EVMaximumChargePower", m.ev_maximum_charge_power, a.ev_maximum_charge_power, true),
            ("V2G20-2198", "EVMaximumChargeCurrent", m.ev_maximum_charge_current, a.ev_maximum_charge_current, true),
            ("V2G20-2198", "EVMaximumVoltage", m.ev_maximum_voltage, a.ev_maximum_voltage, true),
            ("V2G20-2199", "EVMinimumChargePower", m.ev_minimum_charge_power, a.ev_minimum_charge_power, false),
            ("V2G20-2199", "EVMinimumVoltage", m.ev_minimum_voltage, a.ev_minimum_voltage, false),
        ];
        let mut fine = true;
        for (requirement, item, sent, announced, maximum) in checks {
            let (sent, announced) = (sent.to_f64(), announced.to_f64());
            if if maximum { sent > announced } else { sent < announced } {
                self.note(format!("violation: {requirement}: {item} is {sent}, the discovery announced {announced}"));
                fine = false;
            }
        }
        fine
    }

    // ---- AC

    /// The lines of the session: three when the selected parameter set has the ThreePhase connector and the
    /// charger has them.
    fn ac_lines_offered(&self) -> usize {
        if self.ac_three && self.limits().ac_phases >= 3 { 3 } else { 1 }
    }

    /// Whether the charger answers with the sum of the lines in the base elements: on three lines for a vehicle
    /// that gave no `_L2`, `_L3` values (V2G20-1817, 1209).
    fn sum_mode(&self, asymmetric: bool) -> bool {
        self.ac_lines_offered() == 3 && !asymmetric
    }

    /// A value of the vehicle per line: with `_L2` and `_L3` as they are, on three lines without them the base
    /// is the sum, spread evenly (V2G20-1817).
    fn in_lines(&self, base: RationalNumber, l2: Option<RationalNumber>, l3: Option<RationalNumber>) -> Lines {
        if self.ac_lines_offered() == 3 && l2.is_none() && l3.is_none() {
            [Some(base.to_f64() / 3.0); 3]
        } else {
            [Some(base.to_f64()), l2.map(RationalNumber::to_f64), l3.map(RationalNumber::to_f64)]
        }
    }

    /// A per-line limit of the charger as it goes into a message to this vehicle.
    fn per_line(&self, watts: u32) -> (RationalNumber, Option<RationalNumber>, Option<RationalNumber>) {
        let w = f64::from(watts);
        if self.sum_mode(self.ac_asym) {
            (RationalNumber::from_f64(w * 3.0), None, None)
        } else {
            let each = RationalNumber::from_f64(w);
            (each, (self.ac_lines_offered() == 3).then_some(each), (self.ac_lines_offered() == 3).then_some(each))
        }
    }

    fn note_ac_announced(&mut self, mode: &ac::AcCpdReqEnergyTransferModeChoice) {
        let (max, min) = match mode {
            ac::AcCpdReqEnergyTransferModeChoice::Base(m) => (
                (m.ev_maximum_charge_power, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3),
                (m.ev_minimum_charge_power, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3),
            ),
            ac::AcCpdReqEnergyTransferModeChoice::Bpt(m) => (
                (m.ev_maximum_charge_power, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3),
                (m.ev_minimum_charge_power, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3),
            ),
        };
        // Per-line values announced by a vehicle that gave `_L2` or `_L3` (V2G20-1821); otherwise sums.
        self.ac_asym = max.1.is_some() || max.2.is_some();
        let (max_charge, min_charge) = (self.in_lines(max.0, max.1, max.2), self.in_lines(min.0, min.1, min.2));
        self.ac_announced = Some(Announced { max_charge, min_charge });
    }

    /// Notes the values of a charge loop request that break V2G20-2198 or 2199; false when there were any.
    fn check_ac_loop_limits(&mut self, max_charge: Lines, min_charge: Lines) -> bool {
        let Some(a) = self.ac_announced else { return true };
        let mut fine = true;
        for (line, (sent, announced)) in max_charge.iter().zip(&a.max_charge).enumerate() {
            if let (Some(sent), Some(announced)) = (sent, announced)
                && sent > announced
            {
                self.note(format!("violation: V2G20-2198: EVMaximumChargePower line {} is {sent}, the discovery announced {announced}", line + 1));
                fine = false;
            }
        }
        for (line, (sent, announced)) in min_charge.iter().zip(&a.min_charge).enumerate() {
            if let (Some(sent), Some(announced)) = (sent, announced)
                && sent < announced
            {
                self.note(format!("violation: V2G20-2199: EVMinimumChargePower line {} is {sent}, the discovery announced {announced}", line + 1));
                fine = false;
            }
        }
        fine
    }

    /// The entries of a tuple's schedule, per line: `(duration, watts)`; the discharging schedule (bidirectional
    /// sessions) is the charger's discharge power.
    fn schedule_entries(&self, tuple: u32, discharging: bool) -> Vec<(u32, f64)> {
        let l = self.limits();
        let entry = self.config.schedule_entry_s.max(1);
        if discharging {
            return vec![(entry, f64::from(l.ac_max_discharge_power_w))];
        }
        let full = f64::from(l.ac_max_charge_power_w);
        if tuple == 1 { vec![(entry, full), (entry, full / 2.0)] } else { vec![(entry, full / 2.0)] }
    }

    /// The power per line the selected tuple asks for now, counted from the start of the power delivery; the last
    /// entry holds beyond the end.
    fn scheduled_power(&self, discharging: bool) -> f64 {
        let mut left = self.schedule_started.map_or(0, |t| t.elapsed().as_secs());
        let entries = self.schedule_entries(self.selected_tuple, discharging);
        for (duration, watts) in &entries {
            if left < u64::from(*duration) {
                return *watts;
            }
            left -= u64::from(*duration);
        }
        entries.last().map_or(0.0, |e| e.1)
    }

    /// The tuples of the schedule exchange, built from the limits now.
    fn build_tuples(&mut self) {
        // `TimeAnchor` in microseconds of SECC time (Annex J examples; 8.3.5.3.x says milliseconds in one place).
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64);
        let entries = |tuple: u32, discharging: bool| -> &'static [PowerScheduleEntry] {
            leak(
                self.schedule_entries(tuple, discharging)
                    .into_iter()
                    .map(|(duration, watts)| {
                        let (power, power_l2, power_l3) = self.per_line(watts as u32);
                        PowerScheduleEntry { duration, power, power_l2, power_l3 }
                    })
                    .collect(),
            )
        };
        let schedule = |entries: &'static [PowerScheduleEntry]| ChargingSchedule {
            power_schedule: PowerSchedule { time_anchor: now, available_energy: None, power_tolerance: None, power_schedule_entries: PowerScheduleEntryList { power_schedule_entry: entries } },
            schedule: None,
        };
        let discharging = entries(1, true);
        self.tuples = TUPLES.iter().map(|&id| ScheduleTuple { schedule_tuple_id: id, charging_schedule: schedule(entries(id, false)), discharging_schedule: self.bpt.then(|| schedule(discharging)) }).collect();
    }
}

impl Secc for SimSecc {
    fn evse_id(&self) -> &str {
        &self.config.evse_id
    }

    fn now_micros(&self) -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64)
    }

    fn new_session_id(&mut self) -> [u8; 8] {
        // V2G20-2547: an id we do not know is a new session, with an id of its own.
        if self.refused_resume { self.config.session_id.map(|b| !b) } else { self.config.session_id }
    }

    fn gen_challenge(&mut self) -> [u8; 16] {
        let mut challenge = [0u8; 16];
        openssl::rand::rand_bytes(&mut challenge).expect("random bytes");
        challenge
    }

    fn supports_protocol(&self, namespace: &str, major: u32) -> bool {
        !self.config.reject_protocol && (namespace == "urn:iso:std:iso:15118:-20:DC" || namespace == "urn:iso:std:iso:15118:-20:AC") && major == 1
    }

    fn authorization_services(&self) -> &[Authorization] {
        &self.config.offered_authorization
    }

    fn energy_services(&self) -> &[Service] {
        &self.services
    }

    fn parameter_sets(&self, service_id: u16) -> &[ParameterSet<'_>] {
        match service_id {
            SERVICE_AC | SERVICE_AC_BPT => &self.ac_sets,
            SERVICE_DC_BPT => &self.dc_bpt_sets,
            _ => &self.dc_sets,
        }
    }

    fn service_renegotiation_supported(&self) -> bool {
        self.config.service_renegotiation_supported
    }

    fn accept_service_renegotiation(&mut self) -> bool {
        self.config.service_renegotiation_supported && !self.config.refuse_service_renegotiation
    }

    fn resume(&mut self, session_id: [u8; 8]) -> Option<SelectedService> {
        if session_id == self.config.session_id && !self.config.forget_paused_sessions {
            // V2G20-2623: a resumed session; authorization carries over. This process serves a new connection,
            // so the selection is the first DC service's first parameter set.
            let service_id = self.config.energy_services.iter().copied().find(|s| matches!(*s, SERVICE_DC | SERVICE_DC_BPT)).unwrap_or(SERVICE_DC);
            self.service = service_id;
            self.bpt = service_id == SERVICE_DC_BPT;
            Some(SelectedService { service_id, parameter_set_id: self.config.parameter_sets.first().map_or(1, |s| s.id) })
        } else {
            self.refused_resume = true;
            None
        }
    }

    fn authorize(&mut self, request: Authorize<'_, '_>) -> Progress<ResponseCode> {
        self.authorization_reqs_seen += 1;
        let ongoing = self.config.authorization_never_finishes || self.authorization_reqs_seen <= self.config.authorization_ongoing_responses;
        // What the selection and, for Plug & Charge, the contract come to (V2G20-2564, 2565).
        let (verdict, contract, pnc) = match request {
            Authorize::Eim => (ResponseCode::Ok, None, false),
            Authorize::PnC { header, mode, challenge, .. } => {
                let (verdict, contract) = crate::pnc::verify_contract(&self.config.pnc, challenge, header, mode);
                self.note(format!(
                    "PnC authorization: {verdict:?}, challenge {}, contract chain of {} certificate(s)",
                    if mode.gen_challenge == challenge { "matches" } else { "does not match" },
                    1 + mode.contract_certificate_chain.sub_certificates.certificate.len()
                ));
                (verdict, contract, true)
            }
        };
        if ongoing {
            return Progress::Ongoing;
        }
        // The knob can still refuse an authorization that was fine.
        let code = if is_ok(verdict) && self.config.authorization_code != ResponseCode::Ok { self.config.authorization_code } else { verdict };
        if is_ok(code) {
            self.pnc = pnc;
            self.contract = contract;
        }
        Progress::Done(code)
    }

    fn service_selected(&mut self, selection: &ServiceSelectionReq<'_>) {
        let SelectedService { service_id, parameter_set_id } = selection.selected_energy_transfer_service;
        self.service = service_id;
        self.bpt = matches!(service_id, SERVICE_AC_BPT | SERVICE_DC_BPT);
        self.ac = matches!(service_id, SERVICE_AC | SERVICE_AC_BPT);
        let set = self.set_for(service_id, parameter_set_id).cloned();
        self.scheduled = set.as_ref().and_then(|s| parameter(s, "ControlMode")) == Some(1);
        self.ac_three = set.as_ref().and_then(|s| parameter(s, "Connector")) == Some(2);
        if let Some(volts) = set.as_ref().and_then(|s| parameter(s, "EVSENominalVoltage")) {
            self.ac_voltage = f64::from(volts);
        }
    }

    fn dc_charge_parameters(&mut self, bpt: bool, request: &dc::DcChargeParameterDiscoveryReq<'_>) -> Result<dc::DcCpdResEnergyTransferModeChoice, ResponseCode> {
        self.ev_announced = Some(match &request.dc_cpd_req_energy_transfer_mode {
            dc::DcCpdReqEnergyTransferModeChoice::Base(m) => *m,
            dc::DcCpdReqEnergyTransferModeChoice::Bpt(m) => dc::DcCpdReqEnergyTransferMode {
                ev_maximum_charge_power: m.ev_maximum_charge_power,
                ev_minimum_charge_power: m.ev_minimum_charge_power,
                ev_maximum_charge_current: m.ev_maximum_charge_current,
                ev_minimum_charge_current: m.ev_minimum_charge_current,
                ev_maximum_voltage: m.ev_maximum_voltage,
                ev_minimum_voltage: m.ev_minimum_voltage,
                target_soc: m.target_soc,
            },
        });
        self.schedule_reqs_seen = 0;
        let rn = |value: u32| RationalNumber::from_f64(f64::from(value));
        let l = self.limits();
        let base = dc::DcCpdResEnergyTransferMode {
            evse_maximum_charge_power: rn(l.max_charge_power_w),
            evse_minimum_charge_power: rn(l.min_charge_power_w),
            evse_maximum_charge_current: rn(l.max_charge_current_a),
            evse_minimum_charge_current: rn(l.min_charge_current_a),
            evse_maximum_voltage: rn(l.max_voltage_v),
            evse_minimum_voltage: rn(l.min_voltage_v),
            evse_power_ramp_limitation: l.power_ramp_limitation_percent_per_min.map(rn),
        };
        Ok(if bpt {
            dc::DcCpdResEnergyTransferModeChoice::Bpt(dc::BptDcCpdResEnergyTransferMode {
                evse_maximum_charge_power: base.evse_maximum_charge_power,
                evse_minimum_charge_power: base.evse_minimum_charge_power,
                evse_maximum_charge_current: base.evse_maximum_charge_current,
                evse_minimum_charge_current: base.evse_minimum_charge_current,
                evse_maximum_voltage: base.evse_maximum_voltage,
                evse_minimum_voltage: base.evse_minimum_voltage,
                evse_power_ramp_limitation: base.evse_power_ramp_limitation,
                evse_maximum_discharge_power: rn(l.max_discharge_power_w),
                evse_minimum_discharge_power: rn(l.min_discharge_power_w),
                evse_maximum_discharge_current: rn(l.max_discharge_current_a),
                evse_minimum_discharge_current: rn(l.min_discharge_current_a),
            })
        } else {
            dc::DcCpdResEnergyTransferModeChoice::Base(base)
        })
    }

    fn ac_charge_parameters(&mut self, bpt: bool, request: &ac::AcChargeParameterDiscoveryReq<'_>) -> Result<ac::AcCpdResEnergyTransferModeChoice, ResponseCode> {
        self.note_ac_announced(&request.ac_cpd_req_energy_transfer_mode);
        self.schedule_reqs_seen = 0;
        let l = self.limits();
        let (max_1, max_2, max_3) = self.per_line(l.ac_max_charge_power_w);
        let (min_1, min_2, min_3) = self.per_line(l.ac_min_charge_power_w);
        let base = ac::AcCpdResEnergyTransferMode {
            evse_maximum_charge_power: max_1,
            evse_maximum_charge_power_l2: max_2,
            evse_maximum_charge_power_l3: max_3,
            evse_minimum_charge_power: min_1,
            evse_minimum_charge_power_l2: min_2,
            evse_minimum_charge_power_l3: min_3,
            evse_nominal_frequency: RationalNumber::from_f64(f64::from(l.ac_nominal_frequency_hz)),
            maximum_power_asymmetry: None,
            evse_power_ramp_limitation: l.power_ramp_limitation_percent_per_min.map(|p| RationalNumber::from_f64(f64::from(p))),
            evse_present_active_power: None,
            evse_present_active_power_l2: None,
            evse_present_active_power_l3: None,
        };
        Ok(if bpt {
            let (dmax_1, dmax_2, dmax_3) = self.per_line(l.ac_max_discharge_power_w);
            let (dmin_1, dmin_2, dmin_3) = self.per_line(l.ac_min_discharge_power_w);
            ac::AcCpdResEnergyTransferModeChoice::Bpt(ac::BptAcCpdResEnergyTransferMode {
                evse_maximum_charge_power: base.evse_maximum_charge_power,
                evse_maximum_charge_power_l2: base.evse_maximum_charge_power_l2,
                evse_maximum_charge_power_l3: base.evse_maximum_charge_power_l3,
                evse_minimum_charge_power: base.evse_minimum_charge_power,
                evse_minimum_charge_power_l2: base.evse_minimum_charge_power_l2,
                evse_minimum_charge_power_l3: base.evse_minimum_charge_power_l3,
                evse_nominal_frequency: base.evse_nominal_frequency,
                maximum_power_asymmetry: base.maximum_power_asymmetry,
                evse_power_ramp_limitation: base.evse_power_ramp_limitation,
                evse_present_active_power: None,
                evse_present_active_power_l2: None,
                evse_present_active_power_l3: None,
                evse_maximum_discharge_power: dmax_1,
                evse_maximum_discharge_power_l2: dmax_2,
                evse_maximum_discharge_power_l3: dmax_3,
                evse_minimum_discharge_power: dmin_1,
                evse_minimum_discharge_power_l2: dmin_2,
                evse_minimum_discharge_power_l3: dmin_3,
            })
        } else {
            ac::AcCpdResEnergyTransferModeChoice::Base(base)
        })
    }

    fn schedule_ready(&mut self, renegotiation: bool) -> Progress<()> {
        self.schedule_reqs_seen += 1;
        let _ = renegotiation;
        if self.schedule_reqs_seen <= self.config.schedule_ongoing_responses {
            return Progress::Ongoing;
        }
        if self.scheduled {
            self.build_tuples();
        }
        Progress::Done(())
    }

    fn schedule_tuples(&self) -> &[ScheduleTuple<'_>] {
        &self.tuples
    }

    fn cable_check(&mut self) -> Progress<ResponseCode> {
        self.cable_reqs_seen += 1;
        if self.cable_reqs_seen <= self.config.cable_check_ongoing_responses { Progress::Ongoing } else { Progress::Done(self.config.cable_check_code) }
    }

    fn pre_charge(&mut self, target: RationalNumber, _vehicle: RationalNumber) -> RationalNumber {
        self.precharge_reqs_seen += 1;
        // Until "settled" the output is 100 V away from the EV's target.
        if self.precharge_reqs_seen >= self.config.precharge_requests_to_settle {
            target
        } else {
            RationalNumber { exponent: target.exponent, value: target.value.saturating_sub(100) }
        }
    }

    fn welding_detection(&mut self) -> RationalNumber {
        RationalNumber { exponent: 0, value: 5 }
    }

    fn power_delivery(&mut self, request: &PowerDeliveryReq<'_>) -> ResponseCode {
        if let Some(EvpptControlModeChoice::Scheduled(m)) = request.ev_power_profile.as_ref().map(|p| p.evppt_control_mode) {
            self.selected_tuple = m.selected_schedule_tuple_id;
        }
        match request.charge_progress {
            common::ChargeProgress::Start if self.standby.is_some() => {
                // V2G20-1386: Start ends the standby period.
                self.standby = None;
                ResponseCode::Ok
            }
            common::ChargeProgress::Start => {
                self.schedule_started = Some(Instant::now());
                self.config.power_delivery_code
            }
            common::ChargeProgress::Standby => {
                // V2G20-1385, V2G20-1387
                self.pause_answered = true;
                if self.config.standby_allowed {
                    self.standby = Some(0);
                    ResponseCode::Ok
                } else {
                    ResponseCode::WarningStandbyNotAllowed
                }
            }
            _ => ResponseCode::Ok,
        }
    }

    fn schedule_renegotiated(&mut self, request: &PowerDeliveryReq<'_>) -> ResponseCode {
        if let Some(EvpptControlModeChoice::Scheduled(m)) = request.ev_power_profile.as_ref().map(|p| p.evppt_control_mode) {
            self.selected_tuple = m.selected_schedule_tuple_id;
        }
        self.side_renegotiated = true;
        self.schedule_started = Some(Instant::now());
        ResponseCode::Ok
    }

    fn dc_loop(&mut self, request: &dc::DcChargeLoopReq<'_>, bpt: bool) -> Option<DcLoopAnswer> {
        self.loops_seen += 1;
        let notification = self.loop_notification();
        let rn = |value: u32| RationalNumber::from_f64(f64::from(value));
        let l = self.limits();
        // A negative EVTargetEnergyRequest asks to discharge: the vehicle gives energy back, within both
        // sides' discharge limits.
        let discharge = match &request.cl_req_control_mode {
            dc::ClReqControlModeChoice::BptDynamicDc(m) if bpt && m.ev_target_energy_request.to_f64() < 0.0 => {
                Some(Discharge { max_current: m.ev_maximum_discharge_current.to_f64(), max_power: m.ev_maximum_discharge_power.to_f64() })
            }
            _ => None,
        };
        let requested = match &request.cl_req_control_mode {
            dc::ClReqControlModeChoice::DynamicDc(m) => Some(*m),
            dc::ClReqControlModeChoice::BptDynamicDc(m) => Some(dc::DynamicDcClReqControlMode {
                departure_time: m.departure_time,
                ev_target_energy_request: m.ev_target_energy_request,
                ev_maximum_energy_request: m.ev_maximum_energy_request,
                ev_minimum_energy_request: m.ev_minimum_energy_request,
                ev_maximum_charge_power: m.ev_maximum_charge_power,
                ev_minimum_charge_power: m.ev_minimum_charge_power,
                ev_maximum_charge_current: m.ev_maximum_charge_current,
                ev_maximum_voltage: m.ev_maximum_voltage,
                ev_minimum_voltage: m.ev_minimum_voltage,
            }),
            _ => None,
        };
        let ev_voltage = request.ev_present_voltage.to_f64();
        let (ev_max_current, ev_max_power, ev_max_voltage) = match &requested {
            Some(m) => (m.ev_maximum_charge_current.to_f64(), m.ev_maximum_charge_power.to_f64(), m.ev_maximum_voltage.to_f64()),
            None => (0.0, 0.0, 0.0),
        };
        let mut code = ResponseCode::Ok;
        if let Some(m) = &requested
            && !self.check_ev_loop_limits(m)
            && self.config.fail_on_limit_violation
        {
            code = ResponseCode::Failed;
        }
        let (voltage, current, max_power) = match &self.live {
            Some(live) => live.charge_loop(ev_voltage, ev_max_current, ev_max_power, discharge),
            None => (ev_voltage, modelled_current(ev_voltage, ev_max_current, ev_max_power, discharge, &l), f64::from(l.max_charge_power_w)),
        };
        self.account(voltage * current);
        // What limited the delivery: the flags say which limit the charger has reached.
        let limit = |achieved: f64, limit: f64| achieved != 0.0 && achieved.abs() >= limit * 0.999;
        let (current_bound, power_bound) = match discharge {
            Some(d) => (d.max_current.min(f64::from(l.max_discharge_current_a)), d.max_power.min(f64::from(l.max_discharge_power_w))),
            None => (ev_max_current.min(f64::from(l.max_charge_current_a)), ev_max_power.min(f64::from(l.max_charge_power_w))),
        };
        let current_limit_achieved = limit(current, current_bound);
        let power_limit_achieved = limit(voltage * current, power_bound);
        let voltage_limit_achieved = limit(voltage, if ev_max_voltage > 0.0 { ev_max_voltage.min(f64::from(l.max_voltage_v)) } else { f64::from(l.max_voltage_v) });
        let control_mode = if bpt {
            dc::ClResControlModeChoice::BptDynamicDc(dc::BptDynamicDcClResControlMode {
                departure_time: None,
                minimum_soc: None,
                target_soc: None,
                ack_max_delay: None,
                evse_maximum_charge_power: RationalNumber::from_f64(max_power),
                evse_minimum_charge_power: rn(l.min_charge_power_w),
                evse_maximum_charge_current: rn(l.max_charge_current_a),
                evse_maximum_voltage: rn(l.max_voltage_v),
                evse_maximum_discharge_power: rn(l.max_discharge_power_w),
                evse_minimum_discharge_power: rn(l.min_discharge_power_w),
                evse_maximum_discharge_current: rn(l.max_discharge_current_a),
                evse_minimum_voltage: rn(l.min_voltage_v),
            })
        } else {
            dc::ClResControlModeChoice::DynamicDc(dc::DynamicDcClResControlMode {
                departure_time: None,
                minimum_soc: None,
                target_soc: None,
                ack_max_delay: None,
                evse_maximum_charge_power: RationalNumber::from_f64(max_power),
                evse_minimum_charge_power: rn(l.min_charge_power_w),
                evse_maximum_charge_current: rn(l.max_charge_current_a),
                evse_maximum_voltage: rn(l.max_voltage_v),
            })
        };
        Some(DcLoopAnswer {
            response_code: code,
            present_current: RationalNumber::from_f64(current),
            present_voltage: RationalNumber::from_f64(voltage),
            power_limit_achieved,
            current_limit_achieved,
            voltage_limit_achieved,
            control_mode,
            status: notification.map(|(evse_notification, notification_max_delay)| EvseStatus { notification_max_delay, evse_notification }),
        })
    }

    /// `AC_ChargeLoopReq`: the target active power per line is what both sides allow, negative when the vehicle
    /// asks to discharge (a negative `EVTargetEnergyRequest`, as in DC).
    fn ac_loop(&mut self, request: &ac::AcChargeLoopReq<'_>, bpt: bool) -> Option<AcLoopAnswer> {
        self.loops_seen += 1;
        let notification = self.loop_notification();
        let l = self.limits();
        let mut code = ResponseCode::Ok;
        // What the vehicle asks for, per line.
        let (target_energy, ev_max, ev_min, ev_discharge_max) = match &request.cl_req_control_mode {
            ac::ClReqControlModeChoice::DynamicAc(m) => (
                m.ev_target_energy_request.to_f64(),
                self.in_lines(m.ev_maximum_charge_power, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3),
                self.in_lines(m.ev_minimum_charge_power, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3),
                [None; 3],
            ),
            ac::ClReqControlModeChoice::BptDynamicAc(m) => (
                m.ev_target_energy_request.to_f64(),
                self.in_lines(m.ev_maximum_charge_power, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3),
                self.in_lines(m.ev_minimum_charge_power, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3),
                self.in_lines(m.ev_maximum_discharge_power, m.ev_maximum_discharge_power_l2, m.ev_maximum_discharge_power_l3),
            ),
            // Scheduled: what the vehicle left out it does not limit; no target energy is a charge.
            ac::ClReqControlModeChoice::ScheduledAc(m) => (
                m.ev_target_energy_request.map_or(1.0, RationalNumber::to_f64),
                m.ev_maximum_charge_power.map_or([None; 3], |b| self.in_lines(b, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3)),
                m.ev_minimum_charge_power.map_or([None; 3], |b| self.in_lines(b, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3)),
                [None; 3],
            ),
            ac::ClReqControlModeChoice::BptScheduledAc(m) => (
                m.ev_target_energy_request.map_or(1.0, RationalNumber::to_f64),
                m.ev_maximum_charge_power.map_or([None; 3], |b| self.in_lines(b, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3)),
                m.ev_minimum_charge_power.map_or([None; 3], |b| self.in_lines(b, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3)),
                m.ev_maximum_discharge_power.map_or([None; 3], |b| self.in_lines(b, m.ev_maximum_discharge_power_l2, m.ev_maximum_discharge_power_l3)),
            ),
        };
        if !self.check_ac_loop_limits(ev_max, ev_min) && self.config.fail_on_limit_violation {
            code = ResponseCode::Failed;
        }
        // The lines the vehicle says it draws on (Scheduled mode may leave its limits out).
        let raw_lines: [bool; 3] = match &request.cl_req_control_mode {
            ac::ClReqControlModeChoice::ScheduledAc(m) => [true, m.ev_present_active_power_l2.is_some(), m.ev_present_active_power_l3.is_some()],
            ac::ClReqControlModeChoice::BptScheduledAc(m) => [true, m.ev_present_active_power_l2.is_some(), m.ev_present_active_power_l3.is_some()],
            ac::ClReqControlModeChoice::DynamicAc(m) => [true, m.ev_present_active_power_l2.is_some(), m.ev_present_active_power_l3.is_some()],
            ac::ClReqControlModeChoice::BptDynamicAc(m) => [true, m.ev_present_active_power_l2.is_some(), m.ev_present_active_power_l3.is_some()],
        };
        // A vehicle without `_L2`, `_L3` values on three lines is symmetric: sums, no phase elements in the answer (V2G20-1209).
        let sum_mode = self.sum_mode(raw_lines[1] || raw_lines[2]);
        let present_lines = if sum_mode { [true; 3] } else { raw_lines };
        let discharging = bpt && target_energy < 0.0;
        let offered = self.ac_lines_offered();
        let mut target: Lines = [None; 3];
        for line in 0..offered {
            // Dynamic: a line the vehicle does not use carries nothing. Scheduled: the vehicle's limits are optional,
            // and the schedule's entry is what the charger asks for.
            let vehicle = if self.scheduled {
                let own = if discharging { ev_discharge_max[line] } else { ev_max[line] };
                if !present_lines[line] {
                    continue;
                }
                own.unwrap_or(f64::MAX)
            } else {
                let Some(v) = (if discharging { ev_discharge_max[line] } else { ev_max[line] }) else { continue };
                v
            };
            let charger = if self.scheduled { self.scheduled_power(discharging) } else { f64::from(if discharging { l.ac_max_discharge_power_w } else { l.ac_max_charge_power_w }) };
            let minimum = f64::from(if discharging { l.ac_min_discharge_power_w } else { l.ac_min_charge_power_w });
            let power = vehicle.min(charger);
            // Below the minimum the charger cannot deliver with the expected accuracy.
            let power = if power < minimum { 0.0 } else { power };
            target[line] = Some(if discharging { -power } else { power });
        }
        let used = target.iter().flatten().count().max(1) as f64;
        let total: f64 = target.iter().flatten().sum();
        let voltage = self.ac_voltage;
        // A test platform may set the current by hand: the power follows, evenly over the lines.
        let max_total = f64::from(l.ac_max_charge_power_w.max(l.ac_max_discharge_power_w)) * offered as f64;
        let (shown_voltage, current, max_power): (f64, f64, f64) = match &self.live {
            Some(live) => live.ac_charge_loop(voltage, total / voltage, max_total),
            None => (voltage, total / voltage, max_total),
        };
        let total_now = (current * shown_voltage).clamp(-max_power.abs(), max_power.abs());
        let overridden = (total_now - total).abs() > 1e-6;
        if overridden {
            for t in target.iter_mut().flatten() {
                *t = total_now / used;
            }
        }
        self.account(total_now);
        let present = target;
        // The answer in sums for a symmetric vehicle.
        let (target, present) = if sum_mode {
            let sum = |t: Lines| [Some(t.iter().flatten().sum::<f64>()), None, None];
            (sum(target), sum(present))
        } else {
            (target, present)
        };
        let status = notification.map(|(evse_notification, notification_max_delay)| EvseStatus { notification_max_delay, evse_notification });
        let dynamic = ac::DynamicAcClResControlMode {
            departure_time: None,
            minimum_soc: None,
            target_soc: None,
            ack_max_delay: None,
            evse_target_active_power: RationalNumber::from_f64(target[0].unwrap_or(0.0)),
            evse_target_active_power_l2: rational(target[1]),
            evse_target_active_power_l3: rational(target[2]),
            evse_target_reactive_power: None,
            evse_target_reactive_power_l2: None,
            evse_target_reactive_power_l3: None,
            evse_present_active_power: rational(present[0]),
            evse_present_active_power_l2: rational(present[1]),
            evse_present_active_power_l3: rational(present[2]),
        };
        let control_mode = if self.scheduled {
            // V2G20-1826: a target only where something overrides the schedule (a hand-set current).
            let scheduled = ac::ScheduledAcClResControlMode {
                evse_target_active_power: overridden.then_some(dynamic.evse_target_active_power),
                evse_target_active_power_l2: if overridden { dynamic.evse_target_active_power_l2 } else { None },
                evse_target_active_power_l3: if overridden { dynamic.evse_target_active_power_l3 } else { None },
                evse_target_reactive_power: None,
                evse_target_reactive_power_l2: None,
                evse_target_reactive_power_l3: None,
                evse_present_active_power: dynamic.evse_present_active_power,
                evse_present_active_power_l2: dynamic.evse_present_active_power_l2,
                evse_present_active_power_l3: dynamic.evse_present_active_power_l3,
            };
            if bpt {
                ac::ClResControlModeChoice::BptScheduledAc(ac::BptScheduledAcClResControlMode {
                    evse_target_active_power: scheduled.evse_target_active_power,
                    evse_target_active_power_l2: scheduled.evse_target_active_power_l2,
                    evse_target_active_power_l3: scheduled.evse_target_active_power_l3,
                    evse_target_reactive_power: None,
                    evse_target_reactive_power_l2: None,
                    evse_target_reactive_power_l3: None,
                    evse_present_active_power: scheduled.evse_present_active_power,
                    evse_present_active_power_l2: scheduled.evse_present_active_power_l2,
                    evse_present_active_power_l3: scheduled.evse_present_active_power_l3,
                })
            } else {
                ac::ClResControlModeChoice::ScheduledAc(scheduled)
            }
        } else if bpt {
            ac::ClResControlModeChoice::BptDynamicAc(ac::BptDynamicAcClResControlMode {
                departure_time: dynamic.departure_time,
                minimum_soc: dynamic.minimum_soc,
                target_soc: dynamic.target_soc,
                ack_max_delay: dynamic.ack_max_delay,
                evse_target_active_power: dynamic.evse_target_active_power,
                evse_target_active_power_l2: dynamic.evse_target_active_power_l2,
                evse_target_active_power_l3: dynamic.evse_target_active_power_l3,
                evse_target_reactive_power: None,
                evse_target_reactive_power_l2: None,
                evse_target_reactive_power_l3: None,
                evse_present_active_power: dynamic.evse_present_active_power,
                evse_present_active_power_l2: dynamic.evse_present_active_power_l2,
                evse_present_active_power_l3: dynamic.evse_present_active_power_l3,
            })
        } else {
            ac::ClResControlModeChoice::DynamicAc(dynamic)
        };
        Some(AcLoopAnswer { response_code: code, target_frequency: None, control_mode, status })
    }

    fn meter_info(&self, wanted: bool) -> Option<MeterInfo<'_>> {
        if !(wanted || self.config.meter_info_always || self.receipt_due.get()) {
            return None;
        }
        // V2G20-902: the energy charged during the service session so far.
        let charged = self.energy_charged_wh as u64;
        self.last_reading.set(Some(charged));
        Some(MeterInfo {
            meter_id: &self.config.meter_id,
            charged_energy_reading_wh: charged,
            bpt_discharged_energy_reading_wh: self.bpt.then_some(self.energy_discharged_wh as u64),
            capacitive_energy_reading_va_rh: None,
            bpt_inductive_energy_reading_va_rh: None,
            meter_signature: None,
            meter_status: None,
            meter_timestamp: Some(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())),
        })
    }

    fn receipt(&self) -> Option<Receipt<'_>> {
        if !self.receipt_due.get() {
            return None;
        }
        // 0.30 per kWh, in cents (V2G20-1918: at least one cost; V2G20-1919: the MeterInfo goes with it).
        let kwh = self.energy_charged_wh / 1000.0;
        let cents = (kwh * 30.0).round().clamp(0.0, f64::from(i16::MAX)) as i16;
        Some(Receipt {
            time_anchor: self.now_micros(),
            energy_costs: Some(DetailedCost { amount: RationalNumber { exponent: -2, value: cents }, cost_per_unit: RationalNumber { exponent: -2, value: 30 } }),
            occupancy_costs: None,
            additional_services_costs: None,
            overstay_costs: None,
            tax_costs: &[],
        })
    }

    fn confirm_metering(&mut self, request: &MeteringConfirmationReq<'_>, signed: Signed<'_, '_>) -> ResponseCode {
        self.confirmation_outstanding = false;
        // V2G20-904: with the contract of the authorization, when the charger has it.
        let code = match &self.contract {
            Some(contract) => crate::pnc::verify_metering(contract, signed.header.signature.as_ref(), &request.signed_metering_data, signed.fragment, self.last_reading.get()),
            None => ResponseCode::Ok,
        };
        self.note(format!("metering confirmation: {} (reading {} Wh)", code.xml_name(), request.signed_metering_data.meter_info.charged_energy_reading_wh));
        code
    }

    fn session_ended(&mut self, how: Ended) {
        if how == Ended::Stopped(ChargingSession::ServiceRenegotiation) {
            // The connection and the authorization stay; the service flow starts over.
            (self.cable_reqs_seen, self.precharge_reqs_seen, self.loops_seen) = (0, 0, 0);
            self.standby = None;
            self.pause_answered = false;
            self.renegotiated = true;
            self.renegotiation_requested = false;
        }
    }

    fn intercept(&mut self, payload_type: PayloadType, request: &Request<'_>) -> Action {
        if self.config.silent {
            return Action::Silent;
        }
        if self.config.silent_on_setup && matches!(request, Request::Common(common::Message::SessionSetupReq(_))) {
            return Action::Silent;
        }
        if self.config.side_stream_silent && payload_type == PayloadType::SCHEDULE_RENEGOTIATION {
            return Action::Silent;
        }
        Action::Pass
    }

    fn response_code(&mut self, message: ResponseMessage, code: ResponseCode) -> ResponseCode {
        let c = &self.config;
        use ResponseMessage as M;
        // The knobs replace the code of a message that was going fine; a failure of the machine's own stays.
        let knob = match message {
            M::SessionSetup if code == ResponseCode::OkNewSessionEstablished => Some(c.setup_code),
            M::SessionSetup if !is_ok(c.setup_code) => Some(c.setup_code),
            M::ServiceSelection => Some(c.selection_code),
            M::DcChargeParameterDiscovery | M::AcChargeParameterDiscovery => Some(c.cpd_code),
            M::ScheduleExchange => Some(c.schedule_code),
            M::DcWeldingDetection => Some(c.welding_code),
            _ => None,
        };
        match knob {
            Some(k) if is_ok(code) => k,
            _ => code,
        }
    }

    fn note(&mut self, text: core::fmt::Arguments<'_>) {
        self.log.push(text.to_string());
    }
}
