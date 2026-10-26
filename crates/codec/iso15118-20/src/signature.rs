//! The signature of a message (the `ds:Signature` of the header, ISO 15118-20:2022 8.3.3 and 7.9 / V2G20-771) is the
//! type of the `xmldsig` crate; this module has what the standard adds: the algorithms (V2G20-765 to 767, 2473 to 2476)
//! and the fragment encoding of the signed parts (V2G20-119, V2G20-1449). Computing digests and signatures is the
//! host's (`evcc-linux`).

use exi::codec::Enc;
use exi::strings::Table;
use exi::EncodeError;

impl crate::common_types::MessageHeader<'static> {
    /// A header without a signature.
    pub const fn new(session_id: [u8; 8], time_stamp: u64) -> Self {
        Self { session_id, time_stamp, signature: None }
    }
}

pub use crate::generated::fragment;

/// `Algorithm` of the canonicalization method (V2G20-765) and of the one transform (V2G20-766).
pub const CANONICAL_EXI: &str = "http://www.w3.org/TR/canonical-exi/";
/// `SignatureMethod` with ECDSA over secp521r1 and SHA-512 (V2G20-2473).
pub const ECDSA_SHA512: &str = "http://www.w3.org/2001/04/xmldsig-more#ecdsa-sha512";
/// `SignatureMethod` with Ed448 (V2G20-2474).
pub const ED448: &str = "urn:iso:std:iso:15118:-20:Security:xmldsig#Ed448";
/// `DigestMethod` for the ECDSA suite (V2G20-2475).
pub const DIGEST_SHA512: &str = "http://www.w3.org/2001/04/xmlenc#sha512";
/// `DigestMethod` for the Ed448 suite (V2G20-2476).
pub const DIGEST_SHAKE256: &str = "urn:iso:std:iso:15118:-20:Security:xmlenc#SHAKE256";

/// Encodes one element as the EXI stream that is hashed and signed (V2G20-119, V2G20-1449):
/// the schema-informed *fragment* grammar of the CommonMessages schema, with the start event
/// `element` (a constant of [`fragment`]) and the element's content from `content` (a
/// generated type's `encode_content`). Returns the length; the stream starts with the usual
/// EXI header octet, as EXIficient writes it.
pub fn encode_fragment<'v>(
    out: &mut [u8],
    element: u32,
    content: impl FnOnce(&mut Enc<'_, 'v>) -> Result<(), EncodeError>,
) -> Result<usize, EncodeError> {
    let mut table = Table::without_values();
    let mut e = Enc::new(out, &mut table)?;
    e.root(element, fragment::CODE_BITS)?;
    content(&mut e)?;
    e.root(fragment::END, fragment::CODE_BITS)?;
    e.finish()
}
