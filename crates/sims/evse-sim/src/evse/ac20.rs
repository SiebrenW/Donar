//! The ISO 15118-20 AC side of the simulated charger: `AC_ChargeParameterDiscovery` and the
//! `AC_ChargeLoop` in Dynamic control mode (services AC and AC_BPT). Power is per line, with
//! lines 2 and 3 optional; a negative target active power is a discharge (BPT).

use exi::Arena;
use iso15118_20::ac;
use iso15118_20::common::{self, ChargingSchedule, EvpptControlModeChoice, PowerSchedule, PowerScheduleEntry, PowerScheduleEntryList, ScheduleTuple};
use iso15118_20::common_types::{self, RationalNumber, ResponseCode};
use v2gtp::PayloadType;

use super::{Evse, Output, State, frame, is_ok};

/// Service ids of the AC services (Table 204).
pub(super) const SERVICE_ID_AC: u16 = 1;
pub(super) const SERVICE_ID_AC_BPT: u16 = 5;

/// The schedule tuples a scheduled session offers: 1 is the charger's full power for an entry
/// (`schedule_entry_s`) and then half of it, 2 half of it throughout.
const TUPLES: [u32; 2] = [1, 2];

/// A power per line, in watts; a line the charger or the vehicle does not use is `None`.
type Lines = [Option<f64>; 3];

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
    /// The lines of the session: three when the selected parameter set has the ThreePhase connector
    /// and the charger has them.
    pub(super) fn ac_lines_offered(&self) -> usize {
        if self.ac_three && self.limits().ac_phases >= 3 { 3 } else { 1 }
    }

    /// Whether the charger answers with the sum of the lines in the base elements: on three lines
    /// for a vehicle that gave no `_L2`, `_L3` values (V2G20-1817, 1209).
    fn sum_mode(&self, asymmetric: bool) -> bool {
        self.ac_lines_offered() == 3 && !asymmetric
    }

    /// A value of the vehicle per line: with `_L2` and `_L3` as they are, on three lines without them
    /// the base is the sum, spread evenly (V2G20-1817).
    fn in_lines(&self, base: RationalNumber, l2: Option<RationalNumber>, l3: Option<RationalNumber>) -> Lines {
        if self.ac_lines_offered() == 3 && l2.is_none() && l3.is_none() {
            [Some(base.to_f64() / 3.0); 3]
        } else {
            [Some(base.to_f64()), l2.map(RationalNumber::to_f64), l3.map(RationalNumber::to_f64)]
        }
    }

    /// A per-line limit of the charger as it goes into a message to this vehicle.
    fn per_line(&self, watts: u32) -> (RationalNumber, Option<RationalNumber>, Option<RationalNumber>) {
        let w = f64::from(watts);
        if self.sum_mode(self.ac_asym) {
            (RationalNumber::from_f64(w * 3.0), None, None)
        } else {
            let each = RationalNumber::from_f64(w);
            (each, (self.ac_lines_offered() == 3).then_some(each), (self.ac_lines_offered() == 3).then_some(each))
        }
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
        let (max, min) = match mode {
            ac::AcCpdReqEnergyTransferModeChoice::Base(m) => (
                (m.ev_maximum_charge_power, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3),
                (m.ev_minimum_charge_power, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3),
            ),
            ac::AcCpdReqEnergyTransferModeChoice::Bpt(m) => (
                (m.ev_maximum_charge_power, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3),
                (m.ev_minimum_charge_power, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3),
            ),
        };
        // Per-line values announced by a vehicle that gave `_L2` or `_L3` (V2G20-1821); otherwise sums.
        self.ac_asym = max.1.is_some() || max.2.is_some();
        let (max_charge, min_charge) = (self.in_lines(max.0, max.1, max.2), self.in_lines(min.0, min.1, min.2));
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
            (&req.cl_req_control_mode, self.bpt, self.scheduled),
            (ac::ClReqControlModeChoice::DynamicAc(_), false, false)
                | (ac::ClReqControlModeChoice::BptDynamicAc(_), true, false)
                | (ac::ClReqControlModeChoice::ScheduledAc(_), false, true)
                | (ac::ClReqControlModeChoice::BptScheduledAc(_), true, true)
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
                self.in_lines(m.ev_maximum_charge_power, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3),
                self.in_lines(m.ev_minimum_charge_power, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3),
                [None; 3],
            ),
            ac::ClReqControlModeChoice::BptDynamicAc(m) => (
                m.ev_target_energy_request.to_f64(),
                self.in_lines(m.ev_maximum_charge_power, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3),
                self.in_lines(m.ev_minimum_charge_power, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3),
                self.in_lines(m.ev_maximum_discharge_power, m.ev_maximum_discharge_power_l2, m.ev_maximum_discharge_power_l3),
            ),
            // Scheduled: what the vehicle left out it does not limit; no target energy is a charge.
            ac::ClReqControlModeChoice::ScheduledAc(m) => (
                m.ev_target_energy_request.map_or(1.0, RationalNumber::to_f64),
                m.ev_maximum_charge_power.map_or([None; 3], |b| self.in_lines(b, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3)),
                m.ev_minimum_charge_power.map_or([None; 3], |b| self.in_lines(b, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3)),
                [None; 3],
            ),
            ac::ClReqControlModeChoice::BptScheduledAc(m) => (
                m.ev_target_energy_request.map_or(1.0, RationalNumber::to_f64),
                m.ev_maximum_charge_power.map_or([None; 3], |b| self.in_lines(b, m.ev_maximum_charge_power_l2, m.ev_maximum_charge_power_l3)),
                m.ev_minimum_charge_power.map_or([None; 3], |b| self.in_lines(b, m.ev_minimum_charge_power_l2, m.ev_minimum_charge_power_l3)),
                m.ev_maximum_discharge_power.map_or([None; 3], |b| self.in_lines(b, m.ev_maximum_discharge_power_l2, m.ev_maximum_discharge_power_l3)),
            ),
        };
        if !self.check_ac_loop_limits(ev_max, ev_min) && self.config.fail_on_limit_violation {
            code = ResponseCode::Failed;
        }
        // The lines the vehicle says it draws on (Scheduled mode may leave its limits out).
        let raw_lines: [bool; 3] = match &req.cl_req_control_mode {
            ac::ClReqControlModeChoice::ScheduledAc(m) => [true, m.ev_present_active_power_l2.is_some(), m.ev_present_active_power_l3.is_some()],
            ac::ClReqControlModeChoice::BptScheduledAc(m) => [true, m.ev_present_active_power_l2.is_some(), m.ev_present_active_power_l3.is_some()],
            ac::ClReqControlModeChoice::DynamicAc(m) => [true, m.ev_present_active_power_l2.is_some(), m.ev_present_active_power_l3.is_some()],
            ac::ClReqControlModeChoice::BptDynamicAc(m) => [true, m.ev_present_active_power_l2.is_some(), m.ev_present_active_power_l3.is_some()],
        };
        // A vehicle without `_L2`, `_L3` values on three lines is symmetric: sums, no phase elements in the answer (V2G20-1209).
        let sum_mode = self.sum_mode(raw_lines[1] || raw_lines[2]);
        let present_lines = if sum_mode { [true; 3] } else { raw_lines };
        let discharging = self.bpt && target_energy < 0.0;
        let offered = self.ac_lines_offered();
        let mut target: Lines = [None; 3];
        for line in 0..offered {
            // Dynamic: a line the vehicle does not use carries nothing. Scheduled: the vehicle's limits
            // are optional, and the schedule's entry is what the charger asks for.
            let vehicle = if self.scheduled {
                let own = if discharging { ev_discharge_max[line] } else { ev_max[line] };
                // Line 1 always; the others where the vehicle has them (it reported a present power).
                if !present_lines[line] {
                    continue;
                }
                own.unwrap_or(f64::MAX)
            } else {
                let Some(v) = (if discharging { ev_discharge_max[line] } else { ev_max[line] }) else { continue };
                v
            };
            let charger = if self.scheduled { self.scheduled_power(discharging) } else { f64::from(if discharging { l.ac_max_discharge_power_w } else { l.ac_max_charge_power_w }) };
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
        let overridden = (total_now - total).abs() > 1e-6;
        let present = target;
        // The answer in sums for a symmetric vehicle.
        let (target, present) = if sum_mode {
            let sum = |t: Lines| [Some(t.iter().flatten().sum::<f64>()), None, None];
            (sum(target), sum(present))
        } else {
            (target, present)
        };
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
            cl_res_control_mode: if self.scheduled {
                // V2G20-1826: a target only where something overrides the schedule (a hand-set current).
                let ad_hoc = overridden;
                let scheduled = ac::ScheduledAcClResControlMode {
                    evse_target_active_power: ad_hoc.then(|| RationalNumber::from_f64(target[0].unwrap_or(0.0))),
                    evse_target_active_power_l2: if ad_hoc { rational(target[1]) } else { None },
                    evse_target_active_power_l3: if ad_hoc { rational(target[2]) } else { None },
                    evse_target_reactive_power: None,
                    evse_target_reactive_power_l2: None,
                    evse_target_reactive_power_l3: None,
                    evse_present_active_power: rational(present[0]),
                    evse_present_active_power_l2: rational(present[1]),
                    evse_present_active_power_l3: rational(present[2]),
                };
                if self.bpt {
                    ac::ClResControlModeChoice::BptScheduledAc(ac::BptScheduledAcClResControlMode {
                        evse_target_active_power: scheduled.evse_target_active_power,
                        evse_target_active_power_l2: scheduled.evse_target_active_power_l2,
                        evse_target_active_power_l3: scheduled.evse_target_active_power_l3,
                        evse_target_reactive_power: None,
                        evse_target_reactive_power_l2: None,
                        evse_target_reactive_power_l3: None,
                        evse_present_active_power: scheduled.evse_present_active_power,
                        evse_present_active_power_l2: scheduled.evse_present_active_power_l2,
                        evse_present_active_power_l3: scheduled.evse_present_active_power_l3,
                    })
                } else {
                    ac::ClResControlModeChoice::ScheduledAc(scheduled)
                }
            } else if self.bpt {
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

    /// The entries of a tuple's schedule, per line: `(duration, watts)`; the discharging schedule
    /// (bidirectional sessions) is the charger's discharge power.
    fn schedule_entries(&self, tuple: u32, discharging: bool) -> Vec<(u32, f64)> {
        let l = self.limits();
        let entry = self.config.schedule_entry_s.max(1);
        if discharging {
            return vec![(entry, f64::from(l.ac_max_discharge_power_w))];
        }
        let full = f64::from(l.ac_max_charge_power_w);
        if tuple == 1 { vec![(entry, full), (entry, full / 2.0)] } else { vec![(entry, full / 2.0)] }
    }

    /// The power per line the selected tuple asks for now, counted from the start of the power
    /// delivery; the last entry holds beyond the end.
    pub(super) fn scheduled_power(&self, discharging: bool) -> f64 {
        let elapsed = self.schedule_started.map_or(0, |t| t.elapsed().as_secs());
        let mut left = elapsed;
        let entries = self.schedule_entries(self.selected_tuple, discharging);
        for (duration, watts) in &entries {
            if left < u64::from(*duration) {
                return *watts;
            }
            left -= u64::from(*duration);
        }
        entries.last().map_or(0.0, |e| e.1)
    }

    /// `ScheduleExchangeRes`, framed: Dynamic, or in Scheduled mode the offered tuples.
    pub(super) fn schedule_frame(&self, payload_type: PayloadType, processing: common_types::Processing, code: ResponseCode) -> Option<Vec<u8>> {
        if !self.scheduled {
            let res = self.schedule_res(processing, code);
            return frame(payload_type, |out| res.encode(out));
        }
        // `TimeAnchor` in microseconds of SECC time (Annex J examples; 8.3.5.3.x says milliseconds in one place).
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64);
        let per_line = |watts: f64| {
            if self.sum_mode(self.ac_asym) {
                (RationalNumber::from_f64(watts * 3.0), None, None)
            } else {
                let each = RationalNumber::from_f64(watts);
                (each, (self.ac_lines_offered() == 3).then_some(each), (self.ac_lines_offered() == 3).then_some(each))
            }
        };
        let build = |tuple: u32, discharging: bool| -> Vec<PowerScheduleEntry> {
            self.schedule_entries(tuple, discharging)
                .into_iter()
                .map(|(duration, watts)| {
                    let (power, power_l2, power_l3) = per_line(watts);
                    PowerScheduleEntry { duration, power, power_l2, power_l3 }
                })
                .collect()
        };
        let charging: Vec<Vec<PowerScheduleEntry>> = TUPLES.iter().map(|&t| build(t, false)).collect();
        let discharging = build(1, true);
        fn schedule(now: u64, entries: &[PowerScheduleEntry]) -> ChargingSchedule<'_> {
            ChargingSchedule {
                power_schedule: PowerSchedule { time_anchor: now, available_energy: None, power_tolerance: None, power_schedule_entries: PowerScheduleEntryList { power_schedule_entry: entries } },
                schedule: None,
            }
        }
        let tuples: Vec<ScheduleTuple<'_>> = TUPLES
            .iter()
            .zip(&charging)
            .map(|(&id, entries)| ScheduleTuple { schedule_tuple_id: id, charging_schedule: schedule(now, entries), discharging_schedule: self.bpt.then(|| schedule(now, &discharging)) })
            .collect();
        let res = common::ScheduleExchangeRes {
            header: self.header(),
            response_code: code,
            evse_processing: processing,
            go_to_pause: None,
            se_res_control_mode: common::SeResControlModeChoice::Scheduled(common::ScheduledSeResControlMode { schedule_tuple: &tuples }),
        };
        frame(payload_type, |out| res.encode(out))
    }

    /// Whether a power delivery request's profile selects what the session is set up for: in
    /// Scheduled mode one of the offered tuples, in Dynamic mode no tuple. Notes the selection.
    pub(super) fn selects_offered_tuple(&mut self, req: &common::PowerDeliveryReq<'_>) -> bool {
        let Some(profile) = &req.ev_power_profile else { return true };
        match (&profile.evppt_control_mode, self.scheduled) {
            (EvpptControlModeChoice::Scheduled(m), true) if TUPLES.contains(&m.selected_schedule_tuple_id) => {
                self.selected_tuple = m.selected_schedule_tuple_id;
                true
            }
            (EvpptControlModeChoice::Dynamic(_), false) => true,
            _ => false,
        }
    }
}
