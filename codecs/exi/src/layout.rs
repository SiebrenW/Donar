//! Event-code layout of a schema-informed element grammar.
//!
//! EXIficient (and the EXI specification) build, for every complex type, a state machine
//! whose states are positions in the content model with `maxOccurs` unrolled. A [`Layout`]
//! describes that machine compactly as a list of [`Slot`]s (one per particle: an attribute,
//! an element, a choice or a substitution group); the productions of any state are then
//! computed on demand, so a list of 2048 entries costs no table.
//!
//! In a state, the productions are, in order: the alternatives of the current slot if more
//! copies are allowed, then those of the following slots up to and including the first
//! mandatory one, then `EE` if every remaining slot is optional. The event code is the
//! index into that list, written in `ceil(log2(count + 1))` bits (the extra code is the
//! escape to deviations from the schema, which this codec does not accept).

use crate::bits::{BitReader, BitWriter};
use crate::{DecodeError, EncodeError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub min: u32,
    pub max: u32,
    /// Number of alternative productions (choice members, or substitution group members).
    pub alts: u16,
}

/// A state: `count` copies of `slot` have been seen. `Pos::START` is before the first slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pos {
    pub slot: u16,
    pub count: u32,
}

impl Pos {
    pub const START: Pos = Pos { slot: 0, count: 0 };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prod {
    Start { slot: usize, alt: usize },
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Start { slot: usize, alt: usize, next: Pos },
    End,
}

#[derive(Debug, Clone, Copy)]
pub struct Layout(pub &'static [Slot]);

/// `ceil(log2(n))`, 0 for `n <= 1`.
pub const fn ceil_log2(n: u64) -> u32 {
    if n <= 1 { 0 } else { 64 - (n - 1).leading_zeros() }
}

impl Layout {
    /// Calls `f` for every production of state `pos` in event-code order until it returns true.
    pub fn for_each(&self, pos: Pos, mut f: impl FnMut(Prod) -> bool) {
        let mut s = usize::from(pos.slot);
        if pos.count > 0 {
            let slot = self.0[s];
            if pos.count < slot.max {
                for alt in 0..usize::from(slot.alts) {
                    if f(Prod::Start { slot: s, alt }) {
                        return;
                    }
                }
            }
            if pos.count < slot.min {
                return;
            }
            s += 1;
        }
        while s < self.0.len() {
            let slot = self.0[s];
            for alt in 0..usize::from(slot.alts) {
                if f(Prod::Start { slot: s, alt }) {
                    return;
                }
            }
            if slot.min > 0 {
                return;
            }
            s += 1;
        }
        f(Prod::End);
    }

    pub fn production_count(&self, pos: Pos) -> usize {
        let mut n = 0;
        self.for_each(pos, |_| {
            n += 1;
            false
        });
        n
    }

    /// Bits of an event code in state `pos`.
    pub fn width(&self, pos: Pos) -> u32 {
        ceil_log2(self.production_count(pos) as u64 + 1)
    }

    /// The state after `slot` produced one more element in state `pos`.
    pub fn after(pos: Pos, slot: usize) -> Pos {
        if usize::from(pos.slot) == slot && pos.count > 0 {
            Pos { slot: pos.slot, count: pos.count + 1 }
        } else {
            Pos { slot: slot as u16, count: 1 }
        }
    }

    fn code_of(&self, pos: Pos, target: Prod) -> Option<(u32, u32)> {
        let mut n = 0u32;
        let mut found = None;
        self.for_each(pos, |p| {
            if p == target {
                found = Some(n);
            }
            n += 1;
            false
        });
        found.map(|code| (code, ceil_log2(u64::from(n) + 1)))
    }

    /// Writes the start event of alternative `alt` of `slot` and returns the next state.
    pub fn write_start(&self, w: &mut BitWriter<'_>, pos: Pos, slot: usize, alt: usize) -> Result<Pos, EncodeError> {
        let (code, width) = self
            .code_of(pos, Prod::Start { slot, alt })
            .ok_or(EncodeError::InvalidValue("element not allowed here by the schema"))?;
        w.bits(code, width)?;
        Ok(Self::after(pos, slot))
    }

    /// Writes the end-of-element event.
    pub fn write_end(&self, w: &mut BitWriter<'_>, pos: Pos) -> Result<(), EncodeError> {
        let (code, width) = self
            .code_of(pos, Prod::End)
            .ok_or(EncodeError::InvalidValue("a required element is missing"))?;
        w.bits(code, width)
    }

    pub fn read(&self, r: &mut BitReader<'_>, pos: Pos) -> Result<Event, DecodeError> {
        let count = self.production_count(pos);
        let code = r.bits(ceil_log2(count as u64 + 1))? as usize;
        let mut n = 0;
        let mut hit = None;
        self.for_each(pos, |p| {
            if n == code {
                hit = Some(p);
                return true;
            }
            n += 1;
            false
        });
        match hit {
            Some(Prod::Start { slot, alt }) => Ok(Event::Start { slot, alt, next: Self::after(pos, slot) }),
            Some(Prod::End) => Ok(Event::End),
            None => Err(DecodeError::Unsupported("event outside the schema grammar")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    const fn slot(min: u32, max: u32) -> Slot {
        Slot { min, max, alts: 1 }
    }

    /// The message header: SessionID, TimeStamp, optional Signature.
    const HEADER: Layout = Layout(&[slot(1, 1), slot(1, 1), slot(0, 1)]);

    fn productions(l: &Layout, pos: Pos) -> Vec<Prod> {
        let mut v = Vec::new();
        l.for_each(pos, |p| {
            v.push(p);
            false
        });
        v
    }

    #[test]
    fn mandatory_elements_have_one_production_and_one_bit() {
        assert_eq!(productions(&HEADER, Pos::START), [Prod::Start { slot: 0, alt: 0 }]);
        assert_eq!(HEADER.width(Pos::START), 1);
    }

    #[test]
    fn an_optional_tail_offers_the_element_and_the_end() {
        let after_timestamp = Layout::after(Layout::after(Pos::START, 0), 1);
        assert_eq!(productions(&HEADER, after_timestamp), [Prod::Start { slot: 2, alt: 0 }, Prod::End]);
        assert_eq!(HEADER.width(after_timestamp), 2); // two productions plus the escape code
    }

    /// maxOccurs=3, minOccurs=1, followed by a mandatory element: after the first entry the
    /// list may continue or be left; after the last allowed one only the next element remains.
    #[test]
    fn unrolled_list_states() {
        const L: Layout = Layout(&[slot(1, 3), slot(1, 1)]);
        let l = L;
        let one = Layout::after(Pos::START, 0);
        assert_eq!(productions(&l, one), [Prod::Start { slot: 0, alt: 0 }, Prod::Start { slot: 1, alt: 0 }]);
        let three = Layout::after(Layout::after(one, 0), 0);
        assert_eq!(productions(&l, three), [Prod::Start { slot: 1, alt: 0 }]);
        assert_eq!(l.width(three), 1);
    }

    #[test]
    fn a_choice_counts_every_alternative() {
        const L: Layout = Layout(&[Slot { min: 1, max: 1, alts: 5 }]);
        let l = L;
        assert_eq!(l.production_count(Pos::START), 5);
        assert_eq!(l.width(Pos::START), 3);
    }

    #[test]
    fn write_then_read_round_trips() {
        const L: Layout = Layout(&[slot(1, 1), slot(0, 2), slot(0, 1)]);
        let l = L;
        let mut buf = [0u8; 8];
        let mut w = BitWriter::new(&mut buf);
        let mut pos = l.write_start(&mut w, Pos::START, 0, 0).unwrap();
        pos = l.write_start(&mut w, pos, 1, 0).unwrap();
        pos = l.write_start(&mut w, pos, 1, 0).unwrap();
        l.write_end(&mut w, pos).unwrap();
        let n = w.finish().unwrap();
        let mut r = BitReader::new(&buf[..n]);
        let mut pos = Pos::START;
        let mut seen = Vec::new();
        while let Event::Start { slot, next, .. } = l.read(&mut r, pos).unwrap() {
            seen.push(slot);
            pos = next;
        }
        assert_eq!(seen, [0, 1, 1]);
    }

    #[test]
    fn an_element_the_schema_does_not_allow_is_refused() {
        const L: Layout = Layout(&[slot(1, 1), slot(1, 1)]);
        let l = L;
        let mut buf = [0u8; 4];
        let mut w = BitWriter::new(&mut buf);
        assert!(l.write_start(&mut w, Pos::START, 1, 0).is_err()); // slot 0 is mandatory
        assert!(l.write_end(&mut w, Pos::START).is_err());
    }

    #[test]
    fn ceil_log2_matches_the_definition() {
        assert_eq!([0, 1, 2, 3, 4, 5, 8, 9].map(ceil_log2), [0, 0, 1, 2, 2, 3, 3, 4]);
    }
}
