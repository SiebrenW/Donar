//! The XML signature in the message header (`ds:Signature`, ISO 15118-20:2022 8.3.3 and
//! 7.9 / V2G20-771): `SignedInfo` with one canonicalization method, one signature method and
//! references, and the `SignatureValue`. Hand-written because the xmldsig grammar uses
//! wildcards and mixed content, which `exi-gen` does not model; the layouts below follow the
//! grammar EXIficient builds from `xmldsig-core-schema.xsd` and the vectors in
//! `tests/vectors.rs` check the bytes against it.
//!
//! What the profile leaves out is not accepted: `KeyInfo`, `Object`, `HMACOutputLength`, the
//! `Type` attribute of a reference, wildcard content and more than one `Transform` per
//! reference are refused when decoding (and cannot be built). The signature itself is only a
//! structure here: computing digests and signatures is the host's (`evcc-linux`).

use exi::codec::{Dec, Enc};
use exi::layout::{Event, Layout, Pos, Slot, table};
use exi::{DecodeError, EncodeError};

// String table partitions (qname ids) of the attributes; above those of the generated modules.
const Q_ALGORITHM: u8 = 200;
const Q_ID: u8 = 201;
const Q_URI: u8 = 202;

const MANY: u32 = u32::MAX;

// A required attribute, then wildcard content (and generic characters, the mixed content):
// `CanonicalizationMethod` and `DigestMethod`.
const L_METHOD: Layout = Layout(&table([Slot::new(1, 1, 1), Slot::new(0, MANY, 1)]), 1);
// `SignatureMethod`: the algorithm, `HMACOutputLength`, wildcard content, mixed.
const L_SIGNATURE_METHOD: Layout = Layout(&table([Slot::new(1, 1, 1), Slot::new(0, 1, 1), Slot::new(0, MANY, 1)]), 1);
// `Transform`: the algorithm, then repeated (`XPath` or wildcard), mixed.
const L_TRANSFORM: Layout = Layout(&table([Slot::new(1, 1, 1), Slot::new(0, MANY, 2)]), 1);
const L_TRANSFORMS: Layout = Layout(&table([Slot::new(1, MANY, 1)]), 0);
// `Reference`: attributes `Id`, `Type`, `URI`, then `Transforms`, `DigestMethod`, `DigestValue`.
const L_REFERENCE: Layout = Layout(
    &table([
        Slot::new(0, 1, 1),
        Slot::new(0, 1, 1),
        Slot::new(0, 1, 1),
        Slot::new(0, 1, 1),
        Slot::new(1, 1, 1),
        Slot::new(1, 1, 1),
    ]),
    0,
);
const L_SIGNED_INFO: Layout = Layout(&table([Slot::new(0, 1, 1), Slot::new(1, 1, 1), Slot::new(1, 1, 1), Slot::new(1, MANY, 1)]), 0);
// `SignatureValue`: the optional `Id`, then the characters (a slot of its own).
const L_SIGNATURE_VALUE: Layout = Layout(&table([Slot::new(0, 1, 1), Slot::new(1, 1, 1)]), 0);
// `Signature`: `Id`, `SignedInfo`, `SignatureValue`, `KeyInfo`, `Object`.
const L_SIGNATURE: Layout =
    Layout(&table([Slot::new(0, 1, 1), Slot::new(1, 1, 1), Slot::new(1, 1, 1), Slot::new(0, 1, 1), Slot::new(0, MANY, 1)]), 0);

const NOT_IN_PROFILE: DecodeError = DecodeError::Unsupported("xmldsig content outside the ISO 15118-20 profile");
const MISSING: DecodeError = DecodeError::InvalidValue("required element missing");

/// A signature as sent in the message header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Signature<'a> {
    pub id: Option<&'a str>,
    pub signed_info: SignedInfo<'a>,
    pub signature_value: SignatureValue<'a>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedInfo<'a> {
    pub id: Option<&'a str>,
    /// `Algorithm` of the `CanonicalizationMethod` (V2G20-765: `http://www.w3.org/TR/canonical-exi/`).
    pub canonicalization_algorithm: &'a str,
    /// `Algorithm` of the `SignatureMethod` (V2G20-2473, V2G20-2474).
    pub signature_algorithm: &'a str,
    pub reference: &'a [Reference<'a>],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reference<'a> {
    pub id: Option<&'a str>,
    /// The signed element: `#` and its `Id`.
    pub uri: Option<&'a str>,
    /// `Algorithm` of the one `Transform` (V2G20-766, V2G20-767), if there is a `Transforms`.
    pub transform_algorithm: Option<&'a str>,
    /// `Algorithm` of the `DigestMethod` (V2G20-2475, V2G20-2476).
    pub digest_algorithm: &'a str,
    pub digest_value: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureValue<'a> {
    pub id: Option<&'a str>,
    pub value: &'a [u8],
}

/// `<Method Algorithm="...">` without content, for the layout `l`.
fn encode_method<'m>(e: &mut Enc<'_, 'm>, l: &Layout, algorithm: &'m str) -> Result<(), EncodeError> {
    let pos = e.start(l, Pos::START, 0, 0)?;
    e.string(Q_ALGORITHM, algorithm, usize::MAX)?;
    e.end(l, pos)
}

/// Reads such an element: the algorithm, then nothing.
fn decode_method<'r, 's>(d: &mut Dec<'r, 's>, l: &Layout) -> Result<&'s str, DecodeError> {
    let mut pos = Pos::START;
    let mut algorithm = None;
    loop {
        match d.event(l, pos)? {
            Event::End => break,
            Event::Start { slot: 0, next, .. } => {
                algorithm = Some(d.string(Q_ALGORITHM, usize::MAX)?);
                pos = next;
            }
            Event::Start { .. } => return Err(NOT_IN_PROFILE),
        }
    }
    algorithm.ok_or(MISSING)
}

impl<'a> Signature<'a> {
    pub fn encode_content<'m>(&'m self, e: &mut Enc<'_, 'm>) -> Result<(), EncodeError>
    where
        'a: 'm,
    {
        let mut pos = Pos::START;
        if let Some(id) = self.id {
            pos = e.start(&L_SIGNATURE, pos, 0, 0)?;
            e.string(Q_ID, id, usize::MAX)?;
        }
        pos = e.start(&L_SIGNATURE, pos, 1, 0)?;
        self.signed_info.encode_content(e)?;
        pos = e.start(&L_SIGNATURE, pos, 2, 0)?;
        self.signature_value.encode_content(e)?;
        e.end(&L_SIGNATURE, pos)
    }

    pub fn decode_content<'r>(d: &mut Dec<'r, 'a>) -> Result<Self, DecodeError> {
        let mut pos = Pos::START;
        let (mut id, mut info, mut value) = (None, None, None);
        loop {
            match d.event(&L_SIGNATURE, pos)? {
                Event::End => break,
                Event::Start { slot: 0, next, .. } => {
                    id = Some(d.string(Q_ID, usize::MAX)?);
                    pos = next;
                }
                Event::Start { slot: 1, next, .. } => {
                    info = Some(SignedInfo::decode_content(d)?);
                    pos = next;
                }
                Event::Start { slot: 2, next, .. } => {
                    value = Some(SignatureValue::decode_content(d)?);
                    pos = next;
                }
                Event::Start { .. } => return Err(NOT_IN_PROFILE),
            }
        }
        Ok(Self { id, signed_info: info.ok_or(MISSING)?, signature_value: value.ok_or(MISSING)? })
    }
}

impl<'a> SignedInfo<'a> {
    pub fn encode_content<'m>(&'m self, e: &mut Enc<'_, 'm>) -> Result<(), EncodeError>
    where
        'a: 'm,
    {
        if self.reference.is_empty() {
            return Err(EncodeError::InvalidValue("a signature needs at least one reference"));
        }
        let mut pos = Pos::START;
        if let Some(id) = self.id {
            pos = e.start(&L_SIGNED_INFO, pos, 0, 0)?;
            e.string(Q_ID, id, usize::MAX)?;
        }
        pos = e.start(&L_SIGNED_INFO, pos, 1, 0)?;
        encode_method(e, &L_METHOD, self.canonicalization_algorithm)?;
        pos = e.start(&L_SIGNED_INFO, pos, 2, 0)?;
        encode_method(e, &L_SIGNATURE_METHOD, self.signature_algorithm)?;
        for r in self.reference {
            pos = e.start(&L_SIGNED_INFO, pos, 3, 0)?;
            r.encode_content(e)?;
        }
        e.end(&L_SIGNED_INFO, pos)
    }

    pub fn decode_content<'r>(d: &mut Dec<'r, 'a>) -> Result<Self, DecodeError> {
        let mut pos = Pos::START;
        let (mut id, mut canonicalization, mut signature, mut references) = (None, None, None, None);
        loop {
            match d.event(&L_SIGNED_INFO, pos)? {
                Event::End => break,
                Event::Start { slot: 0, next, .. } => {
                    id = Some(d.string(Q_ID, usize::MAX)?);
                    pos = next;
                }
                Event::Start { slot: 1, next, .. } => {
                    canonicalization = Some(decode_method(d, &L_METHOD)?);
                    pos = next;
                }
                Event::Start { slot: 2, next, .. } => {
                    signature = Some(decode_method(d, &L_SIGNATURE_METHOD)?);
                    pos = next;
                }
                Event::Start { slot: 3, next, .. } => {
                    let (items, p) = d.list(&L_SIGNED_INFO, 3, next, Reference::decode_content)?;
                    references = Some(items);
                    pos = p;
                }
                Event::Start { .. } => return Err(NOT_IN_PROFILE),
            }
        }
        Ok(Self {
            id,
            canonicalization_algorithm: canonicalization.ok_or(MISSING)?,
            signature_algorithm: signature.ok_or(MISSING)?,
            reference: references.ok_or(MISSING)?,
        })
    }
}

impl<'a> Reference<'a> {
    pub fn encode_content<'m>(&'m self, e: &mut Enc<'_, 'm>) -> Result<(), EncodeError>
    where
        'a: 'm,
    {
        let mut pos = Pos::START;
        if let Some(id) = self.id {
            pos = e.start(&L_REFERENCE, pos, 0, 0)?;
            e.string(Q_ID, id, usize::MAX)?;
        }
        if let Some(uri) = self.uri {
            pos = e.start(&L_REFERENCE, pos, 2, 0)?;
            e.string(Q_URI, uri, usize::MAX)?;
        }
        if let Some(algorithm) = self.transform_algorithm {
            pos = e.start(&L_REFERENCE, pos, 3, 0)?;
            // `Transforms` holds one `Transform`; the transform has the algorithm only.
            let inner = e.start(&L_TRANSFORMS, Pos::START, 0, 0)?;
            encode_method(e, &L_TRANSFORM, algorithm)?;
            e.end(&L_TRANSFORMS, inner)?;
        }
        pos = e.start(&L_REFERENCE, pos, 4, 0)?;
        encode_method(e, &L_METHOD, self.digest_algorithm)?;
        pos = e.start(&L_REFERENCE, pos, 5, 0)?;
        e.ch()?;
        e.binary(self.digest_value)?;
        e.ee()?;
        e.end(&L_REFERENCE, pos)
    }

    pub fn decode_content<'r>(d: &mut Dec<'r, 'a>) -> Result<Self, DecodeError> {
        let mut pos = Pos::START;
        let (mut id, mut uri, mut transform, mut digest, mut value) = (None, None, None, None, None);
        loop {
            match d.event(&L_REFERENCE, pos)? {
                Event::End => break,
                Event::Start { slot: 0, next, .. } => {
                    id = Some(d.string(Q_ID, usize::MAX)?);
                    pos = next;
                }
                Event::Start { slot: 2, next, .. } => {
                    uri = Some(d.string(Q_URI, usize::MAX)?);
                    pos = next;
                }
                Event::Start { slot: 3, next, .. } => {
                    // V2G20-767: one `Transform` at most.
                    let mut inner = Pos::START;
                    let mut algorithm = None;
                    loop {
                        match d.event(&L_TRANSFORMS, inner)? {
                            Event::End => break,
                            Event::Start { next, .. } if algorithm.is_none() => {
                                algorithm = Some(decode_method(d, &L_TRANSFORM)?);
                                inner = next;
                            }
                            Event::Start { .. } => return Err(NOT_IN_PROFILE),
                        }
                    }
                    transform = algorithm;
                    pos = next;
                }
                Event::Start { slot: 4, next, .. } => {
                    digest = Some(decode_method(d, &L_METHOD)?);
                    pos = next;
                }
                Event::Start { slot: 5, next, .. } => {
                    d.expect_ch()?;
                    value = Some(d.binary(usize::MAX)?);
                    d.expect_ee()?;
                    pos = next;
                }
                Event::Start { .. } => return Err(NOT_IN_PROFILE),
            }
        }
        Ok(Self {
            id,
            uri,
            transform_algorithm: transform,
            digest_algorithm: digest.ok_or(MISSING)?,
            digest_value: value.ok_or(MISSING)?,
        })
    }
}

impl<'a> SignatureValue<'a> {
    pub fn encode_content<'m>(&'m self, e: &mut Enc<'_, 'm>) -> Result<(), EncodeError>
    where
        'a: 'm,
    {
        let mut pos = Pos::START;
        if let Some(id) = self.id {
            pos = e.start(&L_SIGNATURE_VALUE, pos, 0, 0)?;
            e.string(Q_ID, id, usize::MAX)?;
        }
        pos = e.start(&L_SIGNATURE_VALUE, pos, 1, 0)?;
        e.binary(self.value)?;
        e.end(&L_SIGNATURE_VALUE, pos)
    }

    pub fn decode_content<'r>(d: &mut Dec<'r, 'a>) -> Result<Self, DecodeError> {
        let mut pos = Pos::START;
        let (mut id, mut value) = (None, None);
        loop {
            match d.event(&L_SIGNATURE_VALUE, pos)? {
                Event::End => break,
                Event::Start { slot: 0, next, .. } => {
                    id = Some(d.string(Q_ID, usize::MAX)?);
                    pos = next;
                }
                Event::Start { slot: 1, next, .. } => {
                    value = Some(d.binary(usize::MAX)?);
                    pos = next;
                }
                Event::Start { .. } => return Err(NOT_IN_PROFILE),
            }
        }
        Ok(Self { id, value: value.ok_or(MISSING)? })
    }
}

impl crate::common_types::MessageHeader<'static> {
    /// A header without a signature.
    pub const fn new(session_id: [u8; 8], time_stamp: u64) -> Self {
        Self { session_id, time_stamp, signature: None }
    }
}
