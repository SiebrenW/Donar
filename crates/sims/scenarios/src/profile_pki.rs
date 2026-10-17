//! Certificate chains that follow the profiles of ISO 15118-20 Annex B (V2G root, two sub-CAs
//! and a leaf, for a SECC or a vehicle), and chains that break one rule, to test relying parties.

use evcc_linux::certificates::{Peer, key_identifier};
use openssl::asn1::{Asn1Object, Asn1OctetString, Asn1Time};
use openssl::bn::BigNum;
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private};
use openssl::x509::{X509, X509Extension, X509NameBuilder};

use crate::pki::{Curve, Identity, Pki, key};

/// What an OCSP response says or gets wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcspFlaw {
    Good,
    Revoked,
    /// Valid for ten days instead of at most one week.
    TooLong,
    /// Past its nextUpdate.
    Expired,
    /// Signed by a key outside the chain.
    Stranger,
    /// For another certificate.
    OtherCertificate,
}

/// What to get wrong, for the tests of a relying party.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flaw {
    None,
    /// Key identifiers by method 1 (the SHA-1 of the whole key), what OpenSSL makes.
    KeyIdMethod1,
    /// The leaf has no key usage extension.
    LeafWithoutKeyUsage,
    /// The leaf is a CA.
    LeafIsCa,
    /// An extension the profile does not define (Netscape comment) in the leaf.
    ExtraExtension,
    /// sub-CA2 allows one more level.
    WrongPathLength,
    /// The SECC certificate has no OCSP location.
    NoOcsp,
    /// The leaf is expired.
    Expired,
    /// The common name of the leaf is not an identifier.
    BadIdentifier,
    /// Four certificates below the root.
    TooLong,
    /// The leaf's subject has an attribute the profile does not know (a locality).
    ExtraNameAttribute,
    /// The leaf's key usage lacks digitalSignature.
    LeafWithoutDigitalSignature,
    /// The extended key usage of the leaf is clientAuth for a SECC.
    WrongKeyPurpose,
}

const SECC_ID: &str = "DE-8AA-S-00003C4D557878675645330967543476-4";
const EVCC_ID: &str = "DE8-V-AA0000453C4D58Y-2";
const EMAID: &str = "DE-8AA-1A2B3C4D5-3";

fn octet(der: &[u8]) -> Asn1OctetString {
    Asn1OctetString::new_from_bytes(der).unwrap()
}

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    match content.len() {
        n if n < 128 => out.push(n as u8),
        n if n < 256 => out.extend([0x81, n as u8]),
        n => out.extend([0x82, (n >> 8) as u8, n as u8]),
    }
    out.extend_from_slice(content);
    out
}

fn extension(oid: &str, critical: bool, value_der: &[u8]) -> X509Extension {
    X509Extension::new_from_der(&Asn1Object::from_str(oid).unwrap(), critical, &octet(value_der)).unwrap()
}

/// KeyUsage bits in the order of RFC 5280 (digitalSignature first); trailing zeros dropped.
fn key_usage(bits: [bool; 9]) -> Vec<u8> {
    let used = bits.iter().rposition(|&b| b).map_or(0, |i| i + 1);
    let bytes = used.div_ceil(8).max(1);
    let mut value = vec![0u8; bytes];
    for (i, &b) in bits.iter().enumerate().take(used) {
        if b {
            value[i / 8] |= 0x80 >> (i % 8);
        }
    }
    let unused = (bytes * 8 - used) as u8;
    let mut content = vec![unused];
    content.extend(value);
    tlv(0x03, &content)
}

fn name(country: Option<&str>, org: &str, cn: &str, flaw: bool) -> openssl::x509::X509Name {
    use openssl::nid::Nid;
    let mut b = X509NameBuilder::new().unwrap();
    if let Some(c) = country {
        b.append_entry_by_nid(Nid::COUNTRYNAME, c).unwrap();
    }
    b.append_entry_by_nid(Nid::ORGANIZATIONNAME, org).unwrap();
    if flaw {
        b.append_entry_by_nid(Nid::LOCALITYNAME, "Somewhere").unwrap();
    }
    b.append_entry_by_nid(Nid::COMMONNAME, cn).unwrap();
    b.build()
}

struct Spec<'a> {
    subject: openssl::x509::X509Name,
    key: &'a PKey<Private>,
    issuer: Option<(&'a X509, &'a PKey<Private>)>,
    serial: u32,
    ca: bool,
    path_len: Option<u8>,
    usage: [bool; 9],
    eku: Option<&'static [u8]>,
    ocsp: bool,
    no_basic_ca_flag: bool,
    not_before_days: i64,
    not_after_days: i64,
    flaw: Flaw,
    root: bool,
    leaf: bool,
}

fn build(s: Spec<'_>) -> X509 {
    let mut b = X509::builder().unwrap();
    b.set_version(2).unwrap();
    b.set_serial_number(&BigNum::from_u32(s.serial).unwrap().to_asn1_integer().unwrap()).unwrap();
    b.set_subject_name(&s.subject).unwrap();
    b.set_issuer_name(s.issuer.map_or(&s.subject, |(c, _)| c.subject_name())).unwrap();
    b.set_pubkey(s.key).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    b.set_not_before(&Asn1Time::from_unix(now + s.not_before_days * 86_400).unwrap()).unwrap();
    b.set_not_after(&Asn1Time::from_unix(now + s.not_after_days * 86_400).unwrap()).unwrap();
    let method1 = s.flaw == Flaw::KeyIdMethod1;
    let id_of = |k: &PKey<Private>| {
        let bits = spki_bits(&k.public_key_to_der().unwrap());
        if method1 { openssl::hash::hash(MessageDigest::sha1(), &bits).unwrap().to_vec() } else { key_identifier(&bits).to_vec() }
    };
    b.append_extension(extension("2.5.29.14", false, &tlv(0x04, &id_of(s.key)))).unwrap();
    if !s.root {
        let issuer_key = s.issuer.map_or(s.key, |(_, k)| k);
        b.append_extension(extension("2.5.29.35", false, &tlv(0x30, &tlv(0x80, &id_of(issuer_key))))).unwrap();
    }
    if !(s.leaf && s.flaw == Flaw::LeafWithoutKeyUsage) {
        b.append_extension(extension("2.5.29.15", true, &key_usage(s.usage))).unwrap();
    }
    let mut basic = Vec::new();
    if s.ca && !s.no_basic_ca_flag {
        basic.extend(tlv(0x01, &[0xFF]));
    }
    if let Some(n) = s.path_len {
        basic.extend(tlv(0x02, &[n]));
    }
    b.append_extension(extension("2.5.29.19", true, &tlv(0x30, &basic))).unwrap();
    if let Some(eku) = s.eku {
        b.append_extension(extension("2.5.29.37", true, &tlv(0x30, &tlv(0x06, eku)))).unwrap();
    }
    if s.ocsp {
        let method = tlv(0x06, &[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x30, 0x01]);
        let location = tlv(0x86, b"http://ocsp.example.test/");
        let access = tlv(0x30, &[method, location].concat());
        b.append_extension(extension("1.3.6.1.5.5.7.1.1", true, &tlv(0x30, &access))).unwrap();
    }
    if s.leaf && s.flaw == Flaw::ExtraExtension {
        b.append_extension(extension("2.16.840.1.113730.1.13", false, &tlv(0x16, b"not in the profile"))).unwrap();
    }
    let signer = s.issuer.map_or(s.key, |(_, k)| k);
    let digest = if signer.id() == openssl::pkey::Id::ED448 { MessageDigest::null() } else { MessageDigest::sha512() };
    b.sign(signer, digest).unwrap();
    b.build()
}

/// The contents of the BIT STRING `subjectPublicKey` of a SubjectPublicKeyInfo.
fn spki_bits(spki: &[u8]) -> Vec<u8> {
    fn read(der: &[u8]) -> (u8, &[u8], &[u8]) {
        let (tag, len) = (der[0], der[1] as usize);
        if len < 0x80 {
            (tag, &der[2..2 + len], &der[2 + len..])
        } else {
            let n = len & 0x7F;
            let len = der[2..2 + n].iter().fold(0usize, |a, &b| a << 8 | b as usize);
            (tag, &der[2 + n..2 + n + len], &der[2 + n + len..])
        }
    }
    let (_, content, _) = read(spki);
    let (_, _, rest) = read(content);
    let (_, bits, _) = read(rest);
    bits[1..].to_vec()
}

const SERVER_AUTH: &[u8] = &[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x01];
const CLIENT_AUTH: &[u8] = &[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03, 0x02];

/// A V2G root, sub-CA1, sub-CA2 and leaf for `peer` on `curve` (P-521 or Ed448), the leaf
/// carrying the identifier of the profile in its common name; `flaw` breaks one rule. Returns
/// the root as trust anchor and the identity (chain without the root, leaf first, and key).
pub fn profile_pki(curve: Curve, peer: Peer, flaw: Flaw) -> (Pki, Identity) {
    let (pki, identity, _) = profile_pki_with_ocsp(curve, peer, flaw, OcspFlaw::Good);
    (pki, identity)
}

fn generalized(unix: i64) -> Vec<u8> {
    // YYYYMMDDHHMMSSZ from seconds since the epoch (civil-from-days).
    let (days, rem) = (unix.div_euclid(86_400), unix.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    tlv(0x18, format!("{y:04}{m:02}{d:02}{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60).as_bytes())
}

/// An OCSP response (RFC 6960) about `cert`, issued by `issuer` and signed with `signer_key`.
pub fn ocsp_response(cert: &X509, issuer: &X509, signer_key: &PKey<Private>, flaw: OcspFlaw, other: Option<&X509>) -> Vec<u8> {
    use openssl::sign::Signer;
    let sha1 = |bytes: &[u8]| openssl::hash::hash(MessageDigest::sha1(), bytes).unwrap().to_vec();
    let subject = if flaw == OcspFlaw::OtherCertificate { other.unwrap() } else { cert };
    let serial = subject.serial_number().to_bn().unwrap().to_vec();
    let mut serial_content = serial;
    if serial_content.first().is_none_or(|b| b & 0x80 != 0) {
        serial_content.insert(0, 0);
    }
    let alg_sha1 = tlv(0x30, &[tlv(0x06, &[0x2B, 0x0E, 0x03, 0x02, 0x1A]), vec![0x05, 0x00]].concat());
    let cert_id = tlv(0x30, &[alg_sha1, tlv(0x04, &sha1(&issuer.subject_name().to_der().unwrap())), tlv(0x04, &sha1(&spki_bits(&issuer.public_key().unwrap().public_key_to_der().unwrap()))), tlv(0x02, &serial_content)].concat());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let (this, next) = match flaw {
        OcspFlaw::TooLong => (now - 3600, now - 3600 + 10 * 86_400),
        OcspFlaw::Expired => (now - 3 * 86_400, now - 86_400),
        _ => (now - 3600, now - 3600 + 6 * 86_400),
    };
    let status = if flaw == OcspFlaw::Revoked { tlv(0xA1, &generalized(now - 86_400)) } else { vec![0x80, 0x00] };
    let single = tlv(0x30, &[cert_id, status, generalized(this), tlv(0xA0, &generalized(next))].concat());
    let responder_key = if flaw == OcspFlaw::Stranger { key(if signer_key.id() == openssl::pkey::Id::ED448 { Curve::Ed448 } else { Curve::P521 }) } else { signer_key.clone() };
    let responder_id = tlv(0xA2, &tlv(0x04, &sha1(&spki_bits(&responder_key.public_key_to_der().unwrap()))));
    let tbs = tlv(0x30, &[responder_id, generalized(now), tlv(0x30, &single)].concat());
    let ed448 = responder_key.id() == openssl::pkey::Id::ED448;
    let signature = if ed448 {
        Signer::new_without_digest(&responder_key).unwrap().sign_oneshot_to_vec(&tbs).unwrap()
    } else {
        let mut signer = Signer::new(MessageDigest::sha512(), &responder_key).unwrap();
        signer.update(&tbs).unwrap();
        signer.sign_to_vec().unwrap()
    };
    let alg = if ed448 { tlv(0x30, &tlv(0x06, &[0x2B, 0x65, 0x71])) } else { tlv(0x30, &tlv(0x06, &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x04])) };
    let basic = tlv(0x30, &[tbs, alg, tlv(0x03, &[vec![0u8], signature].concat())].concat());
    let response_bytes = tlv(0x30, &[tlv(0x06, &[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x30, 0x01, 0x01]), tlv(0x04, &basic)].concat());
    tlv(0x30, &[tlv(0x0A, &[0]), tlv(0xA0, &response_bytes)].concat())
}

/// [`profile_pki`] and an OCSP response for the leaf, signed by its issuing CA (V2G20-1008 allows
/// that) with the given flaw.
pub fn profile_pki_with_ocsp(curve: Curve, peer: Peer, flaw: Flaw, ocsp: OcspFlaw) -> (Pki, Identity, Vec<u8>) {
    // Annex B of ISO 15118-20 has P-521 and Ed448, ISO 15118-2 secp256r1 (its contract chains).
    assert!(matches!(curve, Curve::P521 | Curve::Ed448 | Curve::P256));
    let (root_key, sub1_key, sub2_key, leaf_key) = (key(curve), key(curve), key(curve), key(curve));
    let (cso, id) = match peer {
        Peer::Secc => ("Test CSO", SECC_ID),
        Peer::Vehicle => ("Test OEM", EVCC_ID),
        Peer::Contract => ("Test eMSP", EMAID),
    };
    let id = if flaw == Flaw::BadIdentifier { "not an identifier" } else { id };
    let ca_usage = |sign: bool| [false, false, false, false, false, true, sign, false, false];
    // Vehicle and contract certificates need revocation information (V2G20-2599), a SECC's chain OCSP.
    let ocsp_ca = true;
    let root = build(Spec {
        subject: name(Some("DE"), "Test V2G", "Test V2G Root", false), key: &root_key, issuer: None, serial: 1, ca: true, path_len: None,
        usage: ca_usage(false), eku: None, ocsp: false, no_basic_ca_flag: false, not_before_days: -3650, not_after_days: 3650, flaw, root: true, leaf: false,
    });
    // Each certificate is issued by the one before; `TooLong` puts one more CA in between.
    let mut certs: Vec<X509> = vec![root];
    let mut keys: Vec<PKey<Private>> = vec![root_key];
    #[allow(clippy::type_complexity)]
    let issue = |certs: &mut Vec<X509>, keys: &mut Vec<PKey<Private>>, spec: &dyn Fn(&X509, &PKey<Private>, &PKey<Private>) -> X509, subject_key: PKey<Private>| {
        let cert = spec(certs.last().unwrap(), keys.last().unwrap(), &subject_key);
        certs.push(cert);
        keys.push(subject_key);
    };
    if flaw == Flaw::TooLong {
        issue(&mut certs, &mut keys, &|c, k, own| build(Spec {
            subject: name(Some("DE"), cso, "Sub-CA0", false), key: own, issuer: Some((c, k)), serial: 9, ca: true, path_len: Some(3),
            usage: ca_usage(true), eku: None, ocsp: ocsp_ca, no_basic_ca_flag: false, not_before_days: -3100, not_after_days: 3100, flaw: Flaw::None, root: false, leaf: false,
        }), key(curve));
    }
    issue(&mut certs, &mut keys, &|c, k, own| build(Spec {
        subject: name(Some("DE"), cso, "Sub-CA1", false), key: own, issuer: Some((c, k)), serial: 2, ca: true, path_len: Some(1),
        usage: ca_usage(true), eku: None, ocsp: ocsp_ca, no_basic_ca_flag: false, not_before_days: -3000, not_after_days: 3000, flaw, root: false, leaf: false,
    }), sub1_key);
    issue(&mut certs, &mut keys, &|c, k, own| build(Spec {
        subject: name(Some("DE"), cso, "Sub-CA2", false), key: own, issuer: Some((c, k)), serial: 3, ca: true,
        path_len: Some(if flaw == Flaw::WrongPathLength { 1 } else { 0 }),
        usage: ca_usage(true), eku: None, ocsp: ocsp_ca, no_basic_ca_flag: false, not_before_days: -2000, not_after_days: 2000, flaw, root: false, leaf: false,
    }), sub2_key);
    let leaf_usage = [flaw != Flaw::LeafWithoutDigitalSignature, false, false, false, peer == Peer::Secc, false, false, false, false];
    let eku = match (peer, flaw) {
        (Peer::Secc, Flaw::WrongKeyPurpose) => Some(CLIENT_AUTH),
        (Peer::Secc, _) => Some(SERVER_AUTH),
        (Peer::Vehicle, _) => Some(CLIENT_AUTH),
        (Peer::Contract, _) => None,
    };
    let leaf_ocsp = peer == Peer::Vehicle || flaw != Flaw::NoOcsp;
    issue(&mut certs, &mut keys, &|c, k, own| build(Spec {
        subject: name(Some("DE"), cso, id, flaw == Flaw::ExtraNameAttribute), key: own, issuer: Some((c, k)), serial: 4,
        ca: flaw == Flaw::LeafIsCa, path_len: None, usage: leaf_usage, eku, ocsp: leaf_ocsp,
        no_basic_ca_flag: false, not_before_days: -10, not_after_days: if flaw == Flaw::Expired { -1 } else { 1000 }, flaw, root: false, leaf: true,
    }), leaf_key);
    // The identity: the chain without the root, leaf first.
    let mut chain = Vec::new();
    for cert in certs.iter().skip(1).rev() {
        chain.extend(cert.to_pem().unwrap());
    }
    let n = certs.len();
    let response = ocsp_response(&certs[n - 1], &certs[n - 2], &keys[n - 2], ocsp, Some(&certs[0]));
    (Pki { pem: certs[0].to_pem().unwrap() }, Identity { chain, key: keys.last().unwrap().private_key_to_pem_pkcs8().unwrap() }, response)
}
