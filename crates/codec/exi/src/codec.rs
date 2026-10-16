//! Encoder and decoder contexts used by the generated codecs.

use crate::bits::{BitReader, BitWriter};
use crate::layout::{Event, Layout, Pos};
use crate::strings::{Found, Table};
use crate::{Arena, DecodeError, EncodeError};

/// The EXI header byte: distinguishing bits `10`, no options, version 0.
pub const HEADER: u8 = 0x80;

pub struct Enc<'b, 'm> {
    w: BitWriter<'b>,
    /// The caller owns the table: it is large, and handing it over by value copied it around.
    strings: &'b mut Table<'m>,
}

impl<'b, 'm> Enc<'b, 'm> {
    /// Starts a stream: the header byte is written, then `out[1..]` takes the bits. `strings`
    /// is an empty string table (`Table::new()`).
    pub fn new(out: &'b mut [u8], strings: &'b mut Table<'m>) -> Result<Self, EncodeError> {
        let (first, rest) = out.split_first_mut().ok_or(EncodeError::BufferTooSmall)?;
        *first = HEADER;
        Ok(Self { w: BitWriter::new(rest), strings })
    }

    /// Pads to a byte and returns the stream length including the header byte.
    #[inline]
    pub fn finish(self) -> Result<usize, EncodeError> {
        Ok(1 + self.w.finish()?)
    }

    #[inline]
    pub fn bits(&mut self, v: u32, n: u32) -> Result<(), EncodeError> {
        self.w.bits(v, n)
    }

    /// A start event of the document grammar (`n` bits wide).
    #[inline]
    pub fn root(&mut self, code: u32, bits: u32) -> Result<(), EncodeError> {
        self.w.bits(code, bits)
    }

    #[inline]
    pub fn start(&mut self, lay: &Layout, pos: Pos, slot: usize, alt: usize) -> Result<Pos, EncodeError> {
        lay.write_start(&mut self.w, pos, slot, alt)
    }

    #[inline]
    pub fn end(&mut self, lay: &Layout, pos: Pos) -> Result<(), EncodeError> {
        lay.write_end(&mut self.w, pos)
    }

    /// The `CH` event that precedes a typed value (the only production, so code 0).
    #[inline]
    pub fn ch(&mut self) -> Result<(), EncodeError> {
        self.w.bits(0, 1)
    }

    /// The `EE` event that follows a typed value.
    #[inline]
    pub fn ee(&mut self) -> Result<(), EncodeError> {
        self.w.bits(0, 1)
    }

    #[inline]
    pub fn bool(&mut self, v: bool) -> Result<(), EncodeError> {
        self.w.bit(v)
    }

    /// An element whose content is `n` bits (at most 30): the `CH` code (0), the value and
    /// the `EE` code (0) in one write.
    #[inline]
    pub fn elem_bits(&mut self, v: u32, n: u32) -> Result<(), EncodeError> {
        self.w.bits(v << 1, n + 2)
    }

    #[inline]
    pub fn uint(&mut self, v: u64) -> Result<(), EncodeError> {
        self.w.uint64(v)
    }

    #[inline]
    pub fn int(&mut self, v: i64) -> Result<(), EncodeError> {
        self.w.bit(v < 0)?;
        self.w.uint64(if v < 0 { !v as u64 } else { v as u64 })
    }

    /// An `xs:integer` of any size (see [`crate::Integer`]).
    pub fn integer(&mut self, v: &crate::Integer<'_>) -> Result<(), EncodeError> {
        v.write(&mut self.w)
    }

    #[inline]
    pub fn binary(&mut self, v: &[u8]) -> Result<(), EncodeError> {
        self.w.uint64(v.len() as u64)?;
        self.w.bytes(v)
    }

    /// A string element's content: `CH`, the value and `EE`. An empty string has no characters
    /// event in the XML infoset, so it is the second-level `EE` of the element's first state
    /// (the escape code 1 of the 1-bit first level, then 3 bits, value 0: the width follows
    /// from the number of second-level productions, checked against EXIficient's output).
    pub fn string_element(&mut self, q: u8, v: &'m str, max_chars: usize) -> Result<(), EncodeError> {
        if v.is_empty() {
            return self.w.bits(0b1000, 4);
        }
        self.ch()?;
        self.string(q, v, max_chars)?;
        self.ee()
    }

    /// A string value under qname id `q`; `max_chars` is the schema's `maxLength`.
    pub fn string(&mut self, q: u8, v: &'m str, max_chars: usize) -> Result<(), EncodeError> {
        let chars = v.chars().count();
        if chars > max_chars {
            return Err(EncodeError::InvalidValue("string longer than the schema allows"));
        }
        // An empty value is never in the table and is not added to it (EXI 7.3.3).
        if chars == 0 {
            return self.w.uint64(2);
        }
        match self.strings.find(q, v) {
            Found::Local(i, bits) => {
                self.w.uint64(0)?;
                self.w.bits(i, bits)
            }
            Found::Global(i, bits) => {
                self.w.uint64(1)?;
                self.w.bits(i, bits)
            }
            Found::Miss => {
                if !self.strings.push(q, v) {
                    return Err(EncodeError::InvalidValue("too many distinct strings in one message"));
                }
                self.w.uint64(chars as u64 + 2)?;
                if v.is_ascii() {
                    // One octet per character, as the code point is below 0x80.
                    return self.w.bytes(v.as_bytes());
                }
                for c in v.chars() {
                    self.w.uint64(u64::from(c as u32))?;
                }
                Ok(())
            }
        }
    }
}

pub struct Dec<'r, 's> {
    r: BitReader<'r>,
    /// `None` while counting the entries of a list: values are validated but not stored.
    arena: Option<&'s Arena<'s>>,
    strings: &'r mut Table<'s>,
}

impl<'r, 's> Dec<'r, 's> {
    /// Checks the header byte and starts reading the bits after it. `strings` is an empty
    /// string table (`Table::new()`).
    pub fn new(buf: &'r [u8], arena: &'s Arena<'s>, strings: &'r mut Table<'s>) -> Result<Self, DecodeError> {
        let (&header, body) = buf.split_first().ok_or(DecodeError::UnexpectedEof)?;
        if header != HEADER {
            return Err(DecodeError::Unsupported("EXI header with options or cookie"));
        }
        Ok(Self { r: BitReader::new(body), arena: Some(arena), strings })
    }

    #[inline]
    pub fn bits(&mut self, n: u32) -> Result<u32, DecodeError> {
        self.r.bits(n)
    }

    #[inline]
    pub fn event(&mut self, lay: &Layout, pos: Pos) -> Result<Event, DecodeError> {
        lay.read(&mut self.r, pos)
    }

    /// Reads what [`Enc::string_element`] writes. Also accepts `CH` with a zero-length value,
    /// which other encoders may produce for an empty string.
    pub fn string_element(&mut self, q: u8, max_chars: usize) -> Result<&'s str, DecodeError> {
        if self.r.bit()? {
            // The escape: only the second-level EE (code 0 of 3 bits) is expected here.
            return match self.r.bits(3)? {
                0 => Ok(""),
                _ => Err(DecodeError::Unsupported("unexpected event code")),
            };
        }
        let v = self.string(q, max_chars)?;
        self.expect_ee()?;
        Ok(v)
    }

    #[inline]
    pub fn expect_ch(&mut self) -> Result<(), DecodeError> {
        self.expect_zero(1)
    }

    #[inline]
    pub fn expect_ee(&mut self) -> Result<(), DecodeError> {
        self.expect_zero(1)
    }

    #[inline]
    fn expect_zero(&mut self, n: u32) -> Result<(), DecodeError> {
        match self.r.bits(n)? {
            0 => Ok(()),
            _ => Err(DecodeError::Unsupported("unexpected event code")),
        }
    }

    #[inline]
    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        self.r.bit()
    }

    /// Reads what [`Enc::elem_bits`] writes: `CH` (0), `n` bits of value, `EE` (0).
    #[inline]
    pub fn elem_bits(&mut self, n: u32) -> Result<u32, DecodeError> {
        let w = self.r.bits(n + 2)?;
        if w & 1 != 0 || w >> (n + 1) != 0 {
            return Err(DecodeError::Unsupported("unexpected event code"));
        }
        Ok((w >> 1) & ((1 << n) - 1))
    }

    #[inline]
    pub fn uint(&mut self) -> Result<u64, DecodeError> {
        self.r.uint64()
    }

    #[inline]
    pub fn int(&mut self) -> Result<i64, DecodeError> {
        let negative = self.r.bit()?;
        let magnitude = self.r.uint64()?;
        if magnitude > i64::MAX as u64 {
            return Err(DecodeError::InvalidValue("integer exceeds 64 bits"));
        }
        Ok(if negative { !(magnitude as i64) } else { magnitude as i64 })
    }

    /// An `xs:integer` of any size; the magnitude lives in the arena.
    pub fn integer(&mut self) -> Result<crate::Integer<'s>, DecodeError> {
        crate::integer::read(&mut self.r, self.arena)
    }

    /// A binary value of at most `max` bytes (or exactly `fixed`).
    #[inline]
    pub fn binary(&mut self, max: usize) -> Result<&'s [u8], DecodeError> {
        let len = self.r.uint64()?;
        if len > max as u64 {
            return Err(DecodeError::InvalidValue("binary value longer than the schema allows"));
        }
        let len = len as usize;
        let Some(arena) = self.arena else {
            self.r.skip_bytes(len)?;
            return Ok(&[]);
        };
        let out = arena.alloc_bytes(len)?;
        self.r.bytes(out)?;
        Ok(out)
    }

    /// Exactly `N` bytes (a `length` facet): no arena needed.
    #[inline]
    pub fn binary_fixed<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        if self.r.uint64()? != N as u64 {
            return Err(DecodeError::InvalidValue("binary value has the wrong length"));
        }
        let mut out = [0u8; N];
        self.r.bytes(&mut out)?;
        Ok(out)
    }

    pub fn string(&mut self, q: u8, max_chars: usize) -> Result<&'s str, DecodeError> {
        match self.r.uint64()? {
            0 => {
                let i = self.r.bits(self.strings.local_bits(q))? as usize;
                self.strings.local(q, i).ok_or(DecodeError::InvalidValue("string table index"))
            }
            1 => {
                let i = self.r.bits(self.strings.global_bits())? as usize;
                self.strings.global(i).ok_or(DecodeError::InvalidValue("string table index"))
            }
            n => {
                let len = usize::try_from(n - 2).map_err(|_| DecodeError::InvalidValue("string too long"))?;
                if len == 0 {
                    return Ok("");
                }
                if len > max_chars {
                    return Err(DecodeError::InvalidValue("string longer than the schema allows"));
                }
                let s = crate::text::decode_string(&mut self.r, len, self.arena)?;
                if !self.strings.push(q, s) {
                    return Err(DecodeError::Unsupported("string table full"));
                }
                Ok(s)
            }
        }
    }

    /// Decodes the rest of a list whose first entry's start event was just read (`next` is
    /// the state after it): the entry count is found by a dry run over a copy of the stream,
    /// so the arena slice has exactly the right size. Returns the entries and the state after
    /// the last one.
    pub fn list<T: Copy>(
        &mut self,
        lay: &Layout,
        slot: usize,
        next: Pos,
        mut one: impl for<'x> FnMut(&mut Dec<'x, 's>) -> Result<T, DecodeError>,
    ) -> Result<(&'s [T], Pos), DecodeError> {
        let Some(arena) = self.arena else {
            // Already counting: just consume the list.
            let (_, end) = self.run_list(lay, slot, next, &mut one)?;
            return Ok((&[], end));
        };
        let mut table = self.strings.clone();
        let mut dry = Dec { r: self.r.clone(), arena: None, strings: &mut table };
        let (n, end) = dry.run_list(lay, slot, next, &mut one)?;
        let mut pos = next;
        let items = arena.alloc_slice_with(n, |i| {
            if i > 0 {
                match lay.read(&mut self.r, pos)? {
                    Event::Start { slot: s, next, .. } if s == slot => pos = next,
                    _ => return Err(DecodeError::InvalidValue("list changed between passes")),
                }
            }
            one(self)
        })?;
        Ok((items, end))
    }

    /// Runs the list loop, returning the number of entries and the final state.
    fn run_list<T>(
        &mut self,
        lay: &Layout,
        slot: usize,
        mut pos: Pos,
        one: &mut impl for<'x> FnMut(&mut Dec<'x, 's>) -> Result<T, DecodeError>,
    ) -> Result<(usize, Pos), DecodeError> {
        let mut n = 0;
        loop {
            one(self)?;
            n += 1;
            let before = self.r.clone();
            match lay.read(&mut self.r, pos)? {
                Event::Start { slot: s, next, .. } if s == slot => pos = next,
                _ => {
                    self.r = before;
                    return Ok((n, pos));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_with_non_ascii_characters_round_trip() {
        let mut out = [0u8; 64];
        let mut table = Table::new();
        let mut e = Enc::new(&mut out, &mut table).unwrap();
        e.string(1, "Grüße 😀", 20).unwrap();
        e.string(1, "abc", 20).unwrap();
        e.string(2, "Grüße 😀", 20).unwrap(); // a table hit, from the other partition
        let n = e.finish().unwrap();
        let mut mem = [0u8; 256];
        let arena = Arena::new(&mut mem);
        let mut table = Table::new();
        let mut d = Dec::new(&out[..n], &arena, &mut table).unwrap();
        assert_eq!(d.string(1, 20).unwrap(), "Grüße 😀");
        assert_eq!(d.string(1, 20).unwrap(), "abc");
        assert_eq!(d.string(2, 20).unwrap(), "Grüße 😀");
        // The failed quick path leaves no garbage in the arena: 8 + 3 bytes of text, no more.
        assert_eq!(arena.used(), "Grüße 😀".len() + 3);
    }
}
