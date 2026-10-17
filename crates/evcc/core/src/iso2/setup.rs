//! The handshake, the session setup, payment, authorization and the charge parameters.

use app_protocol::{self, SupportedAppProtocolReq};
use iso15118_2::body::*;
use iso15118_2::types::*;
use v2gtp::PayloadType;

use super::convert::{physical, rational};
use super::{Machine, MAX_ENTRIES, Mode, Schedule, EvseLimits, frame};
use crate::ev::{DcConnector, Ev};
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
        self.session_id = [0; 8];
        self.session_len = 1; // the session id is zero until the charger assigns one
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
            return self.end(EndReason::NoCompatibleAuthorization);
        }
        self.contract = payment == PaymentOption::Contract;
        let wanted = self.requested_mode();
        let modes = res.charge_service.supported_energy_transfer_mode.energy_transfer_mode;
        if !modes.contains(&wanted) {
            self.log.record(&Record::Iso2NoEnergyMode { wanted, offered: modes });
            return self.end(EndReason::NoCompatibleService);
        }
        self.service_id = res.charge_service.service_id;
        self.next(tx, |m, tx| {
            let selected = [SelectedService { service_id: m.service_id, parameter_set_id: None }];
            let payment = if m.contract { PaymentOption::Contract } else { PaymentOption::ExternalPayment };
            let body = BodyElementChoice::PaymentServiceSelectionReq(PaymentServiceSelectionReq {
                selected_payment_option: payment,
                selected_service_list: SelectedServiceList { selected_service: &selected },
            });
            m.send(body, MsgKind::ServicePaymentSelectionReq, State::AwaitServicePaymentSelectionRes, tx)
        })
    }

    pub(super) fn on_payment_service_selection_res(&mut self, res: PaymentServiceSelectionRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ServicePaymentSelectionRes, res.response_code) {
            return end;
        }
        self.log.record(&Record::ServiceSelected { service_id: self.service_id, parameter_set_id: 0 });
        self.next(tx, |m, tx| if m.contract { m.send_payment_details_req(tx) } else { m.send_authorization_req(tx) })
    }

    /// The authorization request: empty for external payment, with the challenge for a contract.
    pub(super) fn send_authorization_req(&mut self, tx: &mut [u8]) -> Step {
        let body = BodyElementChoice::AuthorizationReq(AuthorizationReq { id: None, gen_challenge: self.challenge });
        let step = self.send(body, MsgKind::AuthorizationReqIso2, State::AwaitContractAuthenticationRes, tx);
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

    pub(super) fn send_payment_details_req(&mut self, tx: &mut [u8]) -> Step {
        // Plug & Charge is added with the contract signing; until then the vehicle cannot pay with one.
        let _ = tx;
        self.end(EndReason::NoCompatibleAuthorization)
    }

    pub(super) fn on_payment_details_res(&mut self, res: PaymentDetailsRes, tx: &mut [u8]) -> Step {
        let _ = tx;
        if let Some(end) = self.received(MsgKind::PaymentDetailsRes, res.response_code) {
            return end;
        }
        self.end(EndReason::UnexpectedMessage)
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
        // The first tuple is the default schedule.
        let Some(SaSchedulesChoice::SaScheduleList(list)) = res.sa_schedules else { return self.end(EndReason::UnexpectedMessage) };
        let Some(tuple) = list.sa_schedule_tuple.first() else { return self.end(EndReason::UnexpectedMessage) };
        let mut schedule = Schedule { id: tuple.sa_schedule_tuple_id, entries: [(0, 0.0); MAX_ENTRIES], len: 0 };
        for entry in tuple.p_max_schedule.p_max_schedule_entry.iter().take(MAX_ENTRIES) {
            let TimeIntervalChoice::Relative(interval) = entry.time_interval;
            schedule.entries[schedule.len] = (interval.start, super::convert::number(entry.p_max));
            schedule.len += 1;
        }
        self.schedule = Some(schedule);
        let step = self.phase_done(Step::default());
        let mut next = if self.stop_requested && !self.power_on {
            self.stop_requested = false;
            self.send_session_stop(tx)
        } else if self.ac {
            // AC has no cable check or pre-charge: power is asked for at once.
            let mut s = self.send_power_delivery(true, tx);
            if s.end.is_none() {
                s.notice = Some(Notice::CableChecked);
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
