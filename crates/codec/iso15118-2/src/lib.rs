//! ISO 15118-2 message types and their schema-informed EXI codec.
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

/// The message header's signature: the structure is the xmldsig element of ISO 15118-20's codec
/// (the profile of V2G2-771 is the same: `SignedInfo` and `SignatureValue`, no `KeyInfo`); what is
/// signed are fragments of this schema.
pub mod signature {
    use exi::codec::Enc;
    use exi::strings::Table;
    use exi::EncodeError;

    pub use crate::generated::fragment;
    pub use iso15118_20::signature::{CANONICAL_EXI, Reference, Signature, SignatureValue, SignedInfo};

    /// `SignatureMethod` with ECDSA over secp256r1 and SHA-256 .
    pub const ECDSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#ecdsa-sha256";
    /// `DigestMethod` with SHA-256.
    pub const DIGEST_SHA256: &str = "http://www.w3.org/2001/04/xmlenc#sha256";

    /// Encodes one element as the EXI stream that is hashed and signed: the schema-informed
    /// fragment grammar, with the start event `element` (a constant of [`fragment`]) and the
    /// element's content from `content` (a generated type's `encode_content`).
    pub fn encode_fragment<'v>(
        out: &mut [u8],
        element: u32,
        content: impl FnOnce(&mut Enc<'_, 'v>) -> Result<(), EncodeError>,
    ) -> Result<usize, EncodeError> {
        // valuePartitionCapacity 0 (Table 11): no string value table.
        let mut table = Table::without_values();
        let mut e = Enc::new(out, &mut table)?;
        e.root(element, fragment::CODE_BITS)?;
        content(&mut e)?;
        e.root(fragment::END, fragment::CODE_BITS)?;
        e.finish()
    }

    /// The octets a signature value is computed over: the `SignedInfo` as a fragment of the grammar of
    /// the xmldsig schema alone (ISO 15118-2 J.2), where the signed elements use that of `V2G_CI_MsgDef`.
    pub fn signed_info_fragment(info: &SignedInfo<'_>, out: &mut [u8]) -> Result<usize, EncodeError> {
        let mut table = Table::without_values();
        let mut e = Enc::new(out, &mut table)?;
        e.root(fragment::XMLDSIG_SIGNED_INFO, fragment::XMLDSIG_CODE_BITS)?;
        info.encode_content(&mut e)?;
        e.root(fragment::XMLDSIG_END, fragment::XMLDSIG_CODE_BITS)?;
        e.finish()
    }
}

pub use generated::{body, def, header, types, xmldsig};
