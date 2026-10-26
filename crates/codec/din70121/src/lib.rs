//! DIN SPEC 70121 message types and their schema-informed EXI codec.
//!
//! The codec is generated from the specification's XSDs by `exi-gen` (run `cargo xtask setup`)
//! and is `no_std` and allocation-free: decoded messages borrow from a host-provided
//! [`exi::Arena`]. Each module is one schema namespace: [`types`] (the shared data types),
//! [`header`], [`body`] (the messages) and [`def`] (the `V2G_Message` document: header and body).
//! A message is encoded with [`def::V2gMessage::encode`] and decoded with
//! [`def::V2gMessage::decode`]; unlike ISO 15118-20 the header is a separate element, not part
//! of each message.
#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

mod generated;
mod physical;

pub use generated::{body, def, header, types};
