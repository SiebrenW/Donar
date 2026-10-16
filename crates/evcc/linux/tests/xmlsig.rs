//! XML signatures over EXI fragments (ISO 15118-20 7.9) with generated keys: a signature
//! travels through the codec in a message header and verifies on the other side.

use evcc_linux::xmlsig::{self, Error, Suite};
use exi::Arena;
use iso15118_20::common::{self, ChargingSession, Message, SessionStopReq, SignedCertificateChain};
use iso15118_20::common_types::MessageHeader;
use iso15118_20::signature::{self, encode_fragment};
use openssl::ec::{EcGroup, EcKey};
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private, Public};

fn ecdsa_key() -> PKey<Private> {
    PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::SECP521R1).unwrap()).unwrap()).unwrap()
}

fn ed448_key() -> PKey<Private> {
    PKey::generate_ed448().unwrap()
}

fn public(key: &PKey<Private>) -> PKey<Public> {
    PKey::public_key_from_der(&key.public_key_to_der().unwrap()).unwrap()
}

const CHAIN: SignedCertificateChain<'static> = SignedCertificateChain { id: "id1", certificate: &[1, 2, 3, 4, 5], sub_certificates: None };

fn chain_fragment(chain: &SignedCertificateChain<'_>) -> Vec<u8> {
    let mut out = [0u8; 512];
    let n = encode_fragment(&mut out, signature::fragment::OEM_PROVISIONING_CERTIFICATE_CHAIN, |e| chain.encode_content(e)).unwrap();
    out[..n].to_vec()
}

/// Signs, sends the signature in the header of a message, decodes it and hands it to `check`.
fn through_the_codec<R>(owned: &xmlsig::OwnedSignature, check: impl FnOnce(&signature::Signature<'_>) -> R) -> R {
    owned.with_signature(|signature| {
        let mut header = MessageHeader::new([1, 2, 3, 4, 5, 6, 7, 8], 1_700_000_000);
        header.signature = Some(*signature);
        let m = SessionStopReq { header, charging_session: ChargingSession::Terminate, ev_termination_code: None, ev_termination_explanation: None };
        let mut out = [0u8; 1024];
        let n = m.encode(&mut out).unwrap();
        let mut mem = [0u8; 8192];
        let arena = Arena::new(&mut mem);
        let Message::SessionStopReq(got) = common::decode(&out[..n], &arena).unwrap() else { panic!() };
        assert_eq!(got.header.signature.as_ref(), Some(signature));
        check(got.header.signature.as_ref().unwrap())
    })
}

#[test]
fn an_ecdsa_signature_survives_the_header_and_verifies() {
    let key = ecdsa_key();
    let fragment = chain_fragment(&CHAIN);
    let owned = xmlsig::sign(&key, &[("#id1", &fragment)]).unwrap();
    owned.with_signature(|s| {
        assert_eq!(s.signature_value.value.len(), 132);
        assert_eq!(s.signed_info.reference[0].digest_value.len(), 64);
        assert_eq!(s.signed_info.signature_algorithm, signature::ECDSA_SHA512);
    });
    through_the_codec(&owned, |s| xmlsig::verify(&public(&key), s, &[("#id1", &fragment)])).unwrap();
}

#[test]
fn an_ed448_signature_survives_the_header_and_verifies() {
    let key = ed448_key();
    let fragment = chain_fragment(&CHAIN);
    let owned = xmlsig::sign(&key, &[("#id1", &fragment)]).unwrap();
    owned.with_signature(|s| {
        assert_eq!(s.signature_value.value.len(), 114);
        assert_eq!(s.signed_info.signature_algorithm, signature::ED448);
        assert_eq!(s.signed_info.reference[0].digest_algorithm, signature::DIGEST_SHAKE256);
    });
    through_the_codec(&owned, |s| xmlsig::verify(&public(&key), s, &[("#id1", &fragment)])).unwrap();
}

#[test]
fn several_references_are_all_checked() {
    let key = ecdsa_key();
    let a = chain_fragment(&CHAIN);
    let other = SignedCertificateChain { id: "id2", certificate: &[9, 9, 9], sub_certificates: None };
    let b = chain_fragment(&other);
    let owned = xmlsig::sign(&key, &[("#id1", &a), ("#id2", &b)]).unwrap();
    let pk = public(&key);
    through_the_codec(&owned, |s| xmlsig::verify(&pk, s, &[("#id2", &b), ("#id1", &a)])).unwrap();
    // A fragment the signature does not mention, or one it mentions but is not given.
    let r = through_the_codec(&owned, |s| xmlsig::verify(&pk, s, &[("#id1", &a)]));
    assert!(matches!(r, Err(Error::Reference(uri)) if uri == "#id2"));
    let r = through_the_codec(&owned, |s| xmlsig::verify(&pk, s, &[("#id1", &a), ("#id2", &b), ("#id3", &b)]));
    assert!(matches!(r, Err(Error::Reference(uri)) if uri == "#id3"));
}

#[test]
fn a_changed_fragment_a_changed_value_or_the_wrong_key_fail() {
    for key in [ecdsa_key(), ed448_key()] {
        let fragment = chain_fragment(&CHAIN);
        let owned = xmlsig::sign(&key, &[("#id1", &fragment)]).unwrap();
        let pk = public(&key);
        // The signed element differs: its digest does not match.
        let tampered = chain_fragment(&SignedCertificateChain { certificate: &[1, 2, 3, 4, 6], ..CHAIN });
        let r = through_the_codec(&owned, |s| xmlsig::verify(&pk, s, &[("#id1", &tampered)]));
        assert!(matches!(r, Err(Error::Digest(_))), "{r:?}");
        // The signature value differs.
        let r = owned.with_signature(|s| {
            let mut value = s.signature_value.value.to_vec();
            value[10] ^= 1;
            let forged = signature::Signature { signature_value: signature::SignatureValue { id: None, value: &value }, ..*s };
            xmlsig::verify(&pk, &forged, &[("#id1", &fragment)])
        });
        assert!(r.is_err() && !matches!(r, Err(Error::Reference(_) | Error::Digest(_))), "{r:?}");
        // Someone else's key.
        let other = if Suite::of_key(&key) == Some(Suite::Ed448) { ed448_key() } else { ecdsa_key() };
        let r = through_the_codec(&owned, |s| xmlsig::verify(&public(&other), s, &[("#id1", &fragment)]));
        assert!(matches!(r, Err(Error::Value)), "{r:?}");
    }
}

#[test]
fn a_signature_of_the_other_suite_is_refused() {
    let (ecdsa, ed448) = (ecdsa_key(), ed448_key());
    let fragment = chain_fragment(&CHAIN);
    let owned = xmlsig::sign(&ecdsa, &[("#id1", &fragment)]).unwrap();
    let r = through_the_codec(&owned, |s| xmlsig::verify(&public(&ed448), s, &[("#id1", &fragment)]));
    assert!(matches!(r, Err(Error::Algorithm(_))), "{r:?}");
}

#[test]
fn other_keys_are_refused() {
    let p256 = PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap()).unwrap()).unwrap();
    assert!(matches!(xmlsig::sign(&p256, &[("#id1", &[0u8])]), Err(Error::UnsupportedKey)));
}
