//! End to end over TLS 1.3 on localhost with freshly generated P-521 and Ed448 certificates.

mod common;

use common::pki::{Curve, Identity, Pki, pki};

use std::net::{TcpListener, TcpStream};
use std::thread;

use evcc_core::session::{Config, EndReason};
use evcc_linux::Outcome;
use evcc_linux::tls::{self, Credentials};
use evse_sim::State as EvseState;

fn credentials<'a>(own: &'a Identity, trust: &'a Pki) -> Credentials<'a> {
    Credentials::new(&own.chain, &own.key, &trust.pem)
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

#[test]
fn p256_certificates_work_when_backwards_compatibility_is_asked_for() {
    // The ISO 15118-2 certificate profile (V2G20-2056, 2057) on top of TLS 1.3.
    let (ca, secc) = pki(Curve::P256, "SECC");
    let (vca, evcc) = pki(Curve::P256, "EVCC");
    let server = tls::server_context(&credentials(&secc, &vca).backwards_compatible()).unwrap();
    let client = tls::client_context(&credentials(&evcc, &ca).backwards_compatible()).unwrap();
    let (addr, server) = start(SecContext(server));
    let outcome = drive_client(addr, &client).unwrap();
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn an_encrypted_private_key_needs_its_passphrase() {
    let (_, secc) = pki(Curve::P521, "SECC");
    let (ca, _) = pki(Curve::P521, "EVCC");
    let key = openssl::pkey::PKey::private_key_from_pem(&secc.key).unwrap();
    let encrypted = key.private_key_to_pem_pkcs8_passphrase(openssl::symm::Cipher::aes_128_cbc(), b"secret").unwrap();
    let with = |password: Option<&[u8]>| {
        let mut c = Credentials::new(&secc.chain, &encrypted, &ca.pem);
        if let Some(p) = password {
            c = c.key_password(p);
        }
        tls::server_context(&c).is_ok()
    };
    assert!(with(Some(b"secret")));
    assert!(!with(Some(b"wrong")));
    assert!(!with(None));
}
