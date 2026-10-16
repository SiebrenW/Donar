//! Cable check, pre-charge, power delivery start and the charge loop.

use iso15118_20::common::{self, *};
use iso15118_20::common_types::{Processing, ResponseCode};
use iso15118_20::dc;
use v2gtp::PayloadType;

use super::{Machine, frame};
use super::config::*;
use super::types::*;
use crate::ev::Ev;
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
        let inlet = rational_to_volts(self.ev.inlet_voltage());
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
        let dc::ClResControlModeChoice::DynamicDc(mode) = res.cl_res_control_mode else {
            // We asked for Dynamic control.
            return self.end(EndReason::UnexpectedMessage);
        };
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
        // V2G20-1416, V2G20-1960: Terminate means shut down and stop the session.
        // The other notifications (pause, renegotiation, metering) are not
        // handled yet (TODO) and are ignored.
        let terminate = res.evse_status.filter(|s| {
            s.evse_notification == iso15118_20::common_types::EvseNotification::Terminate
        });
        if let Some(status) = terminate {
            return self.begin_stop(Some(status.notification_max_delay));
        }
        if core::mem::take(&mut self.stop_requested) {
            return self.begin_stop(None);
        }
        let mut step = self.pace_charge_loop(tx);
        step.notice = Some(Notice::ChargeLoop(status));
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
            ev_present_voltage: battery,
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
        let r = self.ev.schedule_request();
        let l = self.ev.dc_limits();
        let msg = dc::DcChargeLoopReq {
            header: self.header(),
            display_parameters: None,
            meter_info_requested: false,
            ev_present_voltage: self.ev.battery_voltage(),
            cl_req_control_mode: dc::ClReqControlModeChoice::DynamicDc(dc::DynamicDcClReqControlMode {
                departure_time: None,
                ev_target_energy_request: r.target_energy,
                ev_maximum_energy_request: r.maximum_energy,
                ev_minimum_energy_request: r.minimum_energy,
                ev_maximum_charge_power: l.maximum_charge_power,
                ev_minimum_charge_power: l.minimum_charge_power,
                ev_maximum_charge_current: l.maximum_charge_current,
                ev_maximum_voltage: l.maximum_voltage,
                ev_minimum_voltage: l.minimum_voltage,
            }),
        };
        let framed = frame(tx, PayloadType::DC_MAINSTREAM, |out| msg.encode(out));
        if self.state == State::AwaitChargeLoopRes {
            return self.sent(MsgKind::DcChargeLoopReq, framed);
        }
        self.send_then(State::AwaitChargeLoopRes, MsgKind::DcChargeLoopReq, framed)
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
        let r = self.ev.schedule_request();
        let limits = self.ev.dc_limits();
        // A single-entry profile at the vehicle's maximum charge power until departure: the
        // "fastest charging" simulation the standard asks for is simplified (TODO).
        let entries = [common::PowerScheduleEntry {
            duration: r.departure_time,
            power: limits.maximum_charge_power,
            power_l2: None,
            power_l3: None,
        }];
        let time_anchor = self.timestamp();
        let msg = common::PowerDeliveryReq {
            header: self.header(),
            ev_processing: Processing::Finished,
            charge_progress: progress,
            ev_power_profile: Some(common::EvPowerProfile {
                time_anchor,
                evppt_control_mode: common::EvpptControlModeChoice::Dynamic(common::DynamicEvpptControlMode {}),
                ev_power_profile_entries: common::EvPowerProfileEntryList { ev_power_profile_entry: &entries },
            }),
            bpt_channel_selection: None,
        };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        let next = if progress == common::ChargeProgress::Stop {
            State::AwaitPowerDeliveryStopRes
        } else {
            State::AwaitPowerDeliveryRes
        };
        self.send_then(next, MsgKind::PowerDeliveryReq, framed)
    }}
