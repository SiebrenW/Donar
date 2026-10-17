//! The DIN SPEC 70121 side of the simulated charger (DC, external payment): the SECC message
//! sequence of 9.7.4.2.4 with the response codes of 9.7.3.

use din70121::body::*;
use din70121::def::{Body, BodyElementChoice as B, V2gMessage};
use din70121::header::MessageHeader;
use din70121::types::{self as t, PhysicalValue, UnitSymbol};
use exi::Arena;
use iso15118_20::common_types::ResponseCode as Code20;
use v2gtp::PayloadType;

use super::{Evse, Output, State, frame};
use crate::config::DinEnergyTransfer;

/// The request the charger waits for in `state`.
pub(super) fn expects(state: State) -> &'static str {
    match state {
        State::AwaitSupportedAppProtocolReq => "SupportedAppProtocolReq",
        State::AwaitSessionSetupReq => "SessionSetupReq",
        State::AwaitServiceDiscoveryReq => "ServiceDiscoveryReq",
        State::AwaitServicePaymentSelectionReq => "ServicePaymentSelectionReq",
        State::AwaitContractAuthenticationReq => "ContractAuthenticationReq",
        State::AwaitChargeParameterDiscoveryReq => "ChargeParameterDiscoveryReq",
        State::AwaitCableCheckReq => "CableCheckReq",
        State::AwaitPreChargeReq | State::AwaitPowerDeliveryReq => "PreChargeReq or PowerDeliveryReq",
        State::ChargeLoop => "CurrentDemandReq or PowerDeliveryReq",
        State::AwaitWeldingReq => "WeldingDetectionReq or SessionStopReq",
        State::AwaitSessionStopReq => "SessionStopReq",
        _ => "nothing",
    }
}

/// The response code of the ISO 15118-20 knobs in DIN SPEC 70121's code set, by name (the sets
/// share their names; one the older standard does not have is `FAILED`).
fn code(c: Code20) -> t::ResponseCode {
    t::ResponseCode::ALL.into_iter().find(|d| d.xml_name() == c.xml_name()).unwrap_or(t::ResponseCode::Failed)
}

fn is_ok(c: t::ResponseCode) -> bool {
    matches!(c, t::ResponseCode::Ok | t::ResponseCode::OkNewSessionEstablished | t::ResponseCode::OkOldSessionJoined | t::ResponseCode::OkCertificateExpiresSoon)
}

/// `49*89*6360` as hexBinary: a nibble per character, `*` as `A` (V2G-DC-621); an odd count is
/// padded with `F`.
pub(crate) fn evse_id(text: &str) -> Vec<u8> {
    let nibble = |c: char| if c == '*' { 0xA } else { c.to_digit(10).map_or(0xF, |d| d as u8) };
    let nibbles: Vec<u8> = text.chars().take(64).map(nibble).collect();
    nibbles.chunks(2).map(|p| (p[0] << 4) | p.get(1).copied().unwrap_or(0xF)).collect()
}

fn pv(x: f64, unit: UnitSymbol) -> PhysicalValue {
    PhysicalValue::from_f64(x, unit)
}

impl Evse {
    fn din_status(&self, status: t::DcEvseStatusCode, isolation: Option<t::IsolationLevel>) -> t::DcEvseStatus {
        t::DcEvseStatus { evse_isolation_status: isolation, evse_status_code: status, notification_max_delay: 0, evse_notification: t::EvseNotification::None }
    }

    /// Frames a response with the charger's session id in the header.
    fn din_reply(&self, body: B<'_>) -> Option<Vec<u8>> {
        let message = V2gMessage {
            header: MessageHeader { session_id: &self.session_id, notification: None, signature: None },
            body: Body { body_element: Some(body) },
        };
        frame(PayloadType::SAP, |out| message.encode(out))
    }

    fn din_output(&self, body: B<'_>, close: bool) -> Output {
        Output { send: self.din_reply(body), close }
    }

    /// The response a request asks for, with a failing code: the answer to a request out of
    /// sequence or with the wrong session id (V2G-DC-390, V2G-DC-391).
    fn din_failing(&self, request: &B<'_>, c: t::ResponseCode) -> Option<Vec<u8>> {
        let status = self.din_status(t::DcEvseStatusCode::EvseReady, None);
        let zero = pv(0.0, UnitSymbol::V);
        let tuple = [t::SaScheduleTuple {
            sa_schedule_tuple_id: 1,
            p_max_schedule: t::PMaxSchedule {
                p_max_schedule_id: 1,
                p_max_schedule_entry: &[t::PMaxScheduleEntry { time_interval: t::TimeIntervalChoice::Relative(t::RelativeTimeInterval { start: 0, duration: None }), p_max: 0 }],
            },
            sales_tariff: None,
        }];
        let service = t::ServiceCharge {
            service_tag: t::ServiceTag { service_id: 1, service_name: None, service_category: t::ServiceCategory::EvCharging, service_scope: None },
            free_service: false,
            energy_transfer_type: self.din_transfer(),
        };
        let evse_parameter = t::DcEvseChargeParameter {
            dc_evse_status: status,
            evse_maximum_current_limit: pv(0.0, UnitSymbol::A),
            evse_maximum_power_limit: None,
            evse_maximum_voltage_limit: zero,
            evse_minimum_current_limit: pv(0.0, UnitSymbol::A),
            evse_minimum_voltage_limit: zero,
            evse_current_regulation_tolerance: None,
            evse_peak_current_ripple: pv(0.0, UnitSymbol::A),
            evse_energy_to_be_delivered: None,
        };
        let body = match request {
            B::SessionSetupReq(_) => B::SessionSetupRes(SessionSetupRes { response_code: c, evseid: &[0], date_time_now: None }),
            B::ServiceDiscoveryReq(_) => B::ServiceDiscoveryRes(ServiceDiscoveryRes {
                response_code: c,
                payment_options: t::PaymentOptions { payment_option: &[t::PaymentOption::ExternalPayment] },
                charge_service: service,
                service_list: None,
            }),
            B::ServicePaymentSelectionReq(_) => B::ServicePaymentSelectionRes(ServicePaymentSelectionRes { response_code: c }),
            B::ContractAuthenticationReq(_) => B::ContractAuthenticationRes(ContractAuthenticationRes { response_code: c, evse_processing: t::EvseProcessing::Finished }),
            B::ChargeParameterDiscoveryReq(_) => B::ChargeParameterDiscoveryRes(ChargeParameterDiscoveryRes {
                response_code: c,
                evse_processing: t::EvseProcessing::Finished,
                sa_schedules: SaSchedulesChoice::SaScheduleList(t::SaScheduleList { sa_schedule_tuple: &tuple }),
                evse_charge_parameter: EvseChargeParameterChoice::Dc(evse_parameter),
            }),
            B::CableCheckReq(_) => B::CableCheckRes(CableCheckRes { response_code: c, dc_evse_status: status, evse_processing: t::EvseProcessing::Finished }),
            B::PreChargeReq(_) => B::PreChargeRes(PreChargeRes { response_code: c, dc_evse_status: status, evse_present_voltage: zero }),
            B::PowerDeliveryReq(_) => B::PowerDeliveryRes(PowerDeliveryRes { response_code: c, evse_status: EvseStatusChoice::Dc(status) }),
            B::CurrentDemandReq(_) => B::CurrentDemandRes(CurrentDemandRes {
                response_code: c,
                dc_evse_status: status,
                evse_present_voltage: zero,
                evse_present_current: pv(0.0, UnitSymbol::A),
                evse_current_limit_achieved: false,
                evse_voltage_limit_achieved: false,
                evse_power_limit_achieved: false,
                evse_maximum_voltage_limit: None,
                evse_maximum_current_limit: None,
                evse_maximum_power_limit: None,
            }),
            B::WeldingDetectionReq(_) => B::WeldingDetectionRes(WeldingDetectionRes { response_code: c, dc_evse_status: status, evse_present_voltage: zero }),
            B::SessionStopReq(_) => B::SessionStopRes(SessionStopRes { response_code: c }),
            _ => return None,
        };
        self.din_reply(body)
    }

    fn din_transfer(&self) -> t::EvseSupportedEnergyTransfer {
        match self.config.din.energy_transfer {
            DinEnergyTransfer::DcCore => t::EvseSupportedEnergyTransfer::DcCore,
            DinEnergyTransfer::DcExtended => t::EvseSupportedEnergyTransfer::DcExtended,
        }
    }

    /// Notes what the charger answered, by name.
    fn din_note_reply(&mut self, name: &str, c: t::ResponseCode, close: bool) {
        self.note(format!("tx {name} {}{}", c.xml_name(), if close { ", then closing" } else { "" }));
    }

    pub(super) fn on_din_frame(&mut self, payload_type: PayloadType, payload: &[u8]) -> Output {
        let closed = Output { send: None, close: true };
        if payload_type != PayloadType::SAP {
            return closed;
        }
        let mut mem = vec![0u8; 16384];
        let arena = Arena::new(&mut mem);
        let Ok(message) = V2gMessage::decode(payload, &arena) else { return closed };
        let Some(request) = message.body.body_element else { return closed };
        let session_ok = message.header.session_id == self.session_id;
        let state = self.state;
        let limits = self.limits();
        let l = &limits;
        // SessionStopReq is accepted wherever the session has not yet started the power (V2G-DC-664)
        // and in the stopping states.
        if let (B::SessionStopReq(_), true) = (&request, state != State::AwaitSessionSetupReq) {
            if !session_ok {
                return Output { send: self.din_failing(&request, t::ResponseCode::FailedUnknownSession), close: true };
            }
            let stopping = matches!(state, State::AwaitWeldingReq | State::AwaitSessionStopReq)
                || !matches!(state, State::ChargeLoop);
            if stopping {
                self.state = State::Stopped;
                self.din_note_reply("SessionStopRes", t::ResponseCode::Ok, true);
                return self.din_output(B::SessionStopRes(SessionStopRes { response_code: t::ResponseCode::Ok }), true);
            }
        }
        match (state, &request) {
            (State::AwaitSessionSetupReq, B::SessionSetupReq(req)) => {
                if self.config.silent_on_setup {
                    return Output::default();
                }
                if req.evccid.is_empty() {
                    self.note("SessionSetupReq without an EVCCID");
                }
                let c = code(self.config.setup_code);
                self.note(format!("SessionSetupReq from EVCC {:02x?}", req.evccid));
                let id = evse_id(&self.config.din.evse_id);
                // V2G-DC-393: a new session has an id of its own.
                let now = now_micros() / 1_000_000;
                if is_ok(c) {
                    self.state = State::AwaitServiceDiscoveryReq;
                }
                self.din_note_reply("SessionSetupRes", c, !is_ok(c));
                self.din_output(B::SessionSetupRes(SessionSetupRes { response_code: c, evseid: &id, date_time_now: Some(now as i64) }), !is_ok(c))
            }
            (State::AwaitServiceDiscoveryReq, B::ServiceDiscoveryReq(_)) if session_ok => {
                let options = [t::PaymentOption::ExternalPayment, t::PaymentOption::Contract];
                let options = &options[..if self.config.din.offer_contract_payment { 2 } else { 1 }];
                let body = B::ServiceDiscoveryRes(ServiceDiscoveryRes {
                    response_code: t::ResponseCode::Ok,
                    payment_options: t::PaymentOptions { payment_option: options },
                    charge_service: t::ServiceCharge {
                        service_tag: t::ServiceTag { service_id: 1, service_name: None, service_category: t::ServiceCategory::EvCharging, service_scope: None },
                        free_service: false,
                        energy_transfer_type: self.din_transfer(),
                    },
                    service_list: None,
                });
                self.state = State::AwaitServicePaymentSelectionReq;
                self.din_note_reply("ServiceDiscoveryRes", t::ResponseCode::Ok, false);
                self.din_output(body, false)
            }
            (State::AwaitServicePaymentSelectionReq, B::ServicePaymentSelectionReq(req)) if session_ok => {
                let offered_contract = self.config.din.offer_contract_payment;
                let payment_ok = req.selected_payment_option == t::PaymentOption::ExternalPayment || (offered_contract && req.selected_payment_option == t::PaymentOption::Contract);
                let service_ok = req.selected_service_list.selected_service.iter().all(|s| s.service_id == 1);
                // V2G-DC-395, V2G-DC-396.
                let c = if !payment_ok {
                    t::ResponseCode::FailedPaymentSelectionInvalid
                } else if !service_ok || req.selected_service_list.selected_service.is_empty() {
                    t::ResponseCode::FailedServiceSelectionInvalid
                } else {
                    code(self.config.selection_code)
                };
                if is_ok(c) {
                    self.state = State::AwaitContractAuthenticationReq;
                }
                self.din_note_reply("ServicePaymentSelectionRes", c, !is_ok(c));
                self.din_output(B::ServicePaymentSelectionRes(ServicePaymentSelectionRes { response_code: c }), !is_ok(c))
            }
            (State::AwaitContractAuthenticationReq, B::ContractAuthenticationReq(_)) if session_ok => {
                self.authorization_reqs_seen += 1;
                let ongoing = self.config.authorization_never_finishes || self.authorization_reqs_seen <= self.config.authorization_ongoing_responses;
                let c = if ongoing { t::ResponseCode::Ok } else { code(self.config.authorization_code) };
                let processing = if ongoing { t::EvseProcessing::Ongoing } else { t::EvseProcessing::Finished };
                if !ongoing && is_ok(c) {
                    self.state = State::AwaitChargeParameterDiscoveryReq;
                }
                self.din_note_reply("ContractAuthenticationRes", c, !ongoing && !is_ok(c));
                self.din_output(B::ContractAuthenticationRes(ContractAuthenticationRes { response_code: c, evse_processing: processing }), !ongoing && !is_ok(c))
            }
            (State::AwaitChargeParameterDiscoveryReq, B::ChargeParameterDiscoveryReq(req)) if session_ok => {
                self.schedule_reqs_seen += 1;
                let EvChargeParameterChoice::Dc(ev) = req.ev_charge_parameter else {
                    // V2G-DC-398.
                    self.din_note_reply("ChargeParameterDiscoveryRes", t::ResponseCode::FailedWrongChargeParameter, true);
                    return Output { send: self.din_failing(&request, t::ResponseCode::FailedWrongChargeParameter), close: true };
                };
                let wanted_ok = matches!(
                    (req.ev_requested_energy_transfer_type, self.config.din.energy_transfer),
                    (t::EvRequestedEnergyTransfer::DcCore, DinEnergyTransfer::DcCore) | (t::EvRequestedEnergyTransfer::DcExtended, DinEnergyTransfer::DcExtended)
                );
                if !wanted_ok {
                    // V2G-DC-397.
                    self.note(format!("the vehicle asks for {:?}, the charger offers {:?}", req.ev_requested_energy_transfer_type, self.config.din.energy_transfer));
                    self.din_note_reply("ChargeParameterDiscoveryRes", t::ResponseCode::FailedWrongEnergyTransferType, true);
                    return Output { send: self.din_failing(&request, t::ResponseCode::FailedWrongEnergyTransferType), close: true };
                }
                self.din_ev_announced = Some(ev);
                let ongoing = self.schedule_reqs_seen <= self.config.din.cpd_ongoing_responses;
                let c = if ongoing { t::ResponseCode::Ok } else { code(self.config.cpd_code) };
                let processing = if ongoing { t::EvseProcessing::Ongoing } else { t::EvseProcessing::Finished };
                // V2G-DC-559: the schedule covers at least 24 hours.
                let entry = [t::PMaxScheduleEntry {
                    time_interval: t::TimeIntervalChoice::Relative(t::RelativeTimeInterval { start: 0, duration: Some(86_400) }),
                    p_max: (f64::from(l.max_charge_power_w)).min(f64::from(i16::MAX)) as i16,
                }];
                let tuple = [t::SaScheduleTuple { sa_schedule_tuple_id: 1, p_max_schedule: t::PMaxSchedule { p_max_schedule_id: 1, p_max_schedule_entry: &entry }, sales_tariff: None }];
                let status = self.din_status(t::DcEvseStatusCode::EvseReady, None);
                let parameter = t::DcEvseChargeParameter {
                    dc_evse_status: status,
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
                let body = B::ChargeParameterDiscoveryRes(ChargeParameterDiscoveryRes {
                    response_code: c,
                    evse_processing: processing,
                    sa_schedules: SaSchedulesChoice::SaScheduleList(t::SaScheduleList { sa_schedule_tuple: &tuple }),
                    evse_charge_parameter: EvseChargeParameterChoice::Dc(parameter),
                });
                if !ongoing && is_ok(c) {
                    self.state = State::AwaitCableCheckReq;
                }
                self.din_note_reply("ChargeParameterDiscoveryRes", c, !ongoing && !is_ok(c));
                self.din_output(body, !ongoing && !is_ok(c))
            }
            (State::AwaitCableCheckReq, B::CableCheckReq(_)) if session_ok => {
                self.cable_reqs_seen += 1;
                let ongoing = self.cable_reqs_seen <= self.config.cable_check_ongoing_responses;
                let c = if ongoing { t::ResponseCode::Ok } else { code(self.config.cable_check_code) };
                // V2G-DC-499, V2G-DC-455.
                let (status, isolation, processing) = if ongoing {
                    (t::DcEvseStatusCode::EvseIsolationMonitoringActive, t::IsolationLevel::Invalid, t::EvseProcessing::Ongoing)
                } else {
                    (t::DcEvseStatusCode::EvseReady, t::IsolationLevel::Valid, t::EvseProcessing::Finished)
                };
                if !ongoing && is_ok(c) {
                    self.state = State::AwaitPreChargeReq;
                }
                self.din_note_reply("CableCheckRes", c, !ongoing && !is_ok(c));
                let body = B::CableCheckRes(CableCheckRes { response_code: c, dc_evse_status: self.din_status(status, Some(isolation)), evse_processing: processing });
                self.din_output(body, !ongoing && !is_ok(c))
            }
            (State::AwaitPreChargeReq, B::PreChargeReq(req)) if session_ok => {
                self.precharge_reqs_seen += 1;
                // Until "settled" the output is 100 V away from the EV's target.
                let settled = self.precharge_reqs_seen >= self.config.precharge_requests_to_settle;
                let target = req.ev_target_voltage.to_f64();
                let voltage = if settled { target } else { (target - 100.0).max(0.0) };
                self.din_note_reply("PreChargeRes", t::ResponseCode::Ok, false);
                let body = B::PreChargeRes(PreChargeRes {
                    response_code: t::ResponseCode::Ok,
                    dc_evse_status: self.din_status(t::DcEvseStatusCode::EvseReady, Some(t::IsolationLevel::Valid)),
                    evse_present_voltage: pv(voltage, UnitSymbol::V),
                });
                self.din_output(body, false)
            }
            (State::AwaitPreChargeReq, B::PowerDeliveryReq(req)) if session_ok && req.ready_to_charge_state => {
                // V2G-DC-399, V2G-DC-400: the profile the vehicle chose, against the schedule.
                let mut c = code(self.config.power_delivery_code);
                if let Some(profile) = &req.charging_profile {
                    if profile.sa_schedule_tuple_id != 1 {
                        c = t::ResponseCode::FailedTariffSelectionInvalid;
                    } else if profile.profile_entry.iter().any(|e| f64::from(e.charging_profile_entry_max_power) > f64::from(l.max_charge_power_w).min(f64::from(i16::MAX))) {
                        c = t::ResponseCode::FailedChargingProfileInvalid;
                    }
                }
                if is_ok(c) {
                    self.state = State::ChargeLoop;
                }
                self.din_note_reply("PowerDeliveryRes", c, !is_ok(c));
                let status = self.din_status(t::DcEvseStatusCode::EvseReady, Some(t::IsolationLevel::Valid));
                self.din_output(B::PowerDeliveryRes(PowerDeliveryRes { response_code: c, evse_status: EvseStatusChoice::Dc(status) }), !is_ok(c))
            }
            (State::ChargeLoop, B::CurrentDemandReq(req)) if session_ok => {
                self.loops_seen += 1;
                let mut c = t::ResponseCode::Ok;
                // V2G-DC-641: the power the vehicle announced is the most it asks for.
                if let (Some(sent), Some(announced)) = (req.ev_maximum_power_limit, self.din_ev_announced.and_then(|a| a.ev_maximum_power_limit))
                    && sent.to_f64() > announced.to_f64() * 1.000_001 {
                        self.note(format!("violation: V2G-DC-641: EVMaximumPowerLimit is {}, the discovery announced {}", sent.to_f64(), announced.to_f64()));
                        if self.config.fail_on_limit_violation {
                            c = t::ResponseCode::Failed;
                        }
                    }
                let ev_voltage = req.ev_target_voltage.to_f64();
                let ev_max_current = req.ev_target_current.to_f64();
                let ev_max_power = req.ev_maximum_power_limit.map_or(f64::from(l.max_charge_power_w), |p| p.to_f64());
                let (voltage, current, max_power) = match &self.live {
                    Some(live) => live.charge_loop(ev_voltage, ev_max_current, ev_max_power),
                    None => (ev_voltage, crate::live::modelled_current(ev_voltage, ev_max_current, ev_max_power, l), f64::from(l.max_charge_power_w)),
                };
                let limit = |achieved: f64, limit: f64| current > 0.0 && achieved >= limit * 0.999;
                let shutdown = self.config.din.shutdown_after_loops.is_some_and(|n| self.loops_seen >= n);
                let status = self.din_status(if shutdown { t::DcEvseStatusCode::EvseShutdown } else { t::DcEvseStatusCode::EvseReady }, Some(t::IsolationLevel::Valid));
                let body = B::CurrentDemandRes(CurrentDemandRes {
                    response_code: c,
                    dc_evse_status: status,
                    evse_present_voltage: pv(voltage, UnitSymbol::V),
                    evse_present_current: pv(current, UnitSymbol::A),
                    evse_current_limit_achieved: limit(current, ev_max_current.min(f64::from(l.max_charge_current_a))),
                    evse_voltage_limit_achieved: limit(voltage, f64::from(l.max_voltage_v)),
                    evse_power_limit_achieved: limit(voltage * current, ev_max_power.min(f64::from(l.max_charge_power_w))),
                    evse_maximum_voltage_limit: Some(pv(f64::from(l.max_voltage_v), UnitSymbol::V)),
                    evse_maximum_current_limit: Some(pv(f64::from(l.max_charge_current_a), UnitSymbol::A)),
                    evse_maximum_power_limit: Some(pv(max_power, UnitSymbol::W)),
                });
                self.din_note_reply("CurrentDemandRes", c, !is_ok(c));
                self.din_output(body, !is_ok(c))
            }
            (State::ChargeLoop, B::PowerDeliveryReq(req)) if session_ok && !req.ready_to_charge_state => {
                let c = t::ResponseCode::Ok;
                self.state = State::AwaitWeldingReq;
                self.din_note_reply("PowerDeliveryRes", c, false);
                let status = self.din_status(t::DcEvseStatusCode::EvseReady, Some(t::IsolationLevel::Valid));
                self.din_output(B::PowerDeliveryRes(PowerDeliveryRes { response_code: c, evse_status: EvseStatusChoice::Dc(status) }), false)
            }
            (State::AwaitWeldingReq, B::WeldingDetectionReq(_)) if session_ok => {
                let c = code(self.config.welding_code);
                self.din_note_reply("WeldingDetectionRes", c, !is_ok(c));
                let body = B::WeldingDetectionRes(WeldingDetectionRes {
                    response_code: c,
                    dc_evse_status: self.din_status(t::DcEvseStatusCode::EvseReady, Some(t::IsolationLevel::Valid)),
                    evse_present_voltage: pv(5.0, UnitSymbol::V),
                });
                self.din_output(body, !is_ok(c))
            }
            // A request with the wrong session id (V2G-DC-391), or out of sequence (V2G-DC-390):
            // the answer of its own type and the end of the session (V2G-DC-666).
            (_, req) => {
                let c = if session_ok { t::ResponseCode::FailedSequenceError } else { t::ResponseCode::FailedUnknownSession };
                self.note(format!("a request out of sequence (state {state:?}): {}", c.xml_name()));
                self.din_note_reply("response", c, true);
                Output { send: self.din_failing(req, c), close: true }
            }
        }
    }
}

fn now_micros() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64)
}
