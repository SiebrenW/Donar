//! The metering confirmation (ISO 15118-20 8.3.4.3.11): when the charger asks for it in a charge loop
//! answer (`EVSENotification` MeteringConfirmation, V2G20-1083), the vehicle sends a
//! `MeteringConfirmationReq` on its own stream (`0x8102`, V2G20-1084) next to the charge loop: the
//! `SignedMeteringData` (session id, the `MeterInfo` and `Receipt` it just received, V2G20-2236, and the
//! selected schedule tuple in Scheduled mode, V2G20-776), signed with the contract key (V2G20-1808).
//! That is Plug & Charge only (V2G20-2704).

use iso15118_20::common::{self, SignedMeteringData, SmdtControlModeChoice};
use iso15118_20::common_types::{MeterInfo, Receipt};
use v2gtp::PayloadType;

use super::renegotiation::Side;
use super::types::*;
use super::{Machine, frame};
use crate::ev::Iso20Vehicle;
use crate::log::{LogSink, MsgKind, Record};
use crate::{Timer, TimerCmd};
use exi::Arena;

/// `Id` of the `SignedMeteringData` element the signature refers to.
const METER_ID: &str = "meter1";
const METER_URI: &str = "#meter1";

impl<E: Iso20Vehicle, L: LogSink> Machine<E, L> {
    /// Appends the `MeteringConfirmationReq` for `meter` and `receipt` to `step` (which may already hold
    /// a main stream frame in the buffer) and starts waiting for the answer. Not while another side
    /// stream is running (the charger does not repeat the notification until its delay is over): then
    /// the confirmation is skipped.
    pub(super) fn start_metering_confirmation(&mut self, mut step: Step, meter: &MeterInfo<'_>, receipt: Option<&Receipt<'_>>, tx: &mut [u8]) -> Step {
        if self.side != Side::Idle {
            return step;
        }
        if self.ev.contract().is_none() {
            return step;
        }
        let offset = step.send.unwrap_or(0);
        let Some(tail) = tx.get_mut(offset..) else { return self.end(EndReason::TxBufferTooSmall) };
        let session_id = self.session_id;
        let time_stamp = self.timestamp();
        let data = SignedMeteringData {
            id: METER_ID,
            session_id,
            meter_info: *meter,
            receipt: receipt.copied(),
            smdt_control_mode: if self.config.scheduled {
                SmdtControlModeChoice::Scheduled(common::ScheduledSmdtControlMode { selected_schedule_tuple_id: self.selected_tuple })
            } else {
                SmdtControlModeChoice::Dynamic(common::DynamicSmdtControlMode {})
            },
        };
        let Some(contract) = self.ev.contract() else { return step };
        let (out, scratch) = tail.split_at_mut(tail.len() / 2);
        let framed = iso15118_20::signature::encode_fragment(scratch, iso15118_20::signature::fragment::SIGNED_METERING_DATA, |e| data.encode_content(e))
            .map_err(|_| ())
            .and_then(|length| {
                super::setup::sign_fragment(contract, METER_URI, scratch, length, |signature| {
                    let msg = common::MeteringConfirmationReq {
                        header: iso15118_20::common_types::MessageHeader { session_id, time_stamp, signature: Some(signature) },
                        signed_metering_data: data,
                    };
                    frame(out, PayloadType::METERING_CONFIRMATION, |o| msg.encode(o))
                })
            });
        let Ok(len) = framed else { return self.end(EndReason::TxBufferTooSmall) };
        self.log.record(&Record::MsgTx { msg: MsgKind::MeteringConfirmationReq });
        self.side = Side::AwaitMetering;
        step.send = Some(offset + len);
        step.timers = step.timers.with(TimerCmd::Arm { timer: Timer::Side, ms: self.config.response_timeout_ms });
        step
    }

    /// The answer on the metering stream.
    pub(super) fn on_metering_frame(&mut self, payload_type: PayloadType, payload: &[u8], arena: &Arena<'_>) -> Step {
        if self.state != State::AwaitChargeLoopRes || self.side != Side::AwaitMetering {
            return self.ignore(payload_type);
        }
        match common::decode(payload, arena) {
            Ok(common::Message::MeteringConfirmationRes(res)) => {
                // A refusal (`FAILED_MeteringSignatureNotValid`, `FAILED_SignatureError`) is the charger's
                // business to act on (it ends the session with a notification if it wants to).
                let _ = self.received(MsgKind::MeteringConfirmationRes, res.response_code);
                self.side = Side::Idle;
                Step { timers: crate::Timers::one(TimerCmd::Cancel(Timer::Side)), ..Step::default() }
            }
            Ok(_) => self.ignore(payload_type),
            Err(e) => self.malformed(e),
        }
    }
}
