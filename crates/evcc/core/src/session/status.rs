//! ACDP_SystemStatus (ISO 15118-20 8.3.4.7.8): a request/response pair on a stream of its own (payload type 0x8103), independent of the
//! main stream, from the `SessionSetupRes` until the `SessionStopRes` (V2G20-4016, NOTE 2 of 8.3.4.7.8.6). The vehicle tells the charger
//! how it is (ready, immobilized, the radio's strength, the pilot state, an error) and the charger tells how the charging point is
//! (the pantograph, isolation, emergency stop, the PPD's view). It goes out on a timer ([`super::Config::status_interval_ms`]) and at once when the host
//! reports a change with [`Event::AcdpStatusChanged`](super::types::Event::AcdpStatusChanged).

use exi::Arena;
use iso15118_20::acdp::{self, AcdpSystemStatusReq};
use iso15118_20::common_types::ResponseCode;
use v2gtp::PayloadType;

use super::frame;
use super::types::*;
use super::Machine;
use crate::ev::Iso20Vehicle;
use crate::log::{LogSink, MsgKind, Record};
use crate::{Timer, TimerCmd, Timers};

impl<E: Iso20Vehicle, L: LogSink> Machine<E, L> {
    /// Appends an `ACDP_SystemStatusReq` to the frames `step` already holds (a step may carry several, back to back in `tx`).
    pub(super) fn append_status(&mut self, mut step: Step, tx: &mut [u8]) -> Step {
        if step.end.is_some() {
            return step;
        }
        let offset = step.send.unwrap_or(0);
        let msg = AcdpSystemStatusReq { header: self.header(), ev_technical_status: self.ev.acdp_technical_status() };
        let framed = match tx.get_mut(offset..) {
            Some(tail) => frame(tail, PayloadType::ACDP_SYSTEM_STATUS, |out| msg.encode(out)),
            None => Err(()),
        };
        let Ok(len) = framed else { return self.end(EndReason::TxBufferTooSmall) };
        self.log.record(&Record::MsgTx { msg: MsgKind::AcdpSystemStatusReq });
        step.send = Some(offset + len);
        step
    }

    /// The first status request goes out with the first request after `SessionSetupRes` (V2G20-4016) and the timer starts.
    pub(super) fn start_status(&mut self, step: Step, tx: &mut [u8]) -> Step {
        self.status_active = true;
        let mut step = self.append_status(step, tx);
        if step.end.is_none() {
            step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::Status, ms: self.config.status_interval_ms });
        }
        step
    }

    pub(super) fn on_status_timer(&mut self, tx: &mut [u8]) -> Step {
        if !self.status_active {
            return Step::default();
        }
        let step = Step { timers: Timers::one(TimerCmd::Arm { timer: Timer::Status, ms: self.config.status_interval_ms }), ..Step::default() };
        self.append_status(step, tx)
    }

    /// The host says the vehicle's status changed (an error, the pilot, the immobilization): the charger is told at once.
    pub(super) fn status_changed(&mut self, tx: &mut [u8]) -> Step {
        if !self.status_active {
            return Step::default();
        }
        self.append_status(Step::default(), tx)
    }

    /// `ACDP_SystemStatusRes`. A failing answer says the charger could not represent its status: the parameters are ignored
    /// (V2G20-4015). The answer never ends the session and has no response timer (the stream is independent).
    pub(super) fn on_status_frame(&mut self, payload: &[u8], arena: &Arena<'_>) -> Step {
        if !self.status_active {
            return Step::default();
        }
        let res = match acdp::decode(payload, arena) {
            Ok(acdp::Message::AcdpSystemStatusRes(res)) => res,
            Ok(_) => return Step::default(),
            Err(e) => return self.malformed(e),
        };
        self.log.record(&Record::MsgRx { msg: MsgKind::AcdpSystemStatusRes });
        self.log.record(&Record::Response { msg: MsgKind::AcdpSystemStatusRes, code: res.response_code });
        if res.response_code == ResponseCode::Failed || !super::is_ok(res.response_code) {
            return Step::default();
        }
        let status = AcdpSystemStatus {
            mechanical: res.evse_mechanical_charging_device_status,
            ready_to_charge: res.evse_ready_to_charge,
            isolation: res.evse_isolation_status,
            disabled: res.evse_disabled,
            utility_interrupt: res.evse_utility_interrupt_event,
            emergency_shutdown: res.evse_emergency_shutdown,
            malfunction: res.evse_malfunction,
            in_charge_position: res.ev_in_charge_position,
            association: res.ev_association_status,
        };
        Step { notice: Some(Notice::AcdpSystemStatus(status)), ..Step::default() }
    }
}
