//! Throwaway certificate authorities and leaf certificates for the TLS tests.

use openssl::asn1::Asn1Time;
use openssl::bn::BigNum;
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::x509::extension::BasicConstraints;
use openssl::x509::{X509, X509NameBuilder};

#[derive(Clone, Copy)]
pub enum Curve {
    P521,
    Ed448,
    P256,
}

pub struct Pki {
    pub pem: Vec<u8>,
}

pub struct Identity {
    pub chain: Vec<u8>,
    pub key: Vec<u8>,
}

pub fn key(curve: Curve) -> PKey<Private> {
    match curve {
        Curve::P521 => PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::SECP521R1).unwrap()).unwrap()).unwrap(),
        Curve::P256 => PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap()).unwrap()).unwrap(),
        Curve::Ed448 => PKey::generate_ed448().unwrap(),
    }
}

pub fn certificate(cn: &str, key: &PKey<Private>, issuer: Option<(&X509, &PKey<Private>)>, ca: bool, serial: u32) -> X509 {
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_nid(Nid::COMMONNAME, cn).unwrap();
    let name = name.build();
    let mut b = X509::builder().unwrap();
    b.set_version(2).unwrap();
    b.set_serial_number(&BigNum::from_u32(serial).unwrap().to_asn1_integer().unwrap()).unwrap();
    b.set_subject_name(&name).unwrap();
    b.set_issuer_name(issuer.map_or(&name, |(c, _)| c.subject_name())).unwrap();
    b.set_pubkey(key).unwrap();
    b.set_not_before(&Asn1Time::days_from_now(0).unwrap()).unwrap();
    b.set_not_after(&Asn1Time::days_from_now(2).unwrap()).unwrap();
    if ca {
        b.append_extension(BasicConstraints::new().critical().ca().build().unwrap()).unwrap();
    }
    let signer = issuer.map_or(key, |(_, k)| k);
    let digest = if signer.id() == openssl::pkey::Id::ED448 { MessageDigest::null() } else { MessageDigest::sha512() };
    b.sign(signer, digest).unwrap();
    b.build()
}

/// A CA and a leaf signed by it, all on one curve.
pub fn pki(curve: Curve, name: &str) -> (Pki, Identity) {
    let ca_key = key(curve);
    let ca = certificate(&format!("{name} CA"), &ca_key, None, true, 1);
    let leaf_key = key(curve);
    let leaf = certificate(name, &leaf_key, Some((&ca, &ca_key)), false, 2);
    let mut chain = leaf.to_pem().unwrap();
    chain.extend(ca.to_pem().unwrap());
    (Pki { pem: ca.to_pem().unwrap() }, Identity { chain, key: leaf_key.private_key_to_pem_pkcs8().unwrap() })
}

