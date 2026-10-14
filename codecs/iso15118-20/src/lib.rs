//! ISO 15118-20 message types and schema-informed EXI codec.
//!
//! `no_std`, no allocation: decoded messages borrow from a host-provided [`exi::Arena`].
//! Hand-written for the first slice; `exi-gen` will replace it, validated against the
//! same reference vectors.
#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

#[cfg(test)]
extern crate std;

pub mod common;
pub mod dc;
pub mod services;
