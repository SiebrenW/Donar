//! Schedule renegotiation: a side stream (V2GTP payload type `0x8101`) multiplexed with the
//! charge loop (ISO 15118-20:2022, 8.6.5.1, Annex F). The vehicle sends
//! `DC_ChargeParameterDiscoveryReq`, `ScheduleExchangeReq` (repeated while `Ongoing`) and
//! `PowerDeliveryReq` (ChargeProgress ScheduleRenegotiation, with a new EVPowerProfile); the
//! charger's `PowerDeliveryRes` OK ends it. The charge loop goes on meanwhile.
//!
//! The standard does not say which schema the side stream's messages use. They are coded as on
//! the main stream: the DC message with the DC schema, the others with the CommonMessages
//! schema, and the receiver decodes what the exchange expects next.

use iso15118_20::common;
use iso15118_20::common_types::Processing;
use iso15118_20::dc;
use v2gtp::PayloadType;

use super::types::*;
use super::Machine;
use crate::ev::Ev;
use crate::log::{LogSink, MsgKind, Record};
use crate::{Timer, TimerCmd, Timers};
use exi::Arena;

/// Where the side stream is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Idle,
    /// `DC_ChargeParameterDiscoveryReq` is out.
    AwaitChargeParameters,
    /// `ScheduleExchangeReq` is out (or its repeat is waiting for the poll pause).
    AwaitSchedule,
    /// `PowerDeliveryReq` (ScheduleRenegotiation) is out.
    AwaitPowerDelivery,
}

/// The pause before repeating a `ScheduleExchangeReq` that got `Ongoing`, when the main stream's
/// poll interval is zero.
const MIN_REPEAT_PAUSE_MS: u32 = 50;

impl<E: Ev, L: LogSink> Machine<E, L> {
    /// Starts a renegotiation: the first request goes out in `step` (after a frame the step
    /// already holds). Not while one is running, outside the charge loop or before the first
    /// `ChargeLoopReq` (V2G20-1846).
    pub(super) fn start_schedule_renegotiation(&mut self, step: Step, tx: &mut [u8]) -> Step {
        if self.side != Side::Idle || self.state != State::AwaitChargeLoopRes || !self.charge_loop_started {
            return step;
        }
        self.side = Side::AwaitChargeParameters;
        self.side_waited_ms = 0;
        self.push_side(step, tx)
    }

    /// Appends the request of the current side state to `step` (which may already carry a
    /// main stream frame in the same buffer) and arms the side timer.
    fn push_side(&mut self, mut step: Step, tx: &mut [u8]) -> Step {
        let offset = step.send.unwrap_or(0);
        let Some(tail) = tx.get_mut(offset..) else { return self.end(EndReason::TxBufferTooSmall) };
        let (framed, msg) = match self.side {
            Side::AwaitChargeParameters => (
                self.frame_charge_parameter_discovery_req(PayloadType::SCHEDULE_RENEGOTIATION, tail),
                MsgKind::DcChargeParameterDiscoveryReq,
            ),
            Side::AwaitSchedule => {
                (self.frame_schedule_exchange_req(PayloadType::SCHEDULE_RENEGOTIATION, tail), MsgKind::ScheduleExchangeReq)
            }
            Side::AwaitPowerDelivery => (
                self.frame_power_delivery_req(common::ChargeProgress::ScheduleRenegotiation, PayloadType::SCHEDULE_RENEGOTIATION, tail),
                MsgKind::PowerDeliveryReq,
            ),
            Side::Idle => return step,
        };
        let Ok(len) = framed else { return self.end(EndReason::TxBufferTooSmall) };
        self.log.record(&Record::MsgTx { msg });
        self.side_poll_pending = false;
        step.send = Some(offset + len);
        step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::Side, ms: self.config.response_timeout_ms });
        step
    }

    /// The side timer ran out: the pause before a repeat is over, or the response did not come.
    pub(super) fn on_side_timer(&mut self, tx: &mut [u8]) -> Step {
        match (self.side, self.side_poll_pending) {
            (Side::Idle, _) => Step::default(),
            (Side::AwaitSchedule, true) => self.push_side(Step::default(), tx),
            _ => self.end(EndReason::Timeout),
        }
    }

    /// The side stream stops, wherever it was (the charge is ending).
    pub(super) fn abandon_side(&mut self) {
        self.side = Side::Idle;
        self.side_poll_pending = false;
    }

    pub(super) fn on_side_frame(&mut self, payload_type: PayloadType, payload: &[u8], arena: &Arena<'_>, tx: &mut [u8]) -> Step {
        if self.state != State::AwaitChargeLoopRes {
            return self.ignore(payload_type);
        }
        match self.side {
            Side::Idle => self.ignore(payload_type),
            Side::AwaitChargeParameters => match dc::decode(payload, arena) {
                Ok(dc::Message::DcChargeParameterDiscoveryRes(res)) => {
                    if let Some(end) = self.received(MsgKind::DcChargeParameterDiscoveryRes, res.response_code) {
                        return end;
                    }
                    self.note_evse_limits(&res.dc_cpd_res_energy_transfer_mode);
                    self.side = Side::AwaitSchedule;
                    self.push_side(Step::default(), tx)
                }
                Ok(_) => self.ignore(payload_type),
                Err(e) => self.malformed(e),
            },
            Side::AwaitSchedule => match common::decode(payload, arena) {
                Ok(common::Message::ScheduleExchangeRes(res)) => {
                    if let Some(end) = self.received(MsgKind::ScheduleExchangeRes, res.response_code) {
                        return end;
                    }
                    self.log.record(&Record::EvseProcessing { processing: res.evse_processing });
                    if !matches!(res.se_res_control_mode, common::SeResControlModeChoice::Dynamic(_)) {
                        return self.end(EndReason::UnexpectedMessage);
                    }
                    match res.evse_processing {
                        Processing::Finished => {
                            self.side = Side::AwaitPowerDelivery;
                            self.push_side(Step::default(), tx)
                        }
                        // V2G20-2714: the same request again, after a pause; the exchange is
                        // bounded like any repeated `Ongoing` (V2G_EVCC_Ongoing_Timeout).
                        Processing::Ongoing | Processing::OngoingWaitingForCustomerInteraction => {
                            let pause = self.config.poll_interval_ms.max(MIN_REPEAT_PAUSE_MS);
                            self.side_waited_ms = self.side_waited_ms.saturating_add(pause);
                            if self.side_waited_ms > self.config.ongoing_timeout_ms {
                                return self.end(EndReason::Timeout);
                            }
                            self.side_poll_pending = true;
                            Step { timers: Timers::one(TimerCmd::Arm { timer: Timer::Side, ms: pause }), ..Step::default() }
                        }
                    }
                }
                Ok(_) => self.ignore(payload_type),
                Err(e) => self.malformed(e),
            },
            Side::AwaitPowerDelivery => match common::decode(payload, arena) {
                Ok(common::Message::PowerDeliveryRes(res)) => {
                    if let Some(end) = self.received(MsgKind::PowerDeliveryRes, res.response_code) {
                        return end;
                    }
                    // V2G20-1049, V2G20-1050: done; the new profile applies from the next
                    // ChargeLoopReq on.
                    self.side = Side::Idle;
                    Step {
                        timers: Timers::one(TimerCmd::Cancel(Timer::Side)),
                        notice: Some(Notice::ScheduleRenegotiated),
                        ..Step::default()
                    }
                }
                Ok(_) => self.ignore(payload_type),
                Err(e) => self.malformed(e),
            },
        }
    }
}
