//! Cable check, pre-charge, power delivery start and the charge loop.

use iso15118_20::common::{self, *};
use iso15118_20::common_types::{self, EvseNotification, Processing, ResponseCode};
use iso15118_20::ac;
use iso15118_20::dc;
use v2gtp::PayloadType;

use super::{Machine, frame};
use super::config::*;
use super::types::*;
use crate::ev::{AcDischargePowerLimits, AcPowerLimits, AcPresentPower, Ev, Phases};
use crate::log::{LogSink, MsgKind, Record};
use crate::{Timer, TimerCmd, Timers};

impl<E: Ev, L: LogSink> Machine<E, L> {
    pub(super) fn on_cable_check_res(&mut self, res: dc::DcCableCheckRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::DcCableCheckRes, res.response_code) {
            return end;
        }
        self.log.record(&Record::EvseProcessing { processing: res.evse_processing });
        match res.evse_processing {
            // V2G20-1397: wait for ResponseCode OK and EVSEProcessing Finished.
            Processing::Finished => {
                self.ongoing_armed = false;
                let mut step = self.send_precharge_req(false, tx);
                if step.end.is_none() {
                    step.notice = Some(Notice::CableChecked);
                }
                step
            }
            _ => self.ongoing_repeat(tx),
        }
    }

    pub(super) fn on_precharge_res(&mut self, res: dc::DcPreChargeRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::DcPreChargeRes, res.response_code) {
            return end;
        }
        if self.precharge_finishing {
            // The EVSE acknowledged the end of pre-charge: close the contactors.
            self.precharge_finishing = false;
            self.ongoing_armed = false;
            self.goto(State::AwaitContactors);
            return Step {
                timers: Timers::one(TimerCmd::Cancel(Timer::Response))
                    .with(TimerCmd::Cancel(Timer::Ongoing))
                    .with(self.arm_sequence()),
                notice: Some(Notice::CloseContactors),
                ..Step::default()
            };
        }
        // What the vehicle measures at its inlet; the next request reports it.
        self.last_inlet = self.ev.inlet_voltage();
        let inlet = rational_to_volts(self.last_inlet);
        let battery = rational_to_volts(self.ev.battery_voltage());
        if (inlet - battery).abs() < f32::from(self.config.precharge_tolerance_v) {
            self.precharge_finishing = true;
            return self.send_precharge_req(true, tx);
        }
        self.ongoing_repeat(tx)
    }

    pub(super) fn on_power_delivery_start_res(&mut self, res: PowerDeliveryRes, tx: &mut [u8]) -> Step {
        // V2G20-1438: WARNING_PowerToleranceNotConfirmed is not an error here.
        let tolerated = Some(ResponseCode::WarningPowerToleranceNotConfirmed);
        if let Some(end) = self.received_or(MsgKind::PowerDeliveryRes, res.response_code, tolerated) {
            return end;
        }
        // V2G20-1615/1616: with OK the energy transfer starts; the charge loop
        // begins at once.
        let mut step = self.send_charge_loop_req(tx);
        if step.end.is_none() {
            step.notice = Some(Notice::EnergyTransferStarted);
        }
        step
    }

    pub(super) fn on_charge_loop_res(&mut self, res: dc::DcChargeLoopRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::DcChargeLoopRes, res.response_code) {
            return end;
        }
        // We asked for Dynamic control, with the BPT variant in a BPT session.
        let mode = match (res.cl_res_control_mode, self.config.bpt()) {
            (dc::ClResControlModeChoice::DynamicDc(m), false) => m,
            (dc::ClResControlModeChoice::BptDynamicDc(m), true) => dc::DynamicDcClResControlMode {
                departure_time: m.departure_time,
                minimum_soc: m.minimum_soc,
                target_soc: m.target_soc,
                ack_max_delay: m.ack_max_delay,
                evse_maximum_charge_power: m.evse_maximum_charge_power,
                evse_minimum_charge_power: m.evse_minimum_charge_power,
                evse_maximum_charge_current: m.evse_maximum_charge_current,
                evse_maximum_voltage: m.evse_maximum_voltage,
            },
            _ => return self.end(EndReason::UnexpectedMessage),
        };
        self.check_evse_loop_limits(&mode);
        let status = ChargeLoopStatus {
            present_current: res.evse_present_current,
            present_voltage: res.evse_present_voltage,
            power_limit_achieved: res.evse_power_limit_achieved,
            current_limit_achieved: res.evse_current_limit_achieved,
            voltage_limit_achieved: res.evse_voltage_limit_achieved,
            maximum_charge_power: mode.evse_maximum_charge_power,
            maximum_charge_current: mode.evse_maximum_charge_current,
            maximum_voltage: mode.evse_maximum_voltage,
        };
        self.after_charge_loop_res(res.evse_status, Notice::ChargeLoop(status), tx)
    }

    /// The part of a charge loop answer that is the same in every kind of charge loop: what the
    /// EVSE's notification asks for, or else the next request of the cycle. `notice` is handed to
    /// the host when the loop goes on.
    fn after_charge_loop_res(&mut self, evse_status: Option<common_types::EvseStatus>, notice: Notice, tx: &mut [u8]) -> Step {
        // V2G20-1416, V2G20-1960: Terminate means shut down and stop the session.
        let notification = evse_status.map(|s| (s.evse_notification, s.notification_max_delay));
        if let Some((EvseNotification::Terminate, within_s)) = notification {
            self.stop_kind = StopKind::Terminate;
            return self.begin_stop(Some(within_s));
        }
        if core::mem::take(&mut self.stop_requested) {
            return self.begin_stop(None);
        }
        match notification {
            // V2G20-1651, V2G20-1652, V2G20-1661: when the EVSE offered it, an orderly stop
            // (PowerDeliveryReq Stop, welding detection) and SessionStopReq(ServiceRenegotiation).
            Some((EvseNotification::ServiceRenegotiation, within_s)) if self.renegotiation_supported == Some(true) => {
                self.stop_kind = StopKind::ServiceRenegotiation;
                return self.begin_stop(Some(within_s));
            }
            // V2G20-1850: the EVSE wants a pause. A pause of the whole session (SessionStopReq
            // with Pause) is not implemented; a standby period is the other answer the standard
            // allows, and it keeps the session, the TLS connection and the contactors. The
            // current goes to zero first (V2G20-1194).
            Some((EvseNotification::Pause, within_s)) if self.config.pause_on_notification => {
                self.stop_kind = StopKind::Pause;
                return self.begin_stop(Some(within_s));
            }
            Some((EvseNotification::Pause, within_s)) if !self.standby => {
                self.entering_standby = true;
                return self.ramp_down(Some(within_s));
            }
            // V2G20-1854, V2G20-1393: leave the standby period with PowerDeliveryReq(Start).
            Some((EvseNotification::ExitStandby, _)) if self.standby => {
                self.standby = false;
                return self.send_power_delivery(common::ChargeProgress::Start, tx);
            }
            // V2G20-1052, V2G20-1845: renegotiate the schedule on the side stream, now.
            Some((EvseNotification::ScheduleRenegotiation, _)) => {
                let mut step = self.pace_charge_loop(tx);
                step = self.start_schedule_renegotiation(step, tx);
                step.notice = Some(notice);
                return step;
            }
            // Metering confirmation belongs to Plug & Charge and is ignored.
            _ => {}
        }
        let mut step = self.pace_charge_loop(tx);
        step.notice = Some(notice);
        step
    }

    pub(super) fn on_power_delivery_standby_res(&mut self, res: PowerDeliveryRes, tx: &mut [u8]) -> Step {
        // V2G20-1396: WARNING_StandbyNotAllowed is the EVSE's refusal; the charge goes on.
        let refused = Some(ResponseCode::WarningStandbyNotAllowed);
        if let Some(end) = self.received_or(MsgKind::PowerDeliveryRes, res.response_code, refused) {
            return end;
        }
        self.standby = res.response_code != ResponseCode::WarningStandbyNotAllowed;
        let mut step = self.send_charge_loop_req(tx);
        if step.end.is_none() {
            step.notice = Some(if self.standby { Notice::StandbyEntered } else { Notice::EnergyTransferStarted });
        }
        step
    }

    pub(super) fn send_cable_check_req(&mut self, tx: &mut [u8]) -> Step {
        let msg = dc::DcCableCheckReq { header: self.header() };
        let framed = frame(tx, PayloadType::DC_MAINSTREAM, |out| msg.encode(out));
        if self.state == State::AwaitCableCheckRes {
            return self.sent(MsgKind::DcCableCheckReq, framed);
        }
        let mut step = self.send_then(State::AwaitCableCheckRes, MsgKind::DcCableCheckReq, framed);
        // V2G20-700: the cable check timer starts with the first request.
        if step.end.is_none() {
            self.ongoing_armed = true;
            step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::Ongoing, ms: self.config.cable_check_timeout_ms });
        }
        step
    }
    /// One `DC_PreChargeReq`; `finished` is the EV's `EVProcessing`.
    pub(super) fn send_precharge_req(&mut self, finished: bool, tx: &mut [u8]) -> Step {
        let battery = self.ev.battery_voltage();
        let msg = dc::DcPreChargeReq {
            header: self.header(),
            ev_processing: if finished { Processing::Finished } else { Processing::Ongoing },
            // V2G20-2180: the voltage at the inlet, which follows the charger's output until
            // the contactors are closed; the target is the battery's.
            ev_present_voltage: self.last_inlet,
            ev_target_voltage: battery,
        };
        let framed = frame(tx, PayloadType::DC_MAINSTREAM, |out| msg.encode(out));
        if self.state == State::AwaitPreChargeRes {
            return self.sent(MsgKind::DcPreChargeReq, framed);
        }
        let mut step = self.send_then(State::AwaitPreChargeRes, MsgKind::DcPreChargeReq, framed);
        // V2G20-704: the pre-charge timer starts with the first request (arming replaces the
        // cable check timer).
        if step.end.is_none() {
            self.ongoing_armed = true;
            step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::Ongoing, ms: self.config.precharge_timeout_ms });
        }
        step
    }
    pub(super) fn send_charge_loop_req(&mut self, tx: &mut [u8]) -> Step {
        self.charge_loop_started = true;
        if self.config.ac() {
            return self.send_ac_charge_loop_req(tx);
        }
        let r = self.ev.schedule_request();
        let l = self.loop_limits();
        let control_mode = if self.config.bpt() {
            let d = self.loop_discharge_limits();
            dc::ClReqControlModeChoice::BptDynamicDc(dc::BptDynamicDcClReqControlMode {
                departure_time: None,
                ev_target_energy_request: r.target_energy,
                ev_maximum_energy_request: r.maximum_energy,
                ev_minimum_energy_request: r.minimum_energy,
                ev_maximum_charge_power: l.maximum_charge_power,
                ev_minimum_charge_power: l.minimum_charge_power,
                ev_maximum_charge_current: l.maximum_charge_current,
                ev_maximum_voltage: l.maximum_voltage,
                ev_minimum_voltage: l.minimum_voltage,
                ev_maximum_discharge_power: d.maximum_discharge_power,
                ev_minimum_discharge_power: d.minimum_discharge_power,
                ev_maximum_discharge_current: d.maximum_discharge_current,
                ev_maximum_v2x_energy_request: None,
                ev_minimum_v2x_energy_request: None,
            })
        } else {
            dc::ClReqControlModeChoice::DynamicDc(dc::DynamicDcClReqControlMode {
                departure_time: None,
                ev_target_energy_request: r.target_energy,
                ev_maximum_energy_request: r.maximum_energy,
                ev_minimum_energy_request: r.minimum_energy,
                ev_maximum_charge_power: l.maximum_charge_power,
                ev_minimum_charge_power: l.minimum_charge_power,
                ev_maximum_charge_current: l.maximum_charge_current,
                ev_maximum_voltage: l.maximum_voltage,
                ev_minimum_voltage: l.minimum_voltage,
            })
        };
        let msg = dc::DcChargeLoopReq {
            header: self.header(),
            display_parameters: self.display_parameters(),
            meter_info_requested: false,
            ev_present_voltage: self.ev.battery_voltage(),
            cl_req_control_mode: control_mode,
        };
        let framed = frame(tx, PayloadType::DC_MAINSTREAM, |out| msg.encode(out));
        if self.state == State::AwaitChargeLoopRes {
            return self.sent(MsgKind::DcChargeLoopReq, framed);
        }
        self.send_then(State::AwaitChargeLoopRes, MsgKind::DcChargeLoopReq, framed)
    }
    fn send_ac_charge_loop_req(&mut self, tx: &mut [u8]) -> Step {
        let r = self.ev.schedule_request();
        let l = self.loop_ac_limits();
        let l = AcPowerLimits { maximum_charge_power: self.lines(l.maximum_charge_power), minimum_charge_power: self.lines(l.minimum_charge_power) };
        let p = self.ev.ac_present_power();
        let p = AcPresentPower { active: self.lines(p.active), reactive: self.lines(p.reactive) };
        let control_mode = if self.config.scheduled {
            // The charger's schedule sets the power; the vehicle reports its energy needs, what it
            // can take and what it draws.
            let scheduled = ac::ScheduledAcClReqControlMode {
                ev_target_energy_request: Some(r.target_energy),
                ev_maximum_energy_request: Some(r.maximum_energy),
                ev_minimum_energy_request: Some(r.minimum_energy),
                ev_maximum_charge_power: Some(l.maximum_charge_power.l1),
                ev_maximum_charge_power_l2: l.maximum_charge_power.l2,
                ev_maximum_charge_power_l3: l.maximum_charge_power.l3,
                ev_minimum_charge_power: Some(l.minimum_charge_power.l1),
                ev_minimum_charge_power_l2: l.minimum_charge_power.l2,
                ev_minimum_charge_power_l3: l.minimum_charge_power.l3,
                ev_present_active_power: p.active.l1,
                ev_present_active_power_l2: p.active.l2,
                ev_present_active_power_l3: p.active.l3,
                ev_present_reactive_power: Some(p.reactive.l1),
                ev_present_reactive_power_l2: p.reactive.l2,
                ev_present_reactive_power_l3: p.reactive.l3,
            };
            if self.config.bpt() {
                let d = self.loop_ac_discharge_limits();
                let d = AcDischargePowerLimits { maximum_discharge_power: self.lines(d.maximum_discharge_power), minimum_discharge_power: self.lines(d.minimum_discharge_power) };
                ac::ClReqControlModeChoice::BptScheduledAc(ac::BptScheduledAcClReqControlMode {
                    ev_target_energy_request: scheduled.ev_target_energy_request,
                    ev_maximum_energy_request: scheduled.ev_maximum_energy_request,
                    ev_minimum_energy_request: scheduled.ev_minimum_energy_request,
                    ev_maximum_charge_power: scheduled.ev_maximum_charge_power,
                    ev_maximum_charge_power_l2: scheduled.ev_maximum_charge_power_l2,
                    ev_maximum_charge_power_l3: scheduled.ev_maximum_charge_power_l3,
                    ev_minimum_charge_power: scheduled.ev_minimum_charge_power,
                    ev_minimum_charge_power_l2: scheduled.ev_minimum_charge_power_l2,
                    ev_minimum_charge_power_l3: scheduled.ev_minimum_charge_power_l3,
                    ev_present_active_power: scheduled.ev_present_active_power,
                    ev_present_active_power_l2: scheduled.ev_present_active_power_l2,
                    ev_present_active_power_l3: scheduled.ev_present_active_power_l3,
                    ev_present_reactive_power: scheduled.ev_present_reactive_power,
                    ev_present_reactive_power_l2: scheduled.ev_present_reactive_power_l2,
                    ev_present_reactive_power_l3: scheduled.ev_present_reactive_power_l3,
                    ev_maximum_discharge_power: Some(d.maximum_discharge_power.l1),
                    ev_maximum_discharge_power_l2: d.maximum_discharge_power.l2,
                    ev_maximum_discharge_power_l3: d.maximum_discharge_power.l3,
                    ev_minimum_discharge_power: Some(d.minimum_discharge_power.l1),
                    ev_minimum_discharge_power_l2: d.minimum_discharge_power.l2,
                    ev_minimum_discharge_power_l3: d.minimum_discharge_power.l3,
                })
            } else {
                ac::ClReqControlModeChoice::ScheduledAc(scheduled)
            }
        } else if self.config.bpt() {
            let d = self.loop_ac_discharge_limits();
            let d = AcDischargePowerLimits { maximum_discharge_power: self.lines(d.maximum_discharge_power), minimum_discharge_power: self.lines(d.minimum_discharge_power) };
            ac::ClReqControlModeChoice::BptDynamicAc(ac::BptDynamicAcClReqControlMode {
                departure_time: None,
                ev_target_energy_request: r.target_energy,
                ev_maximum_energy_request: r.maximum_energy,
                ev_minimum_energy_request: r.minimum_energy,
                ev_maximum_charge_power: l.maximum_charge_power.l1,
                ev_maximum_charge_power_l2: l.maximum_charge_power.l2,
                ev_maximum_charge_power_l3: l.maximum_charge_power.l3,
                ev_minimum_charge_power: l.minimum_charge_power.l1,
                ev_minimum_charge_power_l2: l.minimum_charge_power.l2,
                ev_minimum_charge_power_l3: l.minimum_charge_power.l3,
                ev_present_active_power: p.active.l1,
                ev_present_active_power_l2: p.active.l2,
                ev_present_active_power_l3: p.active.l3,
                ev_present_reactive_power: p.reactive.l1,
                ev_present_reactive_power_l2: p.reactive.l2,
                ev_present_reactive_power_l3: p.reactive.l3,
                ev_maximum_discharge_power: d.maximum_discharge_power.l1,
                ev_maximum_discharge_power_l2: d.maximum_discharge_power.l2,
                ev_maximum_discharge_power_l3: d.maximum_discharge_power.l3,
                ev_minimum_discharge_power: d.minimum_discharge_power.l1,
                ev_minimum_discharge_power_l2: d.minimum_discharge_power.l2,
                ev_minimum_discharge_power_l3: d.minimum_discharge_power.l3,
                ev_maximum_v2x_energy_request: None,
                ev_minimum_v2x_energy_request: None,
            })
        } else {
            ac::ClReqControlModeChoice::DynamicAc(ac::DynamicAcClReqControlMode {
                departure_time: None,
                ev_target_energy_request: r.target_energy,
                ev_maximum_energy_request: r.maximum_energy,
                ev_minimum_energy_request: r.minimum_energy,
                ev_maximum_charge_power: l.maximum_charge_power.l1,
                ev_maximum_charge_power_l2: l.maximum_charge_power.l2,
                ev_maximum_charge_power_l3: l.maximum_charge_power.l3,
                ev_minimum_charge_power: l.minimum_charge_power.l1,
                ev_minimum_charge_power_l2: l.minimum_charge_power.l2,
                ev_minimum_charge_power_l3: l.minimum_charge_power.l3,
                ev_present_active_power: p.active.l1,
                ev_present_active_power_l2: p.active.l2,
                ev_present_active_power_l3: p.active.l3,
                ev_present_reactive_power: p.reactive.l1,
                ev_present_reactive_power_l2: p.reactive.l2,
                ev_present_reactive_power_l3: p.reactive.l3,
            })
        };
        let msg = ac::AcChargeLoopReq {
            header: self.header(),
            display_parameters: self.display_parameters(),
            meter_info_requested: false,
            cl_req_control_mode: control_mode,
        };
        let framed = frame(tx, PayloadType::AC_MAINSTREAM, |out| msg.encode(out));
        if self.state == State::AwaitChargeLoopRes {
            return self.sent(MsgKind::AcChargeLoopReq, framed);
        }
        self.send_then(State::AwaitChargeLoopRes, MsgKind::AcChargeLoopReq, framed)
    }

    pub(super) fn on_ac_charge_loop_res(&mut self, res: ac::AcChargeLoopRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::AcChargeLoopRes, res.response_code) {
            return end;
        }
        // The mode we asked for, with the BPT variant in a BPT session. In Scheduled mode the target
        // power is optional: without one the vehicle follows the schedule it selected.
        // Without a target (V2G20-1826: the charger names one only for the grid code) the vehicle
        // follows its schedule.
        let followed = self.schedule.power_at(self.secc_now());
        let scheduled_mode = |m: ac::ScheduledAcClResControlMode| ac::DynamicAcClResControlMode {
            departure_time: None,
            minimum_soc: None,
            target_soc: None,
            ack_max_delay: None,
            evse_target_active_power: m.evse_target_active_power.or(followed.map(|p| p.l1)).unwrap_or(iso15118_20::common_types::RationalNumber { exponent: 0, value: 0 }),
            evse_target_active_power_l2: m.evse_target_active_power_l2.or(followed.and_then(|p| p.l2)),
            evse_target_active_power_l3: m.evse_target_active_power_l3.or(followed.and_then(|p| p.l3)),
            evse_target_reactive_power: m.evse_target_reactive_power,
            evse_target_reactive_power_l2: m.evse_target_reactive_power_l2,
            evse_target_reactive_power_l3: m.evse_target_reactive_power_l3,
            evse_present_active_power: m.evse_present_active_power,
            evse_present_active_power_l2: m.evse_present_active_power_l2,
            evse_present_active_power_l3: m.evse_present_active_power_l3,
        };
        let mode = match (res.cl_res_control_mode, self.config.bpt(), self.config.scheduled) {
            (ac::ClResControlModeChoice::ScheduledAc(m), false, true) => scheduled_mode(m),
            (ac::ClResControlModeChoice::BptScheduledAc(m), true, true) => scheduled_mode(ac::ScheduledAcClResControlMode {
                evse_target_active_power: m.evse_target_active_power,
                evse_target_active_power_l2: m.evse_target_active_power_l2,
                evse_target_active_power_l3: m.evse_target_active_power_l3,
                evse_target_reactive_power: m.evse_target_reactive_power,
                evse_target_reactive_power_l2: m.evse_target_reactive_power_l2,
                evse_target_reactive_power_l3: m.evse_target_reactive_power_l3,
                evse_present_active_power: m.evse_present_active_power,
                evse_present_active_power_l2: m.evse_present_active_power_l2,
                evse_present_active_power_l3: m.evse_present_active_power_l3,
            }),
            (ac::ClResControlModeChoice::DynamicAc(m), false, false) => m,
            (ac::ClResControlModeChoice::BptDynamicAc(m), true, false) => ac::DynamicAcClResControlMode {
                departure_time: m.departure_time,
                minimum_soc: m.minimum_soc,
                target_soc: m.target_soc,
                ack_max_delay: m.ack_max_delay,
                evse_target_active_power: m.evse_target_active_power,
                evse_target_active_power_l2: m.evse_target_active_power_l2,
                evse_target_active_power_l3: m.evse_target_active_power_l3,
                evse_target_reactive_power: m.evse_target_reactive_power,
                evse_target_reactive_power_l2: m.evse_target_reactive_power_l2,
                evse_target_reactive_power_l3: m.evse_target_reactive_power_l3,
                evse_present_active_power: m.evse_present_active_power,
                evse_present_active_power_l2: m.evse_present_active_power_l2,
                evse_present_active_power_l3: m.evse_present_active_power_l3,
            },
            _ => return self.end(EndReason::UnexpectedMessage),
        };
        let status = AcChargeLoopStatus {
            target_active_power: crate::ev::Phases { l1: mode.evse_target_active_power, l2: mode.evse_target_active_power_l2, l3: mode.evse_target_active_power_l3 },
            target_reactive_power: [mode.evse_target_reactive_power, mode.evse_target_reactive_power_l2, mode.evse_target_reactive_power_l3],
            present_active_power: [mode.evse_present_active_power, mode.evse_present_active_power_l2, mode.evse_present_active_power_l3],
            target_frequency: res.evse_target_frequency,
        };
        self.after_charge_loop_res(res.evse_status, Notice::AcChargeLoop(status), tx)
    }

    /// The vehicle's `DisplayParameters` for a charge loop request.
    fn display_parameters(&self) -> Option<common_types::DisplayParameters> {
        self.ev.present_soc().map(|soc| common_types::DisplayParameters {
            present_soc: Some(soc.min(100) as i8),
            minimum_soc: None,
            target_soc: None,
            maximum_soc: None,
            remaining_time_to_minimum_soc: None,
            remaining_time_to_target_soc: None,
            remaining_time_to_maximum_soc: None,
            charging_complete: None,
            battery_energy_capacity: None,
            inlet_hot: None,
        })
    }

    /// After a `DC_ChargeLoopRes`: wait the loop interval, then send the next request.
    pub(super) fn pace_charge_loop(&mut self, tx: &mut [u8]) -> Step {
        if self.config.charge_loop_interval_ms == 0 {
            return self.send_charge_loop_req(tx);
        }
        self.poll_pending = true;
        Step {
            timers: Timers::one(TimerCmd::Cancel(Timer::Response))
                .with(TimerCmd::Arm { timer: Timer::Poll, ms: self.config.charge_loop_interval_ms }),
            ..Step::default()
        }
    }
    pub(super) fn send_power_delivery(&mut self, progress: common::ChargeProgress, tx: &mut [u8]) -> Step {
        let framed = self.frame_power_delivery_req(progress, PayloadType::MAINSTREAM, tx);
        let next = match progress {
            common::ChargeProgress::Stop => State::AwaitPowerDeliveryStopRes,
            common::ChargeProgress::Standby => State::AwaitPowerDeliveryStandbyRes,
            _ => State::AwaitPowerDeliveryRes,
        };
        self.send_then(next, MsgKind::PowerDeliveryReq, framed)
    }

    /// `PowerDeliveryReq` with the vehicle's power profile; on the side stream it ends a schedule
    /// renegotiation (V2G20-2317).
    pub(super) fn frame_power_delivery_req(&mut self, progress: common::ChargeProgress, payload_type: PayloadType, tx: &mut [u8]) -> Result<usize, ()> {
        let r = self.ev.schedule_request();
        // Dynamic control mode (8.3.4.3.8.2): the fastest charging the vehicle can think of up to
        // its maximum energy request, at the lower of its own and the charger's maximum power,
        // both as announced in the charge parameter discovery.
        let (power, power_l2, power_l3) = if self.config.ac() {
            let announced = self.ev_announced_ac.unwrap_or_else(|| self.ev.ac20_limits()).maximum_charge_power;
            let evse = self.evse_announced_ac;
            let take = |ev: Option<iso15118_20::common_types::RationalNumber>, evse: Option<iso15118_20::common_types::RationalNumber>| match (ev, evse) {
                (Some(a), Some(b)) => Some(super::limits::smaller(a, b)),
                (a, None) => a,
                (None, _) => None,
            };
            (
                take(Some(announced.l1), evse.map(|e| e.evse_maximum_charge_power)).unwrap_or(announced.l1),
                take(announced.l2, evse.and_then(|e| e.evse_maximum_charge_power_l2)),
                take(announced.l3, evse.and_then(|e| e.evse_maximum_charge_power_l3)),
            )
        } else {
            let announced = self.ev_announced.unwrap_or_else(|| self.ev.dc_limits());
            let power = match self.evse_announced {
                Some(evse) => super::limits::smaller(announced.maximum_charge_power, evse.evse_maximum_charge_power),
                None => announced.maximum_charge_power,
            };
            (power, None, None)
        };
        let total_watts = rational_to_volts(power) + power_l2.map_or(0.0, rational_to_volts) + power_l3.map_or(0.0, rational_to_volts);
        let (watts, watt_hours) = (total_watts, rational_to_volts(r.maximum_energy));
        let seconds = if watts > 0.0 { watt_hours * 3600.0 / watts } else { r.departure_time as f32 };
        // Rounded up, at least a second, at most until departure.
        let whole = seconds as u32;
        let duration = (if (whole as f32) < seconds { whole + 1 } else { whole }).clamp(1, r.departure_time.max(1));
        let own = self.lines(Phases { l1: power, l2: power_l2, l3: power_l3 });
        let mut entries = [common::PowerScheduleEntry { duration, power: own.l1, power_l2: own.l2, power_l3: own.l3 }; super::schedule::MAX_ENTRIES];
        let mut count = 1;
        if self.config.scheduled && !self.schedule.is_empty() {
            // The profile stays within the schedule (V2G20-1559) from now on, and within what the
            // vehicle takes; a standby or stop is a period of zero power (V2G20-1068, 1839).
            let zero = iso15118_20::common_types::RationalNumber { exponent: 0, value: 0 };
            let mut left = [(0u32, Phases::single(zero)); super::schedule::MAX_ENTRIES];
            count = self.schedule.remaining(self.secc_now(), &mut left);
            let ceiling = if self.config.ac() { self.ev_announced_ac.unwrap_or_else(|| self.ev.ac20_limits()).maximum_charge_power } else { Phases::single(power) };
            for (slot, (duration, allowed)) in entries.iter_mut().zip(&left[..count]) {
                let p = if matches!(progress, common::ChargeProgress::Standby | common::ChargeProgress::Stop) {
                    Phases { l1: zero, l2: allowed.l2.map(|_| zero), l3: allowed.l3.map(|_| zero) }
                } else {
                    super::limits::phases_with(*allowed, ceiling, super::limits::smaller)
                };
                let p = self.lines(p);
                *slot = common::PowerScheduleEntry { duration: *duration, power: p.l1, power_l2: p.l2, power_l3: p.l3 };
            }
        }
        let time_anchor = self.timestamp();
        let msg = common::PowerDeliveryReq {
            header: self.header(),
            ev_processing: Processing::Finished,
            charge_progress: progress,
            ev_power_profile: Some(common::EvPowerProfile {
                time_anchor,
                evppt_control_mode: if self.config.scheduled {
                    common::EvpptControlModeChoice::Scheduled(common::ScheduledEvpptControlMode {
                        selected_schedule_tuple_id: self.selected_tuple,
                        // V2G20-1866: always when the charger offered a tolerance with the schedule;
                        // otherwise it is left out (the Python library insists on it: `Config::state_tolerance_always`).
                        power_tolerance_acceptance: if self.tolerance_offered {
                            Some(common::PowerToleranceAcceptance::PowerToleranceConfirmed)
                        } else if self.config.state_tolerance_always {
                            Some(common::PowerToleranceAcceptance::PowerToleranceNotConfirmed)
                        } else {
                            None
                        },
                    })
                } else {
                    common::EvpptControlModeChoice::Dynamic(common::DynamicEvpptControlMode {})
                },
                ev_power_profile_entries: common::EvPowerProfileEntryList { ev_power_profile_entry: &entries[..count] },
            }),
            // A BPT session says which channel the power flows on at the start (unified channel:
            // charging; discharging follows from the charge loop's setpoints).
            bpt_channel_selection: (self.config.bpt() && !self.config.ac() && progress == common::ChargeProgress::Start).then_some(common::ChannelSelection::Charge),
        };
        frame(tx, payload_type, |out| msg.encode(out))
    }

    /// A power per line as the connector of the session wants it (V2G20-1815 to 1818, 1822): on a
    /// single-phase connector only the base element; on a three-phase one a single-phase vehicle marks
    /// its unused lines with zero. A three-phase vehicle without `_L2`, `_L3` values gives the sum.
    pub(super) fn lines(&self, p: Phases) -> Phases {
        let zero = iso15118_20::common_types::RationalNumber { exponent: 0, value: 0 };
        if !self.ac_three_phase {
            Phases { l2: None, l3: None, ..p }
        } else if self.ev.ac_connector() == crate::ev::AcConnector::SinglePhase {
            Phases { l1: p.l1, l2: Some(p.l2.unwrap_or(zero)), l3: Some(p.l3.unwrap_or(zero)) }
        } else {
            p
        }
    }
}
