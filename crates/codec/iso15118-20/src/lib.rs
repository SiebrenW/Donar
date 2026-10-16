//! ISO 15118-20 message types and their schema-informed EXI codec.
//!
//! The codec is generated from the standard's XSDs by `exi-gen` (run `cargo xtask setup`)
//! and is `no_std` and allocation-free: decoded messages borrow from a host-provided
//! [`exi::Arena`]. Each module is one schema namespace: [`common_types`] (shared types such
//! as the header and response codes), [`common`] (the messages every service uses) and
//! [`dc`].
#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

mod generated;
mod rational;
pub mod signature;

pub use generated::{common, common_types, dc, xmldsig};
pub use signature::Signature;
