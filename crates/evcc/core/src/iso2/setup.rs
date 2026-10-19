//! The handshake, the session setup, payment, authorization and the charge parameters.

use app_protocol::{self, SupportedAppProtocolReq};
use iso15118_2::def::V2gMessage;
use iso15118_2::header::MessageHeader;
use iso15118_2::signature::{CANONICAL_EXI, Reference, Signature, SignatureValue, SignedInfo, encode_fragment};
use iso15118_2::body::{Body, *};
use iso15118_2::types::*;
use v2gtp::PayloadType;

use super::convert::{physical, rational};
use super::{Machine, MAX_ENTRIES, Mode, Schedule, EvseLimits, frame};
use crate::ev::{DcConnector, Ev, MAX_DIGEST_LEN, MAX_SIGNATURE_LEN, MAX_SUB_CERTIFICATES};
use crate::log::{LogSink, MsgKind, Record};
use crate::session::{EndReason, Notice, State, Step};
use crate::{Timer, TimerCmd};

impl<E: Ev, L: LogSink> Machine<E, L> {
    pub(super) fn send_supported_app_protocol_req(&mut self, tx: &mut [u8]) -> Step {
        let offer = [self.config.offer];
        let msg = SupportedAppProtocolReq { app_protocol: &offer };
        let framed = frame(tx, PayloadType::SAP, |out| msg.encode(out));
        let Ok(len) = framed else { return self.end(EndReason::TxBufferTooSmall) };
        self.log.record(&Record::MsgTx { msg: MsgKind::SupportedAppProtocolReq });
        let step = Step {
            send: Some(len),
            timers: crate::Timers::one(TimerCmd::Arm { timer: Timer::Response, ms: self.config.response_timeout_ms }),
            ..Step::default()
        };
        self.goto(State::AwaitSupportedAppProtocolRes);
        step
    }

    pub(super) fn on_supported_app_protocol_res(&mut self, res: app_protocol::SupportedAppProtocolRes, tx: &mut [u8]) -> Step {
        if res.response_code == app_protocol::ResponseCode::FailedNoNegotiation || res.schema_id != Some(self.config.offer.schema_id) {
            return self.end(EndReason::NoCommonProtocol);
        }
        let mac = self.ev.evcc_mac();
        match self.config.resume {
            // A paused session is joined again with its id.
            Some(resume) => {
                self.session_id = resume.session_id;
                self.session_len = 8;
            }
            None => {
                self.session_id = [0; 8];
                self.session_len = 1; // the session id is zero until the charger assigns one
            }
        }
        let body = BodyElementChoice::SessionSetupReq(SessionSetupReq { evccid: &mac });
        self.send(body, MsgKind::SessionSetupReq, State::AwaitSessionSetupRes, tx)
    }

    pub(super) fn on_session_setup_res(&mut self, session_id: &[u8], res: SessionSetupRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::SessionSetupRes, res.response_code) {
            return end;
        }
        let len = session_id.len().min(self.session_id.len());
        self.session_id = [0; 8];
        self.session_id[..len].copy_from_slice(&session_id[..len]);
        self.session_len = len.max(1);
        let mut id = [0u8; 8];
        id.copy_from_slice(&self.session_id);
        self.log.record(&Record::SessionSetUp { session_id: id, evse_id: res.evseid });
        let body = BodyElementChoice::ServiceDiscoveryReq(ServiceDiscoveryReq { service_scope: None, service_category: None });
        let mut step = self.send(body, MsgKind::ServiceDiscoveryReq, State::AwaitServiceDiscoveryRes, tx);
        step.notice = Some(Notice::SessionSetUp { session_id: id });
        if step.end.is_none() {
            step.timers = step.timers.with(TimerCmd::Cancel(Timer::Setup));
            step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::ReadyToCharge, ms: self.config.ready_to_charge_timeout_ms });
        }
        step
    }

    /// The energy transfer mode the vehicle asks for.
    pub(super) fn requested_mode(&self) -> EnergyTransferMode {
        match self.config.mode {
            Mode::AcSinglePhase => EnergyTransferMode::AcSinglePhaseCore,
            Mode::AcThreePhase => EnergyTransferMode::AcThreePhaseCore,
            Mode::Dc => match self.ev.dc_connector() {
                DcConnector::Core => EnergyTransferMode::DcCore,
                DcConnector::Extended => EnergyTransferMode::DcExtended,
                DcConnector::Dual2 | DcConnector::Dual4 => EnergyTransferMode::DcComboCore,
            },
        }
    }

    pub(super) fn on_service_discovery_res(&mut self, res: ServiceDiscoveryRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ServiceDiscoveryRes, res.response_code) {
            return end;
        }
        let offered = res.payment_option_list.payment_option;
        // Plug & Charge when asked for, offered and possible; otherwise the charger has to take
        // external payment.
        let wants_contract = self.config.prefer_contract && offered.contains(&PaymentOption::Contract) && self.ev.contract().is_some();
        let payment = if wants_contract { PaymentOption::Contract } else { PaymentOption::ExternalPayment };
        if !offered.contains(&payment) {
            self.log.record(&Record::Iso2NoPayment { wanted: payment, offered });
            return self.end_politely(EndReason::NoCompatibleAuthorization, tx);
        }
        self.contract = payment == PaymentOption::Contract;
        let wanted = self.requested_mode();
        let modes = res.charge_service.supported_energy_transfer_mode.energy_transfer_mode;
        if !modes.contains(&wanted) {
            self.log.record(&Record::Iso2NoEnergyMode { wanted, offered: modes });
            return self.end_politely(EndReason::NoCompatibleService, tx);
        }
        self.service_id = res.charge_service.service_id;
        // The value-added services the vehicle cares about, of those the charger offers.
        self.detail_len = 0;
        self.detail_next = 0;
        self.chosen_len = 0;
        if let Some(list) = res.service_list {
            for service in list.service {
                if self.config.value_added.contains(&service.service_id) && self.detail_len < super::MAX_VALUE_ADDED {
                    self.detail_queue[self.detail_len] = service.service_id;
                    self.detail_len += 1;
                }
            }
        }
        self.next(tx, |m, tx| m.ask_next_detail_or_select(tx))
    }

    /// `ServiceDetailReq` for the next value-added service in line, or, when there is none, the
    /// payment and service selection.
    fn ask_next_detail_or_select(&mut self, tx: &mut [u8]) -> Step {
        if self.detail_next < self.detail_len {
            return self.send_service_detail_req(tx);
        }
        let mut selected = [SelectedService { service_id: self.service_id, parameter_set_id: None }; super::MAX_VALUE_ADDED + 1];
        selected[1..=self.chosen_len].copy_from_slice(&self.chosen[..self.chosen_len]);
        let payment = if self.contract { PaymentOption::Contract } else { PaymentOption::ExternalPayment };
        let body = BodyElementChoice::PaymentServiceSelectionReq(PaymentServiceSelectionReq {
            selected_payment_option: payment,
            selected_service_list: SelectedServiceList { selected_service: &selected[..=self.chosen_len] },
        });
        self.send(body, MsgKind::ServicePaymentSelectionReq, State::AwaitServicePaymentSelectionRes, tx)
    }

    pub(super) fn send_service_detail_req(&mut self, tx: &mut [u8]) -> Step {
        let body = BodyElementChoice::ServiceDetailReq(ServiceDetailReq { service_id: self.detail_queue[self.detail_next] });
        self.send(body, MsgKind::ServiceDetailReq, State::AwaitServiceDetailRes, tx)
    }

    /// What the charger says about a value-added service: the vehicle takes the first parameter
    /// set (or none, for a service without parameters) and goes on to the next service.
    pub(super) fn on_service_detail_res(&mut self, res: ServiceDetailRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ServiceDetailRes, res.response_code) {
            return end;
        }
        let asked = self.detail_queue[self.detail_next];
        if res.service_id != asked {
            return self.end(EndReason::UnexpectedMessage);
        }
        let set = res.service_parameter_list.and_then(|l| l.parameter_set.first().map(|s| s.parameter_set_id));
        self.log.record(&Record::Iso2ServiceDetail { service_id: asked, parameter_sets: res.service_parameter_list.map_or(0, |l| l.parameter_set.len()) });
        self.chosen[self.chosen_len] = SelectedService { service_id: asked, parameter_set_id: set };
        self.chosen_len += 1;
        self.detail_next += 1;
        self.next(tx, |m, tx| m.ask_next_detail_or_select(tx))
    }

    pub(super) fn on_payment_service_selection_res(&mut self, res: PaymentServiceSelectionRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ServicePaymentSelectionRes, res.response_code) {
            return end;
        }
        self.log.record(&Record::ServiceSelected { service_id: self.service_id, parameter_set_id: 0 });
        self.next(tx, |m, tx| if m.contract { m.send_payment_details_req(tx) } else { m.send_authorization_req(tx) })
    }

    /// The authorization request: empty for external payment; for a contract it carries the challenge
    /// and an `Id`, and the header the signature over it with the contract key.
    pub(super) fn send_authorization_req(&mut self, tx: &mut [u8]) -> Step {
        let step = if self.contract {
            let framed = match (self.ev.contract(), self.challenge) {
                (Some(contract), Some(challenge)) => frame_signed_authorization(contract, challenge, &self.session_id[..self.session_len], tx),
                _ => Err(()),
            };
            if framed.is_err() {
                return self.end(EndReason::TxBufferTooSmall);
            }
            self.sent(framed, MsgKind::AuthorizationReqIso2, State::AwaitContractAuthenticationRes)
        } else {
            let body = BodyElementChoice::AuthorizationReq(AuthorizationReq { id: None, gen_challenge: None });
            self.send(body, MsgKind::AuthorizationReqIso2, State::AwaitContractAuthenticationRes, tx)
        };
        let ms = self.config.ongoing_timeout_ms;
        self.time_phase(step, ms)
    }

    pub(super) fn on_authorization_res(&mut self, res: AuthorizationRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::AuthorizationResIso2, res.response_code) {
            return end;
        }
        match res.evse_processing {
            EvseProcessing::Ongoing | EvseProcessing::OngoingWaitingForCustomerInteraction => self.pause_then_repeat(tx),
            EvseProcessing::Finished => {
                let step = self.phase_done(Step::default());
                let mut next = self.next(tx, |m, tx| m.send_charge_parameter_discovery_req(tx));
                next.timers = super::charge::merge(step.timers, next.timers);
                if next.end.is_none() {
                    next.notice = Some(Notice::Authorized);
                }
                next
            }
        }
    }

    /// Plug & Charge: the eMAID and the contract certificate chain.
    pub(super) fn send_payment_details_req(&mut self, tx: &mut [u8]) -> Step {
        let framed = match self.ev.contract() {
            Some(contract) => {
                let mut subs: [&[u8]; MAX_SUB_CERTIFICATES] = [&[]; MAX_SUB_CERTIFICATES];
                let mut count = 0;
                while count < MAX_SUB_CERTIFICATES {
                    let Some(c) = contract.sub_certificate(count) else { break };
                    subs[count] = c;
                    count += 1;
                }
                let body = BodyElementChoice::PaymentDetailsReq(PaymentDetailsReq {
                    e_maid: contract.emaid(),
                    contract_signature_cert_chain: CertificateChain {
                        id: None,
                        certificate: contract.certificate(),
                        sub_certificates: (count > 0).then_some(SubCertificates { certificate: &subs[..count] }),
                    },
                });
                let message = V2gMessage { header: self.header(), body: Body { body_element: Some(body) } };
                frame(tx, PayloadType::SAP, |out| message.encode(out))
            }
            None => Err(()),
        };
        if framed.is_err() {
            return self.end(EndReason::TxBufferTooSmall);
        }
        self.sent(framed, MsgKind::PaymentDetailsReq, State::AwaitPaymentDetailsRes)
    }

    pub(super) fn on_payment_details_res(&mut self, res: PaymentDetailsRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::PaymentDetailsRes, res.response_code) {
            return end;
        }
        // The challenge the authorization is signed over.
        self.challenge = Some(res.gen_challenge);
        self.next(tx, |m, tx| m.send_authorization_req(tx))
    }

    /// The vehicle's limits and needs: DC (9.5.3 of the DC charge parameter) or AC.
    pub(super) fn send_charge_parameter_discovery_req(&mut self, tx: &mut [u8]) -> Step {
        let schedule = self.ev.schedule_request();
        let parameter = if self.ac {
            let l = self.ev.ac_limits();
            EvChargeParameterChoice::Ac(AcEvChargeParameter {
                departure_time: Some(schedule.departure_time),
                e_amount: physical(l.energy_amount, UnitSymbol::Wh),
                ev_max_voltage: physical(l.maximum_voltage, UnitSymbol::V),
                ev_max_current: physical(l.maximum_current, UnitSymbol::A),
                ev_min_current: physical(l.minimum_current, UnitSymbol::A),
            })
        } else {
            let limits = self.ev.dc_limits();
            self.ev_announced = Some(limits);
            let soc = self.ev.present_soc().unwrap_or(0).min(100) as i8;
            EvChargeParameterChoice::Dc(DcEvChargeParameter {
                departure_time: Some(schedule.departure_time),
                dc_ev_status: self.ev_status(soc),
                ev_maximum_current_limit: physical(limits.maximum_charge_current, UnitSymbol::A),
                ev_maximum_power_limit: Some(physical(limits.maximum_charge_power, UnitSymbol::W)),
                ev_maximum_voltage_limit: physical(limits.maximum_voltage, UnitSymbol::V),
                ev_energy_capacity: Some(physical(schedule.maximum_energy, UnitSymbol::Wh)),
                ev_energy_request: Some(physical(schedule.target_energy, UnitSymbol::Wh)),
                full_soc: Some(100),
                bulk_soc: schedule.target_soc.map(|t| t.min(100) as i8),
            })
        };
        let body = BodyElementChoice::ChargeParameterDiscoveryReq(ChargeParameterDiscoveryReq {
            max_entries_sa_schedule_tuple: Some(self.config.max_schedule_tuples),
            requested_energy_transfer_mode: self.requested_mode(),
            ev_charge_parameter: parameter,
        });
        let step = self.send(body, MsgKind::ChargeParameterDiscoveryReq, State::AwaitChargeParameterDiscoveryRes, tx);
        let ms = self.config.ongoing_timeout_ms;
        self.time_phase(step, ms)
    }

    pub(super) fn on_charge_parameter_discovery_res(&mut self, res: ChargeParameterDiscoveryRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ChargeParameterDiscoveryRes, res.response_code) {
            return end;
        }
        self.pilot.set_discovered(true);
        // While the charger is still working its values are ignored (it asks again).
        if res.evse_processing != EvseProcessing::Finished {
            return self.pause_then_repeat(tx);
        }
        match res.evse_charge_parameter {
            EvseChargeParameterChoice::Dc(p) if !self.ac => {
                self.evse = Some(EvseLimits {
                    max_current: rational(p.evse_maximum_current_limit),
                    max_power: Some(rational(p.evse_maximum_power_limit)),
                    max_voltage: rational(p.evse_maximum_voltage_limit),
                });
            }
            EvseChargeParameterChoice::Ac(p) if self.ac => {
                self.evse = Some(EvseLimits { max_current: rational(p.evse_max_current), max_power: None, max_voltage: rational(p.evse_nominal_voltage) });
            }
            // The charger answered with the other kind of parameters.
            _ => return self.end(EndReason::UnexpectedMessage),
        }
        // The first tuple is the default schedule; with price levels on every tuple the vehicle takes the cheapest that
        // delivers what it needs before it leaves.
        let Some(SaSchedulesChoice::SaScheduleList(list)) = res.sa_schedules else { return self.end(EndReason::UnexpectedMessage) };
        if list.sa_schedule_tuple.is_empty() {
            return self.end(EndReason::UnexpectedMessage);
        }
        let request = self.ev.schedule_request();
        let max_watts = if self.ac {
            let l = self.ev.ac_limits();
            let phases = if self.config.mode == Mode::AcThreePhase { 3.0 } else { 1.0 };
            let volts = self.evse.map_or(l.maximum_voltage.to_f64(), |e| e.max_voltage.to_f64());
            let amps = self.evse.map_or(l.maximum_current.to_f64(), |e| e.max_current.to_f64().min(l.maximum_current.to_f64()));
            volts * amps * phases
        } else {
            self.ev.dc_limits().maximum_charge_power.to_f64()
        };
        let need_wh = request.target_energy.to_f64().max(0.0);
        let chosen = super::tariff::choose(list.sa_schedule_tuple, need_wh, max_watts, f64::from(request.departure_time));
        let tuple = &list.sa_schedule_tuple[chosen];
        self.log.record(&Record::ScheduleSelected { tuple_id: u32::from(tuple.sa_schedule_tuple_id), offered: list.sa_schedule_tuple.len(), need_wh: need_wh as u32, departure_s: request.departure_time });
        let mut schedule = Schedule { id: tuple.sa_schedule_tuple_id, entries: [(0, 0.0); MAX_ENTRIES], len: 0 };
        for entry in tuple.p_max_schedule.p_max_schedule_entry.iter().take(MAX_ENTRIES) {
            let TimeIntervalChoice::Relative(interval) = entry.time_interval;
            schedule.entries[schedule.len] = (interval.start, super::convert::number(entry.p_max));
            schedule.len += 1;
        }
        self.schedule = Some(schedule);
        let step = self.phase_done(Step::default());
        let mut next = if self.renegotiating {
            // New schedule: the power goes on again under it.
            self.send_power_delivery(true, tx)
        } else if self.stop_requested && !self.power_on {
            self.stop_requested = false;
            self.send_session_stop(tx)
        } else if self.ac {
            // AC has no cable check or pre-charge: power is asked for at once, and the pilot goes to C or D no later than 250 ms after
            // that request (V2G2-847); the host does that when it sees the notice.
            let mut s = self.send_power_delivery(true, tx);
            if s.end.is_none() {
                s.notice = Some(Notice::ControlPilot(crate::session::ControlPilot::C));
                self.pilot.set_ready(true);
            }
            s
        } else {
            // The control pilot goes to state C before the cable check.
            self.change_control_pilot(crate::session::ControlPilot::C, State::AwaitControlPilotC)
        };
        next.timers = super::charge::merge(step.timers, next.timers);
        next
    }
}

/// A signed `MeteringReceiptReq`: the meter info the charger sent, and in the header the signature
/// over the element with the contract key.
pub(super) fn frame_signed_receipt(contract: &dyn crate::ev::Contract, session_id: &[u8], tuple: u8, meter: MeterInfo<'_>, tx: &mut [u8]) -> Result<usize, ()> {
    let (out, scratch) = tx.split_at_mut(tx.len() / 2);
    let request = MeteringReceiptReq { id: Some(RECEIPT_ID), session_id, sa_schedule_tuple_id: Some(tuple), meter_info: meter };
    let length = encode_fragment(scratch, iso15118_2::signature::fragment::METERING_RECEIPT_REQ, |e| request.encode_content(e)).map_err(|_| ())?;
    let mut digest = [0u8; MAX_DIGEST_LEN];
    let digest_len = contract.digest(&scratch[..length], &mut digest).ok_or(())?;
    let references = [Reference {
        id: None,
        uri: Some(RECEIPT_URI),
        transform_algorithm: Some(CANONICAL_EXI),
        digest_algorithm: contract.digest_algorithm(),
        digest_value: &digest[..digest_len],
    }];
    let info = SignedInfo { id: None, canonicalization_algorithm: CANONICAL_EXI, signature_algorithm: contract.signature_algorithm(), reference: &references };
    let length = iso15118_2::signature::signed_info_fragment(&info, scratch).map_err(|_| ())?;
    let mut value = [0u8; MAX_SIGNATURE_LEN];
    let value_len = contract.sign(&scratch[..length], &mut value).ok_or(())?;
    let message = V2gMessage {
        header: MessageHeader {
            session_id,
            notification: None,
            signature: Some(Signature { id: None, signed_info: info, signature_value: SignatureValue { id: None, value: &value[..value_len] } }),
        },
        body: Body { body_element: Some(BodyElementChoice::MeteringReceiptReq(request)) },
    };
    frame(out, PayloadType::SAP, |o| message.encode(o))
}

const RECEIPT_ID: &str = "ID2";
const RECEIPT_URI: &str = "#ID2";

/// The `Id` of the signed `AuthorizationReq` and the URI that references it.
const AUTHORIZATION_ID: &str = "ID1";
const AUTHORIZATION_URI: &str = "#ID1";

/// A signed `AuthorizationReq`: the element with the challenge and its `Id`, and in the header the
/// signature over its fragment with the contract key. The second half of `tx` is scratch for the
/// two fragments that are digested and signed; the frame is built in the first.
fn frame_signed_authorization(contract: &dyn crate::ev::Contract, challenge: [u8; 16], session_id: &[u8], tx: &mut [u8]) -> Result<usize, ()> {
    let (out, scratch) = tx.split_at_mut(tx.len() / 2);
    let request = AuthorizationReq { id: Some(AUTHORIZATION_ID), gen_challenge: Some(challenge) };
    let length = encode_fragment(scratch, iso15118_2::signature::fragment::AUTHORIZATION_REQ, |e| request.encode_content(e)).map_err(|_| ())?;
    let mut digest = [0u8; MAX_DIGEST_LEN];
    let digest_len = contract.digest(&scratch[..length], &mut digest).ok_or(())?;
    let references = [Reference {
        id: None,
        uri: Some(AUTHORIZATION_URI),
        transform_algorithm: Some(CANONICAL_EXI),
        digest_algorithm: contract.digest_algorithm(),
        digest_value: &digest[..digest_len],
    }];
    let info = SignedInfo { id: None, canonicalization_algorithm: CANONICAL_EXI, signature_algorithm: contract.signature_algorithm(), reference: &references };
    let length = iso15118_2::signature::signed_info_fragment(&info, scratch).map_err(|_| ())?;
    let mut value = [0u8; MAX_SIGNATURE_LEN];
    let value_len = contract.sign(&scratch[..length], &mut value).ok_or(())?;
    let message = V2gMessage {
        header: MessageHeader {
            session_id,
            notification: None,
            signature: Some(Signature { id: None, signed_info: info, signature_value: SignatureValue { id: None, value: &value[..value_len] } }),
        },
        body: Body { body_element: Some(BodyElementChoice::AuthorizationReq(request)) },
    };
    frame(out, PayloadType::SAP, |o| message.encode(o))
}
