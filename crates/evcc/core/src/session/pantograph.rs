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
use crate::{Timer, TimerCmd};

impl<E: Ev, L: LogSink> Machine<E, L> {
    /// `ACDP_ConnectReq` (V2G20-4050): the first one after the schedule exchange and every repeat while the charger says `Ongoing`.
    pub(super) fn send_acdp_connect_req(&mut self, tx: &mut [u8]) -> Step {
        let msg = AcdpConnectReq { header: self.header(), ev_electrical_charging_device_status: self.ev.acdp_electrical_status() };
        let framed = frame(tx, PayloadType::ACDP_MAINSTREAM, |out| msg.encode(out));
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
                    step.notice = Some(Notice::AcdpConnection(connection));
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
}
