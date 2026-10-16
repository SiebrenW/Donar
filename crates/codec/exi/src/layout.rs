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
    /// Alternatives of all earlier slots together. Filled in by [`table`] / [`fill`].
    pre: u32,
    /// The slot after the first mandatory one at or after this slot (the number of slots if
    /// there is none): the following slots of a state end there. Filled in by [`fill`].
    last: u16,
    /// No slot at or after this one is mandatory, so `EE` is a production.
    open: bool,
}

impl Slot {
    /// A slot whose derived fields are still empty: put the slots through [`table`].
    pub const fn new(min: u32, max: u32, alts: u16) -> Self {
        Self { min, max, alts, pre: 0, last: 0, open: false }
    }
}

/// Completes the derived fields of the slots of one layout.
pub const fn fill(slots: &mut [Slot]) {
    let n = slots.len();
    let mut sum = 0;
    let mut i = 0;
    while i < n {
        slots[i].pre = sum;
        sum += slots[i].alts as u32;
        i += 1;
    }
    let mut i = n;
    while i > 0 {
        i -= 1;
        if slots[i].min > 0 {
            slots[i].last = i as u16 + 1;
            slots[i].open = false;
        } else if i + 1 < n {
            slots[i].last = slots[i + 1].last;
            slots[i].open = slots[i + 1].open;
        } else {
            slots[i].last = n as u16;
            slots[i].open = true;
        }
    }
}

/// `fill` for a constant array, so that a layout can be a `const`:
/// `Layout(&table([Slot::new(1, 1, 1), ...]))`.
pub const fn table<const N: usize>(mut slots: [Slot; N]) -> [Slot; N] {
    fill(&mut slots);
    slots
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

    /// The shape of the productions of state `pos`, see [`Span`].
    #[inline]
    fn span(&self, pos: Pos) -> Span {
        let mut repeat = 0;
        let mut start = usize::from(pos.slot);
        if pos.count > 0 {
            let slot = &self.0[start];
            if pos.count < slot.max {
                repeat = u32::from(slot.alts);
            }
            start += 1;
            if pos.count < slot.min {
                return Span { repeat, start, last: start, end: false };
            }
        }
        // The following slots count up to and including the first mandatory one.
        match self.0.get(start) {
            Some(next) => Span { repeat, start, last: usize::from(next.last), end: next.open },
            None => Span { repeat, start, last: start, end: true },
        }
    }

    /// Number of productions in state `pos` (without the escape code).
    pub fn production_count(&self, pos: Pos) -> usize {
        self.total(&self.span(pos)) as usize
    }

    #[inline]
    fn total(&self, span: &Span) -> u32 {
        let run = if span.last > span.start {
            let last = &self.0[span.last - 1];
            last.pre + u32::from(last.alts) - self.0[span.start].pre
        } else {
            0
        };
        span.repeat + run + u32::from(span.end)
    }

    /// Bits of an event code in state `pos`.
    pub fn width(&self, pos: Pos) -> u32 {
        ceil_log2(u64::from(self.total(&self.span(pos))) + 1)
    }

    /// The state after `slot` produced one more element in state `pos`.
    pub fn after(pos: Pos, slot: usize) -> Pos {
        if usize::from(pos.slot) == slot && pos.count > 0 {
            Pos { slot: pos.slot, count: pos.count + 1 }
        } else {
            Pos { slot: slot as u16, count: 1 }
        }
    }

    /// Writes the start event of alternative `alt` of `slot` and returns the next state.
    #[inline]
    pub fn write_start(&self, w: &mut BitWriter<'_>, pos: Pos, slot: usize, alt: usize) -> Result<Pos, EncodeError> {
        let span = self.span(pos);
        let not_allowed = EncodeError::InvalidValue("element not allowed here by the schema");
        let code = if slot == usize::from(pos.slot) && span.repeat > 0 {
            // A further copy of the slot just seen.
            if alt >= self.0[slot].alts as usize {
                return Err(not_allowed);
            }
            alt as u32
        } else if (span.start..span.last).contains(&slot) && alt < self.0[slot].alts as usize {
            span.repeat + (self.0[slot].pre - self.0[span.start].pre) + alt as u32
        } else {
            return Err(not_allowed);
        };
        w.bits(code, ceil_log2(u64::from(self.total(&span)) + 1))?;
        Ok(Self::after(pos, slot))
    }

    /// Writes the end-of-element event.
    #[inline]
    pub fn write_end(&self, w: &mut BitWriter<'_>, pos: Pos) -> Result<(), EncodeError> {
        let span = self.span(pos);
        if !span.end {
            return Err(EncodeError::InvalidValue("a required element is missing"));
        }
        let total = self.total(&span);
        w.bits(total - 1, ceil_log2(u64::from(total) + 1))
    }

    #[inline]
    pub fn read(&self, r: &mut BitReader<'_>, pos: Pos) -> Result<Event, DecodeError> {
        let span = self.span(pos);
        let total = self.total(&span);
        let mut code = r.bits(ceil_log2(u64::from(total) + 1))?;
        if code < span.repeat {
            let slot = usize::from(pos.slot);
            return Ok(Event::Start { slot, alt: code as usize, next: Self::after(pos, slot) });
        }
        code -= span.repeat;
        for slot in span.start..span.last {
            let alts = u32::from(self.0[slot].alts);
            if code < alts {
                return Ok(Event::Start { slot, alt: code as usize, next: Self::after(pos, slot) });
            }
            code -= alts;
        }
        if span.end && code == 0 {
            return Ok(Event::End);
        }
        Err(DecodeError::Unsupported("event outside the schema grammar"))
    }
}

/// The productions of a state, in event-code order: the `repeat` alternatives of the slot
/// just seen (when more copies are allowed), the alternatives of the slots `start..last`,
/// and `EE` if `end`.
struct Span {
    repeat: u32,
    start: usize,
    last: usize,
    end: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    const fn slot(min: u32, max: u32) -> Slot {
        Slot::new(min, max, 1)
    }

    /// The message header: SessionID, TimeStamp, optional Signature.
    const HEADER: Layout = Layout(&table([slot(1, 1), slot(1, 1), slot(0, 1)]));

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
        const L: Layout = Layout(&table([slot(1, 3), slot(1, 1)]));
        let l = L;
        let one = Layout::after(Pos::START, 0);
        assert_eq!(productions(&l, one), [Prod::Start { slot: 0, alt: 0 }, Prod::Start { slot: 1, alt: 0 }]);
        let three = Layout::after(Layout::after(one, 0), 0);
        assert_eq!(productions(&l, three), [Prod::Start { slot: 1, alt: 0 }]);
        assert_eq!(l.width(three), 1);
    }

    #[test]
    fn a_choice_counts_every_alternative() {
        const L: Layout = Layout(&table([Slot::new(1, 1, 5)]));
        let l = L;
        assert_eq!(l.production_count(Pos::START), 5);
        assert_eq!(l.width(Pos::START), 3);
    }

    #[test]
    fn write_then_read_round_trips() {
        const L: Layout = Layout(&table([slot(1, 1), slot(0, 2), slot(0, 1)]));
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
        const L: Layout = Layout(&table([slot(1, 1), slot(1, 1)]));
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

    /// The arithmetic event codes agree with plain enumeration of the productions, in every
    /// state of a few layouts that mix mandatory, optional, repeated and choice slots.
    #[test]
    fn arithmetic_codes_agree_with_enumeration() {
        const A: Layout = Layout(&table([slot(1, 1), slot(0, 1), slot(0, 3), slot(1, 1)]));
        const B: Layout = Layout(&table([Slot::new(0, 1, 4), Slot::new(0, 2, 3), slot(0, 1)]));
        const C: Layout = Layout(&table([slot(2, 4), Slot::new(1, 1, 5), slot(0, 5)]));
        const D: Layout = Layout(&table([slot(0, 1)]));
        for layout in [A, B, C, D] {
            let mut states = Vec::from([Pos::START]);
            for (i, s) in layout.0.iter().enumerate() {
                for count in 1..=s.max {
                    states.push(Pos { slot: i as u16, count });
                }
            }
            for pos in states {
                let prods = productions(&layout, pos);
                assert_eq!(layout.production_count(pos), prods.len(), "{pos:?}");
                assert_eq!(layout.width(pos), ceil_log2(prods.len() as u64 + 1), "{pos:?}");
                for (code, prod) in prods.iter().enumerate() {
                    let mut buf = [0u8; 8];
                    let mut w = BitWriter::new(&mut buf);
                    let width = layout.width(pos);
                    let next = match *prod {
                        Prod::Start { slot, alt } => Some(layout.write_start(&mut w, pos, slot, alt).unwrap()),
                        Prod::End => {
                            layout.write_end(&mut w, pos).unwrap();
                            None
                        }
                    };
                    let n = w.finish().unwrap();
                    let mut r = BitReader::new(&buf[..n]);
                    assert_eq!(r.bits(width).unwrap() as usize, code, "{pos:?} {prod:?}");
                    let mut r = BitReader::new(&buf[..n]);
                    match (layout.read(&mut r, pos).unwrap(), *prod) {
                        (Event::Start { slot, alt, next: got }, Prod::Start { slot: s, alt: a }) => {
                            assert_eq!((slot, alt), (s, a));
                            assert_eq!(Some(got), next);
                        }
                        (Event::End, Prod::End) => {}
                        other => panic!("{pos:?}: {other:?}"),
                    }
                }
                // Anything that is not a production is refused, in both directions.
                let mut buf = [0u8; 8];
                let mut w = BitWriter::new(&mut buf);
                for slot in 0..layout.0.len() {
                    for alt in 0..layout.0[slot].alts as usize + 1 {
                        let allowed = prods.contains(&Prod::Start { slot, alt });
                        assert_eq!(layout.write_start(&mut w, pos, slot, alt).is_ok(), allowed, "{pos:?} {slot} {alt}");
                    }
                }
                assert_eq!(layout.write_end(&mut w, pos).is_ok(), prods.contains(&Prod::End), "{pos:?}");
                let mut escape = [0u8; 4];
                escape[0] = 0xFF;
                let code_bits = layout.width(pos);
                let mut r = BitReader::new(&escape);
                if code_bits > 0 && (1u32 << code_bits) > prods.len() as u32 {
                    assert!(layout.read(&mut r, pos).is_err(), "{pos:?}");
                }
            }
        }
    }
}
