//! `PhysicalValue`: a short and a multiplier of 10 (-3 to 3), the numbers of the DIN SPEC 70121
//! messages.

use crate::types::{PhysicalValue, UnitSymbol};

fn pow10(exp: i8) -> f64 {
    let mut v = 1.0;
    for _ in 0..exp.unsigned_abs() {
        v = if exp < 0 { v / 10.0 } else { v * 10.0 };
    }
    v
}

/// `x` to the nearest integer (halves away from zero), which `core` cannot do on its own.
fn round(x: f64) -> f64 {
    (if x < 0.0 { x - 0.5 } else { x + 0.5 }) as i64 as f64
}

impl PhysicalValue {
    /// The value `x` in `unit`: the multiplier nearest to zero that keeps the number exact, else
    /// the one that keeps the most digits. Out of range, the value saturates.
    pub fn from_f64(x: f64, unit: UnitSymbol) -> PhysicalValue {
        let scaled = |m: i8| round(x / pow10(m));
        let fits = |v: f64| v >= f64::from(i16::MIN) && v <= f64::from(i16::MAX);
        let multiplier = [0, 1, 2, 3, -1, -2, -3]
            .into_iter()
            .find(|&m| {
                let v = scaled(m);
                fits(v) && (v * pow10(m) - x).abs() <= x.abs() * 1e-9
            })
            .or_else(|| (-3..=3).find(|&m| fits(scaled(m))))
            .unwrap_or(3);
        PhysicalValue { multiplier, unit, value: scaled(multiplier).clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16 }
    }

    /// `Value * 10^Multiplier`.
    pub fn to_f64(self) -> f64 {
        f64::from(self.value) * pow10(self.multiplier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_keep_their_value() {
        for x in [0.0, 1.0, 400.0, 150_000.0, 920.0, 0.5, 32.767, -20.0, 3.0] {
            let p = PhysicalValue::from_f64(x, UnitSymbol::W);
            assert!((p.to_f64() - x).abs() <= 1e-9 + x.abs() * 1e-9, "{x}: {p:?}");
        }
        assert_eq!(PhysicalValue::from_f64(400.0, UnitSymbol::V).multiplier, 0);
        assert_eq!(PhysicalValue::from_f64(150_000.0, UnitSymbol::W).multiplier, 1);
        assert_eq!(PhysicalValue::from_f64(0.5, UnitSymbol::A).multiplier, -1);
    }

    #[test]
    fn out_of_range_saturates() {
        let p = PhysicalValue::from_f64(1e9, UnitSymbol::W);
        assert_eq!((p.multiplier, p.value), (3, i16::MAX));
    }
}
