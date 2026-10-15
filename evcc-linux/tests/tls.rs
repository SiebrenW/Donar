//! End to end over TLS 1.3 on localhost with freshly generated P-521 and Ed448 certificates.

mod common;

use std::net::{TcpListener, TcpStream};
use std::thread;

use evcc_core::session::{Config, EndReason};
use evcc_linux::Outcome;
use evcc_linux::tls::{self, Credentials};
use evse_sim::State as EvseState;
use openssl::asn1::Asn1Time;
use openssl::bn::BigNum;
use openssl::ec::{EcGroup, EcKey};
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{PKey, Private};
use openssl::x509::extension::BasicConstraints;
use openssl::x509::{X509, X509NameBuilder};

#[derive(Clone, Copy)]
enum Curve {
    P521,
    Ed448,
    P256,
}

struct Pki {
    pem: Vec<u8>,
}

struct Identity {
    chain: Vec<u8>,
    key: Vec<u8>,
}

fn key(curve: Curve) -> PKey<Private> {
    match curve {
        Curve::P521 => PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::SECP521R1).unwrap()).unwrap()).unwrap(),
        Curve::P256 => PKey::from_ec_key(EcKey::generate(&EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap()).unwrap()).unwrap(),
        Curve::Ed448 => PKey::generate_ed448().unwrap(),
    }
}

fn certificate(cn: &str, key: &PKey<Private>, issuer: Option<(&X509, &PKey<Private>)>, ca: bool, serial: u32) -> X509 {
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
fn pki(curve: Curve, name: &str) -> (Pki, Identity) {
    let ca_key = key(curve);
    let ca = certificate(&format!("{name} CA"), &ca_key, None, true, 1);
    let leaf_key = key(curve);
    let leaf = certificate(name, &leaf_key, Some((&ca, &ca_key)), false, 2);
    let mut chain = leaf.to_pem().unwrap();
    chain.extend(ca.to_pem().unwrap());
    (Pki { pem: ca.to_pem().unwrap() }, Identity { chain, key: leaf_key.private_key_to_pem_pkcs8().unwrap() })
}

fn credentials<'a>(own: &'a Identity, trust: &'a Pki) -> Credentials<'a> {
    Credentials { chain_pem: &own.chain, key_pem: &own.key, trust_pem: &trust.pem }
}

fn fast() -> Config {
    Config { poll_interval_ms: 10, charge_loop_interval_ms: 10, ..Config::DC_20 }
}

type Server = thread::JoinHandle<std::io::Result<EvseState>>;

/// A SECC that accepts one TLS connection and runs the simulator on it.
fn start(secc: SecContext) -> (std::net::SocketAddr, Server) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept()?;
        let mut stream = tls::accept(&secc.0, stream)?;
        evse_sim::serve(&mut stream, evse_sim::Config::default())
    });
    (addr, server)
}

struct SecContext(openssl::ssl::SslContext);

fn context(own: &Identity, trust: &Pki, server: bool) -> openssl::ssl::SslContext {
    let c = credentials(own, trust);
    if server { tls::server_context(&c).unwrap() } else { tls::client_context(&c).unwrap() }
}

fn drive_client(addr: std::net::SocketAddr, client: &openssl::ssl::SslContext) -> std::io::Result<Outcome> {
    let mut stream = tls::connect(client, TcpStream::connect(addr).unwrap())?;
    common::run_car(&mut stream, true, fast(), Some(3))
}

#[test]
fn a_session_runs_over_tls_with_p521_certificates() {
    let (ca, secc) = pki(Curve::P521, "SECC");
    let (vca, evcc) = pki(Curve::P521, "EVCC");
    let (addr, server) = start(SecContext(context(&secc, &vca, true)));
    let outcome = drive_client(addr, &context(&evcc, &ca, false)).unwrap();
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn a_session_runs_over_tls_with_ed448_certificates() {
    let (ca, secc) = pki(Curve::Ed448, "SECC");
    let (vca, evcc) = pki(Curve::Ed448, "EVCC");
    let (addr, server) = start(SecContext(context(&secc, &vca, true)));
    let outcome = drive_client(addr, &context(&evcc, &ca, false)).unwrap();
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn a_server_certificate_from_an_unknown_ca_is_refused() {
    let (_, secc) = pki(Curve::P521, "SECC");
    let (vca, evcc) = pki(Curve::P521, "EVCC");
    let (other_ca, _) = pki(Curve::P521, "Other");
    let (addr, server) = start(SecContext(context(&secc, &vca, true)));
    assert!(drive_client(addr, &context(&evcc, &other_ca, false)).is_err());
    assert!(server.join().unwrap().is_err());
}

#[test]
fn a_client_certificate_from_an_unknown_ca_is_refused() {
    // Mutual authentication: the SECC must not accept an EVCC it does not trust.
    let (ca, secc) = pki(Curve::P521, "SECC");
    let (_, evcc) = pki(Curve::P521, "EVCC");
    let (other_ca, _) = pki(Curve::P521, "Other");
    let (addr, server) = start(SecContext(context(&secc, &other_ca, true)));
    // TLS 1.3 completes the client side before the server verifies the client certificate, so
    // the refusal shows up when the client first talks to the SECC.
    let result = drive_client(addr, &context(&evcc, &ca, false));
    assert!(result.is_err() || result.unwrap() == Outcome::ConnectionClosed);
    assert!(server.join().unwrap().is_err());
}

#[test]
fn certificates_on_a_curve_outside_the_profile_are_refused() {
    let (ca, secc) = pki(Curve::P256, "SECC");
    let (vca, evcc) = pki(Curve::P256, "EVCC");
    let (addr, server) = start(SecContext(context(&secc, &vca, true)));
    assert!(drive_client(addr, &context(&evcc, &ca, false)).is_err());
    assert!(server.join().unwrap().is_err());
}
