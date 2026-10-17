//! The ISO 15118-2 side of the simulated charger (DC and AC, external payment).

use exi::Arena;
use iso15118_2::body::*;
use iso15118_2::def::V2gMessage;
use iso15118_2::header::MessageHeader;
use iso15118_2::types::{self as t, PhysicalValue, UnitSymbol};
use iso15118_20::common_types::ResponseCode as Code20;
use v2gtp::PayloadType;

use super::{Evse, Output, State, frame};
use crate::config::Iso2Mode;

pub(super) fn expects(state: State) -> &'static str {
    match state {
        State::AwaitSupportedAppProtocolReq => "SupportedAppProtocolReq",
        State::AwaitSessionSetupReq => "SessionSetupReq",
        State::AwaitServiceDiscoveryReq => "ServiceDiscoveryReq",
        State::AwaitServicePaymentSelectionReq => "PaymentServiceSelectionReq",
        State::AwaitPaymentDetailsReq => "PaymentDetailsReq",
        State::AwaitContractAuthenticationReq => "AuthorizationReq",
        State::AwaitChargeParameterDiscoveryReq => "ChargeParameterDiscoveryReq",
        State::AwaitCableCheckReq => "CableCheckReq",
        State::AwaitPreChargeReq => "PreChargeReq or PowerDeliveryReq",
        State::AwaitPowerDeliveryReq => "PowerDeliveryReq",
        State::ChargeLoop => "CurrentDemandReq, ChargingStatusReq, MeteringReceiptReq or PowerDeliveryReq",
        State::AwaitWeldingReq => "WeldingDetectionReq or SessionStopReq",
        State::AwaitSessionStopReq => "SessionStopReq",
        _ => "nothing",
    }
}

impl Evse {
    /// Whether the charger asks for a renegotiation with this loop answer (once).
    fn iso2_renegotiation_due(&mut self) -> bool {
        let due = !self.iso2_renegotiation_asked && self.config.iso2.renegotiate_after_loops.is_some_and(|n| self.loops_seen >= n);
        self.iso2_renegotiation_asked |= due;
        due
    }

    /// The meter reading of this loop answer when it asks for a receipt (a contract session).
    fn iso2_receipt_due(&mut self) -> Option<u64> {
        let every = self.config.iso2.receipt_every_loops?;
        if self.iso2_contract.is_none() || every == 0 || !self.loops_seen.is_multiple_of(every) {
            return None;
        }
        let reading = u64::from(self.loops_seen) * 100;
        self.iso2_receipt_reading = Some(reading);
        Some(reading)
    }
}

fn code(c: Code20) -> t::ResponseCode {
    t::ResponseCode::ALL.into_iter().find(|d| d.xml_name() == c.xml_name()).unwrap_or(t::ResponseCode::Failed)
}

fn is_ok(c: t::ResponseCode) -> bool {
    matches!(c, t::ResponseCode::Ok | t::ResponseCode::OkNewSessionEstablished | t::ResponseCode::OkOldSessionJoined | t::ResponseCode::OkCertificateExpiresSoon)
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

fn is_ac(m: t::EnergyTransferMode) -> bool {
    matches!(m, t::EnergyTransferMode::AcSinglePhaseCore | t::EnergyTransferMode::AcThreePhaseCore)
}

impl Evse {
    fn iso2_dc_status(&self, c: t::DcEvseStatusCode, iso: Option<t::IsolationLevel>) -> t::DcEvseStatus {
        t::DcEvseStatus { notification_max_delay: 0, evse_notification: t::EvseNotification::None, evse_isolation_status: iso, evse_status_code: c }
    }

    fn iso2_ac_status(&self, notification: t::EvseNotification) -> t::AcEvseStatus {
        t::AcEvseStatus { notification_max_delay: if notification == t::EvseNotification::None { 0 } else { 5 }, evse_notification: notification, rcd: false }
    }

    fn iso2_status(&self) -> EvseStatusChoice {
        if self.iso2_ac {
            EvseStatusChoice::Ac(self.iso2_ac_status(t::EvseNotification::None))
        } else {
            EvseStatusChoice::Dc(self.iso2_dc_status(t::DcEvseStatusCode::EvseReady, Some(t::IsolationLevel::Valid)))
        }
    }

    fn iso2_reply(&self, body: BodyElementChoice<'_>) -> Option<Vec<u8>> {
        let message = V2gMessage {
            header: MessageHeader { session_id: &self.session_id, notification: None, signature: None },
            body: Body { body_element: Some(body) },
        };
        frame(PayloadType::SAP, |out| message.encode(out))
    }

    fn iso2_output(&self, body: BodyElementChoice<'_>, close: bool) -> Output {
        Output { send: self.iso2_reply(body), close }
    }

    fn iso2_note_reply(&mut self, name: &str, c: t::ResponseCode, close: bool) {
        self.note(format!("tx {name} {}{}", c.xml_name(), if close { ", then closing" } else { "" }));
    }

    /// A failing response of the type the request asks for (wrong session or out of sequence).
    fn iso2_failing(&self, request: &BodyElementChoice<'_>, c: t::ResponseCode) -> Option<Vec<u8>> {
        use BodyElementChoice as B;
        let dc = self.iso2_dc_status(t::DcEvseStatusCode::EvseReady, None);
        let ac = self.iso2_ac_status(t::EvseNotification::None);
        let zero = pv(0.0, UnitSymbol::V);
        let tuple = [t::SaScheduleTuple {
            sa_schedule_tuple_id: 1,
            p_max_schedule: t::PMaxSchedule { p_max_schedule_entry: &[t::PMaxScheduleEntry { time_interval: t::TimeIntervalChoice::Relative(t::RelativeTimeInterval { start: 0, duration: None }), p_max: pv(0.0, UnitSymbol::W) }] },
            sales_tariff: None,
        }];
        let modes = [t::EnergyTransferMode::DcExtended];
        let options = [t::PaymentOption::ExternalPayment];
        let body = match request {
            B::SessionSetupReq(_) => B::SessionSetupRes(SessionSetupRes { response_code: c, evseid: "ZZ00000", evse_time_stamp: None }),
            B::ServiceDiscoveryReq(_) => B::ServiceDiscoveryRes(ServiceDiscoveryRes {
                response_code: c,
                payment_option_list: t::PaymentOptionList { payment_option: &options },
                charge_service: t::ChargeService { service_id: 1, service_name: None, service_category: t::ServiceCategory::EvCharging, service_scope: None, free_service: false, supported_energy_transfer_mode: t::SupportedEnergyTransferMode { energy_transfer_mode: &modes } },
                service_list: None,
            }),
            B::PaymentServiceSelectionReq(_) => B::PaymentServiceSelectionRes(PaymentServiceSelectionRes { response_code: c }),
            B::AuthorizationReq(_) => B::AuthorizationRes(AuthorizationRes { response_code: c, evse_processing: t::EvseProcessing::Finished }),
            B::ChargeParameterDiscoveryReq(_) => B::ChargeParameterDiscoveryRes(ChargeParameterDiscoveryRes {
                response_code: c,
                evse_processing: t::EvseProcessing::Finished,
                sa_schedules: Some(SaSchedulesChoice::SaScheduleList(t::SaScheduleList { sa_schedule_tuple: &tuple })),
                evse_charge_parameter: EvseChargeParameterChoice::Ac(t::AcEvseChargeParameter { ac_evse_status: ac, evse_nominal_voltage: zero, evse_max_current: pv(0.0, UnitSymbol::A) }),
            }),
            B::CableCheckReq(_) => B::CableCheckRes(CableCheckRes { response_code: c, dc_evse_status: dc, evse_processing: t::EvseProcessing::Finished }),
            B::PreChargeReq(_) => B::PreChargeRes(PreChargeRes { response_code: c, dc_evse_status: dc, evse_present_voltage: zero }),
            B::PowerDeliveryReq(_) => B::PowerDeliveryRes(PowerDeliveryRes { response_code: c, evse_status: EvseStatusChoice::Dc(dc) }),
            B::CurrentDemandReq(_) => B::CurrentDemandRes(CurrentDemandRes {
                response_code: c,
                dc_evse_status: dc,
                evse_present_voltage: zero,
                evse_present_current: pv(0.0, UnitSymbol::A),
                evse_current_limit_achieved: false,
                evse_voltage_limit_achieved: false,
                evse_power_limit_achieved: false,
                evse_maximum_voltage_limit: None,
                evse_maximum_current_limit: None,
                evse_maximum_power_limit: None,
                evseid: "ZZ00000",
                sa_schedule_tuple_id: 1,
                meter_info: None,
                receipt_required: None,
            }),
            B::ChargingStatusReq(_) => B::ChargingStatusRes(ChargingStatusRes { response_code: c, evseid: "ZZ00000", sa_schedule_tuple_id: 1, evse_max_current: None, meter_info: None, receipt_required: None, ac_evse_status: ac }),
            B::WeldingDetectionReq(_) => B::WeldingDetectionRes(WeldingDetectionRes { response_code: c, dc_evse_status: dc, evse_present_voltage: zero }),
            B::SessionStopReq(_) => B::SessionStopRes(SessionStopRes { response_code: c }),
            _ => return None,
        };
        self.iso2_reply(body)
    }

    pub(super) fn on_iso2_frame(&mut self, payload_type: PayloadType, payload: &[u8]) -> Output {
        use BodyElementChoice as B;
        let closed = Output { send: None, close: true };
        if payload_type != PayloadType::SAP {
            return closed;
        }
        let mut mem = vec![0u8; 32768];
        let arena = Arena::new(&mut mem);
        let Ok(message) = V2gMessage::decode(payload, &arena) else { return closed };
        let Some(request) = message.body.body_element else { return closed };
        let session_ok = message.header.session_id == self.session_id;
        let state = self.state;
        let limits = self.limits();
        let l = &limits;
        // A SessionStopReq ends the session wherever the power is not on (and after the stop).
        if let (B::SessionStopReq(req), true) = (&request, state != State::AwaitSessionSetupReq) {
            if !session_ok {
                return Output { send: self.iso2_failing(&request, t::ResponseCode::FailedUnknownSession), close: true };
            }
            if !matches!(state, State::ChargeLoop) {
                self.state = if req.charging_session == t::ChargingSession::Pause { State::Paused } else { State::Stopped };
                self.iso2_note_reply("SessionStopRes", t::ResponseCode::Ok, true);
                return self.iso2_output(B::SessionStopRes(SessionStopRes { response_code: t::ResponseCode::Ok }), true);
            }
        }
        match (state, &request) {
            (State::AwaitSessionSetupReq, B::SessionSetupReq(req)) => {
                if self.config.silent_on_setup {
                    return Output::default();
                }
                let mut c = code(self.config.setup_code);
                let asked = message.header.session_id;
                if asked.len() == 8 && asked.iter().any(|b| *b != 0) && is_ok(c) {
                    if asked == self.session_id && !self.config.forget_paused_sessions {
                        // A paused session joins again.
                        c = t::ResponseCode::OkOldSessionJoined;
                    } else {
                        // An id we do not know: a new session, with an id of its own.
                        c = t::ResponseCode::OkNewSessionEstablished;
                        self.session_id = self.config.session_id.map(|b| !b);
                    }
                }
                self.note(format!("SessionSetupReq from EVCC {:02x?}, session {:02x?}", req.evccid, asked));
                if is_ok(c) {
                    self.state = State::AwaitServiceDiscoveryReq;
                }
                self.iso2_note_reply("SessionSetupRes", c, !is_ok(c));
                let id = self.config.iso2.evse_id.clone();
                let stamp = (super::now_micros() / 1_000_000) as i64;
                self.iso2_output(B::SessionSetupRes(SessionSetupRes { response_code: c, evseid: &id, evse_time_stamp: Some(stamp) }), !is_ok(c))
            }
            (State::AwaitServiceDiscoveryReq, B::ServiceDiscoveryReq(_)) if session_ok => {
                let options = [t::PaymentOption::ExternalPayment, t::PaymentOption::Contract];
                let options = &options[..if self.config.iso2.offer_contract_payment { 2 } else { 1 }];
                let modes: Vec<t::EnergyTransferMode> = self.config.iso2.modes.iter().map(|m| mode(*m)).collect();
                let services: Vec<t::Service<'_>> = self.config.iso2.value_added_services.iter().map(|&service_id| t::Service { service_id, service_name: None, service_category: t::ServiceCategory::Internet, service_scope: None, free_service: true }).collect();
                let body = B::ServiceDiscoveryRes(ServiceDiscoveryRes {
                    response_code: t::ResponseCode::Ok,
                    payment_option_list: t::PaymentOptionList { payment_option: options },
                    charge_service: t::ChargeService {
                        service_id: 1,
                        service_name: None,
                        service_category: t::ServiceCategory::EvCharging,
                        service_scope: None,
                        free_service: false,
                        supported_energy_transfer_mode: t::SupportedEnergyTransferMode { energy_transfer_mode: &modes },
                    },
                    service_list: (!services.is_empty()).then_some(t::ServiceList { service: &services }),
                });
                self.state = State::AwaitServicePaymentSelectionReq;
                self.iso2_note_reply("ServiceDiscoveryRes", t::ResponseCode::Ok, false);
                self.iso2_output(body, false)
            }
            (State::AwaitServicePaymentSelectionReq, B::ServiceDetailReq(req)) if session_ok => {
                let known = self.config.iso2.value_added_services.contains(&req.service_id);
                let c = if known { t::ResponseCode::Ok } else { t::ResponseCode::FailedServiceIdInvalid };
                let http = [t::Parameter { name: "Port", value: t::ValueChoice::Int(80) }];
                let https = [t::Parameter { name: "Port", value: t::ValueChoice::Int(443) }];
                let sets = [t::ParameterSet { parameter_set_id: 1, parameter: &http }, t::ParameterSet { parameter_set_id: 2, parameter: &https }];
                self.iso2_note_reply("ServiceDetailRes", c, false);
                let body = B::ServiceDetailRes(ServiceDetailRes { response_code: c, service_id: req.service_id, service_parameter_list: known.then_some(t::ServiceParameterList { parameter_set: &sets }) });
                self.iso2_output(body, false)
            }
            (State::AwaitServicePaymentSelectionReq, B::PaymentServiceSelectionReq(req)) if session_ok => {
                let contract = req.selected_payment_option == t::PaymentOption::Contract && self.config.iso2.offer_contract_payment;
                let payment_ok = req.selected_payment_option == t::PaymentOption::ExternalPayment || contract;
                let service_ok = !req.selected_service_list.selected_service.is_empty() && req.selected_service_list.selected_service.iter().all(|s| s.service_id == 1 || self.config.iso2.value_added_services.contains(&s.service_id));
                let c = if !payment_ok {
                    t::ResponseCode::FailedPaymentSelectionInvalid
                } else if !service_ok {
                    t::ResponseCode::FailedServiceSelectionInvalid
                } else {
                    code(self.config.selection_code)
                };
                if is_ok(c) {
                    self.state = if contract { State::AwaitPaymentDetailsReq } else { State::AwaitContractAuthenticationReq };
                    self.iso2_contract = None;
                }
                self.iso2_note_reply("PaymentServiceSelectionRes", c, !is_ok(c));
                self.iso2_output(B::PaymentServiceSelectionRes(PaymentServiceSelectionRes { response_code: c }), !is_ok(c))
            }
            (State::AwaitPaymentDetailsReq, B::PaymentDetailsReq(req)) if session_ok => {
                // The contract chain, against the trust anchors; what the vehicle then signs with.
                let mut ders: Vec<Vec<u8>> = vec![req.contract_signature_cert_chain.certificate.to_vec()];
                if let Some(subs) = &req.contract_signature_cert_chain.sub_certificates {
                    ders.extend(subs.certificate.iter().map(|c| c.to_vec()));
                }
                let verdict = match crate::pnc::check_contract_chain(&self.config.pnc, &ders) {
                    Ok(found) => Ok(found),
                    Err(crate::pnc::ChainFault::Expired) => Err(t::ResponseCode::FailedCertificateExpired),
                    Err(_) => Err(t::ResponseCode::FailedCertChainError),
                };
                self.note(format!("PaymentDetailsReq: eMAID {}, contract chain of {} certificate(s): {}", req.e_maid, ders.len(), match &verdict { Ok(_) => "accepted".to_string(), Err(c) => c.xml_name().to_string() }));
                let (c, stamp) = match verdict {
                    Ok(found) => {
                        self.iso2_contract = Some(found);
                        self.state = State::AwaitContractAuthenticationReq;
                        (t::ResponseCode::Ok, (super::now_micros() / 1_000_000) as i64)
                    }
                    Err(c) => (c, 0),
                };
                self.iso2_note_reply("PaymentDetailsRes", c, !is_ok(c));
                self.iso2_output(B::PaymentDetailsRes(PaymentDetailsRes { response_code: c, gen_challenge: self.challenge, evse_time_stamp: stamp }), !is_ok(c))
            }
            (State::AwaitContractAuthenticationReq, B::AuthorizationReq(auth)) if session_ok => {
                self.authorization_reqs_seen += 1;
                let ongoing = self.config.authorization_never_finishes || self.authorization_reqs_seen <= self.config.authorization_ongoing_responses;
                let mut c = if ongoing { t::ResponseCode::Ok } else { code(self.config.authorization_code) };
                // A contract: the challenge and the signature (the first time the answer is final).
                let checked = match (&self.iso2_contract, ongoing) {
                    (Some((leaf, expires_soon)), false) => Some((crate::pnc::verify_iso2(self.challenge, leaf, message.header.signature.as_ref(), auth), *expires_soon)),
                    _ => None,
                };
                if let Some((verdict, expires_soon)) = checked {
                    self.note(format!("contract authorization: {}", verdict.xml_name()));
                    if is_ok(c) {
                        c = if verdict == t::ResponseCode::Ok && expires_soon { t::ResponseCode::OkCertificateExpiresSoon } else { verdict };
                    }
                }
                let processing = if ongoing { t::EvseProcessing::Ongoing } else { t::EvseProcessing::Finished };
                if !ongoing && is_ok(c) {
                    self.state = State::AwaitChargeParameterDiscoveryReq;
                }
                self.iso2_note_reply("AuthorizationRes", c, !ongoing && !is_ok(c));
                self.iso2_output(B::AuthorizationRes(AuthorizationRes { response_code: c, evse_processing: processing }), !ongoing && !is_ok(c))
            }
            (State::AwaitChargeParameterDiscoveryReq, B::ChargeParameterDiscoveryReq(req)) if session_ok => {
                self.schedule_reqs_seen += 1;
                let offered = self.config.iso2.modes.iter().any(|m| mode(*m) == req.requested_energy_transfer_mode);
                let ac = is_ac(req.requested_energy_transfer_mode);
                let matches = matches!((&req.ev_charge_parameter, ac), (EvChargeParameterChoice::Ac(_), true) | (EvChargeParameterChoice::Dc(_), false));
                if !offered || !matches {
                    let c = if !offered { t::ResponseCode::FailedWrongEnergyTransferMode } else { t::ResponseCode::FailedWrongChargeParameter };
                    self.note(format!("the vehicle asks for {:?}, the charger offers {:?}", req.requested_energy_transfer_mode, self.config.iso2.modes));
                    self.iso2_note_reply("ChargeParameterDiscoveryRes", c, true);
                    return Output { send: self.iso2_failing(&request, c), close: true };
                }
                self.iso2_ac = ac;
                self.iso2_phases = if req.requested_energy_transfer_mode == t::EnergyTransferMode::AcThreePhaseCore { 3.0 } else { 1.0 };
                self.iso2_ev_power = match &req.ev_charge_parameter {
                    EvChargeParameterChoice::Dc(p) => p.ev_maximum_power_limit.map(|p| p.to_f64()),
                    _ => None,
                };
                let ongoing = self.schedule_reqs_seen <= self.config.iso2.cpd_ongoing_responses;
                let c = if ongoing { t::ResponseCode::Ok } else { code(self.config.cpd_code) };
                let processing = if ongoing { t::EvseProcessing::Ongoing } else { t::EvseProcessing::Finished };
                let max_w = if ac { f64::from(self.config.iso2.ac_nominal_voltage_v) * f64::from(self.config.iso2.ac_max_current_a) * if req.requested_energy_transfer_mode == t::EnergyTransferMode::AcThreePhaseCore { 3.0 } else { 1.0 } } else { f64::from(l.max_charge_power_w) };
                let entry = [t::PMaxScheduleEntry {
                    time_interval: t::TimeIntervalChoice::Relative(t::RelativeTimeInterval { start: 0, duration: Some(86_400) }),
                    p_max: pv(max_w, UnitSymbol::W),
                }];
                let tuple = [t::SaScheduleTuple { sa_schedule_tuple_id: 1, p_max_schedule: t::PMaxSchedule { p_max_schedule_entry: &entry }, sales_tariff: None }];
                let parameter = if ac {
                    EvseChargeParameterChoice::Ac(t::AcEvseChargeParameter {
                        ac_evse_status: self.iso2_ac_status(t::EvseNotification::None),
                        evse_nominal_voltage: pv(f64::from(self.config.iso2.ac_nominal_voltage_v), UnitSymbol::V),
                        evse_max_current: pv(f64::from(self.config.iso2.ac_max_current_a), UnitSymbol::A),
                    })
                } else {
                    EvseChargeParameterChoice::Dc(t::DcEvseChargeParameter {
                        dc_evse_status: self.iso2_dc_status(t::DcEvseStatusCode::EvseReady, None),
                        evse_maximum_current_limit: pv(f64::from(l.max_charge_current_a), UnitSymbol::A),
                        evse_maximum_power_limit: pv(f64::from(l.max_charge_power_w), UnitSymbol::W),
                        evse_maximum_voltage_limit: pv(f64::from(l.max_voltage_v), UnitSymbol::V),
                        evse_minimum_current_limit: pv(f64::from(l.min_charge_current_a), UnitSymbol::A),
                        evse_minimum_voltage_limit: pv(f64::from(l.min_voltage_v), UnitSymbol::V),
                        evse_current_regulation_tolerance: None,
                        evse_peak_current_ripple: pv(0.0, UnitSymbol::A),
                        evse_energy_to_be_delivered: None,
                    })
                };
                let body = B::ChargeParameterDiscoveryRes(ChargeParameterDiscoveryRes {
                    response_code: c,
                    evse_processing: processing,
                    sa_schedules: (!ongoing).then_some(SaSchedulesChoice::SaScheduleList(t::SaScheduleList { sa_schedule_tuple: &tuple })),
                    evse_charge_parameter: parameter,
                });
                if !ongoing && is_ok(c) {
                    // After a renegotiation the cable is checked already: power is asked for again.
                    self.state = if ac || self.iso2_renegotiating { State::AwaitPowerDeliveryReq } else { State::AwaitCableCheckReq };
                }
                self.iso2_note_reply("ChargeParameterDiscoveryRes", c, !ongoing && !is_ok(c));
                self.iso2_output(body, !ongoing && !is_ok(c))
            }
            (State::AwaitCableCheckReq, B::CableCheckReq(_)) if session_ok => {
                self.cable_reqs_seen += 1;
                let ongoing = self.cable_reqs_seen <= self.config.cable_check_ongoing_responses;
                let c = if ongoing { t::ResponseCode::Ok } else { code(self.config.cable_check_code) };
                let (status, isolation, processing) = if ongoing {
                    (t::DcEvseStatusCode::EvseIsolationMonitoringActive, t::IsolationLevel::Invalid, t::EvseProcessing::Ongoing)
                } else {
                    (t::DcEvseStatusCode::EvseReady, t::IsolationLevel::Valid, t::EvseProcessing::Finished)
                };
                if !ongoing && is_ok(c) {
                    self.state = State::AwaitPreChargeReq;
                }
                self.iso2_note_reply("CableCheckRes", c, !ongoing && !is_ok(c));
                let body = B::CableCheckRes(CableCheckRes { response_code: c, dc_evse_status: self.iso2_dc_status(status, Some(isolation)), evse_processing: processing });
                self.iso2_output(body, !ongoing && !is_ok(c))
            }
            (State::AwaitPreChargeReq, B::PreChargeReq(req)) if session_ok => {
                self.precharge_reqs_seen += 1;
                let settled = self.precharge_reqs_seen >= self.config.precharge_requests_to_settle;
                let target = req.ev_target_voltage.to_f64();
                let voltage = if settled { target } else { (target - 100.0).max(0.0) };
                self.iso2_note_reply("PreChargeRes", t::ResponseCode::Ok, false);
                let body = B::PreChargeRes(PreChargeRes {
                    response_code: t::ResponseCode::Ok,
                    dc_evse_status: self.iso2_dc_status(t::DcEvseStatusCode::EvseReady, Some(t::IsolationLevel::Valid)),
                    evse_present_voltage: pv(voltage, UnitSymbol::V),
                });
                self.iso2_output(body, false)
            }
            (State::AwaitPreChargeReq | State::AwaitPowerDeliveryReq, B::PowerDeliveryReq(req)) if session_ok && req.charge_progress == t::ChargeProgress::Start => {
                // The tuple the vehicle follows must be one of those offered; the profile within its limits.
                let mut c = code(self.config.power_delivery_code);
                if req.sa_schedule_tuple_id != 1 {
                    c = t::ResponseCode::FailedTariffSelectionInvalid;
                } else if let Some(profile) = &req.charging_profile {
                    let max_w = if self.iso2_ac { f64::from(self.config.iso2.ac_nominal_voltage_v) * f64::from(self.config.iso2.ac_max_current_a) * 3.0 } else { f64::from(l.max_charge_power_w) };
                    if profile.profile_entry.iter().any(|e| e.charging_profile_entry_max_power.to_f64() > max_w * 1.0001) {
                        c = t::ResponseCode::FailedChargingProfileInvalid;
                    }
                }
                if is_ok(c) {
                    self.state = State::ChargeLoop;
                    self.iso2_renegotiating = false;
                }
                self.iso2_note_reply("PowerDeliveryRes", c, !is_ok(c));
                self.iso2_output(B::PowerDeliveryRes(PowerDeliveryRes { response_code: c, evse_status: self.iso2_status() }), !is_ok(c))
            }
            (State::ChargeLoop, B::CurrentDemandReq(req)) if session_ok && !self.iso2_ac => {
                self.loops_seen += 1;
                let mut c = t::ResponseCode::Ok;
                if let (Some(sent), Some(announced)) = (req.ev_maximum_power_limit, self.iso2_ev_power)
                    && sent.to_f64() > announced * 1.000_001 {
                        self.note(format!("violation: the CurrentDemandReq power limit is {}, the discovery announced {announced}", sent.to_f64()));
                        if self.config.fail_on_limit_violation {
                            c = t::ResponseCode::Failed;
                        }
                    }
                let ev_voltage = req.ev_target_voltage.to_f64();
                let ev_current = req.ev_target_current.to_f64();
                let ev_power = req.ev_maximum_power_limit.map_or(f64::from(l.max_charge_power_w), |p| p.to_f64());
                let (voltage, current, max_power) = match &self.live {
                    Some(live) => live.charge_loop(ev_voltage, ev_current, ev_power, None),
                    None => (ev_voltage, crate::live::modelled_current(ev_voltage, ev_current, ev_power, None, l), f64::from(l.max_charge_power_w)),
                };
                let limit = |achieved: f64, limit: f64| current > 0.0 && achieved >= limit * 0.999;
                let shutdown = self.config.iso2.shutdown_after_loops.is_some_and(|n| self.loops_seen >= n);
                let mut status = self.iso2_dc_status(if shutdown { t::DcEvseStatusCode::EvseShutdown } else { t::DcEvseStatusCode::EvseReady }, Some(t::IsolationLevel::Valid));
                if self.iso2_renegotiation_due() {
                    status.evse_notification = t::EvseNotification::ReNegotiation;
                    status.notification_max_delay = 5;
                }
                let receipt = self.iso2_receipt_due();
                let meter = receipt.map(|reading| t::MeterInfo { meter_id: "EVSE-METER-1", meter_reading: Some(reading), sig_meter_reading: None, meter_status: None, t_meter: None });
                let id = self.config.iso2.evse_id.clone();
                let body = B::CurrentDemandRes(CurrentDemandRes {
                    response_code: c,
                    dc_evse_status: status,
                    evse_present_voltage: pv(voltage, UnitSymbol::V),
                    evse_present_current: pv(current, UnitSymbol::A),
                    evse_current_limit_achieved: limit(current, ev_current.min(f64::from(l.max_charge_current_a))),
                    evse_voltage_limit_achieved: limit(voltage, f64::from(l.max_voltage_v)),
                    evse_power_limit_achieved: limit(voltage * current, ev_power.min(f64::from(l.max_charge_power_w))),
                    evse_maximum_voltage_limit: Some(pv(f64::from(l.max_voltage_v), UnitSymbol::V)),
                    evse_maximum_current_limit: Some(pv(f64::from(l.max_charge_current_a), UnitSymbol::A)),
                    evse_maximum_power_limit: Some(pv(max_power, UnitSymbol::W)),
                    evseid: &id,
                    sa_schedule_tuple_id: 1,
                    meter_info: meter,
                    receipt_required: receipt.map(|_| true),
                });
                self.iso2_note_reply("CurrentDemandRes", c, !is_ok(c));
                self.iso2_output(body, !is_ok(c))
            }
            (State::ChargeLoop, B::ChargingStatusReq(_)) if session_ok && self.iso2_ac => {
                self.loops_seen += 1;
                let shutdown = self.config.iso2.shutdown_after_loops.is_some_and(|n| self.loops_seen >= n);
                // For a test platform the AC power is shown like a DC one: the nominal voltage and
                // the current summed over the phases.
                if let Some(live) = &self.live {
                    let current = f64::from(self.config.iso2.ac_max_current_a) * self.iso2_phases;
                    let voltage = f64::from(self.config.iso2.ac_nominal_voltage_v);
                    live.charge_loop(voltage, current, voltage * current, None);
                }
                let notification = if shutdown {
                    t::EvseNotification::StopCharging
                } else if self.iso2_renegotiation_due() {
                    t::EvseNotification::ReNegotiation
                } else {
                    t::EvseNotification::None
                };
                let status = self.iso2_ac_status(notification);
                let receipt = self.iso2_receipt_due();
                let meter = receipt.map(|reading| t::MeterInfo { meter_id: "EVSE-METER-1", meter_reading: Some(reading), sig_meter_reading: None, meter_status: None, t_meter: None });
                let id = self.config.iso2.evse_id.clone();
                let body = B::ChargingStatusRes(ChargingStatusRes {
                    response_code: t::ResponseCode::Ok,
                    evseid: &id,
                    sa_schedule_tuple_id: 1,
                    evse_max_current: Some(pv(f64::from(self.config.iso2.ac_max_current_a), UnitSymbol::A)),
                    meter_info: meter,
                    receipt_required: Some(receipt.is_some()),
                    ac_evse_status: status,
                });
                self.iso2_note_reply("ChargingStatusRes", t::ResponseCode::Ok, false);
                self.iso2_output(body, false)
            }
            (State::ChargeLoop, B::PowerDeliveryReq(req)) if session_ok && req.charge_progress == t::ChargeProgress::Renegotiate => {
                // New charge parameters and a new schedule; the contactors stay closed.
                self.state = State::AwaitChargeParameterDiscoveryReq;
                self.iso2_renegotiating = true;
                self.note("renegotiation: new charge parameters, the power stays connected".to_string());
                self.schedule_reqs_seen = 0;
                self.iso2_note_reply("PowerDeliveryRes", t::ResponseCode::Ok, false);
                self.iso2_output(B::PowerDeliveryRes(PowerDeliveryRes { response_code: t::ResponseCode::Ok, evse_status: self.iso2_status() }), false)
            }
            (State::ChargeLoop, B::MeteringReceiptReq(req)) if session_ok => {
                let verdict = match &self.iso2_contract {
                    Some((leaf, _)) => crate::pnc::verify_iso2_receipt(leaf, message.header.signature.as_ref(), req, self.iso2_receipt_reading),
                    None => t::ResponseCode::FailedMeteringSignatureNotValid,
                };
                self.note(format!("metering receipt: {}", verdict.xml_name()));
                self.iso2_note_reply("MeteringReceiptRes", verdict, !is_ok(verdict));
                self.iso2_output(B::MeteringReceiptRes(MeteringReceiptRes { response_code: verdict, evse_status: self.iso2_status() }), !is_ok(verdict))
            }
            (State::ChargeLoop, B::PowerDeliveryReq(req)) if session_ok && req.charge_progress == t::ChargeProgress::Stop => {
                self.state = if self.iso2_ac { State::AwaitSessionStopReq } else { State::AwaitWeldingReq };
                self.iso2_note_reply("PowerDeliveryRes", t::ResponseCode::Ok, false);
                self.iso2_output(B::PowerDeliveryRes(PowerDeliveryRes { response_code: t::ResponseCode::Ok, evse_status: self.iso2_status() }), false)
            }
            (State::AwaitWeldingReq, B::WeldingDetectionReq(_)) if session_ok => {
                let c = code(self.config.welding_code);
                self.iso2_note_reply("WeldingDetectionRes", c, !is_ok(c));
                let body = B::WeldingDetectionRes(WeldingDetectionRes {
                    response_code: c,
                    dc_evse_status: self.iso2_dc_status(t::DcEvseStatusCode::EvseReady, Some(t::IsolationLevel::Valid)),
                    evse_present_voltage: pv(5.0, UnitSymbol::V),
                });
                self.iso2_output(body, !is_ok(c))
            }
            (_, req) => {
                let c = if session_ok { t::ResponseCode::FailedSequenceError } else { t::ResponseCode::FailedUnknownSession };
                self.note(format!("a request out of sequence (state {state:?}): {}", c.xml_name()));
                self.iso2_note_reply("response", c, true);
                Output { send: self.iso2_failing(req, c), close: true }
            }
        }
    }
}
