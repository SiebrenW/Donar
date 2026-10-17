//! Plug & Charge credentials for the session machine from PEM files: the contract certificate
//! chain and the contract's private key, signing with OpenSSL (see [`crate::xmlsig`]).

use evcc_core::ev::Contract;
use iso15118_20::signature::{DIGEST_SHA512, DIGEST_SHAKE256, ECDSA_SHA512, ED448};
use openssl::error::ErrorStack;
use openssl::pkey::{PKey, Private};
use openssl::x509::X509;

use crate::xmlsig::{self, Suite};

/// A contract certificate with its sub-CA certificates and key.
pub struct PemContract {
    /// The contract certificate first, then the sub-CA certificates, DER.
    chain: Vec<Vec<u8>>,
    key: PKey<Private>,
    suite: Suite,
    /// The eMAID: the common name of the contract certificate's subject (Annex B.9 / ISO 15118-2 Annex E).
    emaid: String,
}

#[derive(Debug)]
pub enum Error {
    Pem(ErrorStack),
    /// No certificate in the chain.
    Empty,
    /// The key is neither secp521r1 nor Ed448.
    UnsupportedKey,
    /// The key does not belong to the first certificate.
    KeyMismatch,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Pem(e) => write!(f, "{e}"),
            Error::Empty => write!(f, "the contract certificate chain is empty"),
            Error::UnsupportedKey => write!(f, "the contract key is neither secp521r1 nor Ed448"),
            Error::KeyMismatch => write!(f, "the key does not belong to the contract certificate"),
        }
    }
}

impl std::error::Error for Error {}

impl From<ErrorStack> for Error {
    fn from(e: ErrorStack) -> Self {
        Error::Pem(e)
    }
}

impl PemContract {
    /// `chain_pem`: the contract certificate, then its sub-CAs (the root is not sent); the key
    /// may be encrypted with `password`.
    pub fn new(chain_pem: &[u8], key_pem: &[u8], password: Option<&[u8]>) -> Result<PemContract, Error> {
        let certs = X509::stack_from_pem(chain_pem)?;
        let chain: Vec<Vec<u8>> = certs.iter().map(|c| c.to_der()).collect::<Result<_, _>>()?;
        let key = match password {
            Some(p) => PKey::private_key_from_pem_passphrase(key_pem, p)?,
            None => PKey::private_key_from_pem(key_pem)?,
        };
        let leaf = certs.first().ok_or(Error::Empty)?;
        if !leaf.public_key()?.public_eq(&key) {
            return Err(Error::KeyMismatch);
        }
        let suite = Suite::of_key(&key).ok_or(Error::UnsupportedKey)?;
        let emaid = leaf
            .subject_name()
            .entries_by_nid(openssl::nid::Nid::COMMONNAME)
            .next()
            .and_then(|e| e.data().to_string().ok())
            .unwrap_or_default()
            // In a message the eMAID has no separators (ISO 15118-2 `eMAIDType`, 14 or 15 characters).
            .replace('-', "");
        Ok(PemContract { chain, key, suite, emaid })
    }
}

fn copy_into(value: &[u8], out: &mut [u8]) -> Option<usize> {
    out.get_mut(..value.len())?.copy_from_slice(value);
    Some(value.len())
}

impl Contract for PemContract {
    fn certificate(&self) -> &[u8] {
        &self.chain[0]
    }

    fn sub_certificate(&self, index: usize) -> Option<&[u8]> {
        self.chain.get(index + 1).map(Vec::as_slice)
    }

    fn signature_algorithm(&self) -> &str {
        match self.suite {
            Suite::EcdsaP521 => ECDSA_SHA512,
            Suite::Ed448 => ED448,
            Suite::EcdsaP256 => iso15118_2::signature::ECDSA_SHA256,
        }
    }

    fn digest_algorithm(&self) -> &str {
        match self.suite {
            Suite::EcdsaP521 => DIGEST_SHA512,
            Suite::Ed448 => DIGEST_SHAKE256,
            Suite::EcdsaP256 => iso15118_2::signature::DIGEST_SHA256,
        }
    }

    fn emaid(&self) -> &str {
        &self.emaid
    }

    fn digest(&self, data: &[u8], out: &mut [u8]) -> Option<usize> {
        copy_into(&self.suite.digest(data).ok()?, out)
    }

    fn sign(&self, data: &[u8], out: &mut [u8]) -> Option<usize> {
        copy_into(&xmlsig::sign_value(&self.key, self.suite, data).ok()?, out)
    }
}
