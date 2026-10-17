//! XML signatures of ISO 15118-20 messages with OpenSSL (7.9, V2G20-117 ff.): detached
//! signatures over EXI fragments. A [`sign`] call digests each referenced fragment, builds the
//! `SignedInfo`, encodes it as a fragment and signs that; [`verify`] does the same in reverse.
//! The fragments themselves come from [`iso15118_20::signature::encode_fragment`], the
//! structure goes into the message header ([`iso15118_20::common_types::MessageHeader`]).
//!
//! Two suites, chosen by the key: ECDSA over secp521r1 with SHA-512 (V2G20-2473, 2475; the value
//! is the 132 octets r || s) and Ed448 with SHAKE256 digests of 64 octets (V2G20-2474, 2476;
//! the standard names the digest algorithm but not its output length).
//!
//! The octets that are hashed and signed are the complete EXI stream of the fragment as
//! EXIficient writes it, header octet included; the standard does not say whether the header
//! belongs to the canonical form, so this is the one assumption to check against another
//! implementation.

use std::fmt;

use iso15118_20::signature::{
    CANONICAL_EXI, DIGEST_SHA512, DIGEST_SHAKE256, ECDSA_SHA512, ED448, Reference, Signature, SignatureValue, SignedInfo,
};
use openssl::bn::BigNum;
use openssl::ec::EcKey;
use openssl::ecdsa::EcdsaSig;
use openssl::error::ErrorStack;
use openssl::hash::{Hasher, MessageDigest, hash};
use openssl::nid::Nid;
use openssl::pkey::{Id, PKeyRef, Private, Public};
use openssl::sign::{Signer, Verifier};

/// Size of a P-521 coordinate in octets.
const P521_OCTETS: usize = 66;
/// Output length of the SHAKE256 digest.
const SHAKE256_OCTETS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suite {
    /// ECDSA with secp521r1 and SHA-512.
    EcdsaP521,
    /// Ed448 with SHAKE256 digests.
    Ed448,
}

impl Suite {
    /// The suite a key belongs to, if it is one of the two.
    pub fn of_key<T: openssl::pkey::HasPublic>(key: &PKeyRef<T>) -> Option<Suite> {
        match key.id() {
            Id::ED448 => Some(Suite::Ed448),
            Id::EC if key.ec_key().ok()?.group().curve_name() == Some(Nid::SECP521R1) => Some(Suite::EcdsaP521),
            _ => None,
        }
    }

    pub fn signature_algorithm(self) -> &'static str {
        match self {
            Suite::EcdsaP521 => ECDSA_SHA512,
            Suite::Ed448 => ED448,
        }
    }

    pub fn digest_algorithm(self) -> &'static str {
        match self {
            Suite::EcdsaP521 => DIGEST_SHA512,
            Suite::Ed448 => DIGEST_SHAKE256,
        }
    }

    /// The digest of a fragment.
    pub fn digest(self, fragment: &[u8]) -> Result<Vec<u8>, ErrorStack> {
        match self {
            Suite::EcdsaP521 => Ok(hash(MessageDigest::sha512(), fragment)?.to_vec()),
            Suite::Ed448 => {
                let mut hasher = Hasher::new(MessageDigest::shake_256())?;
                hasher.update(fragment)?;
                let mut out = vec![0u8; SHAKE256_OCTETS];
                hasher.finish_xof(&mut out)?;
                Ok(out)
            }
        }
    }
}

#[derive(Debug)]
pub enum Error {
    /// The key is neither secp521r1 nor Ed448.
    UnsupportedKey,
    /// A fragment, or the `SignedInfo`, did not fit its buffer or could not be encoded.
    Encode(exi::EncodeError),
    /// OpenSSL failed.
    Crypto(ErrorStack),
    /// The signature does not use the profile's algorithms, or the key's.
    Algorithm(&'static str),
    /// A reference has no fragment to check against, or points at one twice.
    Reference(String),
    /// The digest of a referenced fragment differs from the one in the signature.
    Digest(String),
    /// The signature value does not verify.
    Value,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnsupportedKey => write!(f, "the key is neither secp521r1 nor Ed448"),
            Error::Encode(e) => write!(f, "cannot encode the signature: {e:?}"),
            Error::Crypto(e) => write!(f, "{e}"),
            Error::Algorithm(what) => write!(f, "{what}"),
            Error::Reference(uri) => write!(f, "reference {uri} does not match a signed fragment"),
            Error::Digest(uri) => write!(f, "the digest of {uri} is wrong"),
            Error::Value => write!(f, "the signature value is wrong"),
        }
    }
}

impl std::error::Error for Error {}

impl From<ErrorStack> for Error {
    fn from(e: ErrorStack) -> Self {
        Error::Crypto(e)
    }
}

impl From<exi::EncodeError> for Error {
    fn from(e: exi::EncodeError) -> Self {
        Error::Encode(e)
    }
}

/// A signature that owns its strings and octets; [`Self::signature`] lends it to the codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedSignature {
    suite: Suite,
    references: Vec<(String, Vec<u8>)>,
    value: Vec<u8>,
}

impl OwnedSignature {
    /// Calls `f` with the signature as the codec's structure, e.g. to put it into a header.
    pub fn with_signature<R>(&self, f: impl FnOnce(&Signature<'_>) -> R) -> R {
        let references: Vec<Reference<'_>> = self
            .references
            .iter()
            .map(|(uri, digest)| Reference {
                id: None,
                uri: Some(uri),
                transform_algorithm: Some(CANONICAL_EXI),
                digest_algorithm: self.suite.digest_algorithm(),
                digest_value: digest,
            })
            .collect();
        let signature = Signature {
            id: None,
            signed_info: SignedInfo {
                id: None,
                canonicalization_algorithm: CANONICAL_EXI,
                signature_algorithm: self.suite.signature_algorithm(),
                reference: &references,
            },
            signature_value: SignatureValue { id: None, value: &self.value },
        };
        f(&signature)
    }
}

fn fragment_of(info: &SignedInfo<'_>) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0u8; 4096];
    let n = info.encode_fragment(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Signs the given fragments (`(uri, EXI fragment)`, the uri being `#` and the element's `Id`).
pub fn sign(key: &PKeyRef<Private>, fragments: &[(&str, &[u8])]) -> Result<OwnedSignature, Error> {
    let suite = Suite::of_key(key).ok_or(Error::UnsupportedKey)?;
    let references = fragments
        .iter()
        .map(|(uri, fragment)| Ok((uri.to_string(), suite.digest(fragment)?)))
        .collect::<Result<Vec<_>, ErrorStack>>()?;
    let mut owned = OwnedSignature { suite, references, value: Vec::new() };
    let to_sign = owned.with_signature(|s| fragment_of(&s.signed_info))?;
    owned.value = sign_value(key, suite, &to_sign)?;
    Ok(owned)
}

/// The signature value over the octets of a `SignedInfo` fragment: for ECDSA `r || s` of 132
/// octets, for Ed448 the plain signature.
pub fn sign_value(key: &PKeyRef<Private>, suite: Suite, signed: &[u8]) -> Result<Vec<u8>, ErrorStack> {
    Ok(match suite {
        Suite::EcdsaP521 => {
            let signature = EcdsaSig::sign(&hash(MessageDigest::sha512(), signed)?, &*key.ec_key()?)?;
            let (r, s) = (signature.r().to_vec_padded(P521_OCTETS as i32)?, signature.s().to_vec_padded(P521_OCTETS as i32)?);
            [r, s].concat()
        }
        Suite::Ed448 => Signer::new_without_digest(key)?.sign_oneshot_to_vec(signed)?,
    })
}

/// Verifies a signature received in a header against the fragments it should cover and the
/// signer's public key (from the certificate the standard prescribes for the message, Table 17).
pub fn verify(key: &PKeyRef<Public>, signature: &Signature<'_>, fragments: &[(&str, &[u8])]) -> Result<(), Error> {
    let suite = Suite::of_key(key).ok_or(Error::UnsupportedKey)?;
    let info = &signature.signed_info;
    if info.canonicalization_algorithm != CANONICAL_EXI {
        return Err(Error::Algorithm("the canonicalization method is not canonical EXI"));
    }
    if info.signature_algorithm != suite.signature_algorithm() {
        return Err(Error::Algorithm("the signature method does not match the key"));
    }
    // Every referenced fragment is digested and compared; every fragment must be referenced.
    let mut used = vec![false; fragments.len()];
    for reference in info.reference {
        let uri = reference.uri.unwrap_or_default();
        if reference.digest_algorithm != suite.digest_algorithm() {
            return Err(Error::Algorithm("the digest method does not match the key"));
        }
        if reference.transform_algorithm.is_some_and(|t| t != CANONICAL_EXI) {
            return Err(Error::Algorithm("the transform is not canonical EXI"));
        }
        let i = fragments
            .iter()
            .position(|(u, _)| *u == uri)
            .filter(|&i| !std::mem::replace(&mut used[i], true))
            .ok_or_else(|| Error::Reference(uri.to_string()))?;
        if suite.digest(fragments[i].1)? != reference.digest_value {
            return Err(Error::Digest(uri.to_string()));
        }
    }
    if let Some(i) = used.iter().position(|u| !u) {
        return Err(Error::Reference(fragments[i].0.to_string()));
    }
    let signed = fragment_of(info)?;
    let value = signature.signature_value.value;
    let ok = match suite {
        Suite::EcdsaP521 => {
            if value.len() != 2 * P521_OCTETS {
                return Err(Error::Value);
            }
            let (r, s) = value.split_at(P521_OCTETS);
            let signature = EcdsaSig::from_private_components(BigNum::from_slice(r)?, BigNum::from_slice(s)?)?;
            let ec: EcKey<Public> = key.ec_key()?;
            signature.verify(&hash(MessageDigest::sha512(), &signed)?, &*ec)?
        }
        Suite::Ed448 => Verifier::new_without_digest(key)?.verify_oneshot(value, &signed)?,
    };
    if ok { Ok(()) } else { Err(Error::Value) }
}
