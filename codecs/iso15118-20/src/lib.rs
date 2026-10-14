//! ISO 15118-20 message types and schema-informed EXI codec.
//!
//! `no_std`, no allocation: decoded messages borrow from a host-provided [`Arena`].
//! Hand-written for the first slice; `exi-gen` will replace it, validated against the
//! same reference vectors.
#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
extern crate std;

pub mod app_protocol;
pub mod arena;
pub mod bits;
pub mod common;

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
    /// string table hits, ...).
    Unsupported(&'static str),
    InvalidValue(&'static str),
}

impl From<ArenaFull> for DecodeError {
    fn from(_: ArenaFull) -> Self {
        DecodeError::ArenaFull
    }
}
