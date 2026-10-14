//! Encoder and decoder contexts used by the generated codecs.

use crate::bits::{BitReader, BitWriter};
use crate::layout::{Event, Layout, Pos};
use crate::strings::{Found, Table};
use crate::{Arena, DecodeError, EncodeError};

/// The EXI header byte: distinguishing bits `10`, no options, version 0.
pub const HEADER: u8 = 0x80;

pub struct Enc<'b, 'm> {
    w: BitWriter<'b>,
    strings: Table<'m>,
}

impl<'b, 'm> Enc<'b, 'm> {
    /// Starts a stream: the header byte is written, then `out[1..]` takes the bits.
    pub fn new(out: &'b mut [u8]) -> Result<Self, EncodeError> {
        let (first, rest) = out.split_first_mut().ok_or(EncodeError::BufferTooSmall)?;
        *first = HEADER;
        Ok(Self { w: BitWriter::new(rest), strings: Table::new() })
    }

    /// Pads to a byte and returns the stream length including the header byte.
    pub fn finish(self) -> Result<usize, EncodeError> {
        Ok(1 + self.w.finish()?)
    }

    pub fn bits(&mut self, v: u32, n: u32) -> Result<(), EncodeError> {
        self.w.bits(v, n)
    }

    /// A start event of the document grammar (`n` bits wide).
    pub fn root(&mut self, code: u32, bits: u32) -> Result<(), EncodeError> {
        self.w.bits(code, bits)
    }

    pub fn start(&mut self, lay: &Layout, pos: Pos, slot: usize, alt: usize) -> Result<Pos, EncodeError> {
        lay.write_start(&mut self.w, pos, slot, alt)
    }

    pub fn end(&mut self, lay: &Layout, pos: Pos) -> Result<(), EncodeError> {
        lay.write_end(&mut self.w, pos)
    }

    /// The `CH` event that precedes a typed value (the only production, so code 0).
    pub fn ch(&mut self) -> Result<(), EncodeError> {
        self.w.bits(0, 1)
    }

    /// The `EE` event that follows a typed value.
    pub fn ee(&mut self) -> Result<(), EncodeError> {
        self.w.bits(0, 1)
    }

    pub fn bool(&mut self, v: bool) -> Result<(), EncodeError> {
        self.w.bit(v)
    }

    pub fn uint(&mut self, v: u64) -> Result<(), EncodeError> {
        self.w.uint64(v)
    }

    pub fn int(&mut self, v: i64) -> Result<(), EncodeError> {
        self.w.bit(v < 0)?;
        self.w.uint64(if v < 0 { !v as u64 } else { v as u64 })
    }

    pub fn binary(&mut self, v: &[u8]) -> Result<(), EncodeError> {
        self.w.uint64(v.len() as u64)?;
        for b in v {
            self.w.bits(u32::from(*b), 8)?;
        }
        Ok(())
    }

    /// A string value under qname id `q`; `max_chars` is the schema's `maxLength`.
    pub fn string(&mut self, q: u8, v: &'m str, max_chars: usize) -> Result<(), EncodeError> {
        let chars = v.chars().count();
        if chars > max_chars {
            return Err(EncodeError::InvalidValue("string longer than the schema allows"));
        }
        // EXIficient codes an empty string element differently (its event code is not CH).
        if chars == 0 {
            return Err(EncodeError::InvalidValue("empty string not supported yet"));
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
    strings: Table<'s>,
}

impl<'r, 's> Dec<'r, 's> {
    /// Checks the header byte and starts reading the bits after it.
    pub fn new(buf: &'r [u8], arena: &'s Arena<'s>) -> Result<Self, DecodeError> {
        let (&header, body) = buf.split_first().ok_or(DecodeError::UnexpectedEof)?;
        if header != HEADER {
            return Err(DecodeError::Unsupported("EXI header with options or cookie"));
        }
        Ok(Self { r: BitReader::new(body), arena: Some(arena), strings: Table::new() })
    }

    pub fn bits(&mut self, n: u32) -> Result<u32, DecodeError> {
        self.r.bits(n)
    }

    pub fn event(&mut self, lay: &Layout, pos: Pos) -> Result<Event, DecodeError> {
        lay.read(&mut self.r, pos)
    }

    pub fn expect_ch(&mut self) -> Result<(), DecodeError> {
        self.expect_zero(1)
    }

    pub fn expect_ee(&mut self) -> Result<(), DecodeError> {
        self.expect_zero(1)
    }

    fn expect_zero(&mut self, n: u32) -> Result<(), DecodeError> {
        match self.r.bits(n)? {
            0 => Ok(()),
            _ => Err(DecodeError::Unsupported("unexpected event code")),
        }
    }

    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        self.r.bit()
    }

    pub fn uint(&mut self) -> Result<u64, DecodeError> {
        self.r.uint64()
    }

    pub fn int(&mut self) -> Result<i64, DecodeError> {
        let negative = self.r.bit()?;
        let magnitude = self.r.uint64()?;
        if magnitude > i64::MAX as u64 {
            return Err(DecodeError::InvalidValue("integer exceeds 64 bits"));
        }
        Ok(if negative { !(magnitude as i64) } else { magnitude as i64 })
    }

    /// A binary value of at most `max` bytes (or exactly `fixed`).
    pub fn binary(&mut self, max: usize) -> Result<&'s [u8], DecodeError> {
        let len = self.r.uint64()?;
        if len > max as u64 {
            return Err(DecodeError::InvalidValue("binary value longer than the schema allows"));
        }
        let len = len as usize;
        let Some(arena) = self.arena else {
            for _ in 0..len {
                self.r.bits(8)?;
            }
            return Ok(&[]);
        };
        let out = arena.alloc_bytes(len)?;
        for b in out.iter_mut() {
            *b = self.r.bits(8)? as u8;
        }
        Ok(out)
    }

    /// Exactly `N` bytes (a `length` facet): no arena needed.
    pub fn binary_fixed<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        if self.r.uint64()? != N as u64 {
            return Err(DecodeError::InvalidValue("binary value has the wrong length"));
        }
        let mut out = [0u8; N];
        for b in &mut out {
            *b = self.r.bits(8)? as u8;
        }
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
                    return Err(DecodeError::Unsupported("empty string element"));
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
        mut one: impl FnMut(&mut Dec<'r, 's>) -> Result<T, DecodeError>,
    ) -> Result<(&'s [T], Pos), DecodeError> {
        let Some(arena) = self.arena else {
            // Already counting: just consume the list.
            let (_, end) = self.run_list(lay, slot, next, &mut one)?;
            return Ok((&[], end));
        };
        let mut dry = Dec { r: self.r.clone(), arena: None, strings: self.strings.clone() };
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
        one: &mut impl FnMut(&mut Dec<'r, 's>) -> Result<T, DecodeError>,
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
