//! The `supportedAppProtocol` handshake (namespace `urn:iso:15118:2:2010:AppProtocol`).
//!
//! It runs on the first V2GTP payload type (`0x8001`), before the EVCC and EVSE have agreed
//! on a protocol, so it belongs to none of the standards. The codec is generated from the
//! schema by `exi-gen` (run `cargo xtask setup`); decoded messages borrow from an
//! [`exi::Arena`].
#![no_std]

mod generated;

pub use generated::app_protocol::*;
