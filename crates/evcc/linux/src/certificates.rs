//! Certificate profile checks of ISO 15118-20:2022 Annex B (V2G20-1001 and the profile tables):
//! what a relying party does on top of the path validation the TLS library already did. The
//! chain of a SECC (V2G root, CSO sub-CA1 and sub-CA2, SECC certificate; tables B.3 and B.5/B.6)
//! or of a vehicle (vehicle sub-CA1 and sub-CA2, vehicle certificate; B.13/B.14) is checked
//! against the profile of each position: version, serial number, algorithms and key, name
//! attributes, validity, every extension (the ones the profile does not define are refused,
//! V2G20-2686), key usage bits, basic constraints, key identifiers (V2G20-1851, 1852), the
//! chain length (V2G20-1003) and the identifier in the common name of the leaf (Annex C).
//!
//! Two curve sets exist: secp521r1 with ecdsa-with-SHA512 (V2G20-2674) and Ed448 (V2G20-2319);
//! the leaf's key decides which one a chain has to follow.
//!
//! What is not covered: cross certificates (V2G20-1002, 1005, 1853), certificates of the other
//! services (provisioning, e-mobility, OEM provisioning, private environment, OCSP signer),
//! the encoding of times as GeneralizedTime and the 25 years of a root (a real root has what its
//! CA chose; the profile text is at odds with RFC 5280 for years before 2050).

use std::time::{SystemTime, UNIX_EPOCH};

use openssl::hash::{MessageDigest, hash};
use openssl::ssl::SslRef;
use openssl::x509::X509;
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::prelude::*;

/// Whose chain is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    /// The SECC's chain, as the EVCC sees it (tables B.3, B.5, B.6).
    Secc,
    /// The vehicle's chain, as the SECC sees it (tables B.13, B.14).
    Vehicle,
    /// The contract certificate chain of Plug & Charge, as the SECC sees it (tables B.9, B.10):
    /// eMSP root, sub-CA1, sub-CA2 and the contract certificate.
    Contract,
}

/// One requirement a certificate does not meet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// Position in the chain, 0 for the leaf.
    pub depth: usize,
    /// The requirement or table it comes from.
    pub requirement: &'static str,
    pub message: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "certificate {} ({}): {}", self.depth, self.requirement, self.message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Secp521r1,
    Ed448,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Root,
    SubCa1,
    SubCa2,
    Leaf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Required,
    Optional,
    Absent,
}

/// A key usage bit in the table: fixed, or 0/1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bit {
    Zero,
    One,
    Any,
}
use Bit::{Any, One, Zero};

/// The names of the key usage bits, in the order of the table (and of RFC 5280).
const USAGE_BITS: [&str; 9] = ["digitalSignature", "nonRepudiation", "keyEncipherment", "dataEncipherment", "keyAgreement", "keyCertSign", "cRLSign", "encipherOnly", "decipherOnly"];

struct Profile {
    name: &'static str,
    kind: Kind,
    /// AuthorityKeyIdentifier, SubjectKeyIdentifier, CertificatePolicies, CRLDistributionPoints,
    /// AuthorityInfoAccess (OCSP), ExtendedKeyUsage.
    aki: Presence,
    ski: Presence,
    policies: Presence,
    crl: Presence,
    aia: Presence,
    /// SubjectInfoAccess (the operator and tariff names of a contract certificate).
    sia: Presence,
    eku: Presence,
    key_usage: [Bit; 9],
    path_len: Option<u32>,
    /// Extended key purposes allowed, and the one that has to be there.
    eku_allowed: &'static [&'static str],
    eku_required: Option<&'static str>,
}

const SERVER_AUTH: &str = "1.3.6.1.5.5.7.3.1";
const CLIENT_AUTH: &str = "1.3.6.1.5.5.7.3.2";

fn profile(peer: Peer, kind: Kind) -> Profile {
    use Presence::*;
    // The rows that differ between the chains: the usage bits of the leaf, extended key usage,
    // revocation information.
    let (name, ku, eku, eku_allowed, eku_required, crl, aia): (&'static str, [Bit; 9], Presence, &[&str], Option<&str>, Presence, Presence) = match (peer, kind) {
        (_, Kind::Root) => ("V2G root CA", [Any, Any, Any, Zero, Any, One, Any, Zero, Zero], Absent, &[], None, Absent, Absent),
        (Peer::Secc, Kind::SubCa1) => ("CSO sub-CA1", [Any, Any, Any, Zero, Any, One, One, Zero, Zero], Absent, &[], None, Absent, Required),
        (Peer::Secc, Kind::SubCa2) => ("CSO sub-CA2", [Any, Any, Any, Zero, Any, One, One, Zero, Zero], Absent, &[], None, Absent, Required),
        (Peer::Secc, Kind::Leaf) => ("SECC certificate", [One, Any, Any, Zero, One, Zero, Zero, Zero, Zero], Optional, &[SERVER_AUTH], Some(SERVER_AUTH), Absent, Required),
        (Peer::Vehicle, Kind::SubCa1) => ("vehicle sub-CA1", [Any, Any, Any, Zero, Any, One, One, Zero, Zero], Absent, &[], None, Optional, Optional),
        (Peer::Vehicle, Kind::SubCa2) => ("vehicle sub-CA2", [Any, Any, Any, Zero, Any, One, One, Zero, Zero], Absent, &[], None, Optional, Optional),
        (Peer::Vehicle, Kind::Leaf) => ("vehicle certificate", [One, Any, Any, Zero, Any, Zero, Zero, Zero, Zero], Optional, &[CLIENT_AUTH, SERVER_AUTH], Some(CLIENT_AUTH), Optional, Optional),
        (Peer::Contract, Kind::SubCa1) => ("eMSP sub-CA1", [Any, Any, Any, Zero, Any, One, One, Zero, Zero], Absent, &[], None, Optional, Optional),
        (Peer::Contract, Kind::SubCa2) => ("eMSP sub-CA2", [Any, Any, Any, Zero, Any, One, One, Zero, Zero], Absent, &[], None, Optional, Optional),
        (Peer::Contract, Kind::Leaf) => ("contract certificate", [One, Any, Any, Zero, Any, Zero, Zero, Zero, Zero], Absent, &[], None, Optional, Optional),
    };
    // The root of an eMSP is not the V2G root.
    let name = if peer == Peer::Contract && kind == Kind::Root { "eMSP root CA" } else { name };
    Profile {
        name,
        kind,
        aki: if kind == Kind::Root { Absent } else { Required },
        ski: Required,
        policies: if kind == Kind::Root { Absent } else { Optional },
        crl,
        aia,
        sia: if peer == Peer::Contract && kind == Kind::Leaf { Optional } else { Absent },
        eku,
        key_usage: ku,
        path_len: match kind {
            Kind::SubCa1 => Some(1),
            Kind::SubCa2 => Some(0),
            _ => None,
        },
        eku_allowed,
        eku_required,
    }
}

const OID_AKI: &str = "2.5.29.35";
const OID_SKI: &str = "2.5.29.14";
const OID_KEY_USAGE: &str = "2.5.29.15";
const OID_EKU: &str = "2.5.29.37";
const OID_POLICIES: &str = "2.5.29.32";
const OID_BASIC: &str = "2.5.29.19";
const OID_CRL: &str = "2.5.29.31";
const OID_AIA: &str = "1.3.6.1.5.5.7.1.1";
const OID_SIA: &str = "1.3.6.1.5.5.7.1.11";
const OID_OCSP: &str = "1.3.6.1.5.5.7.48.1";
const OID_ECDSA_SHA512: &str = "1.2.840.10045.4.3.4";
const OID_EC_PUBLIC_KEY: &str = "1.2.840.10045.2.1";
const OID_SECP521R1: &str = "1.3.132.0.35";
const OID_ED448: &str = "1.3.101.113";
const OID_ATTR_COUNTRY: &str = "2.5.4.6";
const OID_ATTR_ORG: &str = "2.5.4.10";
const OID_ATTR_UNIT: &str = "2.5.4.11";
const OID_ATTR_CN: &str = "2.5.4.3";
const OID_ATTR_DC: &str = "0.9.2342.19200300.100.1.25";

/// The key identifier of RFC 5280 4.2.1.2 method 2: `0100` followed by the least significant 60
/// bits of the SHA-1 of the public key bits (V2G20-1851, 1852).
pub fn key_identifier(public_key_bits: &[u8]) -> [u8; 8] {
    let digest = hash(MessageDigest::sha1(), public_key_bits).expect("sha1");
    let mut id = [0u8; 8];
    id.copy_from_slice(&digest[12..20]);
    id[0] = 0x40 | (id[0] & 0x0F);
    id
}

struct Report {
    depth: usize,
    found: Vec<Violation>,
}

impl Report {
    fn add(&mut self, requirement: &'static str, message: impl Into<String>) {
        self.found.push(Violation { depth: self.depth, requirement, message: message.into() });
    }
}

fn family_of(cert: &X509Certificate<'_>) -> Option<Family> {
    match cert.public_key().algorithm.algorithm.to_id_string().as_str() {
        OID_EC_PUBLIC_KEY => Some(Family::Secp521r1),
        OID_ED448 => Some(Family::Ed448),
        _ => None,
    }
}

/// Checks `chain`, DER certificates from the leaf up to and including the root, at `now`
/// (seconds since the epoch). Signatures are checked too, each against the next certificate.
pub fn check_chain(chain: &[Vec<u8>], peer: Peer, now: i64) -> Vec<Violation> {
    let mut found = Vec::new();
    let mut parsed = Vec::new();
    for (depth, der) in chain.iter().enumerate() {
        match X509Certificate::from_der(der) {
            Ok(([], cert)) => parsed.push(cert),
            Ok(_) => found.push(Violation { depth, requirement: "X.509", message: "bytes after the certificate".into() }),
            Err(e) => found.push(Violation { depth, requirement: "X.509", message: format!("cannot be parsed: {e}") }),
        }
    }
    if !found.is_empty() || parsed.is_empty() {
        return found;
    }
    let n = parsed.len();
    // V2G20-1003: at most three certificates without the root.
    if n - 1 > 3 {
        found.push(Violation { depth: 0, requirement: "V2G20-1003", message: format!("{} certificates below the root, at most 3 are allowed", n - 1) });
    }
    let Some(family) = family_of(&parsed[0]) else {
        found.push(Violation { depth: 0, requirement: "V2G20-2674", message: "the key is neither secp521r1 nor Ed448".into() });
        return found;
    };
    for (depth, cert) in parsed.iter().enumerate() {
        // The position decides the profile; with a single sub-CA it is a sub-CA2 (V2G20-911).
        let kind = if depth == 0 && n > 1 {
            Kind::Leaf
        } else if depth == n - 1 {
            Kind::Root
        } else if depth == n - 2 && n >= 4 {
            Kind::SubCa1
        } else {
            Kind::SubCa2
        };
        let kind = if n == 1 { Kind::Root } else { kind };
        let mut report = Report { depth, found: Vec::new() };
        check_certificate(&mut report, cert, &profile(peer, kind), family, now);
        let issuer = parsed.get(depth + 1);
        check_against_issuer(&mut report, cert, issuer, &chain[depth], chain.get(depth + 1));
        found.extend(report.found);
    }
    if let Some(leaf) = parsed.first().filter(|_| n > 1) {
        let mut report = Report { depth: 0, found: Vec::new() };
        check_subject_id(&mut report, leaf, peer);
        found.extend(report.found);
    }
    found
}

fn check_certificate(r: &mut Report, cert: &X509Certificate<'_>, p: &Profile, family: Family, now: i64) {
    let tbs = &cert.tbs_certificate;
    if tbs.version != X509Version::V3 {
        r.add("Annex B", "the version is not 3");
    }
    if tbs.raw_serial().first().is_none_or(|b| b & 0x80 != 0) || tbs.raw_serial().iter().all(|&b| b == 0) {
        r.add("Annex B", "the serial number is not a positive integer");
    }
    // Algorithms (V2G20-2674, V2G20-2319): the same in the signed part and outside, no parameters.
    let (want_sig, want_key) = match family {
        Family::Secp521r1 => (OID_ECDSA_SHA512, OID_EC_PUBLIC_KEY),
        Family::Ed448 => (OID_ED448, OID_ED448),
    };
    for (what, alg) in [("signature algorithm in the certificate", &tbs.signature), ("signature algorithm", &cert.signature_algorithm)] {
        if alg.algorithm.to_id_string() != want_sig {
            r.add("Annex B", format!("the {what} is {}, expected {want_sig}", alg.algorithm.to_id_string()));
        }
        if alg.parameters.is_some() {
            r.add("Annex B", format!("the {what} has parameters"));
        }
    }
    let spki = cert.public_key();
    if spki.algorithm.algorithm.to_id_string() != want_key {
        r.add("Annex B", format!("the key is {}, the chain's leaf has a key of the other curve set", spki.algorithm.algorithm.to_id_string()));
    } else if family == Family::Secp521r1 {
        let curve = spki.algorithm.parameters.as_ref().and_then(|a| a.as_oid().ok()).map(|o| o.to_id_string());
        if curve.as_deref() != Some(OID_SECP521R1) {
            r.add("Annex B", format!("the curve is {curve:?}, expected secp521r1"));
        }
        if spki.subject_public_key.data.len() != 133 {
            r.add("Annex B", "the public key is not 133 octets (an uncompressed secp521r1 point)");
        }
    } else {
        if spki.algorithm.parameters.is_some() {
            r.add("Annex B", "the Ed448 key has parameters");
        }
        if spki.subject_public_key.data.len() != 57 {
            r.add("Annex B", "the public key is not 57 octets (Ed448)");
        }
    }
    check_name(r, cert.issuer(), "issuer", None, false);
    let leaf_country = p.kind == Kind::Leaf && p.name == "SECC certificate";
    check_name(r, cert.subject(), "subject", (p.kind == Kind::Leaf).then_some(if leaf_country { "CSO" } else { "EV" }), leaf_country);
    // V2G20-1001 c): valid now.
    let v = cert.validity();
    if now < v.not_before.timestamp() {
        r.add("V2G20-1001", "not valid yet");
    }
    if now > v.not_after.timestamp() {
        r.add("V2G20-1001", "expired");
    }
    check_extensions(r, cert, p);
}

/// Distinguished names: only the attributes of the profile, once each (domain components
/// excepted); organization and common name are mandatory.
fn check_name(r: &mut Report, name: &X509Name<'_>, which: &'static str, domain_component: Option<&str>, country_required: bool) {
    let mut seen: Vec<String> = Vec::new();
    for attr in name.iter_attributes() {
        let oid = attr.attr_type().to_id_string();
        if ![OID_ATTR_COUNTRY, OID_ATTR_ORG, OID_ATTR_UNIT, OID_ATTR_CN, OID_ATTR_DC].contains(&oid.as_str()) {
            r.add("V2G20-2685", format!("the {which} has an attribute ({oid}) the profile does not define"));
        } else if oid != OID_ATTR_DC && seen.contains(&oid) {
            r.add("Annex B", format!("the {which} has {oid} more than once"));
        }
        if oid == OID_ATTR_DC
            && let (Some(expected), Ok(value)) = (domain_component, attr.as_str())
                && which == "subject" && value != expected {
                    r.add("Annex B", format!("the domain component is {value:?}, expected {expected:?}"));
                }
        seen.push(oid);
    }
    for (oid, label) in [(OID_ATTR_ORG, "organization"), (OID_ATTR_CN, "common name")] {
        if !seen.iter().any(|s| s == oid) {
            r.add("Annex B", format!("the {which} has no {label}"));
        }
    }
    if which == "subject" && country_required && !seen.iter().any(|s| s == OID_ATTR_COUNTRY) {
        r.add("Annex B", "the subject has no country");
    }
}

fn check_extensions(r: &mut Report, cert: &X509Certificate<'_>, p: &Profile) {
    let mut seen: Vec<String> = Vec::new();
    for ext in cert.extensions() {
        let oid = ext.oid.to_id_string();
        if seen.contains(&oid) {
            r.add("RFC 5280", format!("extension {oid} appears twice"));
        }
        seen.push(oid.clone());
        let allowed = match oid.as_str() {
            OID_AKI => p.aki != Presence::Absent,
            OID_SKI => p.ski != Presence::Absent,
            OID_KEY_USAGE | OID_BASIC => true,
            OID_EKU => p.eku != Presence::Absent,
            OID_POLICIES => p.policies != Presence::Absent,
            OID_CRL => p.crl != Presence::Absent,
            OID_AIA => p.aia != Presence::Absent,
            OID_SIA => p.sia != Presence::Absent,
            _ => false,
        };
        if !allowed {
            r.add("V2G20-2686", format!("{} has an extension the profile does not define ({oid}{})", p.name, if ext.critical { ", critical" } else { "" }));
            continue;
        }
        // Criticality as the tables have it where they fix it: key usage, basic constraints,
        // extended key usage and authority information access are critical, the key identifiers
        // and policies not (the vehicle tables say critical for the identifiers, against RFC 5280:
        // either is accepted there).
        let must_be_critical = matches!(oid.as_str(), OID_KEY_USAGE | OID_BASIC | OID_EKU | OID_AIA | OID_CRL);
        let must_not_be_critical = matches!(oid.as_str(), OID_POLICIES);
        if must_be_critical && !ext.critical {
            r.add("Annex B", format!("extension {oid} has to be critical"));
        }
        if must_not_be_critical && ext.critical {
            r.add("Annex B", format!("extension {oid} must not be critical"));
        }
    }
    let present = |oid: &str| seen.iter().any(|s| s == oid);
    for (presence, oid, label) in [
        (p.aki, OID_AKI, "AuthorityKeyIdentifier"),
        (p.ski, OID_SKI, "SubjectKeyIdentifier"),
        (p.policies, OID_POLICIES, "CertificatePolicies"),
        (p.crl, OID_CRL, "CRLDistributionPoints"),
        (p.aia, OID_AIA, "AuthorityInfoAccess"),
        (p.eku, OID_EKU, "ExtendedKeyUsage"),
    ] {
        if presence == Presence::Required && !present(oid) {
            r.add("Annex B", format!("{} lacks {label}", p.name));
        }
    }
    for (oid, label) in [(OID_KEY_USAGE, "KeyUsage"), (OID_BASIC, "BasicConstraints")] {
        if !present(oid) {
            r.add("Annex B", format!("{} lacks {label}", p.name));
        }
    }
    if p.name.starts_with("vehicle") && !present(OID_CRL) && !present(OID_AIA) {
        r.add("V2G20-2599", "a vehicle certificate needs CRLDistributionPoints or AuthorityInfoAccess");
    }
    if (p.name.starts_with("eMSP sub") || p.name == "contract certificate") && !present(OID_CRL) && !present(OID_AIA) {
        r.add("V2G20-2590", format!("{} needs CRLDistributionPoints or AuthorityInfoAccess", p.name));
    }
    for ext in cert.extensions() {
        match ext.parsed_extension() {
            ParsedExtension::KeyUsage(ku) => {
                let flags = [ku.digital_signature(), ku.non_repudiation(), ku.key_encipherment(), ku.data_encipherment(), ku.key_agreement(), ku.key_cert_sign(), ku.crl_sign(), ku.encipher_only(), ku.decipher_only()];
                for ((bit, name), want) in flags.into_iter().zip(USAGE_BITS).zip(p.key_usage) {
                    match want {
                        One if !bit => r.add("Annex B", format!("key usage {name} has to be set for {}", p.name)),
                        Zero if bit => r.add("Annex B", format!("key usage {name} must not be set for {}", p.name)),
                        _ => {}
                    }
                }
            }
            ParsedExtension::BasicConstraints(bc) => {
                let ca = p.kind != Kind::Leaf;
                if bc.ca != ca {
                    r.add("Annex B", format!("basic constraints: CA is {}, {} needs {ca}", bc.ca, p.name));
                }
                if bc.path_len_constraint != p.path_len {
                    r.add("Annex B", format!("basic constraints: path length {:?}, {} needs {:?}", bc.path_len_constraint, p.name, p.path_len));
                }
            }
            ParsedExtension::ExtendedKeyUsage(eku) => {
                let mut purposes: Vec<String> = Vec::new();
                if eku.server_auth {
                    purposes.push(SERVER_AUTH.into());
                }
                if eku.client_auth {
                    purposes.push(CLIENT_AUTH.into());
                }
                if eku.any || eku.code_signing || eku.email_protection || eku.time_stamping || eku.ocsp_signing || !eku.other.is_empty() {
                    r.add("Annex B", "extended key usage has a purpose the profile does not define");
                }
                if let Some(required) = p.eku_required
                    && !purposes.iter().any(|x| x == required) {
                        r.add("Annex B", format!("extended key usage lacks {required}"));
                    }
                if purposes.iter().any(|x| !p.eku_allowed.contains(&x.as_str())) {
                    r.add("Annex B", "extended key usage has a purpose the profile does not allow");
                }
            }
            ParsedExtension::AuthorityInfoAccess(aia) => {
                if aia.accessdescs.is_empty() {
                    r.add("RFC 5280", "AuthorityInfoAccess is empty");
                }
                for d in &aia.accessdescs {
                    let method = d.access_method.to_id_string();
                    if method != OID_OCSP {
                        r.add("V2G20-2685", format!("AuthorityInfoAccess has an access method ({method}) the profile does not define"));
                    } else if !matches!(d.access_location, GeneralName::URI(u) if u.contains("://")) {
                        r.add("V2G20-2582", "the OCSP location is not a URI");
                    }
                }
            }
            ParsedExtension::AuthorityKeyIdentifier(aki) => {
                if aki.key_identifier.is_none() {
                    r.add("V2G20-1851", "AuthorityKeyIdentifier has no keyIdentifier");
                }
                if aki.authority_cert_issuer.is_some() != aki.authority_cert_serial.is_some() {
                    r.add("Annex B", "authorityCertIssuer and authorityCertSerialNumber come together or not at all");
                }
            }
            ParsedExtension::ParseError { error } => r.add("RFC 5280", format!("extension {} cannot be parsed: {error}", ext.oid)),
            _ => {}
        }
    }
}

/// What a certificate has to share with its issuer: the name, the key identifiers, the
/// signature, the validity (V2G20-1004).
fn check_against_issuer(r: &mut Report, cert: &X509Certificate<'_>, issuer: Option<&X509Certificate<'_>>, der: &[u8], issuer_der: Option<&Vec<u8>>) {
    let own_key = cert.public_key().subject_public_key.data.as_ref();
    for ext in cert.extensions() {
        if let ParsedExtension::SubjectKeyIdentifier(ski) = ext.parsed_extension()
            && ski.0 != key_identifier(own_key) {
                r.add("V2G20-1852", "SubjectKeyIdentifier is not the method 2 identifier of the certificate's key");
            }
    }
    let (self_signed, issuer) = match issuer {
        Some(i) => (false, i),
        None => (true, cert),
    };
    let issuer_der = issuer_der.map_or(der, |v| v.as_slice());
    if cert.issuer().as_raw() != issuer.subject().as_raw() {
        r.add("RFC 5280", "the issuer is not the subject of the next certificate");
    }
    for ext in cert.extensions() {
        if let ParsedExtension::AuthorityKeyIdentifier(aki) = ext.parsed_extension()
            && let Some(id) = &aki.key_identifier
                && id.0 != key_identifier(issuer.public_key().subject_public_key.data.as_ref()) {
                    r.add("V2G20-1851", "AuthorityKeyIdentifier is not the method 2 identifier of the issuer's key");
                }
    }
    let verified = X509::from_der(issuer_der).and_then(|i| i.public_key()).and_then(|k| X509::from_der(der)?.verify(&k));
    if !matches!(verified, Ok(true)) {
        r.add("V2G20-1001", if self_signed { "the root's signature does not verify with its own key" } else { "the signature does not verify with the issuer's key" });
    }
    // V2G20-1004: issued within the validity of the issuer.
    let (v, iv) = (cert.validity(), issuer.validity());
    if v.not_before.timestamp() < iv.not_before.timestamp() || v.not_before.timestamp() > iv.not_after.timestamp() {
        r.add("V2G20-1004", "issued outside the validity of the issuer");
    }
}

/// The identifier in the common name of the leaf: SECCID or EVCCID (Annex C).
fn check_subject_id(r: &mut Report, leaf: &X509Certificate<'_>, peer: Peer) {
    let Some(cn) = leaf.subject().iter_common_name().next().and_then(|a| a.as_str().ok()) else { return };
    let (result, requirement) = match peer {
        Peer::Secc => (check_secc_id(cn), "V2G20-2583"),
        Peer::Vehicle => (check_evcc_id(cn), "V2G20-2597"),
        Peer::Contract => (check_emaid(cn), "V2G20-2588"),
    };
    if let Err(message) = result {
        r.add(requirement, format!("the common name {cn:?}: {message}"));
    }
}

/// Syntax and check digit of an identifier made of `parts` fixed-length prefixes and a
/// variable part (C.3, C.5, C.6): letters and digits with optional hyphens between the
/// elements, the type letter in its place, a check digit at the end.
fn check_id(id: &str, prefix: &[(usize, Option<char>)], rest: (usize, usize), min_total: usize) -> Result<(), String> {
    if !id.is_ascii() || id.len() > 255 {
        return Err("not ASCII or longer than 255 characters".into());
    }
    let compact: String = id.chars().filter(|&c| c != '-').collect();
    if id.starts_with('-') || id.ends_with('-') || id.contains("--") {
        return Err("misplaced separators".into());
    }
    if !compact.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err("only letters, digits and separators are allowed".into());
    }
    if compact.len() < min_total {
        return Err(format!("{} characters without separators, at least {min_total} are needed", compact.len()));
    }
    let mut at = 0;
    for &(len, fixed) in prefix {
        let part = &compact[at..at + len.min(compact.len() - at)];
        if part.len() < len {
            return Err("too short".into());
        }
        match fixed {
            Some(c) if !part.eq_ignore_ascii_case(&c.to_string()) => return Err(format!("expected the identifier type {c:?}")),
            None if !part.chars().all(|c| c.is_ascii_alphanumeric()) => return Err("bad element".into()),
            _ => {}
        }
        at += len;
    }
    let body_len = compact.len() - at - 1;
    if body_len < rest.0 || body_len > rest.1 {
        return Err(format!("the unique part has {body_len} characters, {} to {} are allowed", rest.0, rest.1));
    }
    let (body, digit) = compact.split_at(compact.len() - 1);
    let expected = check_digit(&prefix_elements(id, body));
    if !digit.eq_ignore_ascii_case(&expected.to_string()) {
        return Err(format!("the check digit is {digit}, expected {expected}"));
    }
    Ok(())
}

/// The elements of an identifier (separated by hyphens, or split at the fixed lengths when it has
/// none) without the check digit, for the check digit's removal of leading zeros per element.
fn prefix_elements(id: &str, body_without_separators: &str) -> Vec<String> {
    let parts: Vec<&str> = id.split('-').collect();
    if parts.len() > 1 {
        // Drop the check digit, which is the last element or its last character.
        let mut v: Vec<String> = parts.iter().map(|s| s.to_string()).collect();
        let last = v.pop().unwrap();
        if last.len() > 1 {
            v.push(last[..last.len() - 1].to_string());
        }
        v
    } else {
        vec![body_without_separators.to_string()]
    }
}

/// C.6: leading zeros of each element are dropped, letters become 10 to 35, digit `i` has the
/// weight 2^(i mod 28), the sum modulo 11 is the digit (10 is X).
fn check_digit(elements: &[String]) -> char {
    let mut values: Vec<u32> = Vec::new();
    for element in elements {
        for c in element.trim_start_matches('0').chars() {
            let v = c.to_digit(36).unwrap_or(0);
            // A letter becomes two digits (10 to 35), a digit stays one.
            if v >= 10 {
                values.push(v / 10);
                values.push(v % 10);
            } else {
                values.push(v);
            }
        }
    }
    let sum: u64 = values.iter().enumerate().map(|(i, &d)| u64::from(d) << (i % 28)).sum();
    match sum % 11 {
        10 => 'X',
        d => char::from_digit(d as u32, 10).unwrap(),
    }
}

/// `<Country Code> <S> <EVSE Operator ID> <S> "S" <S> <ControllerID> <S> <Check Digit>` (V2G20-2083).
pub fn check_secc_id(id: &str) -> Result<(), String> {
    check_id(id, &[(2, None), (3, None), (1, Some('S'))], (32, 248), 39)
}

/// `<WMI> <S> "V" <S> <OEM's own unique ID> <S> <Check Digit>` (V2G20-2093).
pub fn check_evcc_id(id: &str) -> Result<(), String> {
    check_id(id, &[(3, None), (1, Some('V'))], (15, 250), 20)
}

/// `<Country Code> <S> <Provider ID> <S> <Instance> <S> <Check Digit>` (C.1, IEC 63119-2): two
/// letters, three letters or digits, nine letters or digits and the check digit of C.6. The
/// format is read from the example of C.6, since IEC 63119-2 is not at hand.
pub fn check_emaid(id: &str) -> Result<(), String> {
    check_id(id, &[(2, None), (3, None)], (9, 9), 15)
}

/// The chain the TLS library verified for the peer (leaf first, root last), checked now.
pub fn check_peer(ssl: &SslRef, peer: Peer) -> Vec<Violation> {
    let Some(chain) = ssl.verified_chain() else {
        return vec![Violation { depth: 0, requirement: "V2G20-1001", message: "no verified chain".into() }];
    };
    let ders: Result<Vec<Vec<u8>>, _> = chain.iter().map(|c| c.to_der()).collect();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    match ders {
        Ok(ders) => check_chain(&ders, peer, now),
        Err(e) => vec![Violation { depth: 0, requirement: "X.509", message: e.to_string() }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_digits_follow_the_examples_of_the_standard() {
        // C.6: the EMAID example and the EVCCID example of C.5.2 come out as printed. The
        // SECCID example of C.3.2 ("...-2") does not: the same algorithm gives 4.
        assert_eq!(check_digit(&["DE".into(), "0008AA".into(), "1A2B3C4D5".into()]), '3');
        assert_eq!(check_evcc_id("DE8-V-AA0000453C4D58Y-2"), Ok(()));
        assert_eq!(check_secc_id("DE-8AA-S-00003C4D557878675645330967543476-4"), Ok(()));
        // Case does not matter (V2G20-2084, 2094), a wrong digit, type letter or length does.
        assert_eq!(check_secc_id("de-8aA-S-00003C4d557878675645330967543476-4"), Ok(()));
        assert!(check_secc_id("DE-8AA-S-00003C4D557878675645330967543476-2").unwrap_err().contains("check digit"));
        assert!(check_secc_id("DE-8AA-V-00003C4D557878675645330967543476-4").is_err());
        assert!(check_evcc_id("DE8-V-AA0000453C4D58-2").is_err());
        assert!(check_evcc_id("DE8-V-AA0000453C4D58Y_2").is_err());
    }

    #[test]
    fn the_key_identifier_is_method_2() {
        let id = key_identifier(b"some public key bits");
        assert_eq!(id[0] & 0xF0, 0x40);
        assert_eq!(id.len(), 8);
    }
}
