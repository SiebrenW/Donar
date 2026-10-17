//! The limits of the charge loop against those of the charge parameter discovery.
//!
//! ISO 15118-20:2022 8.3.4.5.5: the maximums (current, power, voltage) of a charge loop message
//! are at most the ones sent in the charge parameter discovery, the minimums at least
//! (V2G20-2198, 2199 for the vehicle, 2654, 2655 for the charger).

use iso15118_20::common_types::RationalNumber;
use iso15118_20::dc;

use super::Machine;
use super::config::rational_to_volts;
use crate::ev::{DcLimits, Ev};
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

impl<E: Ev, L: LogSink> Machine<E, L> {
    /// Remembers what the charger offered (the charge loop answers are held to it).
    pub(super) fn note_evse_limits(&mut self, mode: &dc::DcCpdResEnergyTransferModeChoice) {
        if let dc::DcCpdResEnergyTransferModeChoice::Base(m) = mode {
            self.evse_announced = Some(*m);
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
