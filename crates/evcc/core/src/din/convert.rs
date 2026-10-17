//! `PhysicalValue` (a short with a multiplier of -3 to 3) and the `RationalNumber` of the core.

use din70121::types::{PhysicalValue, UnitSymbol};
use iso15118_20::common_types::RationalNumber;

/// `r` as a physical value.
pub(crate) fn physical(r: RationalNumber, unit: UnitSymbol) -> PhysicalValue {
    PhysicalValue::from_f64(r.to_f64(), unit)
}

pub(crate) fn rational(p: PhysicalValue) -> RationalNumber {
    RationalNumber { exponent: p.multiplier, value: p.value }
}

pub(crate) fn number(p: PhysicalValue) -> f64 {
    rational(p).to_f64()
}

/// The power in watts as the `PMaxType` short of the schedule and the profile.
pub(crate) fn pmax(watts: f64) -> i16 {
    watts.clamp(0.0, f64::from(i16::MAX)) as i16
}

