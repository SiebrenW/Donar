//! `PhysicalValue` (a short with a multiplier of -3 to 3) and the `Rational` of the core.

use iso15118_2::types::{PhysicalValue, UnitSymbol};
use crate::rational::Rational;

/// `r` as a physical value.
pub(crate) fn physical(r: Rational, unit: UnitSymbol) -> PhysicalValue {
    PhysicalValue::from_f64(r.to_f64(), unit)
}

pub(crate) fn rational(p: PhysicalValue) -> Rational {
    Rational { exponent: p.multiplier, value: p.value }
}

pub(crate) fn number(p: PhysicalValue) -> f64 {
    rational(p).to_f64()
}
