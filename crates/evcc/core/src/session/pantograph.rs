//! ACDP (ISO 15118-20 8.3.4.7): the pantograph is connected after the schedule exchange (`ACDP_Connect`, Table 80) and
//! disconnected after the power delivery has stopped (`ACDP_Disconnect`), in place of the welding detection. Both are repeated
//! while the charger answers `Ongoing`, as long as the pantograph moves, and both report the charger's mechanical state (home,
//! moving, at its end position) and the control pilot state it sees.

use iso15118_20::acdp::{AcdpConnectReq, AcdpConnectRes};
use iso15118_20::common_types::Processing;
use v2gtp::PayloadType;

use super::types::*;
use super::{Machine, frame};
use crate::ev::Ev;
use crate::log::{LogSink, MsgKind, Record};
use crate::{Timer, TimerCmd, Timers};

impl<E: Ev, L: LogSink> Machine<E, L> {
    /// `ACDP_ConnectReq` (V2G20-4050): the first one after the schedule exchange and every repeat while the charger says `Ongoing`.
    pub(super) fn send_acdp_connect_req(&mut self, tx: &mut [u8]) -> Step {
        let msg = AcdpConnectReq { header: self.header(), ev_electrical_charging_device_status: self.ev.acdp_electrical_status() };
        let framed = frame(tx, PayloadType::ACDP_MAINSTREAM, |out| msg.encode(out));
        self.pantograph_engaged = true;
        if self.state == State::AwaitConnectRes {
            self.sent(MsgKind::AcdpConnectReq, framed)
        } else {
            self.send_then(State::AwaitConnectRes, MsgKind::AcdpConnectReq, framed)
        }
    }

    /// `ACDP_DisconnectReq` (V2G20-4053): after the power delivery has stopped and the contactors are open.
    pub(super) fn send_acdp_disconnect_req(&mut self, tx: &mut [u8]) -> Step {
        // The message of the connect request, under the element name of the disconnect (the schema types it so).
        let msg = AcdpConnectReq { header: self.header(), ev_electrical_charging_device_status: self.ev.acdp_electrical_status() };
        let framed = frame(tx, PayloadType::ACDP_MAINSTREAM, |out| msg.encode_as_acdp_disconnect_req(out));
        if self.state == State::AwaitDisconnectRes {
            self.sent(MsgKind::AcdpDisconnectReq, framed)
        } else {
            self.send_then(State::AwaitDisconnectRes, MsgKind::AcdpDisconnectReq, framed)
        }
    }

    pub(super) fn on_acdp_connect_res(&mut self, res: AcdpConnectRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::AcdpConnectRes, res.response_code) {
            return end;
        }
        let connection = self.note_pantograph(&res);
        match res.evse_processing {
            Processing::Ongoing | Processing::OngoingWaitingForCustomerInteraction => {
                let mut step = self.ongoing_repeat(tx);
                step.notice = Some(Notice::AcdpConnection(connection));
                step
            }
            // V2G20-4052: the cable check follows (with the pilot at C first when the host sets the pilot).
            Processing::Finished => {
                self.ongoing_armed = false;
                self.poll_pending = false;
                let mut step = if self.config.control_pilot {
                    self.change_control_pilot(ControlPilot::C, State::AwaitControlPilotC)
                } else {
                    self.send_cable_check_req(tx)
                };
                if step.end.is_none() {
                    // The notice slot holds one: the pilot change the host has to carry out comes first.
                    step.notice.get_or_insert(Notice::AcdpConnection(connection));
                    step.timers = step.timers.with(TimerCmd::Cancel(Timer::Poll));
                }
                step
            }
        }
    }

    pub(super) fn on_acdp_disconnect_res(&mut self, res: AcdpConnectRes<'_>, tx: &mut [u8]) -> Step {
        if let Some(end) = self.received(MsgKind::AcdpDisconnectRes, res.response_code) {
            return end;
        }
        let connection = self.note_pantograph(&res);
        match res.evse_processing {
            Processing::Ongoing | Processing::OngoingWaitingForCustomerInteraction => {
                let mut step = self.ongoing_repeat(tx);
                step.notice = Some(Notice::AcdpConnection(connection));
                step
            }
            // V2G20-4055: the pantograph is home, the session stops.
            Processing::Finished => {
                self.ongoing_armed = false;
                self.poll_pending = false;
                self.pantograph_engaged = false;
                let mut step = self.send_session_stop_req(tx);
                if step.end.is_none() {
                    step.notice = Some(Notice::AcdpConnection(connection));
                }
                step
            }
        }
    }

    fn note_pantograph(&mut self, res: &AcdpConnectRes<'_>) -> AcdpConnection {
        let connection = AcdpConnection {
            finished: res.evse_processing == Processing::Finished,
            electrical: res.evse_electrical_charging_device_status,
            mechanical: res.evse_mechanical_charging_device_status,
        };
        self.log.record(&Record::Pantograph { processing: res.evse_processing, electrical: connection.electrical, mechanical: connection.mechanical });
        connection
    }

    /// A failing answer or an emergency in an ACDP session (V2G20-4026 to 4044): the communication goes on until the pantograph is
    /// home. With the pantograph possibly down the contactors are opened, then `ACDP_DisconnectReq` follows, then the session stop; before
    /// it a `SessionStopReq` ends the session. A failing `ACDP_DisconnectRes` goes straight to the session stop (V2G20-4044). The
    /// session ends with `reason` in the end.
    pub(super) fn acdp_wind_down(&mut self, reason: EndReason, tx: &mut [u8]) -> Step {
        match self.state {
            // Answers of a stop already under way: nothing to add.
            State::AwaitContactorsOpened | State::AwaitSessionStopRes | State::Ended => return Step::default(),
            _ => {}
        }
        self.pending_end.get_or_insert(reason);
        self.stop_kind = StopKind::Terminate;
        self.termination_code = Some(reason.termination_code());
        self.ongoing_armed = false;
        self.poll_pending = false;
        self.stop_requested = false;
        self.pilot.set_stopping(true);
        let mut step = if self.pantograph_engaged && self.state != State::AwaitDisconnectRes {
            self.goto(State::AwaitContactorsOpened);
            Step {
                timers: Timers::one(TimerCmd::Cancel(Timer::Response)).with(self.arm_sequence()),
                notice: Some(Notice::OpenContactors),
                ..Step::default()
            }
        } else {
            self.send_session_stop_req(tx)
        };
        if step.end.is_none() {
            step.timers = step.timers.with(TimerCmd::Cancel(Timer::Poll));
        }
        step
    }
}
