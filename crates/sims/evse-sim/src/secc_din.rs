//! The simulated charger behind the DIN SPEC 70121 machine of `evse-core`: this charger's settings, faults, live
//! measurements and log.

use std::sync::Arc;

use din70121::body::{CurrentDemandReq, PowerDeliveryReq, ServicePaymentSelectionReq};
use din70121::def::BodyElementChoice;
use din70121::types::{self as t, PhysicalValue, UnitSymbol};
use evse_core::din::{DinSecc, LoopAnswer, Offer, ResponseCode, SessionSetup};
use evse_core::secc::{Action, Ended, Progress};
use iso15118_20::common_types::ResponseCode as Code20;

use crate::config::{Config, DinEnergyTransfer, Limits};
use crate::live::EvseLive;

/// The response code of the ISO 15118-20 knobs in DIN SPEC 70121's code set, by name (the sets share their names; one the
/// older standard does not have is `FAILED`).
pub(crate) fn code(c: Code20) -> ResponseCode {
    ResponseCode::ALL.into_iter().find(|d| d.xml_name() == c.xml_name()).unwrap_or(ResponseCode::Failed)
}

/// `49*89*6360` as hexBinary: a nibble per character, `*` as `A` (V2G-DC-621); an odd count is padded with `F`.
pub(crate) fn evse_id(text: &str) -> Vec<u8> {
    let nibble = |c: char| if c == '*' { 0xA } else { c.to_digit(10).map_or(0xF, |d| d as u8) };
    let nibbles: Vec<u8> = text.chars().take(64).map(nibble).collect();
    nibbles.chunks(2).map(|p| (p[0] << 4) | p.get(1).copied().unwrap_or(0xF)).collect()
}

fn pv(x: f64, unit: UnitSymbol) -> PhysicalValue {
    PhysicalValue::from_f64(x, unit)
}

pub(crate) struct SimDin {
    config: Config,
    live: Option<Arc<EvseLive>>,
    id: Vec<u8>,
    pub(crate) log: Vec<String>,
    authorization_reqs_seen: u32,
    schedule_reqs_seen: u32,
    cable_reqs_seen: u32,
    precharge_reqs_seen: u32,
    loops_seen: u32,
}

impl SimDin {
    pub(crate) fn new(config: Config, live: Option<Arc<EvseLive>>) -> Self {
        let mut s = Self { id: Vec::new(), config: config.clone(), live, log: Vec::new(), authorization_reqs_seen: 0, schedule_reqs_seen: 0, cable_reqs_seen: 0, precharge_reqs_seen: 0, loops_seen: 0 };
        s.set_config(config);
        s
    }

    /// New settings (a test platform changed them during the session).
    pub(crate) fn set_config(&mut self, config: Config) {
        self.id = evse_id(&config.din.evse_id);
        self.config = config;
    }

    fn limits(&self) -> Limits {
        self.live.as_ref().map_or(self.config.limits, |l| l.limits())
    }
}

impl DinSecc for SimDin {
    fn evse_id(&self) -> &[u8] {
        &self.id
    }
    fn now_secs(&self) -> i64 {
        (crate::evse::now_micros() / 1_000_000) as i64
    }
    fn session_setup(&mut self, _evccid: &[u8]) -> SessionSetup {
        SessionSetup { code: code(self.config.setup_code), session_id: self.config.session_id }
    }
    fn contract_payment(&self) -> bool {
        self.config.din.offer_contract_payment
    }
    fn energy_transfer(&self) -> t::EvseSupportedEnergyTransfer {
        match self.config.din.energy_transfer {
            DinEnergyTransfer::DcCore => t::EvseSupportedEnergyTransfer::DcCore,
            DinEnergyTransfer::DcExtended => t::EvseSupportedEnergyTransfer::DcExtended,
        }
    }
    fn payment_selected(&mut self, _request: &ServicePaymentSelectionReq<'_>) -> ResponseCode {
        code(self.config.selection_code)
    }
    fn authorize(&mut self) -> Progress<ResponseCode> {
        self.authorization_reqs_seen += 1;
        if self.config.authorization_never_finishes || self.authorization_reqs_seen <= self.config.authorization_ongoing_responses {
            Progress::Ongoing
        } else {
            Progress::Done(code(self.config.authorization_code))
        }
    }
    fn charge_parameters(&mut self, ev: &t::DcEvChargeParameter) -> Progress<(ResponseCode, Offer)> {
        self.schedule_reqs_seen += 1;
        self.log.push(format!("the vehicle announces at most {:?} W", ev.ev_maximum_power_limit.map(|p| p.to_f64())));
        if self.schedule_reqs_seen <= self.config.din.cpd_ongoing_responses {
            return Progress::Ongoing;
        }
        let l = self.limits();
        let parameter = t::DcEvseChargeParameter {
            dc_evse_status: t::DcEvseStatus { evse_isolation_status: None, evse_status_code: t::DcEvseStatusCode::EvseReady, notification_max_delay: 0, evse_notification: t::EvseNotification::None },
            evse_maximum_current_limit: pv(f64::from(l.max_charge_current_a), UnitSymbol::A),
            // V2G-DC-626: the maximum power limit is provided.
            evse_maximum_power_limit: Some(pv(f64::from(l.max_charge_power_w), UnitSymbol::W)),
            evse_maximum_voltage_limit: pv(f64::from(l.max_voltage_v), UnitSymbol::V),
            evse_minimum_current_limit: pv(f64::from(l.min_charge_current_a), UnitSymbol::A),
            evse_minimum_voltage_limit: pv(f64::from(l.min_voltage_v), UnitSymbol::V),
            evse_current_regulation_tolerance: None,
            evse_peak_current_ripple: pv(0.0, UnitSymbol::A),
            evse_energy_to_be_delivered: None,
        };
        Progress::Done((code(self.config.cpd_code), Offer { parameter, p_max_w: f64::from(l.max_charge_power_w).min(f64::from(i16::MAX)) as i16 }))
    }
    fn cable_check(&mut self) -> Progress<ResponseCode> {
        self.cable_reqs_seen += 1;
        if self.cable_reqs_seen <= self.config.cable_check_ongoing_responses { Progress::Ongoing } else { Progress::Done(code(self.config.cable_check_code)) }
    }
    fn pre_charge(&mut self, target: PhysicalValue) -> PhysicalValue {
        self.precharge_reqs_seen += 1;
        // Until "settled" the output is 100 V away from the EV's target.
        let settled = self.precharge_reqs_seen >= self.config.precharge_requests_to_settle;
        let target = target.to_f64();
        pv(if settled { target } else { (target - 100.0).max(0.0) }, UnitSymbol::V)
    }
    fn power_delivery(&mut self, request: &PowerDeliveryReq<'_>) -> ResponseCode {
        if request.ready_to_charge_state { code(self.config.power_delivery_code) } else { ResponseCode::Ok }
    }
    fn current_demand(&mut self, req: &CurrentDemandReq, announced: &t::DcEvChargeParameter) -> LoopAnswer {
        self.loops_seen += 1;
        let l = self.limits();
        let mut c = ResponseCode::Ok;
        // V2G-DC-641: the power the vehicle announced is the most it asks for.
        if let (Some(sent), Some(announced)) = (req.ev_maximum_power_limit, announced.ev_maximum_power_limit)
            && sent.to_f64() > announced.to_f64() * 1.000_001
        {
            self.log.push(format!("violation: V2G-DC-641: EVMaximumPowerLimit is {}, the discovery announced {}", sent.to_f64(), announced.to_f64()));
            if self.config.fail_on_limit_violation {
                c = ResponseCode::Failed;
            }
        }
        let ev_voltage = req.ev_target_voltage.to_f64();
        let ev_max_current = req.ev_target_current.to_f64();
        let ev_max_power = req.ev_maximum_power_limit.map_or(f64::from(l.max_charge_power_w), |p| p.to_f64());
        let (voltage, current, max_power) = match &self.live {
            Some(live) => live.charge_loop(ev_voltage, ev_max_current, ev_max_power, None),
            None => (ev_voltage, crate::live::modelled_current(ev_voltage, ev_max_current, ev_max_power, None, &l), f64::from(l.max_charge_power_w)),
        };
        let limit = |achieved: f64, limit: f64| current > 0.0 && achieved >= limit * 0.999;
        LoopAnswer {
            response_code: c,
            shutdown: self.config.din.shutdown_after_loops.is_some_and(|n| self.loops_seen >= n),
            present_voltage: pv(voltage, UnitSymbol::V),
            present_current: pv(current, UnitSymbol::A),
            current_limit_achieved: limit(current, ev_max_current.min(f64::from(l.max_charge_current_a))),
            voltage_limit_achieved: limit(voltage, f64::from(l.max_voltage_v)),
            power_limit_achieved: limit(voltage * current, ev_max_power.min(f64::from(l.max_charge_power_w))),
            maximum_voltage: Some(pv(f64::from(l.max_voltage_v), UnitSymbol::V)),
            maximum_current: Some(pv(f64::from(l.max_charge_current_a), UnitSymbol::A)),
            maximum_power: Some(pv(max_power, UnitSymbol::W)),
        }
    }
    fn welding_detection(&mut self) -> (ResponseCode, PhysicalValue) {
        (code(self.config.welding_code), pv(f64::from(self.config.welding_voltage_v), UnitSymbol::V))
    }
    fn session_ended(&mut self, _how: Ended) {}
    fn intercept(&mut self, request: &BodyElementChoice<'_>) -> Action {
        if self.config.silent || (self.config.silent_on_setup && matches!(request, BodyElementChoice::SessionSetupReq(_))) {
            Action::Silent
        } else {
            Action::Pass
        }
    }
    fn note(&mut self, text: core::fmt::Arguments<'_>) {
        self.log.push(text.to_string());
    }
}
