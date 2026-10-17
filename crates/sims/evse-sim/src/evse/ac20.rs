//! The ISO 15118-20 AC side of the simulated charger: `AC_ChargeParameterDiscovery` and the
//! `AC_ChargeLoop` in Dynamic control mode (services AC and AC_BPT). Power is per line, with
//! lines 2 and 3 optional; a negative target active power is a discharge (BPT).

use exi::Arena;
use iso15118_20::ac;
use iso15118_20::common_types::{self, RationalNumber, ResponseCode};
use v2gtp::PayloadType;

use super::{Evse, Output, State, frame, is_ok};

/// Service ids of the AC services (Table 204).
pub(super) const SERVICE_ID_AC: u16 = 1;
pub(super) const SERVICE_ID_AC_BPT: u16 = 5;

/// A power per line, in watts; a line the charger or the vehicle does not use is `None`.
type Lines = [Option<f64>; 3];

fn lines(a: RationalNumber, b: Option<RationalNumber>, c: Option<RationalNumber>) -> Lines {
    [Some(a.to_f64()), b.map(RationalNumber::to_f64), c.map(RationalNumber::to_f64)]
}

fn rational(x: Option<f64>) -> Option<RationalNumber> {
    x.map(RationalNumber::from_f64)
}

/// What a vehicle announced in its charge parameter discovery (V2G20-2198, 2199 hold the charge
/// loop to it).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Announced {
    max_charge: Lines,
    min_charge: Lines,
}

impl Evse {
    fn ac_lines_offered(&self) -> usize {
        if self.limits().ac_phases >= 3 { 3 } else { 1 }
    }

    /// A per-line limit for the lines the charger has.
    fn per_line(&self, watts: u32) -> (RationalNumber, Option<RationalNumber>, Option<RationalNumber>) {
        let w = RationalNumber::from_f64(f64::from(watts));
        (w, (self.ac_lines_offered() == 3).then_some(w), (self.ac_lines_offered() == 3).then_some(w))
    }

    pub(crate) fn ac_cpd_res(&self, code: ResponseCode) -> ac::AcChargeParameterDiscoveryRes<'static> {
        let l = self.limits();
        let (max_1, max_2, max_3) = self.per_line(l.ac_max_charge_power_w);
        let (min_1, min_2, min_3) = self.per_line(l.ac_min_charge_power_w);
        let base = ac::AcCpdResEnergyTransferMode {
            evse_maximum_charge_power: max_1,
            evse_maximum_charge_power_l2: max_2,
            evse_maximum_charge_power_l3: max_3,
            evse_minimum_charge_power: min_1,
            evse_minimum_charge_power_l2: min_2,
            evse_minimum_charge_power_l3: min_3,
            evse_nominal_frequency: RationalNumber::from_f64(f64::from(l.ac_nominal_frequency_hz)),
            maximum_power_asymmetry: None,
            evse_power_ramp_limitation: l.power_ramp_limitation_percent_per_min.map(|p| RationalNumber::from_f64(f64::from(p))),
            evse_present_active_power: None,
            evse_present_active_power_l2: None,
            evse_present_active_power_l3: None,
        };
        let mode = if self.bpt {
            let (dmax_1, dmax_2, dmax_3) = self.per_line(l.ac_max_discharge_power_w);
            let (dmin_1, dmin_2, dmin_3) = self.per_line(l.ac_min_discharge_power_w);
            ac::AcCpdResEnergyTransferModeChoice::Bpt(ac::BptAcCpdResEnergyTransferMode {
                evse_maximum_charge_power: base.evse_maximum_charge_power,
                evse_maximum_charge_power_l2: base.evse_maximum_charge_power_l2,
                evse_maximum_charge_power_l3: base.evse_maximum_charge_power_l3,
                evse_minimum_charge_power: base.evse_minimum_charge_power,
                evse_minimum_charge_power_l2: base.evse_minimum_charge_power_l2,
                evse_minimum_charge_power_l3: base.evse_minimum_charge_power_l3,
                evse_nominal_frequency: base.evse_nominal_frequency,
                maximum_power_asymmetry: base.maximum_power_asymmetry,
                evse_power_ramp_limitation: base.evse_power_ramp_limitation,
                evse_present_active_power: None,
                evse_present_active_power_l2: None,
                evse_present_active_power_l3: None,
                evse_maximum_discharge_power: dmax_1,
                evse_maximum_discharge_power_l2: dmax_2,
                evse_maximum_discharge_power_l3: dmax_3,
                evse_minimum_discharge_power: dmin_1,
                evse_minimum_discharge_power_l2: dmin_2,
                evse_minimum_discharge_power_l3: dmin_3,
            })
        } else {
            ac::AcCpdResEnergyTransferModeChoice::Base(base)
        };
        ac::AcChargeParameterDiscoveryRes { header: self.header(), response_code: code, ac_cpd_res_energy_transfer_mode: mode }
    }

    fn note_ac_announced(&mut self, mode: &ac::AcCpdReqEnergyTransferModeChoice) {
        let (max_charge, min_charge) = match mode {
            ac::AcCpdReqEnergyTransferModeChoice::Base(m) => (
                lines(m.ev_maximum_charge_power, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3),
                lines(m.ev_minimum_charge_power, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3),
            ),
            ac::AcCpdReqEnergyTransferModeChoice::Bpt(m) => (
                lines(m.ev_maximum_charge_power, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3),
                lines(m.ev_minimum_charge_power, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3),
            ),
        };
        self.ac_announced = Some(Announced { max_charge, min_charge });
    }

    /// `AC_ChargeParameterDiscoveryReq`: answered with the charger's limits per line. Also the
    /// first message of a schedule renegotiation (`side`).
    pub(super) fn on_ac_cpd(&mut self, payload: &[u8], arena: &Arena<'_>, side: bool) -> Output {
        let Ok(ac::Message::AcChargeParameterDiscoveryReq(req)) = ac::decode(payload, arena) else { return Output { send: None, close: true } };
        self.note_ac_announced(&req.ac_cpd_req_energy_transfer_mode);
        let wrong_mode = matches!(req.ac_cpd_req_energy_transfer_mode, ac::AcCpdReqEnergyTransferModeChoice::Bpt(_)) != self.bpt;
        let code = if req.header.session_id != self.session_id {
            ResponseCode::FailedUnknownSession
        } else if wrong_mode {
            // Table 226: the mode has to be the one of the selected service.
            ResponseCode::FailedWrongChargeParameter
        } else {
            self.config.cpd_code
        };
        let res = self.ac_cpd_res(code);
        if side {
            self.side_schedule_reqs_seen = 0;
        } else if is_ok(code) {
            self.state = State::AwaitScheduleExchangeReq;
        }
        let payload_type = if side { PayloadType::SCHEDULE_RENEGOTIATION } else { PayloadType::AC_MAINSTREAM };
        Output { send: frame(payload_type, |out| res.encode(out)), close: !is_ok(code) }
    }

    /// Notes the values of a charge loop request that break V2G20-2198 or 2199; false when there
    /// were any.
    fn check_ac_loop_limits(&mut self, max_charge: Lines, min_charge: Lines) -> bool {
        let Some(a) = self.ac_announced else { return true };
        let mut fine = true;
        for (line, (sent, announced)) in max_charge.iter().zip(&a.max_charge).enumerate() {
            if let (Some(sent), Some(announced)) = (sent, announced)
                && sent > announced
            {
                self.note(format!("violation: V2G20-2198: EVMaximumChargePower line {} is {sent}, the discovery announced {announced}", line + 1));
                fine = false;
            }
        }
        for (line, (sent, announced)) in min_charge.iter().zip(&a.min_charge).enumerate() {
            if let (Some(sent), Some(announced)) = (sent, announced)
                && sent < announced
            {
                self.note(format!("violation: V2G20-2199: EVMinimumChargePower line {} is {sent}, the discovery announced {announced}", line + 1));
                fine = false;
            }
        }
        fine
    }

    /// `AC_ChargeLoopReq`: the target active power per line is what both sides allow, negative
    /// when the vehicle asks to discharge (a negative `EVTargetEnergyRequest`, as in DC).
    pub(super) fn on_ac_loop(&mut self, payload: &[u8], arena: &Arena<'_>) -> Output {
        let Ok(ac::Message::AcChargeLoopReq(req)) = ac::decode(payload, arena) else { return Output { send: None, close: true } };
        self.loops_seen += 1;
        let matching = matches!(
            (&req.cl_req_control_mode, self.bpt),
            (ac::ClReqControlModeChoice::DynamicAc(_), false) | (ac::ClReqControlModeChoice::BptDynamicAc(_), true)
        );
        let mut code = if req.header.session_id != self.session_id {
            ResponseCode::FailedUnknownSession
        } else if !matching {
            // Table 226: FAILED_WrongChargeParameter is for the parameter discovery only.
            ResponseCode::Failed
        } else {
            ResponseCode::Ok
        };
        let notification = self.loop_notification();
        let l = self.limits();
        // What the vehicle asks for, per line.
        let (target_energy, ev_max, ev_min, ev_discharge_max) = match &req.cl_req_control_mode {
            ac::ClReqControlModeChoice::DynamicAc(m) => (
                m.ev_target_energy_request.to_f64(),
                lines(m.ev_maximum_charge_power, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3),
                lines(m.ev_minimum_charge_power, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3),
                [None; 3],
            ),
            ac::ClReqControlModeChoice::BptDynamicAc(m) => (
                m.ev_target_energy_request.to_f64(),
                lines(m.ev_maximum_charge_power, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3),
                lines(m.ev_minimum_charge_power, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3),
                lines(m.ev_maximum_discharge_power, m.ev_maximum_discharge_power_l2, m.ev_maximum_discharge_power_l3),
            ),
            _ => (0.0, [None; 3], [None; 3], [None; 3]),
        };
        if !self.check_ac_loop_limits(ev_max, ev_min) && self.config.fail_on_limit_violation {
            code = ResponseCode::Failed;
        }
        let discharging = self.bpt && target_energy < 0.0;
        let offered = self.ac_lines_offered();
        let mut target: Lines = [None; 3];
        for line in 0..offered {
            // A line the vehicle does not use carries nothing.
            let Some(vehicle) = (if discharging { ev_discharge_max[line] } else { ev_max[line] }) else { continue };
            let charger = f64::from(if discharging { l.ac_max_discharge_power_w } else { l.ac_max_charge_power_w });
            let minimum = f64::from(if discharging { l.ac_min_discharge_power_w } else { l.ac_min_charge_power_w });
            let power = vehicle.min(charger);
            // Below the minimum the charger cannot deliver with the expected accuracy.
            let power = if power < minimum { 0.0 } else { power };
            target[line] = Some(if discharging { -power } else { power });
        }
        let used = target.iter().flatten().count().max(1) as f64;
        let total: f64 = target.iter().flatten().sum();
        let voltage = self.ac_voltage;
        // A test platform may set the current by hand: the power follows, evenly over the lines.
        let max_total = f64::from(l.ac_max_charge_power_w.max(l.ac_max_discharge_power_w)) * offered as f64;
        let (shown_voltage, current, max_power): (f64, f64, f64) = match &self.live {
            Some(live) => live.ac_charge_loop(voltage, total / voltage, max_total),
            None => (voltage, total / voltage, max_total),
        };
        let total_now = (current * shown_voltage).clamp(-max_power.abs(), max_power.abs());
        if (total_now - total).abs() > 1e-6 {
            for t in target.iter_mut().flatten() {
                *t = total_now / used;
            }
        }
        let present = target;
        let status = notification.map(|(evse_notification, notification_max_delay)| common_types::EvseStatus { notification_max_delay, evse_notification });
        let mode = ac::DynamicAcClResControlMode {
            departure_time: None,
            minimum_soc: None,
            target_soc: None,
            ack_max_delay: None,
            evse_target_active_power: RationalNumber::from_f64(target[0].unwrap_or(0.0)),
            evse_target_active_power_l2: rational(target[1]),
            evse_target_active_power_l3: rational(target[2]),
            evse_target_reactive_power: None,
            evse_target_reactive_power_l2: None,
            evse_target_reactive_power_l3: None,
            evse_present_active_power: rational(present[0]),
            evse_present_active_power_l2: rational(present[1]),
            evse_present_active_power_l3: rational(present[2]),
        };
        let res = ac::AcChargeLoopRes {
            header: self.header(),
            response_code: code,
            evse_status: status,
            meter_info: None,
            receipt: None,
            evse_target_frequency: None,
            cl_res_control_mode: if self.bpt {
                ac::ClResControlModeChoice::BptDynamicAc(ac::BptDynamicAcClResControlMode {
                    departure_time: mode.departure_time,
                    minimum_soc: mode.minimum_soc,
                    target_soc: mode.target_soc,
                    ack_max_delay: mode.ack_max_delay,
                    evse_target_active_power: mode.evse_target_active_power,
                    evse_target_active_power_l2: mode.evse_target_active_power_l2,
                    evse_target_active_power_l3: mode.evse_target_active_power_l3,
                    evse_target_reactive_power: mode.evse_target_reactive_power,
                    evse_target_reactive_power_l2: mode.evse_target_reactive_power_l2,
                    evse_target_reactive_power_l3: mode.evse_target_reactive_power_l3,
                    evse_present_active_power: mode.evse_present_active_power,
                    evse_present_active_power_l2: mode.evse_present_active_power_l2,
                    evse_present_active_power_l3: mode.evse_present_active_power_l3,
                })
            } else {
                ac::ClResControlModeChoice::DynamicAc(mode)
            },
        };
        Output { send: frame(PayloadType::AC_MAINSTREAM, |out| res.encode(out)), close: !is_ok(code) }
    }
}
