//! Cable check, pre-charge, power delivery, the DC current demand loop and the AC charging status
//! loop, and the shutdown.

use iso15118_2::body::*;
use iso15118_2::types::*;
use iso15118_20::common_types::RationalNumber;

use super::convert::{number, physical, rational};
use super::Machine;
use crate::ev::Iso2Vehicle;
use crate::log::{LogSink, MsgKind};
use crate::session::{ChargeLoopStatus, ControlPilot, EndReason, Notice, State, Step};
use crate::{Timer, TimerCmd, Timers};

/// The timer commands of two steps in one (a step carries at most three).
pub(super) fn merge(a: Timers, b: Timers) -> Timers {
    let mut out = a;
    for cmd in b.iter() {
        out = out.with(cmd);
    }
    out
}

fn n(r: RationalNumber) -> f64 {
    r.to_f64()
}

fn lower(a: RationalNumber, b: RationalNumber) -> RationalNumber {
    if n(b) < n(a) { b } else { a }
}

impl<E: Iso2Vehicle, L: LogSink> Machine<E, L> {
    pub(super) fn ev_status(&self, soc: i8) -> DcEvStatus {
        DcEvStatus { ev_ready: true, ev_error_code: DcEvErrorCode::NoError, evresssoc: soc }
    }

    fn status(&self) -> DcEvStatus {
        self.ev_status(self.ev.present_soc().unwrap_or(0).min(100) as i8)
    }

    pub(super) fn change_control_pilot(&mut self, to: ControlPilot, next: State) -> Step {
        if to == ControlPilot::B {
            self.pilot.set_ready(false);
            self.pilot.set_stopping(true);
        }
        self.goto(next);
        Step {
            notice: Some(Notice::ControlPilot(to)),
            timers: Timers::one(TimerCmd::Arm { timer: Timer::Sequence, ms: self.config.sequence_performance_ms }),
            ..Step::default()
        }
    }

    pub(super) fn send_cable_check_req(&mut self, tx: &mut [u8]) -> Step {
        let body = BodyElementChoice::CableCheckReq(CableCheckReq { dc_ev_status: self.status() });
        let step = self.send(body, MsgKind::CableCheckReq, State::AwaitCableCheckRes, tx);
        let ms = self.config.cable_check_timeout_ms;
        self.time_phase(step, ms)
    }

    pub(super) fn on_cable_check_res(&mut self, res: CableCheckRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::CableCheckRes, res.response_code) {
            return end;
        }
        if res.evse_processing != EvseProcessing::Finished {
            return self.pause_then_repeat(tx);
        }
        let step = self.phase_done(Step::default());
        let mut next = self.next(tx, |m, tx| m.send_precharge_req(tx));
        next.timers = merge(step.timers, next.timers);
        if next.end.is_none() {
            next.notice = Some(Notice::CableChecked);
        }
        next
    }

    pub(super) fn send_precharge_req(&mut self, tx: &mut [u8]) -> Step {
        let minimum = self.ev.dc_limits().minimum_charge_current;
        let current = if n(minimum) > 0.0 { minimum } else { RationalNumber { exponent: 0, value: 1 } };
        let body = BodyElementChoice::PreChargeReq(PreChargeReq {
            dc_ev_status: self.status(),
            ev_target_voltage: physical(self.ev.battery_voltage(), UnitSymbol::V),
            ev_target_current: physical(current, UnitSymbol::A),
        });
        let step = self.send(body, MsgKind::PreChargeReq, State::AwaitPreChargeRes, tx);
        let ms = self.config.precharge_timeout_ms;
        self.time_phase(step, ms)
    }

    pub(super) fn on_precharge_res(&mut self, res: PreChargeRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::PreChargeRes, res.response_code) {
            return end;
        }
        self.last_inlet = self.ev.inlet_voltage();
        let off = (n(self.last_inlet) - n(self.ev.battery_voltage())).abs();
        if off >= f64::from(self.config.precharge_tolerance_v) {
            return self.pause_then_repeat(tx);
        }
        let mut step = self.phase_done(Step::default());
        if self.stop_requested {
            self.stop_requested = false;
            let next = self.send_session_stop(tx);
            return Step { timers: merge(step.timers, next.timers), ..next };
        }
        self.goto(State::AwaitContactors);
        step.timers = step.timers.with(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Arm { timer: Timer::Sequence, ms: self.config.sequence_performance_ms });
        step.notice = Some(Notice::CloseContactors);
        step
    }

    /// `ChargeProgress` Start or Stop, with the schedule tuple the vehicle follows and a charging
    /// profile within it.
    pub(super) fn send_power_delivery(&mut self, start: bool, tx: &mut [u8]) -> Step {
        self.send_power_delivery_as(if start { ChargeProgress::Start } else { ChargeProgress::Stop }, tx)
    }

    /// `PowerDeliveryReq` with `progress`: Start (with the profile), Stop, or Renegotiate (the
    /// vehicle wants new charge parameters and a new schedule).
    pub(super) fn send_power_delivery_as(&mut self, progress: ChargeProgress, tx: &mut [u8]) -> Step {
        let start = progress == ChargeProgress::Start;
        let mut entries = [ProfileEntry { charging_profile_entry_start: 0, charging_profile_entry_max_power: physical(RationalNumber { exponent: 0, value: 0 }, UnitSymbol::W), charging_profile_entry_max_number_of_phases_in_use: None }; super::MAX_ENTRIES];
        let mut profile_len = None;
        let mut tuple = 1;
        if let Some(schedule) = self.schedule {
            tuple = schedule.id;
            if start && self.config.send_charging_profile {
                let ev_power = if self.ac { f64::INFINITY } else { n(self.ev.dc_limits().maximum_charge_power) };
                for (i, (at, watts)) in schedule.entries[..schedule.len].iter().enumerate() {
                    let phases = match self.config.mode {
                        super::Mode::AcThreePhase => Some(3),
                        super::Mode::AcSinglePhase => Some(1),
                        super::Mode::Dc => None,
                    };
                    entries[i] = ProfileEntry {
                        charging_profile_entry_start: *at,
                        charging_profile_entry_max_power: physical(RationalNumber::from_f64(watts.min(ev_power)), UnitSymbol::W),
                        charging_profile_entry_max_number_of_phases_in_use: phases,
                    };
                }
                profile_len = Some(schedule.len);
            }
        }
        let stopping = progress == ChargeProgress::Stop;
        let status = if stopping { DcEvStatus { ev_ready: false, ..self.status() } } else { self.status() };
        let parameter = (!self.ac).then_some(EvPowerDeliveryParameterChoice::Dc(DcEvPowerDeliveryParameter {
            dc_ev_status: status,
            bulk_charging_complete: None,
            charging_complete: stopping,
        }));
        let body = BodyElementChoice::PowerDeliveryReq(PowerDeliveryReq {
            charge_progress: progress,
            sa_schedule_tuple_id: tuple,
            charging_profile: profile_len.map(|len| ChargingProfile { profile_entry: &entries[..len] }),
            ev_power_delivery_parameter: parameter,
        });
        if start {
            self.power_on = true;
        }
        let next = if start { State::AwaitPowerDeliveryRes } else { State::AwaitPowerDeliveryStopRes };
        // `Start` after a renegotiation is not a new start of the session.
        self.send(body, MsgKind::PowerDeliveryReq, next, tx)
    }

    pub(super) fn on_power_delivery_start_res(&mut self, res: PowerDeliveryRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::PowerDeliveryRes, res.response_code) {
            return end;
        }
        self.note_status(&res.evse_status);
        self.ready = true;
        self.renegotiating = false;
        let mut step = if self.ac { self.send_charging_status_req(tx) } else { self.send_current_demand_req(tx) };
        if step.end.is_none() {
            step.timers = step.timers.with(TimerCmd::Cancel(Timer::ReadyToCharge));
            step.notice = Some(Notice::EnergyTransferStarted);
        }
        step
    }

    /// The charger asks the vehicle to stop in a status, or has a fault.
    fn note_status(&mut self, status: &EvseStatusChoice) {
        let (notification, stop) = match status {
            EvseStatusChoice::Dc(s) => (
                s.evse_notification,
                matches!(s.evse_status_code, DcEvseStatusCode::EvseShutdown | DcEvseStatusCode::EvseEmergencyShutdown | DcEvseStatusCode::EvseMalfunction),
            ),
            EvseStatusChoice::Ac(s) => (s.evse_notification, s.rcd),
        };
        if stop || notification == EvseNotification::StopCharging {
            self.evse_stops = true;
        }
        if notification == EvseNotification::ReNegotiation {
            self.renegotiate_requested = true;
        }
    }

    pub(super) fn send_current_demand_req(&mut self, tx: &mut [u8]) -> Step {
        let mut limits = self.ev.dc_loop_limits();
        if self.config.enforce_cpd_limits
            && let Some(a) = self.ev_announced {
                limits.maximum_charge_power = lower(limits.maximum_charge_power, a.maximum_charge_power);
                limits.maximum_charge_current = lower(limits.maximum_charge_current, a.maximum_charge_current);
                limits.maximum_voltage = lower(limits.maximum_voltage, a.maximum_voltage);
            }
        let battery = n(self.ev.battery_voltage()).max(1.0);
        let mut current = n(limits.maximum_charge_current).min(n(limits.maximum_charge_power) / battery);
        let mut voltage = n(limits.maximum_voltage).min(battery + 5.0);
        if let Some(evse) = self.evse {
            current = current.min(n(evse.max_current));
            voltage = voltage.min(n(evse.max_voltage));
            if let Some(power) = evse.max_power {
                current = current.min(n(power) / battery);
            }
        }
        let body = BodyElementChoice::CurrentDemandReq(CurrentDemandReq {
            dc_ev_status: self.status(),
            ev_target_current: physical(RationalNumber::from_f64(current.max(0.0)), UnitSymbol::A),
            ev_maximum_voltage_limit: Some(physical(limits.maximum_voltage, UnitSymbol::V)),
            ev_maximum_current_limit: Some(physical(limits.maximum_charge_current, UnitSymbol::A)),
            ev_maximum_power_limit: Some(physical(limits.maximum_charge_power, UnitSymbol::W)),
            bulk_charging_complete: None,
            charging_complete: false,
            remaining_time_to_full_so_c: None,
            remaining_time_to_bulk_so_c: None,
            ev_target_voltage: physical(RationalNumber::from_f64(voltage.max(0.0)), UnitSymbol::V),
        });
        self.send(body, MsgKind::CurrentDemandReq, State::AwaitCurrentDemandRes, tx)
    }

    pub(super) fn on_current_demand_res(&mut self, res: CurrentDemandRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::CurrentDemandRes, res.response_code) {
            return end;
        }
        self.note_status(&EvseStatusChoice::Dc(res.dc_evse_status));
        let announced = self.evse;
        let zero = RationalNumber { exponent: 0, value: 0 };
        let status = ChargeLoopStatus {
            present_current: rational(res.evse_present_current),
            present_voltage: rational(res.evse_present_voltage),
            power_limit_achieved: res.evse_power_limit_achieved,
            current_limit_achieved: res.evse_current_limit_achieved,
            voltage_limit_achieved: res.evse_voltage_limit_achieved,
            maximum_charge_power: res.evse_maximum_power_limit.map(rational).or(announced.and_then(|a| a.max_power)).unwrap_or(zero),
            maximum_charge_current: res.evse_maximum_current_limit.map(rational).or(announced.map(|a| a.max_current)).unwrap_or(zero),
            maximum_voltage: res.evse_maximum_voltage_limit.map(rational).or(announced.map(|a| a.max_voltage)).unwrap_or(zero),
            meter: None,
            mobility_needs: None,
        };
        if res.receipt_required == Some(true) && let Some(meter) = res.meter_info {
            return self.answer_with_receipt(meter, res.sa_schedule_tuple_id, status, tx);
        }
        self.after_loop_answer(status)
    }

    /// AC: asks the charger for its status; sent in a cycle.
    pub(super) fn send_charging_status_req(&mut self, tx: &mut [u8]) -> Step {
        let body = BodyElementChoice::ChargingStatusReq(ChargingStatusReq {});
        self.send(body, MsgKind::ChargingStatusReq, State::AwaitChargingStatusRes, tx)
    }

    pub(super) fn on_charging_status_res(&mut self, res: ChargingStatusRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::ChargingStatusRes, res.response_code) {
            return end;
        }
        self.note_status(&EvseStatusChoice::Ac(res.ac_evse_status));
        let zero = RationalNumber { exponent: 0, value: 0 };
        let max_current = res.evse_max_current.map(rational).or(self.evse.map(|e| e.max_current)).unwrap_or(zero);
        // The status carries no measurements: the vehicle assumes it gets the lower of the
        // charger's and its own maximum current on every phase, at the nominal voltage (so
        // that the current is the sum over the phases, and voltage times current the power).
        let phases = if self.config.mode == crate::iso2::Mode::AcThreePhase { 3.0 } else { 1.0 };
        let current = RationalNumber::from_f64(max_current.to_f64().min(self.ev.ac_loop_limits().maximum_current.to_f64()) * phases);
        let status = ChargeLoopStatus {
            present_current: current,
            present_voltage: self.evse.map_or(zero, |e| e.max_voltage),
            power_limit_achieved: false,
            current_limit_achieved: false,
            voltage_limit_achieved: false,
            maximum_charge_power: zero,
            maximum_charge_current: max_current,
            maximum_voltage: self.evse.map_or(zero, |e| e.max_voltage),
            meter: None,
            mobility_needs: None,
        };
        if res.receipt_required == Some(true) && let Some(meter) = res.meter_info {
            return self.answer_with_receipt(meter, res.sa_schedule_tuple_id, status, tx);
        }
        self.after_loop_answer(status)
    }

    /// A loop answer is in: stop if the vehicle or the charger wants to, else wait for the next round.
    fn after_loop_answer(&mut self, status: ChargeLoopStatus) -> Step {
        if let Some(step) = self.loop_exit() {
            return step;
        }
        let mut step = self.next_round();
        step.notice = Some(Notice::ChargeLoop(status));
        step
    }

    /// The pause before the next request of the loop.
    fn next_round(&mut self) -> Step {
        self.poll_pending = true;
        Step {
            timers: Timers::one(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Arm { timer: Timer::Poll, ms: self.config.charge_loop_interval_ms }),
            ..Step::default()
        }
    }

    /// The loop ends here when the vehicle or the charger wants to stop, or a renegotiation is due.
    fn loop_exit(&mut self) -> Option<Step> {
        if self.stop_requested || self.evse_stops {
            self.stop_requested = false;
            return Some(self.begin_stop());
        }
        if self.renegotiate_requested {
            return Some(self.begin_renegotiation());
        }
        None
    }

    /// The host (or `Event::RenegotiateSchedule`) wants new charge parameters: at once between two
    /// requests of the loop, else at the next answer.
    pub(super) fn on_renegotiate_requested(&mut self) -> Step {
        if !self.power_on || self.renegotiating {
            return Step::default();
        }
        self.renegotiate_requested = true;
        if matches!(self.state, State::AwaitCurrentDemandRes | State::AwaitChargingStatusRes) && self.poll_pending {
            return self.begin_renegotiation();
        }
        Step::default()
    }

    /// A renegotiation is a stop of the energy transfer with `ChargeProgress` Renegotiate, and a
    /// new charge parameter discovery; the contactors stay closed.
    fn begin_renegotiation(&mut self) -> Step {
        self.renegotiate_requested = false;
        self.renegotiating = true;
        self.begin_stop()
    }

    /// The charger wants a signed receipt of the meter reading it sent (a contract session): the
    /// reading is copied, signed with the contract key, and sent before the loop goes on.
    fn answer_with_receipt(&mut self, meter: MeterInfo<'_>, tuple: u8, status: ChargeLoopStatus, tx: &mut [u8]) -> Step {
        if let Some(step) = self.loop_exit() {
            return step;
        }
        let framed = match self.ev.contract().filter(|_| self.contract) {
            Some(contract) => super::setup::frame_signed_receipt(contract, &self.session_id[..self.session_len], tuple, meter, tx),
            // Without a contract there is nothing to sign with: the receipt is not sent.
            None => return self.after_loop_answer(status),
        };
        let mut step = self.sent(framed, MsgKind::MeteringReceiptReq, State::AwaitMeteringReceiptRes);
        step.notice = Some(Notice::ChargeLoop(status));
        step
    }

    pub(super) fn begin_stop(&mut self) -> Step {
        self.poll_pending = false;
        self.pilot.set_stopping(true);
        self.goto(State::AwaitCurrentStopped);
        Step {
            notice: Some(Notice::StopEnergyTransfer { within_s: None }),
            timers: Timers::one(TimerCmd::Cancel(Timer::Poll)).with(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Arm { timer: Timer::Sequence, ms: self.config.sequence_performance_ms }),
            ..Step::default()
        }
    }

    pub(super) fn on_power_delivery_stop_res(&mut self, res: PowerDeliveryRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::PowerDeliveryRes, res.response_code) {
            return end;
        }
        if self.renegotiating {
            // The contactors stay closed: new parameters, then the power again.
            self.phase_timed = false;
            return self.send_charge_parameter_discovery_req(tx);
        }
        if self.ac {
            // No contactors to open or control pilot to change for the vehicle in AC.
            return self.send_session_stop(tx);
        }
        self.goto(State::AwaitContactorsOpened);
        Step {
            notice: Some(Notice::OpenContactors),
            timers: Timers::one(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Arm { timer: Timer::Sequence, ms: self.config.sequence_performance_ms }),
            ..Step::default()
        }
    }

    pub(super) fn after_stop(&mut self, tx: &mut [u8]) -> Step {
        if self.config.welding_detection { self.send_welding_req(tx) } else { self.send_session_stop(tx) }
    }

    pub(super) fn send_welding_req(&mut self, tx: &mut [u8]) -> Step {
        let status = DcEvStatus { ev_ready: false, ..self.status() };
        let body = BodyElementChoice::WeldingDetectionReq(WeldingDetectionReq { dc_ev_status: status });
        let step = self.send(body, MsgKind::WeldingDetectionReq, State::AwaitWeldingDetectionRes, tx);
        let ms = self.config.ongoing_timeout_ms;
        self.time_phase(step, ms)
    }

    pub(super) fn on_welding_detection_res(&mut self, res: WeldingDetectionRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::WeldingDetectionRes, res.response_code) {
            return end;
        }
        if number(res.evse_present_voltage) >= f64::from(self.config.welding_detection_voltage_v) {
            return self.pause_then_repeat(tx);
        }
        let step = self.phase_done(Step::default());
        let next = self.send_session_stop(tx);
        Step { timers: merge(step.timers, next.timers), ..next }
    }

    /// The vehicle ends the session for a non-critical reason of its own (nothing fits its needs, say) before the power
    /// is on: `SessionStopReq` as the next request, then the answer (V2G-DC-648), and only then the end with this reason.
    pub(super) fn end_politely(&mut self, reason: EndReason, tx: &mut [u8]) -> Step {
        self.pending_end = Some(reason);
        self.phase_timed = false;
        self.send_session_stop(tx)
    }

    pub(super) fn send_session_stop(&mut self, tx: &mut [u8]) -> Step {
        self.pilot.set_stopping(true);
        let session = if self.pausing { ChargingSession::Pause } else { ChargingSession::Terminate };
        let body = BodyElementChoice::SessionStopReq(SessionStopReq { charging_session: session });
        self.send(body, MsgKind::SessionStopReq, State::AwaitSessionStopRes, tx)
    }

    pub(super) fn on_session_stop_res(&mut self, res: SessionStopRes) -> Step {
        if let Some(end) = self.received(MsgKind::SessionStopRes, res.response_code) {
            return end;
        }
        if self.pausing {
            // The id and the service of the session are what resuming it needs.
            let resume = crate::session::Resume {
                session_id: self.session_id,
                selected: iso15118_20::common::SelectedService { service_id: self.service_id, parameter_set_id: 0 },
                renegotiation_supported: None,
            };
            return self.end(EndReason::Paused(resume));
        }
        self.end(EndReason::Completed)
    }

    /// The charger took the receipt: the loop goes on (or ends, if it was to).
    pub(super) fn on_metering_receipt_res(&mut self, res: MeteringReceiptRes, _tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::MeteringReceiptRes, res.response_code) {
            return end;
        }
        if let Some(step) = self.loop_exit() {
            return step;
        }
        // Back in the loop: the next round repeats its request.
        self.goto(if self.ac { State::AwaitChargingStatusRes } else { State::AwaitCurrentDemandRes });
        self.next_round()
    }
}
