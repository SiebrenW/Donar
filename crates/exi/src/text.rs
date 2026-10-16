//! Strings.

use crate::bits::BitReader;
use crate::{Arena, DecodeError};

/// Reads `len` code points. With an arena the UTF-8 is stored there; without one the
/// characters are validated and skipped.
pub fn decode_string<'s>(
    r: &mut BitReader<'_>,
    len: usize,
    arena: Option<&'s Arena<'_>>,
) -> Result<&'s str, DecodeError> {
    let char_at = |r: &mut BitReader<'_>| {
        char::from_u32(r.uint()?).ok_or(DecodeError::InvalidValue("invalid code point"))
    };
    let Some(arena) = arena else {
        for _ in 0..len {
            char_at(r)?;
        }
        return Ok("");
    };
    // Nearly every string is ASCII: read it in one pass into exactly `len` bytes. A code
    // point above 0x7F aborts that attempt (the arena rolls back) and the string is read again
    // in two passes, one to size it and one to store it.
    struct NotAscii;
    impl From<crate::ArenaFull> for NotAscii {
        fn from(_: crate::ArenaFull) -> Self {
            NotAscii
        }
    }
    let start = r.clone();
    let quick = arena.alloc_slice_with(len, |_| match r.uint() {
        Ok(c) if c < 0x80 => Ok(c as u8),
        _ => Err(NotAscii),
    });
    if let Ok(out) = quick {
        return core::str::from_utf8(out).map_err(|_| DecodeError::InvalidValue("invalid UTF-8"));
    }
    *r = start;
    let mut sizing = r.clone();
    let mut bytes = 0;
    for _ in 0..len {
        bytes += char_at(&mut sizing)?.len_utf8();
    }
    let out = arena.alloc_bytes(bytes)?;
    let mut at = 0;
    for _ in 0..len {
        at += char_at(r)?.encode_utf8(&mut out[at..]).len();
    }
    core::str::from_utf8(out).map_err(|_| DecodeError::InvalidValue("invalid UTF-8"))
}
