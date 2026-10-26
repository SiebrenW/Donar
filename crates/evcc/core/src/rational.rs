//! `Rational`: how the vehicle describes a physical value to the core (`value * 10^exponent`, in the unit of the field
//! it is in: W, A, V, Wh, Hz, ...). Every protocol has its own type for it (ISO 15118-20 `RationalNumber`, the
//! `PhysicalValue` of DIN SPEC 70121 and ISO 15118-2); the machines convert at the edge, so the vehicle's code does not
//! depend on a codec. A host that works with floats uses [`Rational::from_f64`] and [`Rational::to_f64`].

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rational {
    pub exponent: i8,
    pub value: i16,
}

fn pow10(exponent: i8) -> f64 {
    let mut v = 1.0;
    for _ in 0..exponent.unsigned_abs() {
        v = if exponent < 0 { v / 10.0 } else { v * 10.0 };
    }
    v
}

impl Rational {
    pub const ZERO: Rational = Rational { exponent: 0, value: 0 };

    /// The number as `value * 10^exponent` with the smallest exponent (from -3) that lets the value fit 16 bits: exact
    /// for whole numbers that fit, rounded to the value's resolution otherwise, saturating at the largest
    /// representable number.
    pub fn from_f64(x: f64) -> Rational {
        let mut exponent: i8 = -3;
        loop {
            let q = x / pow10(exponent);
            let scaled = (if q < 0.0 { q - 0.5 } else { q + 0.5 }) as i64;
            if scaled.abs() <= i64::from(i16::MAX) {
                return Rational { exponent, value: scaled as i16 };
            }
            if exponent == 10 {
                return Rational { exponent, value: if x < 0.0 { i16::MIN } else { i16::MAX } };
            }
            exponent += 1;
        }
    }

    pub fn to_f64(self) -> f64 {
        f64::from(self.value) * pow10(self.exponent)
    }
}

impl From<f64> for Rational {
    fn from(x: f64) -> Self {
        Rational::from_f64(x)
    }
}

impl From<f32> for Rational {
    fn from(x: f32) -> Self {
        Rational::from_f64(f64::from(x))
    }
}

impl From<Rational> for f64 {
    fn from(r: Rational) -> f64 {
        r.to_f64()
    }
}

impl From<Rational> for f32 {
    fn from(r: Rational) -> f32 {
        r.to_f64() as f32
    }
}

#[cfg(feature = "iso20")]
impl From<Rational> for iso15118_20::common_types::RationalNumber {
    fn from(r: Rational) -> Self {
        Self { exponent: r.exponent, value: r.value }
    }
}

#[cfg(feature = "iso20")]
impl From<iso15118_20::common_types::RationalNumber> for Rational {
    fn from(r: iso15118_20::common_types::RationalNumber) -> Self {
        Self { exponent: r.exponent, value: r.value }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_keep_their_value() {
        for x in [0.0, 1.0, 400.0, 0.5, 150_000.0, 920.0, -20.0, 32.767, 12.345] {
            let r = Rational::from_f64(x);
            assert!((r.to_f64() - x).abs() <= 0.0005 + x.abs() * 1e-4, "{x} -> {r:?}");
        }
        assert_eq!(Rational::from_f64(400.0), Rational { exponent: -1, value: 4000 });
        assert_eq!(f32::from(Rational { exponent: 3, value: 11 }), 11_000.0);
    }

    #[test]
    fn huge_numbers_saturate() {
        assert_eq!(Rational::from_f64(1e30).value, i16::MAX);
    }
}
