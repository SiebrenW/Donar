//! The certificate profile checks of ISO 15118-20 Annex B against chains made to the profile
//! and chains that break one rule each.

use evcc_linux::certificates::{Peer, Violation, check_chain};
use openssl::x509::X509;
use scenarios::pki::Curve;
use scenarios::profile_pki::{Flaw, profile_pki};

fn der_chain(identity: &scenarios::pki::Identity, root_pem: &[u8]) -> Vec<Vec<u8>> {
    let mut all = X509::stack_from_pem(&identity.chain).unwrap();
    all.extend(X509::stack_from_pem(root_pem).unwrap());
    all.iter().map(|c| c.to_der().unwrap()).collect()
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn check(curve: Curve, peer: Peer, flaw: Flaw) -> Vec<Violation> {
    let (root, identity) = profile_pki(curve, peer, flaw);
    check_chain(&der_chain(&identity, &root.pem), peer, now())
}

fn requirements(found: &[Violation]) -> Vec<&'static str> {
    found.iter().map(|v| v.requirement).collect()
}

#[test]
fn chains_made_to_the_profiles_are_accepted() {
    for curve in [Curve::P521, Curve::Ed448] {
        for peer in [Peer::Secc, Peer::Vehicle] {
            let found = check(curve, peer, Flaw::None);
            assert!(found.is_empty(), "{curve:?} {peer:?}: {found:#?}");
        }
    }
}

#[test]
fn each_broken_rule_is_found_where_it_is() {
    // (flaw, the requirement that has to be named, the position in the chain: 0 is the leaf)
    let cases = [
        (Flaw::KeyIdMethod1, "V2G20-1852", None),
        (Flaw::LeafWithoutKeyUsage, "Annex B", Some(0)),
        (Flaw::LeafIsCa, "Annex B", Some(0)),
        (Flaw::ExtraExtension, "V2G20-2686", Some(0)),
        (Flaw::WrongPathLength, "Annex B", Some(1)),
        (Flaw::NoOcsp, "Annex B", Some(0)),
        (Flaw::Expired, "V2G20-1001", Some(0)),
        (Flaw::BadIdentifier, "V2G20-2583", Some(0)),
        (Flaw::TooLong, "V2G20-1003", Some(0)),
        (Flaw::ExtraNameAttribute, "V2G20-2685", Some(0)),
        (Flaw::LeafWithoutDigitalSignature, "Annex B", Some(0)),
        (Flaw::WrongKeyPurpose, "Annex B", Some(0)),
    ];
    for (flaw, requirement, depth) in cases {
        for curve in [Curve::P521, Curve::Ed448] {
            let found = check(curve, Peer::Secc, flaw);
            assert!(
                found.iter().any(|v| v.requirement == requirement && depth.is_none_or(|d| v.depth == d)),
                "{flaw:?} on {curve:?}: expected {requirement} at {depth:?}, found {found:#?}"
            );
        }
    }
}

#[test]
fn a_vehicle_needs_revocation_information_and_client_authentication() {
    let found = check(Curve::P521, Peer::Vehicle, Flaw::WrongKeyPurpose);
    assert!(found.is_empty(), "{found:#?}: the flaw only touches a SECC");
    // A SECC chain read as a vehicle's: wrong key purpose, wrong identifier.
    let (root, identity) = profile_pki(Curve::P521, Peer::Secc, Flaw::None);
    let found = check_chain(&der_chain(&identity, &root.pem), Peer::Vehicle, now());
    let named = requirements(&found);
    assert!(named.contains(&"V2G20-2597"), "{found:#?}");
}

#[test]
fn a_chain_with_a_foreign_curve_is_refused() {
    let (root, mut identity) = profile_pki(Curve::P521, Peer::Secc, Flaw::None);
    let (_, other) = profile_pki(Curve::Ed448, Peer::Secc, Flaw::None);
    // The Ed448 leaf under P-521 CAs: the leaf decides the curve set, the CAs do not match it.
    let leaf = X509::stack_from_pem(&other.chain).unwrap().remove(0);
    let mut chain = X509::stack_from_pem(&identity.chain).unwrap();
    chain[0] = leaf;
    identity.chain = chain.iter().flat_map(|c| c.to_pem().unwrap()).collect();
    let found = check_chain(&der_chain(&identity, &root.pem), Peer::Secc, now());
    assert!(!found.is_empty());
}

#[test]
fn a_certificate_made_by_openssl_defaults_does_not_meet_the_profile() {
    // The throwaway PKI of the other tests: no key usage, version 3 by luck, method 1 key identifiers (none).
    let (ca, identity) = scenarios::pki::pki(Curve::P521, "SECC");
    let found = check_chain(&der_chain(&identity, &ca.pem), Peer::Secc, now());
    assert!(!found.is_empty());
}

mod ocsp {
    use super::*;
    use evcc_linux::ocsp::check_revocation;
    use scenarios::profile_pki::{OcspFlaw, profile_pki_with_ocsp};

    fn check(curve: Curve, ocsp: OcspFlaw, require_all: bool) -> Vec<Violation> {
        let (root, identity, response) = profile_pki_with_ocsp(curve, Peer::Secc, Flaw::None, ocsp);
        let chain = super::der_chain(&identity, &root.pem);
        let trust = vec![chain.last().unwrap().clone()];
        check_revocation(&chain, &[response], &trust, super::now(), require_all)
    }

    #[test]
    fn a_good_response_covers_the_leaf() {
        for curve in [Curve::P521, Curve::Ed448] {
            let found = check(curve, OcspFlaw::Good, false);
            assert!(found.is_empty(), "{curve:?}: {found:#?}");
        }
    }

    #[test]
    fn every_certificate_below_the_root_needs_a_status_when_asked() {
        // The response is for the leaf; the sub-CAs have none (V2G20-2416).
        let found = check(Curve::P521, OcspFlaw::Good, true);
        let missing: Vec<usize> = found.iter().filter(|v| v.requirement == "V2G20-2416").map(|v| v.depth).collect();
        assert_eq!(missing, [1, 2], "{found:#?}");
    }

    #[test]
    fn what_is_wrong_with_a_response_is_named() {
        for (flaw, requirement) in [
            (OcspFlaw::Revoked, "V2G20-2415"),
            (OcspFlaw::TooLong, "V2G20-1021"),
            (OcspFlaw::Expired, "V2G20-1021"),
            (OcspFlaw::Stranger, "V2G20-2414"),
            (OcspFlaw::OtherCertificate, "V2G20-2416"),
        ] {
            for curve in [Curve::P521, Curve::Ed448] {
                let found = check(curve, flaw, false);
                assert!(found.iter().any(|v| v.requirement == requirement && v.depth == 0), "{flaw:?} on {curve:?}: {found:#?}");
            }
        }
    }

    #[test]
    fn no_response_at_all_is_a_missing_status() {
        let (root, identity, _) = profile_pki_with_ocsp(Curve::P521, Peer::Secc, Flaw::None, OcspFlaw::Good);
        let chain = super::der_chain(&identity, &root.pem);
        let trust = vec![chain.last().unwrap().clone()];
        let found = check_revocation(&chain, &[], &trust, super::now(), false);
        assert_eq!(super::requirements(&found), ["V2G20-2416"]);
    }
}
