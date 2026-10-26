//! The xmldsig types of the ISO 15118 profile (`ds:Signature` in the message header of ISO 15118-20, ISO 15118-2 and
//! DIN SPEC 70121, and the issuer and serial number of a root certificate) and their schema-informed EXI codec.
//!
//! The types are generated from the W3C schema by `exi-gen` (run `cargo xtask setup`), like those of the standards.
//! The profile uses a part of the schema: `KeyInfo`, `Object`, `HMACOutputLength`, `XPath` transforms, the `Type` of a
//! reference and wildcard content are not modelled (they keep their event codes and are refused when decoding).
#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]

mod generated;

pub use generated::fragment;
pub use generated::xmldsig::*;

use exi::EncodeError;
use exi::codec::Enc;
use exi::strings::Table;

impl SignedInfo<'_> {
    /// The octets a signature value is computed over: the `SignedInfo` as a fragment of the grammar of the xmldsig
    /// schema alone (ISO 15118-2 J.2, ISO 15118-20 V2G20-1449), so the start event is chosen among the declarations of
    /// that schema. The stream starts with the usual EXI header octet, as EXIficient writes it. Returns the length.
    pub fn encode_fragment(&self, out: &mut [u8]) -> Result<usize, EncodeError> {
        let mut table = Table::without_values();
        let mut e = Enc::new(out, &mut table)?;
        e.root(fragment::SIGNED_INFO, fragment::CODE_BITS)?;
        self.encode_content(&mut e)?;
        e.root(fragment::END, fragment::CODE_BITS)?;
        e.finish()
    }
}
