//! The handshake, the session setup and everything up to the charge parameters.

use app_protocol::{self, SupportedAppProtocolReq};
use din70121::body::*;
use din70121::def::BodyElementChoice;
use din70121::types::*;

use super::convert::{physical, rational};
use super::{EvseLimits, Machine, MAX_ENTRIES, Schedule, frame};
use crate::ev::{DcConnector, Ev};
use crate::log::{LogSink, MsgKind, Record};
use crate::session::{EndReason, Notice, State, Step};
use crate::{Timer, TimerCmd};
use v2gtp::PayloadType;

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
        // V2G-DC-227: without a negotiated protocol the session is not started.
        if res.response_code == app_protocol::ResponseCode::FailedNoNegotiation
            || res.schema_id != Some(self.config.offer.schema_id)
        {
            return self.end(EndReason::NoCommonProtocol);
        }
        let mac = self.ev.evcc_mac();
        self.session_id = [0; 8];
        self.session_len = 1; // V2G-DC-241: the session id is zero
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
        self.log.record(&Record::SessionSetUp { session_id: id, evse_id: "" });
        let category = self.config.service_category.then_some(ServiceCategory::EvCharging);
        let body = BodyElementChoice::ServiceDiscoveryReq(ServiceDiscoveryReq { service_scope: None, service_category: category });
        let mut step = self.send(body, MsgKind::ServiceDiscoveryReq, State::AwaitServiceDiscoveryRes, tx);
        step.notice = Some(Notice::SessionSetUp { session_id: id });
        if step.end.is_none() {
            step.timers = step.timers.with(TimerCmd::Cancel(Timer::Setup));
            // The ready-to-charge timer runs from here to the first power delivery.
            step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::ReadyToCharge, ms: self.config.ready_to_charge_timeout_ms });
        }
        step
    }

    /// The energy transfer the vehicle asks for, from its inlet: the extended pins of the
    /// combined inlets (V2G-DC-623), the core pins of a plain one (V2G-DC-624).
    fn requested_transfer(&self) -> EvRequestedEnergyTransfer {
        match self.ev.dc_connector() {
            DcConnector::Core => EvRequestedEnergyTransfer::DcCore,
            DcConnector::Extended | DcConnector::Dual2 | DcConnector::Dual4 => EvRequestedEnergyTransfer::DcExtended,
        }
    }

    pub(super) fn on_service_discovery_res(&mut self, res: ServiceDiscoveryRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ServiceDiscoveryRes, res.response_code) {
            return end;
        }
        // V2G-DC-252: only external payment is used.
        if !res.payment_options.payment_option.contains(&PaymentOption::ExternalPayment) {
            self.log.record(&Record::DinNoPayment { offered: res.payment_options.payment_option });
            return self.end(EndReason::NoCompatibleAuthorization);
        }
        let wanted = self.requested_transfer();
        let offered = res.charge_service.energy_transfer_type;
        let matches = matches!(
            (wanted, offered),
            (EvRequestedEnergyTransfer::DcCore, EvseSupportedEnergyTransfer::DcCore)
                | (EvRequestedEnergyTransfer::DcExtended, EvseSupportedEnergyTransfer::DcExtended)
        );
        if !matches {
            self.log.record(&Record::DinNoEnergyService { wanted, offered });
            return self.end(EndReason::NoCompatibleService);
        }
        self.service_id = res.charge_service.service_tag.service_id;
        self.next(tx, |m, tx| {
            let selected = [SelectedService { service_id: m.service_id, parameter_set_id: None }];
            let body = BodyElementChoice::ServicePaymentSelectionReq(ServicePaymentSelectionReq {
                selected_payment_option: PaymentOption::ExternalPayment,
                selected_service_list: SelectedServiceList { selected_service: &selected },
            });
            m.send(body, MsgKind::ServicePaymentSelectionReq, State::AwaitServicePaymentSelectionRes, tx)
        })
    }

    pub(super) fn on_service_payment_selection_res(&mut self, res: ServicePaymentSelectionRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ServicePaymentSelectionRes, res.response_code) {
            return end;
        }
        self.log.record(&Record::ServiceSelected { service_id: self.service_id, parameter_set_id: 0 });
        self.next(tx, |m, tx| m.send_contract_authentication_req(tx))
    }

    /// The same request every time: V2G-DC-492 asks for an identical repeat.
    pub(super) fn send_contract_authentication_req(&mut self, tx: &mut [u8]) -> Step {
        let body = BodyElementChoice::ContractAuthenticationReq(ContractAuthenticationReq { id: None, gen_challenge: None });
        let step = self.send(body, MsgKind::ContractAuthenticationReq, State::AwaitContractAuthenticationRes, tx);
        let ms = self.config.ongoing_timeout_ms;
        self.time_phase(step, ms)
    }

    pub(super) fn on_contract_authentication_res(&mut self, res: ContractAuthenticationRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ContractAuthenticationRes, res.response_code) {
            return end;
        }
        match res.evse_processing {
            EvseProcessing::Ongoing => self.pause_then_repeat(tx),
            EvseProcessing::Finished => {
                let step = self.phase_done(Step::default());
                let mut next = self.next(tx, |m, tx| m.send_charge_parameter_discovery_req(tx));
                next.timers = merge(step.timers, next.timers);
                if next.end.is_none() {
                    next.notice = Some(Notice::Authorized);
                }
                next
            }
        }
    }

    /// The vehicle's limits and needs (the charge parameters of 9.5.3.3).
    pub(super) fn send_charge_parameter_discovery_req(&mut self, tx: &mut [u8]) -> Step {
        let limits = self.ev.dc_limits();
        self.ev_announced = Some(limits);
        let schedule = self.ev.schedule_request();
        let soc = self.ev.present_soc().unwrap_or(0).min(100) as i8;
        let status = self.ev_status(soc);
        let parameter = DcEvChargeParameter {
            dc_ev_status: status,
            ev_maximum_current_limit: physical(limits.maximum_charge_current, UnitSymbol::A),
            ev_maximum_power_limit: Some(physical(limits.maximum_charge_power, UnitSymbol::W)),
            ev_maximum_voltage_limit: physical(limits.maximum_voltage, UnitSymbol::V),
            ev_energy_capacity: Some(physical(schedule.maximum_energy, UnitSymbol::Wh)),
            ev_energy_request: Some(physical(schedule.target_energy, UnitSymbol::Wh)),
            full_soc: Some(100),
            bulk_soc: schedule.target_soc.map(|t| t.min(100) as i8),
        };
        let body = BodyElementChoice::ChargeParameterDiscoveryReq(ChargeParameterDiscoveryReq {
            ev_requested_energy_transfer_type: self.requested_transfer(),
            ev_charge_parameter: EvChargeParameterChoice::Dc(parameter),
        });
        let step = self.send(body, MsgKind::ChargeParameterDiscoveryReq, State::AwaitChargeParameterDiscoveryRes, tx);
        let ms = self.config.ongoing_timeout_ms;
        self.time_phase(step, ms)
    }

    pub(super) fn on_charge_parameter_discovery_res(&mut self, res: ChargeParameterDiscoveryRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ChargeParameterDiscoveryRes, res.response_code) {
            return end;
        }
        // V2G-DC-493: while the charger is still working its values are ignored.
        if res.evse_processing == EvseProcessing::Ongoing {
            return self.pause_then_repeat(tx);
        }
        let EvseChargeParameterChoice::Dc(p) = res.evse_charge_parameter else {
            return self.end(EndReason::UnexpectedMessage);
        };
        self.evse = Some(EvseLimits {
            max_current: rational(p.evse_maximum_current_limit),
            max_power: p.evse_maximum_power_limit.map(rational),
            max_voltage: rational(p.evse_maximum_voltage_limit),
        });
        // V2G-DC-319, V2G-DC-320: the first tuple is the default schedule.
        let SaSchedulesChoice::SaScheduleList(list) = res.sa_schedules;
        let Some(tuple) = list.sa_schedule_tuple.first() else { return self.end(EndReason::UnexpectedMessage) };
        let mut schedule = Schedule { id: tuple.sa_schedule_tuple_id, entries: [(0, 0); MAX_ENTRIES], len: 0 };
        for entry in tuple.p_max_schedule.p_max_schedule_entry.iter().take(MAX_ENTRIES) {
            let TimeIntervalChoice::Relative(interval) = entry.time_interval;
            schedule.entries[schedule.len] = (interval.start, entry.p_max);
            schedule.len += 1;
        }
        self.schedule = Some(schedule);
        let step = self.phase_done(Step::default());
        // V2G-DC-501: the control pilot goes to state C before the cable check.
        let mut next = if self.stop_requested && !self.power_on {
            self.stop_requested = false;
            self.send_session_stop(tx)
        } else {
            self.change_control_pilot(crate::session::ControlPilot::C, State::AwaitControlPilotC)
        };
        next.timers = merge(step.timers, next.timers);
        next
    }
}

/// The timer commands of two steps in one (a step carries at most three).
pub(super) fn merge(a: crate::Timers, b: crate::Timers) -> crate::Timers {
    let mut out = a;
    for cmd in b.iter() {
        out = out.with(cmd);
    }
    out
}
