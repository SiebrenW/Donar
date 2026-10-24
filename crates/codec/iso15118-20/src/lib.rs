//! ISO 15118-20 message types and their schema-informed EXI codec.
//!
//! The codec is generated from the standard's XSDs by `exi-gen` (run `cargo xtask setup`)
//! and is `no_std` and allocation-free: decoded messages borrow from a host-provided
//! [`exi::Arena`]. Each module is one schema namespace: [`common_types`] (shared types such
//! as the header and response codes), [`common`] (the messages every service uses) and
//! [`dc`], [`ac`], [`acdp`] and [`wpt`]. The ACDP schema types its disconnect messages as the
//! connect messages, so `AcdpConnectReq` encodes as either: `encode` is `ACDP_ConnectReq`,
//! `encode_as_acdp_disconnect_req` is `ACDP_DisconnectReq` (likewise for the response).
#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

mod generated;
mod rational;
pub mod response_codes;
pub mod signature;

pub use generated::{ac, acdp, common, common_types, dc, wpt, xmldsig};
pub use response_codes::ResponseMessage;
pub use signature::Signature;
