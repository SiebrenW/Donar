//! ACDP (a pantograph on the infrastructure, ISO 15118-20 8.3.4.7): the positioning of the vehicle that follows
//! `SessionSetupRes` and precedes the authorization.
//!
//! The vehicle asks with `ACDP_VehiclePositioningReq` (mobilized, with or without positioning support of its own) and the
//! charger answers `Ongoing` with where its pairing and positioning device sees the vehicle, for as long as the vehicle
//! is not immobilized (V2G20-4047). The host gets each answer as [`Notice::AcdpPosition`], moves the vehicle (the driver,
//! the parking system) and tells the session it is positioned and immobilized through [`Ev::immobilized`]; the next request
//! says so (V2G20-4048) and the charger answers `Finished` (V2G20-4049). Three cases of positioning support (8.3.4.7.5.1)
//! differ only in what the charger puts into the answers and in what the host does with them.

use iso15118_20::acdp::{self, AcdpVehiclePositioningRes};
use iso15118_20::common_types::Processing;
use v2gtp::PayloadType;

use super::types::*;
use super::{Machine, frame, is_ok};
use crate::ev::Ev;
use crate::log::{LogSink, MsgKind, Record};
use crate::{Timer, TimerCmd};

impl<E: Ev, L: LogSink> Machine<E, L> {
    /// `ACDP_VehiclePositioningReq`: the first one after `SessionSetupRes` and every repeat while the charger says `Ongoing`.
    pub(super) fn send_vehicle_positioning_req(&mut self, tx: &mut [u8]) -> Step {
        let immobilized = self.ev.immobilized();
        self.positioning_immobilized = immobilized;
        let msg = acdp::AcdpVehiclePositioningReq {
            header: self.header(),
            ev_mobility_status: immobilized,
            ev_positioning_support: self.ev.acdp_positioning_support(),
        };
        let framed = frame(tx, PayloadType::ACDP_MAINSTREAM, |out| msg.encode(out));
        if self.state == State::AwaitVehiclePositioningRes {
            self.sent(MsgKind::AcdpVehiclePositioningReq, framed)
        } else {
            self.send_then(State::AwaitVehiclePositioningRes, MsgKind::AcdpVehiclePositioningReq, framed)
        }
    }

    pub(super) fn on_vehicle_positioning_res(&mut self, res: AcdpVehiclePositioningRes<'_>, tx: &mut [u8]) -> Step {
        self.log.record(&Record::MsgRx { msg: MsgKind::AcdpVehiclePositioningRes });
        self.log.record(&Record::Response { msg: MsgKind::AcdpVehiclePositioningRes, code: res.response_code });
        // V2G20-4031: a failing answer (`FAILED_AssociationError` when the vehicle left the zone of the charger's pairing
        // and positioning device) ends it with a `SessionStopReq`, after which the discovery starts again.
        if !is_ok(res.response_code) {
            return self.end_politely(EndReason::Rejected(res.response_code), tx);
        }
        let position = AcdpPosition {
            evse_positioning_support: res.evse_positioning_support,
            x_deviation_mm: res.ev_relative_x_deviation,
            y_deviation_mm: res.ev_relative_y_deviation,
            window_x_mm: res.contact_window_xc,
            window_y_mm: res.contact_window_yc,
            in_charge_position: res.ev_in_charge_position,
        };
        self.log.record(&Record::VehiclePositioning {
            processing: res.evse_processing,
            evse_positioning_support: position.evse_positioning_support,
            x_deviation_mm: position.x_deviation_mm,
            y_deviation_mm: position.y_deviation_mm,
            window_x_mm: position.window_x_mm,
            window_y_mm: position.window_y_mm,
            in_charge_position: position.in_charge_position,
        });
        match res.evse_processing {
            // Not yet: the host sees where the vehicle is and the next request follows after the poll interval.
            Processing::Ongoing | Processing::OngoingWaitingForCustomerInteraction => {
                let mut step = self.ongoing_repeat(tx);
                step.notice = Some(Notice::AcdpPosition(position));
                step
            }
            // `Finished` answers the request that said the vehicle is immobilized (V2G20-4069) and nothing else.
            Processing::Finished if self.positioning_immobilized => {
                let mut step = if self.config.positioning_only { self.stop_session(tx) } else { self.send_authorization_setup_req(tx) };
                if step.end.is_none() {
                    step.notice = Some(Notice::AcdpPosition(position));
                    step.timers = step.timers.with(TimerCmd::Cancel(Timer::Poll));
                }
                step
            }
            Processing::Finished => self.end(EndReason::UnexpectedMessage),
        }
    }
}
