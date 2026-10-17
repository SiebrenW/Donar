//! The limits of the charge loop against those of the charge parameter discovery.
//!
//! ISO 15118-20:2022 8.3.4.5.5: the maximums (current, power, voltage) of a charge loop message
//! are at most the ones sent in the charge parameter discovery, the minimums at least
//! (V2G20-2198, 2199 for the vehicle, 2654, 2655 for the charger).

use iso15118_20::common_types::RationalNumber;
use iso15118_20::ac;
use iso15118_20::dc;

use super::Machine;
use super::config::rational_to_volts;
use crate::ev::{AcDischargePowerLimits, AcPowerLimits, DcLimits, Ev, Phases};
use crate::log::{LogSink, Record};

fn value(r: RationalNumber) -> f32 {
    rational_to_volts(r)
}

/// The smaller of two quantities.
pub(super) fn smaller(a: RationalNumber, b: RationalNumber) -> RationalNumber {
    if value(a) <= value(b) { a } else { b }
}

/// The larger of two quantities.
pub(super) fn larger(a: RationalNumber, b: RationalNumber) -> RationalNumber {
    if value(a) >= value(b) { a } else { b }
}

/// Combines two per-phase quantities phase by phase with `pick`; a phase only one of them has is
/// left out (the request may not go beyond what was announced).
fn phases_with(a: Phases, b: Phases, pick: fn(RationalNumber, RationalNumber) -> RationalNumber) -> Phases {
    let both = |x: Option<RationalNumber>, y: Option<RationalNumber>| match (x, y) {
        (Some(x), Some(y)) => Some(pick(x, y)),
        _ => None,
    };
    Phases { l1: pick(a.l1, b.l1), l2: both(a.l2, b.l2), l3: both(a.l3, b.l3) }
}

impl<E: Ev, L: LogSink> Machine<E, L> {
    /// Remembers what the charger offered (the charge loop answers are held to it).
    pub(super) fn note_evse_limits(&mut self, mode: &dc::DcCpdResEnergyTransferModeChoice) {
        self.evse_announced = Some(match mode {
            dc::DcCpdResEnergyTransferModeChoice::Base(m) => *m,
            // The charge side of a BPT answer is the base answer.
            dc::DcCpdResEnergyTransferModeChoice::Bpt(m) => dc::DcCpdResEnergyTransferMode {
                evse_maximum_charge_power: m.evse_maximum_charge_power,
                evse_minimum_charge_power: m.evse_minimum_charge_power,
                evse_maximum_charge_current: m.evse_maximum_charge_current,
                evse_minimum_charge_current: m.evse_minimum_charge_current,
                evse_maximum_voltage: m.evse_maximum_voltage,
                evse_minimum_voltage: m.evse_minimum_voltage,
                evse_power_ramp_limitation: m.evse_power_ramp_limitation,
            },
        });
    }

    /// Remembers what the charger offered for AC.
    pub(super) fn note_evse_ac_limits(&mut self, mode: &ac::AcCpdResEnergyTransferModeChoice) {
        self.evse_announced_ac = Some(match mode {
            ac::AcCpdResEnergyTransferModeChoice::Base(m) => *m,
            ac::AcCpdResEnergyTransferModeChoice::Bpt(m) => ac::AcCpdResEnergyTransferMode {
                evse_maximum_charge_power: m.evse_maximum_charge_power,
                evse_maximum_charge_power_l2: m.evse_maximum_charge_power_l2,
                evse_maximum_charge_power_l3: m.evse_maximum_charge_power_l3,
                evse_minimum_charge_power: m.evse_minimum_charge_power,
                evse_minimum_charge_power_l2: m.evse_minimum_charge_power_l2,
                evse_minimum_charge_power_l3: m.evse_minimum_charge_power_l3,
                evse_nominal_frequency: m.evse_nominal_frequency,
                maximum_power_asymmetry: m.maximum_power_asymmetry,
                evse_power_ramp_limitation: m.evse_power_ramp_limitation,
                evse_present_active_power: m.evse_present_active_power,
                evse_present_active_power_l2: m.evse_present_active_power_l2,
                evse_present_active_power_l3: m.evse_present_active_power_l3,
            },
        });
    }

    /// The vehicle's AC power limits for a charge loop request: the host's, and within what was
    /// announced in the charge parameter discovery (V2G20-2198, 2199).
    pub(super) fn loop_ac_limits(&self) -> AcPowerLimits {
        let now = self.ev.ac20_limits();
        match (self.config.enforce_cpd_limits, self.ev_announced_ac) {
            (true, Some(then)) => AcPowerLimits {
                maximum_charge_power: phases_with(now.maximum_charge_power, then.maximum_charge_power, smaller),
                minimum_charge_power: phases_with(now.minimum_charge_power, then.minimum_charge_power, larger),
            },
            _ => now,
        }
    }

    pub(super) fn loop_ac_discharge_limits(&self) -> AcDischargePowerLimits {
        let now = self.ev.ac20_discharge_limits();
        match (self.config.enforce_cpd_limits, self.ev_announced_ac_discharge) {
            (true, Some(then)) => AcDischargePowerLimits {
                maximum_discharge_power: phases_with(now.maximum_discharge_power, then.maximum_discharge_power, smaller),
                minimum_discharge_power: phases_with(now.minimum_discharge_power, then.minimum_discharge_power, larger),
            },
            _ => now,
        }
    }

    /// The vehicle's limits for a charge loop request: the host's, and within what was announced
    /// in the charge parameter discovery when [`super::Config::enforce_cpd_limits`] is on.
    pub(super) fn loop_limits(&self) -> DcLimits {
        let now = self.ev.dc_limits();
        match (self.config.enforce_cpd_limits, self.ev_announced) {
            (true, Some(then)) => DcLimits {
                maximum_charge_power: smaller(now.maximum_charge_power, then.maximum_charge_power),
                minimum_charge_power: larger(now.minimum_charge_power, then.minimum_charge_power),
                maximum_charge_current: smaller(now.maximum_charge_current, then.maximum_charge_current),
                minimum_charge_current: larger(now.minimum_charge_current, then.minimum_charge_current),
                maximum_voltage: smaller(now.maximum_voltage, then.maximum_voltage),
                minimum_voltage: larger(now.minimum_voltage, then.minimum_voltage),
                target_soc: now.target_soc,
            },
            _ => now,
        }
    }

    /// The vehicle's discharge limits for a BPT charge loop request: the host's, and within what
    /// was announced in the charge parameter discovery (V2G20-2198, 2199 hold for them too).
    pub(super) fn loop_discharge_limits(&self) -> crate::ev::DischargeLimits {
        let now = self.ev.dc_discharge_limits();
        match (self.config.enforce_cpd_limits, self.ev_announced_discharge) {
            (true, Some(then)) => crate::ev::DischargeLimits {
                maximum_discharge_power: smaller(now.maximum_discharge_power, then.maximum_discharge_power),
                minimum_discharge_power: larger(now.minimum_discharge_power, then.minimum_discharge_power),
                maximum_discharge_current: smaller(now.maximum_discharge_current, then.maximum_discharge_current),
                minimum_discharge_current: larger(now.minimum_discharge_current, then.minimum_discharge_current),
            },
            _ => now,
        }
    }

    /// Notes the values of a charge loop answer that break V2G20-2654 or 2655.
    pub(super) fn check_evse_loop_limits(&mut self, mode: &dc::DynamicDcClResControlMode) {
        let Some(offer) = self.evse_announced else { return };
        let checks = [
            ("V2G20-2654", "EVSEMaximumChargePower", mode.evse_maximum_charge_power, offer.evse_maximum_charge_power, true),
            ("V2G20-2654", "EVSEMaximumChargeCurrent", mode.evse_maximum_charge_current, offer.evse_maximum_charge_current, true),
            ("V2G20-2654", "EVSEMaximumVoltage", mode.evse_maximum_voltage, offer.evse_maximum_voltage, true),
            ("V2G20-2655", "EVSEMinimumChargePower", mode.evse_minimum_charge_power, offer.evse_minimum_charge_power, false),
        ];
        for (requirement, item, sent, offered, is_maximum) in checks {
            let broken = if is_maximum { value(sent) > value(offered) } else { value(sent) < value(offered) };
            if broken {
                self.log.record(&Record::Violation { requirement, item, value: sent, limit: offered });
            }
        }
    }
}
