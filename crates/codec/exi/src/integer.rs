//! `xs:integer` of any size (EXI 7.1.5: a sign bit, then an unsigned integer in 7-bit groups,
//! the magnitude minus one for negative values). The one schema use is the serial number of
//! an X.509 certificate (`X509SerialNumber`): RFC 5280 limits it to 20 octets, which a native
//! integer type cannot hold, so the value is kept as a big-endian magnitude. Nothing here
//! limits the size apart from the buffers.

use crate::bits::{BitReader, BitWriter};
use crate::{Arena, DecodeError, EncodeError};

/// A sign and a big-endian magnitude without leading zero octets. Zero is the empty magnitude
/// and never negative, so equal values compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Integer<'a> {
    negative: bool,
    magnitude: &'a [u8],
}

impl<'a> Integer<'a> {
    pub const ZERO: Integer<'static> = Integer { negative: false, magnitude: &[] };

    /// From a sign and a big-endian magnitude; leading zero octets are dropped.
    pub fn new(negative: bool, magnitude: &'a [u8]) -> Self {
        let start = magnitude.iter().position(|&b| b != 0).unwrap_or(magnitude.len());
        let magnitude = &magnitude[start..];
        Integer { negative: negative && !magnitude.is_empty(), magnitude }
    }

    /// A non-negative value, e.g. a certificate serial number as DER content octets (a leading
    /// zero octet that keeps the DER value positive is dropped).
    pub fn unsigned(magnitude: &'a [u8]) -> Self {
        Self::new(false, magnitude)
    }

    pub fn is_negative(&self) -> bool {
        self.negative
    }

    /// The absolute value, big endian, no leading zero octets (empty for zero).
    pub fn magnitude(&self) -> &'a [u8] {
        self.magnitude
    }

    /// The value as a `u64` if it is non-negative and fits.
    pub fn to_u64(&self) -> Option<u64> {
        if self.negative || self.magnitude.len() > 8 {
            return None;
        }
        Some(self.magnitude.iter().fold(0u64, |v, &b| v << 8 | u64::from(b)))
    }
}

/// The unsigned integer EXI codes for `value`: the magnitude, or for a negative value the
/// magnitude minus one. Octet `i` counts from the least significant end.
struct Coded<'a> {
    magnitude: &'a [u8],
    /// Octets of the magnitude below the first non-zero one (the borrow of "minus one" runs
    /// through them); only used when `minus_one`.
    zeros: usize,
    minus_one: bool,
}

impl Coded<'_> {
    fn octet(&self, i: usize) -> u8 {
        let n = self.magnitude.len();
        if i >= n {
            return 0;
        }
        let b = self.magnitude[n - 1 - i];
        if !self.minus_one || i > self.zeros {
            b
        } else if i < self.zeros {
            0xff
        } else {
            b - 1
        }
    }

    /// Bits `7 * group .. 7 * group + 7`.
    fn group(&self, group: usize) -> u8 {
        let bit = 7 * group;
        let (i, shift) = (bit / 8, bit % 8);
        let word = u16::from(self.octet(i)) | u16::from(self.octet(i + 1)) << 8;
        (word >> shift) as u8 & 0x7f
    }

    /// The number of 7-bit groups needed (at least one).
    fn groups(&self) -> usize {
        let top = (0..self.magnitude.len()).rev().find(|&i| self.octet(i) != 0);
        match top {
            None => 1,
            Some(i) => (8 * i + 8 - self.octet(i).leading_zeros() as usize).div_ceil(7),
        }
    }
}

impl Integer<'_> {
    fn coded(&self) -> Coded<'_> {
        let zeros = self.magnitude.iter().rev().take_while(|&&b| b == 0).count();
        Coded { magnitude: self.magnitude, zeros, minus_one: self.negative }
    }

    /// Writes the sign bit and the groups.
    pub(crate) fn write(&self, w: &mut BitWriter<'_>) -> Result<(), EncodeError> {
        w.bit(self.negative)?;
        let coded = self.coded();
        let groups = coded.groups();
        for g in 0..groups {
            let more = if g + 1 < groups { 0x80 } else { 0 };
            w.bits(u32::from(coded.group(g) | more), 8)?;
        }
        Ok(())
    }
}

/// Reads the sign bit and the groups into a big-endian magnitude allocated from `arena`
/// (without an arena the value is validated and skipped, and zero is returned).
pub(crate) fn read<'s>(r: &mut BitReader<'_>, arena: Option<&'s Arena<'_>>) -> Result<Integer<'s>, DecodeError> {
    let negative = r.bit()?;
    let mut scan = r.clone();
    let mut groups = 1usize;
    while scan.bits(8)? & 0x80 != 0 {
        groups += 1;
    }
    let Some(arena) = arena else {
        *r = scan;
        return Ok(Integer::ZERO);
    };
    // One octet more than the groups need: adding one to a negative value may carry out.
    let len = (7 * groups).div_ceil(8) + 1;
    let out = arena.alloc_bytes(len)?;
    for g in 0..groups {
        let group = u16::from(r.bits(8)? as u8 & 0x7f);
        let bit = 7 * g;
        let word = group << (bit % 8);
        // Octet `bit / 8` counted from the least significant end, and the one above it.
        out[len - 1 - bit / 8] |= word as u8;
        out[len - 2 - bit / 8] |= (word >> 8) as u8;
    }
    if negative {
        for b in out.iter_mut().rev() {
            let (v, carry) = b.overflowing_add(1);
            *b = v;
            if !carry {
                break;
            }
        }
    }
    Ok(Integer::new(negative, out))
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::vec::Vec;

    /// The bits as written: the sign bit, then the groups as octets.
    fn encode(v: Integer<'_>) -> Vec<u8> {
        let mut buf = [0u8; 128];
        let mut w = BitWriter::new(&mut buf);
        v.write(&mut w).unwrap();
        let n = w.finish().unwrap();
        // The sign bit leads, the octets follow at an offset of one bit.
        let mut r = BitReader::new(&buf[..n]);
        let mut out = std::vec![u8::from(r.bit().unwrap())];
        while let Ok(o) = r.bits(8) {
            out.push(o as u8);
            if o & 0x80 == 0 {
                break;
            }
        }
        out
    }

    fn decode(bytes: &[u8]) -> (bool, Vec<u8>) {
        let mut buf = [0u8; 128];
        let mut w = BitWriter::new(&mut buf);
        w.bit(bytes[0] == 1).unwrap();
        for &o in &bytes[1..] {
            w.bits(u32::from(o), 8).unwrap();
        }
        let n = w.finish().unwrap();
        let mut mem = [0u8; 256];
        let arena = Arena::new(&mut mem);
        let v = read(&mut BitReader::new(&buf[..n]), Some(&arena)).unwrap();
        (v.is_negative(), v.magnitude().to_vec())
    }

    #[test]
    fn small_values_are_the_exi_unsigned_integers() {
        assert_eq!(encode(Integer::ZERO), [0, 0]);
        assert_eq!(encode(Integer::unsigned(&[1])), [0, 1]);
        assert_eq!(encode(Integer::unsigned(&[0x7f])), [0, 0x7f]);
        assert_eq!(encode(Integer::unsigned(&[0x80])), [0, 0x80, 0x01]);
        assert_eq!(encode(Integer::unsigned(&[0x01, 0x00])), [0, 0x80, 0x02]);
        // Negative: the magnitude minus one, with the sign bit set.
        assert_eq!(encode(Integer::new(true, &[1])), [1, 0]);
        assert_eq!(encode(Integer::new(true, &[0x80])), [1, 0x7f]);
        assert_eq!(encode(Integer::new(true, &[1, 0])), [1, 0xff, 0x01]);
    }

    #[test]
    fn values_of_every_length_up_to_the_serial_number_limit_and_beyond_round_trip() {
        for len in 1..=40usize {
            for fill in [0x01u8, 0x7f, 0x80, 0xff] {
                for pattern in 0..3 {
                    let mut m: Vec<u8> = (0..len).map(|i| fill.wrapping_add((i as u8).wrapping_mul(37))).collect();
                    m[0] |= 1; // no leading zero
                    if pattern == 1 {
                        // trailing zero octets: the borrow of "minus one" runs through them
                        for b in m.iter_mut().skip(1) {
                            *b = 0;
                        }
                    }
                    if pattern == 2 {
                        m.iter_mut().for_each(|b| *b = 0xff);
                    }
                    for negative in [false, true] {
                        let enc = encode(Integer::new(negative, &m));
                        assert_eq!(decode(&enc), (negative, m.clone()), "len {len} fill {fill:#x} neg {negative}");
                    }
                }
            }
        }
    }

    #[test]
    fn zero_has_one_form_and_leading_zeros_are_dropped() {
        assert_eq!(Integer::new(true, &[0, 0]), Integer::ZERO);
        assert_eq!(Integer::unsigned(&[0, 0, 5]), Integer::unsigned(&[5]));
        assert_eq!(Integer::unsigned(&[0, 5]).to_u64(), Some(5));
        assert_eq!(Integer::new(true, &[5]).to_u64(), None);
    }
}
