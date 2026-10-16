//! `RationalNumber` (value × 10^exponent) from and to ordinary numbers.

use crate::common_types::RationalNumber;

fn pow10(exponent: i8) -> f64 {
    let mut v = 1.0;
    for _ in 0..exponent.unsigned_abs() {
        v = if exponent < 0 { v / 10.0 } else { v * 10.0 };
    }
    v
}

impl RationalNumber {
    /// The number as `value * 10^exponent` with the smallest exponent (from -3) that lets the
    /// value fit 16 bits; exact for whole numbers that fit, rounded to the value's resolution
    /// otherwise, saturating at the largest representable number.
    pub fn from_f64(x: f64) -> RationalNumber {
        let mut exponent: i8 = -3;
        loop {
            let q = x / pow10(exponent);
            let scaled = (if q < 0.0 { q - 0.5 } else { q + 0.5 }) as i64;
            if scaled.abs() <= i64::from(i16::MAX) {
                return RationalNumber { exponent, value: scaled as i16 };
            }
            if exponent == 10 {
                return RationalNumber { exponent, value: if x < 0.0 { i16::MIN } else { i16::MAX } };
            }
            exponent += 1;
        }
    }

    pub fn to_f64(self) -> f64 {
        f64::from(self.value) * pow10(self.exponent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_keep_their_value() {
        for x in [0.0, 1.0, 400.0, 0.5, 150_000.0, 920.0, -20.0, 32.767, 12.345] {
            let r = RationalNumber::from_f64(x);
            assert!((r.to_f64() - x).abs() <= 0.0005 + x.abs() * 1e-4, "{x} -> {r:?}");
        }
        assert_eq!(RationalNumber::from_f64(400.0), RationalNumber { exponent: -1, value: 4000 });
    }

    #[test]
    fn huge_numbers_saturate() {
        assert_eq!(RationalNumber::from_f64(1e30).value, i16::MAX);
    }
}
