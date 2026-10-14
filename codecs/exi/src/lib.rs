//! Runtime support shared by the schema-informed EXI codecs (`app-protocol`, `iso15118-20`,
//! later DIN SPEC 70121 and ISO 15118-2): bit-level I/O, the bump arena decoded messages
//! live in, and the error types. `no_std`, no allocation.
#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

pub mod arena;
pub mod bits;
pub mod text;

pub use arena::{Arena, ArenaFull};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodeError {
    BufferTooSmall,
    /// A field is outside the range the schema allows.
    InvalidValue(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    UnexpectedEof,
    ArenaFull,
    /// A well-formed EXI construct this codec does not handle (escape event codes,
    /// empty string elements, ...).
    Unsupported(&'static str),
    InvalidValue(&'static str),
}

impl From<ArenaFull> for DecodeError {
    fn from(_: ArenaFull) -> Self {
        DecodeError::ArenaFull
    }
}
