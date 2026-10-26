//! The simulated charger behind the ISO 15118-2 machine of `evse-core`: this charger's settings, faults, live
//! measurements, Plug & Charge verification and log.

use std::sync::Arc;

use evse_core::iso2::{AcLoopAnswer, DcLoopAnswer, Iso2Secc, Iso2DcSecc, Iso2AcSecc, Offer, ResponseCode, SessionSetup};
use evse_core::secc::{Action, Progress};
use iso15118_2::body::*;
use iso15118_2::header::MessageHeader;
use iso15118_2::types::{self as t, PhysicalValue, UnitSymbol};
use iso15118_20::common_types::ResponseCode as Code20;
use openssl::x509::X509;

use crate::config::{Config, Iso2Mode, Limits};
use crate::live::EvseLive;

fn code(c: Code20) -> ResponseCode {
    ResponseCode::ALL.into_iter().find(|d| d.xml_name() == c.xml_name()).unwrap_or(ResponseCode::Failed)
}

fn pv(x: f64, unit: UnitSymbol) -> PhysicalValue {
    PhysicalValue::from_f64(x, unit)
}

fn mode(m: Iso2Mode) -> t::EnergyTransferMode {
    match m {
        Iso2Mode::AcSinglePhaseCore => t::EnergyTransferMode::AcSinglePhaseCore,
        Iso2Mode::AcThreePhaseCore => t::EnergyTransferMode::AcThreePhaseCore,
        Iso2Mode::DcCore => t::EnergyTransferMode::DcCore,
        Iso2Mode::DcExtended => t::EnergyTransferMode::DcExtended,
        Iso2Mode::DcComboCore => t::EnergyTransferMode::DcComboCore,
    }
}

/// Two tuples of 11 kW for a day with sales tariffs (`Iso2::sales_tariffs`): dear first / cheap first.
const DAY: t::TimeIntervalChoice = t::TimeIntervalChoice::Relative(t::RelativeTimeInterval { start: 0, duration: Some(86_400) });
static P_MAX: [t::PMaxScheduleEntry; 1] = [t::PMaxScheduleEntry { time_interval: DAY, p_max: PhysicalValue { multiplier: 3, unit: UnitSymbol::W, value: 11 } }];
const fn level(start: u32, level: u8) -> t::SalesTariffEntry<'static> {
    t::SalesTariffEntry { time_interval: t::TimeIntervalChoice::Relative(t::RelativeTimeInterval { start, duration: None }), e_price_level: Some(level), consumption_cost: &[] }
}
static DEAR_FIRST: [t::SalesTariffEntry<'static>; 2] = [level(0, 3), level(14_400, 0)];
static CHEAP_FIRST: [t::SalesTariffEntry<'static>; 2] = [level(0, 0), level(14_400, 3)];
const fn tuple(id: u8, entries: &'static [t::SalesTariffEntry<'static>]) -> t::SaScheduleTuple<'static> {
    t::SaScheduleTuple {
        sa_schedule_tuple_id: id,
        p_max_schedule: t::PMaxSchedule { p_max_schedule_entry: &P_MAX },
        sales_tariff: Some(t::SalesTariff { id: None, sales_tariff_id: id, sales_tariff_description: None, num_e_price_levels: Some(4), sales_tariff_entry: entries }),
    }
}
static TARIFF_TUPLES: [t::SaScheduleTuple<'static>; 2] = [tuple(1, &DEAR_FIRST), tuple(2, &CHEAP_FIRST)];

/// The two parameter sets of a value-added service: port 80 and port 443.
static HTTP: [t::Parameter<'static>; 1] = [t::Parameter { name: "Port", value: t::ValueChoice::Int(80) }];
static HTTPS: [t::Parameter<'static>; 1] = [t::Parameter { name: "Port", value: t::ValueChoice::Int(443) }];
static SETS: [t::ParameterSet<'static>; 2] = [t::ParameterSet { parameter_set_id: 1, parameter: &HTTP }, t::ParameterSet { parameter_set_id: 2, parameter: &HTTPS }];

fn random_challenge() -> [u8; 16] {
    let mut challenge = [0u8; 16];
    openssl::rand::rand_bytes(&mut challenge).expect("random bytes");
    challenge
}

pub(crate) struct SimIso2 {
    config: Config,
    live: Option<Arc<EvseLive>>,
    pub(crate) log: Vec<String>,
    options: Vec<t::PaymentOption>,
    modes: Vec<t::EnergyTransferMode>,
    services: Vec<t::Service<'static>>,
    session_id: [u8; 8],
    challenge: [u8; 16],
    authorization_reqs_seen: u32,
    schedule_reqs_seen: u32,
    cable_reqs_seen: u32,
    precharge_reqs_seen: u32,
    loops_seen: u32,
    /// Phases of the AC mode of the session (1 or 3).
    phases: f64,
    ac: bool,
    renegotiation_asked: bool,
    /// The last loop answer asked for a receipt (the reading it carried).
    receipt_reading: Option<u64>,
    /// The contract certificate of a Plug & Charge session, and whether it expires soon.
    contract: Option<(X509, bool)>,
}

impl SimIso2 {
    pub(crate) fn loops_seen(&self) -> u32 {
        self.loops_seen
    }

    pub(crate) fn new(config: Config, live: Option<Arc<EvseLive>>) -> Self {
        let mut s = Self {
            session_id: config.session_id,
            config: config.clone(),
            live,
            log: Vec::new(),
            options: Vec::new(),
            modes: Vec::new(),
            services: Vec::new(),
            challenge: random_challenge(),
            authorization_reqs_seen: 0,
            schedule_reqs_seen: 0,
            cable_reqs_seen: 0,
            precharge_reqs_seen: 0,
            loops_seen: 0,
            phases: 1.0,
            ac: false,
            renegotiation_asked: false,
            receipt_reading: None,
            contract: None,
        };
        s.set_config(config);
        s
    }

    /// New settings (a test platform changed them during the session).
    pub(crate) fn set_config(&mut self, config: Config) {
        self.options = if config.iso2.offer_contract_payment { vec![t::PaymentOption::ExternalPayment, t::PaymentOption::Contract] } else { vec![t::PaymentOption::ExternalPayment] };
        self.modes = config.iso2.modes.iter().map(|m| mode(*m)).collect();
        self.services = config
            .iso2
            .value_added_services
            .iter()
            .map(|&service_id| t::Service { service_id, service_name: None, service_category: t::ServiceCategory::Internet, service_scope: None, free_service: true })
            .collect();
        self.config = config;
    }

    fn limits(&self) -> Limits {
        self.live.as_ref().map_or(self.config.limits, |l| l.limits())
    }

    /// Whether the charger asks for a renegotiation with this loop answer (once).
    fn renegotiation_due(&mut self) -> bool {
        let due = !self.renegotiation_asked && self.config.iso2.renegotiate_after_loops.is_some_and(|n| self.loops_seen >= n);
        self.renegotiation_asked |= due;
        due
    }

    /// The meter reading of this loop answer when it asks for a receipt (a contract session).
    fn receipt_due(&mut self) -> Option<t::MeterInfo<'static>> {
        let every = self.config.iso2.receipt_every_loops?;
        if self.contract.is_none() || every == 0 || !self.loops_seen.is_multiple_of(every) {
            return None;
        }
        let reading = u64::from(self.loops_seen) * 100;
        self.receipt_reading = Some(reading);
        Some(t::MeterInfo { meter_id: "EVSE-METER-1", meter_reading: Some(reading), sig_meter_reading: None, meter_status: None, t_meter: None })
    }
}

impl Iso2Secc for SimIso2 {
    fn evse_id(&self) -> &str {
        &self.config.iso2.evse_id
    }
    fn now_secs(&self) -> i64 {
        (crate::evse::now_micros() / 1_000_000) as i64
    }
    fn session_setup(&mut self, asked: &[u8], _evccid: &[u8]) -> SessionSetup {
        let mut c = code(self.config.setup_code);
        if asked.len() == 8 && asked.iter().any(|b| *b != 0) && matches!(c, ResponseCode::Ok | ResponseCode::OkNewSessionEstablished | ResponseCode::OkOldSessionJoined | ResponseCode::OkCertificateExpiresSoon) {
            if asked == self.session_id && !self.config.forget_paused_sessions {
                // A paused session joins again.
                c = ResponseCode::OkOldSessionJoined;
            } else {
                // An id we do not know: a new session, with an id of its own.
                c = ResponseCode::OkNewSessionEstablished;
                self.session_id = self.config.session_id.map(|b| !b);
            }
        }
        SessionSetup { code: c, session_id: self.session_id }
    }
    fn payment_options(&self) -> &[t::PaymentOption] {
        &self.options
    }
    fn energy_modes(&self) -> &[t::EnergyTransferMode] {
        &self.modes
    }
    fn value_added_services(&self) -> &[t::Service<'_>] {
        &self.services
    }
    fn service_parameters(&self, service_id: u16) -> Option<&[t::ParameterSet<'_>]> {
        self.config.iso2.value_added_services.contains(&service_id).then_some(&SETS[..])
    }
    fn payment_selected(&mut self, _request: &PaymentServiceSelectionReq<'_>, _contract: bool) -> ResponseCode {
        self.contract = None;
        code(self.config.selection_code)
    }
    fn payment_details(&mut self, req: &PaymentDetailsReq<'_>) -> Result<(), ResponseCode> {
        // The contract chain, against the trust anchors; what the vehicle then signs with.
        let mut ders: Vec<Vec<u8>> = vec![req.contract_signature_cert_chain.certificate.to_vec()];
        if let Some(subs) = &req.contract_signature_cert_chain.sub_certificates {
            ders.extend(subs.certificate.iter().map(|c| c.to_vec()));
        }
        let verdict = match crate::pnc::check_contract_chain(&self.config.pnc, &ders) {
            Ok(found) => Ok(found),
            Err(crate::pnc::ChainFault::Expired) => Err(ResponseCode::FailedCertificateExpired),
            Err(_) => Err(ResponseCode::FailedCertChainError),
        };
        self.log.push(format!(
            "PaymentDetailsReq: eMAID {}, contract chain of {} certificate(s): {}",
            req.e_maid,
            ders.len(),
            match &verdict {
                Ok(_) => "accepted".to_string(),
                Err(c) => c.xml_name().to_string(),
            }
        ));
        verdict.map(|found| self.contract = Some(found))
    }
    fn gen_challenge(&mut self) -> [u8; 16] {
        self.challenge
    }
    fn authorize(&mut self, header: &MessageHeader<'_>, auth: &AuthorizationReq<'_>) -> Progress<ResponseCode> {
        self.authorization_reqs_seen += 1;
        if self.config.authorization_never_finishes || self.authorization_reqs_seen <= self.config.authorization_ongoing_responses {
            return Progress::Ongoing;
        }
        let mut c = code(self.config.authorization_code);
        // A contract: the challenge and the signature (the first time the answer is final).
        if let Some((leaf, expires_soon)) = &self.contract {
            let verdict = crate::pnc::verify_iso2(self.challenge, leaf, header.signature.as_ref(), auth);
            self.log.push(format!("contract authorization: {}", verdict.xml_name()));
            if evse_core::iso2::is_ok(c) {
                c = if verdict == ResponseCode::Ok && *expires_soon { ResponseCode::OkCertificateExpiresSoon } else { verdict };
            }
        }
        Progress::Done(c)
    }
    fn charge_parameters(&mut self, req: &ChargeParameterDiscoveryReq, renegotiating: bool) -> Progress<(ResponseCode, Offer)> {
        self.schedule_reqs_seen += 1;
        let _ = renegotiating;
        let ac = matches!(req.requested_energy_transfer_mode, t::EnergyTransferMode::AcSinglePhaseCore | t::EnergyTransferMode::AcThreePhaseCore);
        self.ac = ac;
        self.phases = if req.requested_energy_transfer_mode == t::EnergyTransferMode::AcThreePhaseCore { 3.0 } else { 1.0 };
        if self.schedule_reqs_seen <= self.config.iso2.cpd_ongoing_responses {
            return Progress::Ongoing;
        }
        let l = self.limits();
        let iso2 = &self.config.iso2;
        let ac_status = t::AcEvseStatus { notification_max_delay: 0, evse_notification: t::EvseNotification::None, rcd: false };
        // The most power a tuple allows is what the profile of the vehicle is held to.
        let (parameter, max_w) = if ac {
            (
                EvseChargeParameterChoice::Ac(t::AcEvseChargeParameter {
                    ac_evse_status: ac_status,
                    evse_nominal_voltage: pv(f64::from(iso2.ac_nominal_voltage_v), UnitSymbol::V),
                    evse_max_current: pv(f64::from(iso2.ac_max_current_a), UnitSymbol::A),
                }),
                f64::from(iso2.ac_nominal_voltage_v) * f64::from(iso2.ac_max_current_a) * self.phases,
            )
        } else {
            (
                EvseChargeParameterChoice::Dc(t::DcEvseChargeParameter {
                    dc_evse_status: t::DcEvseStatus { notification_max_delay: 0, evse_notification: t::EvseNotification::None, evse_isolation_status: None, evse_status_code: t::DcEvseStatusCode::EvseReady },
                    evse_maximum_current_limit: pv(f64::from(l.max_charge_current_a), UnitSymbol::A),
                    evse_maximum_power_limit: pv(f64::from(l.max_charge_power_w), UnitSymbol::W),
                    evse_maximum_voltage_limit: pv(f64::from(l.max_voltage_v), UnitSymbol::V),
                    evse_minimum_current_limit: pv(f64::from(l.min_charge_current_a), UnitSymbol::A),
                    evse_minimum_voltage_limit: pv(f64::from(l.min_voltage_v), UnitSymbol::V),
                    evse_current_regulation_tolerance: None,
                    evse_peak_current_ripple: pv(0.0, UnitSymbol::A),
                    evse_energy_to_be_delivered: None,
                }),
                f64::from(l.max_charge_power_w),
            )
        };
        Progress::Done((code(self.config.cpd_code), Offer { parameter, p_max: pv(max_w, UnitSymbol::W) }))
    }
    fn schedule_tuples(&self) -> &[t::SaScheduleTuple<'_>] {
        if self.config.iso2.sales_tariffs { &TARIFF_TUPLES } else { &[] }
    }
    fn power_delivery(&mut self, request: &PowerDeliveryReq<'_>) -> ResponseCode {
        if request.charge_progress == t::ChargeProgress::Start {
            self.log.push(format!("schedule tuple {} selected", request.sa_schedule_tuple_id));
            code(self.config.power_delivery_code)
        } else {
            ResponseCode::Ok
        }
    }
    fn renegotiation(&mut self) {
        self.schedule_reqs_seen = 0;
    }
    fn metering_receipt(&mut self, header: &MessageHeader<'_>, req: &MeteringReceiptReq<'_>) -> ResponseCode {
        let verdict = match &self.contract {
            Some((leaf, _)) => crate::pnc::verify_iso2_receipt(leaf, header.signature.as_ref(), req, self.receipt_reading),
            None => ResponseCode::FailedMeteringSignatureNotValid,
        };
        self.log.push(format!("metering receipt: {}", verdict.xml_name()));
        verdict
    }
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

impl Iso2DcSecc for SimIso2 {
    fn cable_check(&mut self) -> Progress<ResponseCode> {
        self.cable_reqs_seen += 1;
        if self.cable_reqs_seen <= self.config.cable_check_ongoing_responses { Progress::Ongoing } else { Progress::Done(code(self.config.cable_check_code)) }
    }
    fn pre_charge(&mut self, target: PhysicalValue) -> PhysicalValue {
        self.precharge_reqs_seen += 1;
        let settled = self.precharge_reqs_seen >= self.config.precharge_requests_to_settle;
        let target = target.to_f64();
        pv(if settled { target } else { (target - 100.0).max(0.0) }, UnitSymbol::V)
    }
    fn dc_loop(&mut self, req: &CurrentDemandReq, announced_power: Option<PhysicalValue>) -> DcLoopAnswer {
        self.loops_seen += 1;
        let l = self.limits();
        let mut c = ResponseCode::Ok;
        if let (Some(sent), Some(announced)) = (req.ev_maximum_power_limit, announced_power)
            && sent.to_f64() > announced.to_f64() * 1.000_001
        {
            self.log.push(format!("violation: the CurrentDemandReq power limit is {}, the discovery announced {}", sent.to_f64(), announced.to_f64()));
            if self.config.fail_on_limit_violation {
                c = ResponseCode::Failed;
            }
        }
        let ev_voltage = req.ev_target_voltage.to_f64();
        let ev_current = req.ev_target_current.to_f64();
        let ev_power = req.ev_maximum_power_limit.map_or(f64::from(l.max_charge_power_w), |p| p.to_f64());
        let (voltage, current, max_power) = match &self.live {
            Some(live) => live.charge_loop(ev_voltage, ev_current, ev_power, None),
            None => (ev_voltage, crate::live::modelled_current(ev_voltage, ev_current, ev_power, None, &l), f64::from(l.max_charge_power_w)),
        };
        let limit = |achieved: f64, limit: f64| current > 0.0 && achieved >= limit * 0.999;
        let notification = if self.renegotiation_due() { t::EvseNotification::ReNegotiation } else { t::EvseNotification::None };
        let meter_info = self.receipt_due();
        DcLoopAnswer {
            response_code: c,
            shutdown: self.config.iso2.shutdown_after_loops.is_some_and(|n| self.loops_seen >= n),
            notification,
            present_voltage: pv(voltage, UnitSymbol::V),
            present_current: pv(current, UnitSymbol::A),
            current_limit_achieved: limit(current, ev_current.min(f64::from(l.max_charge_current_a))),
            voltage_limit_achieved: limit(voltage, f64::from(l.max_voltage_v)),
            power_limit_achieved: limit(voltage * current, ev_power.min(f64::from(l.max_charge_power_w))),
            maximum_voltage: Some(pv(f64::from(l.max_voltage_v), UnitSymbol::V)),
            maximum_current: Some(pv(f64::from(l.max_charge_current_a), UnitSymbol::A)),
            maximum_power: Some(pv(max_power, UnitSymbol::W)),
            receipt_required: meter_info.map(|_| true),
            meter_info,
        }
    }
    fn welding_detection(&mut self) -> (ResponseCode, PhysicalValue) {
        (code(self.config.welding_code), pv(f64::from(self.config.welding_voltage_v), UnitSymbol::V))
    }
}

impl Iso2AcSecc for SimIso2 {
    fn ac_loop(&mut self, _req: &ChargingStatusReq) -> AcLoopAnswer {
        self.loops_seen += 1;
        let shutdown = self.config.iso2.shutdown_after_loops.is_some_and(|n| self.loops_seen >= n);
        // For a test platform the AC power is shown like a DC one: the nominal voltage and the current summed over the phases.
        if let Some(live) = &self.live {
            let current = f64::from(self.config.iso2.ac_max_current_a) * self.phases;
            let voltage = f64::from(self.config.iso2.ac_nominal_voltage_v);
            live.charge_loop(voltage, current, voltage * current, None);
        }
        let notification = if shutdown {
            t::EvseNotification::StopCharging
        } else if self.renegotiation_due() {
            t::EvseNotification::ReNegotiation
        } else {
            t::EvseNotification::None
        };
        let meter_info = self.receipt_due();
        AcLoopAnswer {
            response_code: ResponseCode::Ok,
            notification,
            max_current: Some(pv(f64::from(self.config.iso2.ac_max_current_a), UnitSymbol::A)),
            receipt_required: Some(meter_info.is_some()),
            meter_info,
        }
    }
}
