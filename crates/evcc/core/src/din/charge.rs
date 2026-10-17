//! The cable check, pre-charge, power delivery, the current demand loop and the shutdown.

use din70121::body::*;
use din70121::def::BodyElementChoice;
use din70121::types::*;
use iso15118_20::common_types::RationalNumber;

use super::convert::{number, physical, pmax, rational};
use super::setup::merge;
use super::Machine;
use crate::ev::Ev;
use crate::log::{LogSink, MsgKind};
use crate::session::{ChargeLoopStatus, ControlPilot, EndReason, Notice, State, Step};
use crate::{Timer, TimerCmd, Timers};

impl<E: Ev, L: LogSink> Machine<E, L> {
    /// The status of the vehicle in a request (9.5.3.2): ready, no error, the state of charge.
    pub(super) fn ev_status(&self, soc: i8) -> DcEvStatus {
        DcEvStatus { ev_ready: true, ev_cabin_conditioning: None, evress_conditioning: None, ev_error_code: DcEvErrorCode::NoError, evresssoc: soc }
    }

    fn status(&self) -> DcEvStatus {
        self.ev_status(self.ev.present_soc().unwrap_or(0).min(100) as i8)
    }

    /// Asks the host to change the control pilot; the next step follows its answer.
    pub(super) fn change_control_pilot(&mut self, to: ControlPilot, next: State) -> Step {
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
        match res.evse_processing {
            EvseProcessing::Ongoing => self.pause_then_repeat(tx),
            EvseProcessing::Finished => {
                let step = self.phase_done(Step::default());
                let mut next = self.next(tx, |m, tx| m.send_precharge_req(tx));
                next.timers = merge(step.timers, next.timers);
                if next.end.is_none() {
                    next.notice = Some(Notice::CableChecked);
                }
                next
            }
        }
    }

    /// Asks for the battery voltage at a small current; sent again until the voltage at the inlet
    /// has followed (V2G-DC-421, V2G-DC-422).
    pub(super) fn send_precharge_req(&mut self, tx: &mut [u8]) -> Step {
        let limits = self.ev.dc_limits();
        let minimum = limits.minimum_charge_current;
        let current = if number_of(minimum) > 0.0 { minimum } else { RationalNumber { exponent: 0, value: 1 } };
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
        // The vehicle's own measurement at its inlet decides, not the charger's report.
        self.last_inlet = self.ev.inlet_voltage();
        let battery = self.ev.battery_voltage();
        let off = (number_of(self.last_inlet) - number_of(battery)).abs();
        if off >= f64::from(self.config.precharge_tolerance_v) {
            return self.pause_then_repeat(tx);
        }
        // Done: the contactors close, then the charger is asked for power (V2G-DC-421).
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

    /// `ReadyToChargeState` true starts the energy transfer, false stops it.
    pub(super) fn send_power_delivery(&mut self, start: bool, tx: &mut [u8]) -> Step {
        let status = if start {
            self.status()
        } else {
            DcEvStatus { ev_ready: false, ..self.status() }
        };
        let mut profile_entries = [ProfileEntry { charging_profile_entry_start: 0, charging_profile_entry_max_power: 0 }; super::MAX_ENTRIES];
        let mut profile = None;
        if start && self.config.send_charging_profile
            && let Some(schedule) = self.schedule {
                // The profile follows the schedule, within what the vehicle can take.
                let ev_power = number_of(self.ev.dc_limits().maximum_charge_power);
                for (i, (start, pmax_w)) in schedule.entries[..schedule.len].iter().enumerate() {
                    profile_entries[i] = ProfileEntry {
                        charging_profile_entry_start: *start,
                        charging_profile_entry_max_power: (*pmax_w).min(pmax(ev_power)),
                    };
                }
                profile = Some((schedule.id, schedule.len));
            }
        let body = BodyElementChoice::PowerDeliveryReq(PowerDeliveryReq {
            ready_to_charge_state: start,
            charging_profile: profile.map(|(id, len)| ChargingProfile { sa_schedule_tuple_id: id, profile_entry: &profile_entries[..len] }),
            // V2G-DC-558: the parameters of the vehicle are sent.
            ev_power_delivery_parameter: Some(EvPowerDeliveryParameterChoice::Dc(DcEvPowerDeliveryParameter {
                dc_ev_status: status,
                bulk_charging_complete: None,
                charging_complete: !start,
            })),
        });
        let next = if start { State::AwaitPowerDeliveryRes } else { State::AwaitPowerDeliveryStopRes };
        if start {
            self.power_on = true;
        }
        self.send(body, MsgKind::PowerDeliveryReq, next, tx)
    }

    pub(super) fn on_power_delivery_start_res(&mut self, res: PowerDeliveryRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::PowerDeliveryRes, res.response_code) {
            return end;
        }
        if let EvseStatusChoice::Dc(status) = res.evse_status {
            self.note_evse_status(&status);
        }
        self.ready = true;
        let mut step = self.send_current_demand_req(tx);
        if step.end.is_none() {
            step.timers = step.timers.with(TimerCmd::Cancel(Timer::ReadyToCharge));
            step.notice = Some(Notice::EnergyTransferStarted);
        }
        step
    }

    /// The charger asks the vehicle to stop in its status (V2G-DC-650): the vehicle stops as if
    /// it wanted to itself.
    fn note_evse_status(&mut self, status: &DcEvseStatus) {
        if matches!(
            status.evse_status_code,
            DcEvseStatusCode::EvseShutdown | DcEvseStatusCode::EvseEmergencyShutdown | DcEvseStatusCode::EvseMalfunction
        ) || status.evse_notification == EvseNotification::StopCharging
        {
            self.evse_stops = true;
        }
    }

    pub(super) fn send_current_demand_req(&mut self, tx: &mut [u8]) -> Step {
        let mut limits = self.ev.dc_loop_limits();
        // V2G-DC-641: nothing above what the charge parameter discovery announced.
        if self.config.enforce_cpd_limits
            && let Some(announced) = self.ev_announced {
                limits.maximum_charge_power = lower(limits.maximum_charge_power, announced.maximum_charge_power);
                limits.maximum_charge_current = lower(limits.maximum_charge_current, announced.maximum_charge_current);
                limits.maximum_voltage = lower(limits.maximum_voltage, announced.maximum_voltage);
            }
        // The current asked for: what the vehicle, the charger and the power allow.
        let battery = number_of(self.ev.battery_voltage()).max(1.0);
        let mut current = number_of(limits.maximum_charge_current).min(number_of(limits.maximum_charge_power) / battery);
        // The voltage the vehicle wants at its inlet: the battery's, with a little more so that the
        // charger is limited by the current, within the vehicle's maximum.
        let mut voltage = number_of(limits.maximum_voltage).min(battery + 5.0);
        if let Some(evse) = self.evse {
            current = current.min(number_of(evse.max_current));
            voltage = voltage.min(number_of(evse.max_voltage));
            if let Some(power) = evse.max_power {
                current = current.min(number_of(power) / battery);
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

    pub(super) fn on_current_demand_res(&mut self, res: CurrentDemandRes) -> Step {
        if let Some(end) = self.received(MsgKind::CurrentDemandRes, res.response_code) {
            return end;
        }
        self.note_evse_status(&res.dc_evse_status);
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
        };
        if self.stop_requested || self.evse_stops {
            // The notice that stops the energy transfer takes the place of the status of this answer.
            self.stop_requested = false;
            return self.begin_stop();
        }
        // The next request after the pause of the charge loop; the host may ask to stop in it.
        self.poll_pending = true;
        Step {
            notice: Some(Notice::ChargeLoop(status)),
            timers: Timers::one(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Arm { timer: Timer::Poll, ms: self.config.charge_loop_interval_ms }),
            ..Step::default()
        }
    }

    /// The host stops drawing current; `PowerDeliveryReq(false)` follows (V2G-DC-673).
    pub(super) fn begin_stop(&mut self) -> Step {
        self.poll_pending = false;
        self.goto(State::AwaitCurrentStopped);
        Step {
            notice: Some(Notice::StopEnergyTransfer { within_s: None }),
            timers: Timers::one(TimerCmd::Cancel(Timer::Poll)).with(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Arm { timer: Timer::Sequence, ms: self.config.sequence_performance_ms }),
            ..Step::default()
        }
    }

    pub(super) fn on_power_delivery_stop_res(&mut self, res: PowerDeliveryRes, _tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::PowerDeliveryRes, res.response_code) {
            return end;
        }
        // The vehicle opens its contactors, then changes the control pilot to B (V2G-DC-502).
        self.goto(State::AwaitContactorsOpened);
        Step {
            notice: Some(Notice::OpenContactors),
            timers: Timers::one(TimerCmd::Cancel(Timer::Response)).with(TimerCmd::Arm { timer: Timer::Sequence, ms: self.config.sequence_performance_ms }),
            ..Step::default()
        }
    }

    /// With the control pilot at B: welding detection, if the vehicle does it, then the stop.
    pub(super) fn after_stop(&mut self, tx: &mut [u8]) -> Step {
        if self.config.welding_detection {
            self.send_welding_req(tx)
        } else {
            self.send_session_stop(tx)
        }
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
        // The charger's disabled output has discharged: the contactors are not welded
        // (V2G-DC-430); until then ask again (V2G-DC-431).
        if number(res.evse_present_voltage) >= f64::from(self.config.welding_detection_voltage_v) {
            return self.pause_then_repeat(tx);
        }
        let step = self.phase_done(Step::default());
        let next = self.send_session_stop(tx);
        Step { timers: merge(step.timers, next.timers), ..next }
    }

    pub(super) fn send_session_stop(&mut self, tx: &mut [u8]) -> Step {
        let body = BodyElementChoice::SessionStopReq(SessionStop {});
        self.send(body, MsgKind::SessionStopReq, State::AwaitSessionStopRes, tx)
    }

    pub(super) fn on_session_stop_res(&mut self, res: SessionStopRes) -> Step {
        if let Some(end) = self.received(MsgKind::SessionStopRes, res.response_code) {
            return end;
        }
        self.end(EndReason::Completed)
    }
}

fn number_of(r: RationalNumber) -> f64 {
    r.to_f64()
}

/// The smaller of two numbers.
fn lower(a: RationalNumber, b: RationalNumber) -> RationalNumber {
    if b.to_f64() < a.to_f64() { b } else { a }
}
