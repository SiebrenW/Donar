//! Shutdown: power delivery stop, welding detection and session stop.

use iso15118_20::common::{self, *};
use iso15118_20::common_types::Processing;
use iso15118_20::dc;
use v2gtp::PayloadType;

use super::{Machine, frame};
use super::config::*;
use super::types::*;
use crate::ev::Ev;
use crate::log::{LogSink, MsgKind};
use crate::{Timer, TimerCmd, Timers};

impl<E: Ev, L: LogSink> Machine<E, L> {
    pub(super) fn on_power_delivery_stop_res(&mut self, res: PowerDeliveryRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::PowerDeliveryRes, res.response_code) {
            return end;
        }
        if self.config.ac() {
            // Nothing to open or weld-check on the AC side: the session stops.
            self.ongoing_armed = false;
            return self.send_session_stop_req(tx);
        }
        // The energy flow is over. Contactors open, then welding detection
        // (V2G20-1448), then the session stops.
        self.goto(State::AwaitContactorsOpened);
        Step {
            timers: Timers::one(TimerCmd::Cancel(Timer::Response)).with(self.arm_sequence()),
            notice: Some(Notice::OpenContactors),
            ..Step::default()
        }
    }

    pub(super) fn on_welding_detection_res(&mut self, res: dc::DcWeldingDetectionRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::DcWeldingDetectionRes, res.response_code) {
            return end;
        }
        if self.welding_finishing {
            // V2G20-1462: OK to the final request: stop the session.
            self.welding_finishing = false;
            self.ongoing_armed = false;
            return self.send_session_stop_req(tx);
        }
        // The EV decides when welding detection is over (IEC 61851-23 CC.3.3: it
        // may read the side B voltage from several responses).
        if rational_to_volts(res.evse_present_voltage).abs()
            < f32::from(self.config.welding_detection_voltage_v)
        {
            self.welding_finishing = true;
            return self.send_welding_req(true, tx);
        }
        self.ongoing_repeat(tx)
    }

    /// `SessionStopReq` of the kind the shutdown under way ends in.
    fn send_session_stop_req(&mut self, tx: &mut [u8]) -> Step {
        let msg = common::SessionStopReq {
            header: self.header(),
            charging_session: match self.stop_kind {
                StopKind::Terminate => common::ChargingSession::Terminate,
                StopKind::Pause => common::ChargingSession::Pause,
                StopKind::ServiceRenegotiation => common::ChargingSession::ServiceRenegotiation,
            },
            ev_termination_code: None,
            ev_termination_explanation: None,
        };
        let framed = frame(tx, PayloadType::MAINSTREAM, |out| msg.encode(out));
        let mut step = self.send_then(State::AwaitSessionStopRes, MsgKind::SessionStopReq, framed);
        if step.end.is_none() {
            step.timers = step.timers.with(TimerCmd::Cancel(Timer::Ongoing));
        }
        step
    }

    pub(super) fn on_session_stop_res(&mut self, res: SessionStopRes, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::SessionStopRes, res.response_code) {
            return end;
        }
        let kind = core::mem::take(&mut self.stop_kind);
        if kind == StopKind::Pause {
            // V2G20-1540: the session stays resumable; the host closes the connection
            // (V2G20-2071) and may come back with `Config::resume`.
            let resume = Resume { session_id: self.session_id, selected: self.selected, renegotiation_supported: self.renegotiation_supported };
            return self.end(EndReason::Paused(resume));
        }
        if kind == StopKind::ServiceRenegotiation {
            // V2G20-1477: after SessionStopRes OK the EVCC sends a ServiceDiscoveryReq; the
            // connection and the authorization stay (V2G20-1643).
            self.ongoing_armed = false;
            self.poll_pending = false;
            self.stop_requested = false;
            self.standby = false;
            let mut step = self.send_service_discovery_req(tx);
            if step.end.is_none() {
                step.notice = Some(Notice::ServicesRenegotiating);
            }
            return step;
        }
        // V2G20-1463: after SessionStopRes OK the EVCC terminates the connection.
        self.end(EndReason::Completed)
    }

    /// Starts the shutdown: the host ramps the current down first (V2G20-1310).
    pub(super) fn begin_stop(&mut self, within_s: Option<u16>) -> Step {
        self.standby = false;
        self.ramp_down(within_s)
    }
    /// Asks the host to bring the current to zero; the shutdown or a standby period follows.
    pub(super) fn ramp_down(&mut self, within_s: Option<u16>) -> Step {
        self.abandon_side();
        self.charge_loop_started = false;
        self.goto(State::AwaitCurrentStopped);
        Step {
            timers: Timers::one(TimerCmd::Cancel(Timer::Response))
                .with(TimerCmd::Cancel(Timer::Poll))
                .with(self.arm_sequence()),
            notice: Some(Notice::StopEnergyTransfer { within_s }),
            ..Step::default()
        }
    }
    /// One `DC_WeldingDetectionReq`; `finished` is the EV's `EVProcessing`.
    pub(super) fn send_welding_req(&mut self, finished: bool, tx: &mut [u8]) -> Step {
        let msg = dc::DcWeldingDetectionReq {
            header: self.header(),
            ev_processing: if finished { Processing::Finished } else { Processing::Ongoing },
        };
        let framed = frame(tx, PayloadType::DC_MAINSTREAM, |out| msg.encode(out));
        if self.state == State::AwaitWeldingDetectionRes {
            return self.sent(MsgKind::DcWeldingDetectionReq, framed);
        }
        let mut step = self.send_then(State::AwaitWeldingDetectionRes, MsgKind::DcWeldingDetectionReq, framed);
        // The standard sets no timer of its own for welding detection; the generic bound on
        // repeated `Ongoing` exchanges applies.
        if step.end.is_none() {
            self.ongoing_armed = true;
            step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::Ongoing, ms: self.config.ongoing_timeout_ms });
        }
        step
    }}
