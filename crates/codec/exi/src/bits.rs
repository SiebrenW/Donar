//! Bit-packed EXI primitives: MSB-first bits and EXI unsigned integers.

use crate::{DecodeError, EncodeError};

pub struct BitWriter<'b> {
    buf: &'b mut [u8],
    /// Whole bytes already written to `buf`.
    len: usize,
    /// Bits not yet written, right-aligned; fewer than eight between calls.
    acc: u64,
    nbits: u32,
}

impl<'b> BitWriter<'b> {
    pub fn new(buf: &'b mut [u8]) -> Self {
        Self { buf, len: 0, acc: 0, nbits: 0 }
    }

    #[inline]
    pub fn bit(&mut self, v: bool) -> Result<(), EncodeError> {
        self.bits(u32::from(v), 1)
    }

    /// Writes the low `n` bits of `v` (`n` at most 32), most significant first.
    #[inline]
    pub fn bits(&mut self, v: u32, n: u32) -> Result<(), EncodeError> {
        debug_assert!(n <= 32);
        let mask = (1u64 << n) - 1;
        self.acc = (self.acc << n) | (u64::from(v) & mask);
        self.nbits += n;
        if self.nbits >= 8 { self.flush() } else { Ok(()) }
    }

    /// Moves the complete bytes of the accumulator to the buffer.
    #[inline]
    fn flush(&mut self) -> Result<(), EncodeError> {
        let bytes = (self.nbits / 8) as usize;
        let out = self.buf.get_mut(self.len..self.len + bytes).ok_or(EncodeError::BufferTooSmall)?;
        // Left-align the pending bits so that the oldest ones come out first. The octets one by one:
        // a copy of variable length is a call to `memmove`, and at most four octets are written.
        let top = (self.acc << (64 - self.nbits)).to_be_bytes();
        for (o, t) in out.iter_mut().zip(top) {
            *o = t;
        }
        self.len += bytes;
        self.nbits %= 8;
        self.acc &= (1u64 << self.nbits) - 1;
        Ok(())
    }

    /// Writes whole octets.
    #[inline]
    pub fn bytes(&mut self, v: &[u8]) -> Result<(), EncodeError> {
        if self.nbits == 0 {
            let out = self.buf.get_mut(self.len..self.len + v.len()).ok_or(EncodeError::BufferTooSmall)?;
            out.copy_from_slice(v);
            self.len += v.len();
            return Ok(());
        }
        for &b in v {
            self.bits(u32::from(b), 8)?;
        }
        Ok(())
    }

    /// EXI unsigned integer: 7-bit groups, least significant group first, high bit = more.
    #[inline]
    pub fn uint(&mut self, v: u32) -> Result<(), EncodeError> {
        self.uint64(u64::from(v))
    }

    #[inline]
    pub fn uint64(&mut self, mut v: u64) -> Result<(), EncodeError> {
        if v < 0x80 {
            return self.bits(v as u32, 8);
        }
        loop {
            let group = v & 0x7f;
            v >>= 7;
            self.bits(group as u32 | if v != 0 { 0x80 } else { 0 }, 8)?;
            if v == 0 {
                return Ok(());
            }
        }
    }

    /// EXI integer: sign bit, then the magnitude (minus one if negative) as an unsigned integer.
    #[inline]
    pub fn int(&mut self, v: i32) -> Result<(), EncodeError> {
        self.bit(v < 0)?;
        self.uint(if v < 0 { !v as u32 } else { v as u32 })
    }

    /// Pads with zero bits to a byte boundary and returns the stream length in bytes.
    #[inline]
    pub fn finish(mut self) -> Result<usize, EncodeError> {
        if self.nbits > 0 {
            let pad = 8 - self.nbits;
            self.bits(0, pad)?;
        }
        Ok(self.len)
    }
}

#[derive(Clone)]
pub struct BitReader<'b> {
    buf: &'b [u8],
    pos: usize,
}

impl<'b> BitReader<'b> {
    pub fn new(buf: &'b [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    #[inline]
    pub fn bit(&mut self) -> Result<bool, DecodeError> {
        let byte = *self.buf.get(self.pos / 8).ok_or(DecodeError::UnexpectedEof)?;
        let v = byte & (0x80 >> (self.pos % 8)) != 0;
        self.pos += 1;
        Ok(v)
    }

    /// Reads `n` bits (at most 32), most significant first.
    #[inline]
    pub fn bits(&mut self, n: u32) -> Result<u32, DecodeError> {
        debug_assert!(n <= 32);
        if n == 0 {
            return Ok(0);
        }
        let end = self.pos + n as usize;
        if end > self.buf.len() * 8 {
            return Err(DecodeError::UnexpectedEof);
        }
        let first = self.pos / 8;
        // Up to five octets cover the bits; load eight when they exist, else pad with zeros.
        let word = match self.buf.get(first..first + 8) {
            Some(w) => u64::from_be_bytes(w.try_into().unwrap()),
            None => {
                // Near the end of the buffer: the (at most five) octets that hold the bits, one by
                // one. A variable-length copy here was a call to `memmove` for most bits of a short message.
                let need = (self.pos % 8 + n as usize).div_ceil(8);
                let mut word = 0u64;
                for i in 0..need {
                    word = (word << 8) | u64::from(self.buf[first + i]);
                }
                word << (8 * (8 - need))
            }
        };
        let v = ((word << (self.pos % 8)) >> (64 - n)) as u32;
        self.pos = end;
        Ok(v)
    }

    /// Skips `n` octets.
    #[inline]
    pub fn skip_bytes(&mut self, n: usize) -> Result<(), DecodeError> {
        let end = self.pos + n * 8;
        if end > self.buf.len() * 8 {
            return Err(DecodeError::UnexpectedEof);
        }
        self.pos = end;
        Ok(())
    }

    /// Reads whole octets into `out`.
    #[inline]
    pub fn bytes(&mut self, out: &mut [u8]) -> Result<(), DecodeError> {
        if self.pos.is_multiple_of(8) {
            let start = self.pos / 8;
            let src = self.buf.get(start..start + out.len()).ok_or(DecodeError::UnexpectedEof)?;
            out.copy_from_slice(src);
            self.pos += out.len() * 8;
            return Ok(());
        }
        for b in out {
            *b = self.bits(8)? as u8;
        }
        Ok(())
    }

    #[inline]
    pub fn uint(&mut self) -> Result<u32, DecodeError> {
        u32::try_from(self.uint64()?)
            .map_err(|_| DecodeError::InvalidValue("unsigned integer exceeds 32 bits"))
    }

    #[inline]
    pub fn int(&mut self) -> Result<i32, DecodeError> {
        let negative = self.bit()?;
        let magnitude = i32::try_from(self.uint()?).map_err(|_| DecodeError::InvalidValue("integer exceeds 32 bits"))?;
        Ok(if negative { !magnitude } else { magnitude })
    }

    #[inline]
    pub fn uint64(&mut self) -> Result<u64, DecodeError> {
        let mut v: u64 = 0;
        for i in 0..10 {
            let octet = self.bits(8)?;
            let group = u64::from(octet & 0x7f);
            if i == 9 && group > 1 {
                return Err(DecodeError::InvalidValue("unsigned integer exceeds 64 bits"));
            }
            v |= group << (7 * i);
            if octet & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(DecodeError::InvalidValue("unsigned integer exceeds 64 bits"))
    }
}
