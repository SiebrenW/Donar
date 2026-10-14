//! Bit-packed EXI primitives: MSB-first bits and EXI unsigned integers.

use crate::{DecodeError, EncodeError};

pub struct BitWriter<'b> {
    buf: &'b mut [u8],
    pos: usize,
}

impl<'b> BitWriter<'b> {
    pub fn new(buf: &'b mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub fn bit(&mut self, v: bool) -> Result<(), EncodeError> {
        let byte = self.pos / 8;
        let slot = self.buf.get_mut(byte).ok_or(EncodeError::BufferTooSmall)?;
        if self.pos.is_multiple_of(8) {
            *slot = 0;
        }
        if v {
            *slot |= 0x80 >> (self.pos % 8);
        }
        self.pos += 1;
        Ok(())
    }

    /// Writes the low `n` bits of `v`, most significant first.
    pub fn bits(&mut self, v: u32, n: u32) -> Result<(), EncodeError> {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1 == 1)?;
        }
        Ok(())
    }

    /// EXI unsigned integer: 7-bit groups, least significant group first, high bit = more.
    pub fn uint(&mut self, mut v: u32) -> Result<(), EncodeError> {
        loop {
            let group = v & 0x7f;
            v >>= 7;
            self.bits(group | if v != 0 { 0x80 } else { 0 }, 8)?;
            if v == 0 {
                return Ok(());
            }
        }
    }

    /// Pads with zero bits to a byte boundary and returns the stream length in bytes.
    pub fn finish(mut self) -> Result<usize, EncodeError> {
        while !self.pos.is_multiple_of(8) {
            self.bit(false)?;
        }
        Ok(self.pos / 8)
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

    pub fn bit(&mut self) -> Result<bool, DecodeError> {
        let byte = *self.buf.get(self.pos / 8).ok_or(DecodeError::UnexpectedEof)?;
        let v = byte & (0x80 >> (self.pos % 8)) != 0;
        self.pos += 1;
        Ok(v)
    }

    pub fn bits(&mut self, n: u32) -> Result<u32, DecodeError> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | u32::from(self.bit()?);
        }
        Ok(v)
    }

    pub fn uint(&mut self) -> Result<u32, DecodeError> {
        let mut v: u32 = 0;
        for i in 0..5 {
            let octet = self.bits(8)?;
            let group = octet & 0x7f;
            if i == 4 && group > 0x0f {
                return Err(DecodeError::InvalidValue("unsigned integer exceeds 32 bits"));
            }
            v |= group << (7 * i);
            if octet & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(DecodeError::InvalidValue("unsigned integer exceeds 32 bits"))
    }
}
