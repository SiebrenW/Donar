//! Throwaway certificate authorities and leaf certificates for TLS scenarios and tests.

use openssl::asn1::Asn1Time;
use openssl::bn::BigNum;
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::x509::extension::BasicConstraints;
use openssl::x509::{X509, X509NameBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
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


/// What both sides of a mutually authenticated session need: each its own chain and key, and
/// the CA of the other as trust anchor.
pub struct Material {
    pub secc: Identity,
    pub evcc: Identity,
    /// Trust anchors of the SECC (the EVCC's CA).
    pub secc_trust: Vec<u8>,
    /// Trust anchors of the EVCC (the SECC's CA).
    pub evcc_trust: Vec<u8>,
    /// An OCSP response for the SECC's leaf (profile chains only).
    pub secc_ocsp: Option<Vec<u8>>,
}

/// P-521 and Ed448 chains follow the certificate profiles of ISO 15118-20 Annex B (V2G root,
/// two sub-CAs, a SECC or vehicle certificate); P-256 ones, the ISO 15118-2 profile, are plain.
pub fn material(curve: Curve) -> Material {
    use crate::profile_pki::{Flaw, profile_pki};
    use evcc_linux::certificates::Peer;
    use crate::profile_pki::{OcspFlaw, profile_pki_with_ocsp};
    let (secc_ca, secc, evcc_ca, evcc, secc_ocsp);
    if curve == Curve::P256 {
        (secc_ca, secc) = pki(curve, "SECC");
        (evcc_ca, evcc) = pki(curve, "EVCC");
        secc_ocsp = None;
    } else {
        let (ca, identity, response) = profile_pki_with_ocsp(curve, Peer::Secc, Flaw::None, OcspFlaw::Good);
        (secc_ca, secc, secc_ocsp) = (ca, identity, Some(response));
        (evcc_ca, evcc) = profile_pki(curve, Peer::Vehicle, Flaw::None);
    }
    Material { secc, evcc, secc_trust: evcc_ca.pem, evcc_trust: secc_ca.pem, secc_ocsp }
}
