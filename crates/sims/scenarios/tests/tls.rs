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

/// Runs a session and reports what the client negotiated.
fn negotiated(addr: std::net::SocketAddr, client: &openssl::ssl::SslContext) -> std::io::Result<(String, String, Outcome)> {
    let mut stream = tls::connect(client, TcpStream::connect(addr).unwrap())?;
    let (version, cipher) = (stream.ssl().version_str().to_string(), stream.ssl().current_cipher().unwrap().name().to_string());
    let outcome = common::run_car(&mut stream, true, fast(), Some(3))?;
    Ok((version, cipher, outcome))
}

#[test]
fn a_tls12_only_charger_is_served_with_the_iso15118_2_suite_when_asked() {
    // V2G20-2054 ff.: after TLS 1.3, TLS 1.2 with the ISO 15118-2 suite, secp256r1 certificates.
    let (ca, secc) = pki(Curve::P256, "SECC");
    let (vca, evcc) = pki(Curve::P256, "EVCC");
    let server = tls::server_context(&credentials(&secc, &vca).versions(tls::Versions::Tls12)).unwrap();
    let client = tls::client_context(&credentials(&evcc, &ca).versions(tls::Versions::Tls13And12)).unwrap();
    let (addr, server) = start(SecContext(server));
    let (version, cipher, outcome) = negotiated(addr, &client).unwrap();
    assert_eq!((version.as_str(), cipher.as_str()), ("TLSv1.2", "ECDHE-ECDSA-AES128-SHA256"));
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

#[test]
fn a_tls13_only_vehicle_does_not_fall_back() {
    let (ca, secc) = pki(Curve::P256, "SECC");
    let (vca, evcc) = pki(Curve::P256, "EVCC");
    let server = tls::server_context(&credentials(&secc, &vca).versions(tls::Versions::Tls12)).unwrap();
    let client = tls::client_context(&credentials(&evcc, &ca).backwards_compatible()).unwrap();
    let (addr, server) = start(SecContext(server));
    assert!(negotiated(addr, &client).is_err());
    assert!(server.join().unwrap().is_err());
}

#[test]
fn two_tls13_capable_peers_stay_on_tls13_when_both_offer_the_fallback() {
    let (ca, secc) = pki(Curve::P256, "SECC");
    let (vca, evcc) = pki(Curve::P256, "EVCC");
    let server = tls::server_context(&credentials(&secc, &vca).versions(tls::Versions::Tls13And12)).unwrap();
    let client = tls::client_context(&credentials(&evcc, &ca).versions(tls::Versions::Tls13And12)).unwrap();
    let (addr, server) = start(SecContext(server));
    let (version, _, outcome) = negotiated(addr, &client).unwrap();
    assert_eq!(version, "TLSv1.3");
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
}

/// Both sides check the chain they got against the Annex B profile of the other.
fn profile_session(curve: Curve, secc_flaw: scenarios::profile_pki::Flaw, vehicle_checks: tls::CertificateChecks) -> std::io::Result<Outcome> {
    use evcc_linux::certificates::Peer;
    use scenarios::profile_pki::{Flaw, profile_pki};
    let (ca, secc) = profile_pki(curve, Peer::Secc, secc_flaw);
    let (vca, evcc) = profile_pki(curve, Peer::Vehicle, Flaw::None);
    let server = tls::server_context(&credentials(&secc, &vca)).unwrap();
    let client = tls::client_context(&credentials(&evcc, &ca)).unwrap();
    let (addr, server) = start(SecContext(server));
    let mut stream = tls::connect(&client, TcpStream::connect(addr).unwrap())?;
    let mut reported = Vec::new();
    let checked = tls::check_peer_certificates(&stream, evcc_linux::certificates::Peer::Secc, vehicle_checks, |v| reported.push(v.to_string()));
    if secc_flaw != Flaw::None {
        assert!(!reported.is_empty(), "the flaw {secc_flaw:?} went unreported");
    } else {
        assert!(reported.is_empty(), "{reported:?}");
    }
    checked?;
    let outcome = common::run_car(&mut stream, true, fast(), Some(3))?;
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
    Ok(outcome)
}

#[test]
fn sessions_run_over_chains_made_to_the_profiles() {
    for curve in [Curve::P521, Curve::Ed448] {
        let outcome = profile_session(curve, scenarios::profile_pki::Flaw::None, tls::CertificateChecks::Enforce).unwrap();
        assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    }
}

#[test]
fn a_chain_against_the_profile_is_refused_only_when_enforced() {
    use scenarios::profile_pki::Flaw;
    // The handshake and the session work (OpenSSL has no quarrel with it); the profile check decides.
    let warned = profile_session(Curve::P521, Flaw::KeyIdWrong, tls::CertificateChecks::Warn).unwrap();
    assert_eq!(warned, Outcome::Ended(EndReason::Completed));
    let err = profile_session(Curve::P521, Flaw::KeyIdWrong, tls::CertificateChecks::Enforce).unwrap_err();
    assert!(err.to_string().contains("do not meet the profile"), "{err}");
}

/// A session where the SECC staples `response` (or nothing) and the vehicle checks the status.
fn ocsp_session(ocsp: Option<scenarios::profile_pki::OcspFlaw>, require_all: bool) -> std::io::Result<Outcome> {
    use evcc_linux::certificates::Peer;
    use scenarios::profile_pki::{Flaw, OcspFlaw, profile_pki, profile_pki_with_ocsp};
    let (ca, secc, response) = profile_pki_with_ocsp(Curve::P521, Peer::Secc, Flaw::None, ocsp.unwrap_or(OcspFlaw::Good));
    let (vca, evcc) = profile_pki(Curve::P521, Peer::Vehicle, Flaw::None);
    let mut server_credentials = credentials(&secc, &vca);
    if ocsp.is_some() {
        server_credentials = server_credentials.ocsp_response(&response);
    }
    let server = tls::server_context(&server_credentials).unwrap();
    let client = tls::client_context(&credentials(&evcc, &ca)).unwrap();
    let (addr, server) = start(SecContext(server));
    let mut stream = tls::connect(&client, TcpStream::connect(addr).unwrap())?;
    let stapled = stream.ssl().ocsp_status().is_some();
    assert_eq!(stapled, ocsp.is_some(), "the response is stapled when the server has one");
    tls::check_peer_ocsp(&stream, &ca.pem, &[], tls::CertificateChecks::Enforce, require_all, |_| {})?;
    let outcome = common::run_car(&mut stream, true, fast(), Some(3))?;
    assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
    Ok(outcome)
}

#[test]
fn a_stapled_good_response_for_the_leaf_is_accepted() {
    use scenarios::profile_pki::OcspFlaw;
    assert_eq!(ocsp_session(Some(OcspFlaw::Good), false).unwrap(), Outcome::Ended(EndReason::Completed));
}

#[test]
fn a_revoked_certificate_and_a_missing_response_are_refused() {
    use scenarios::profile_pki::OcspFlaw;
    let revoked = ocsp_session(Some(OcspFlaw::Revoked), false).unwrap_err();
    assert!(revoked.to_string().contains("V2G20-2415"), "{revoked}");
    let missing = ocsp_session(None, false).unwrap_err();
    assert!(missing.to_string().contains("V2G20-2416"), "{missing}");
}

#[test]
fn the_sub_ca_statuses_openssl_cannot_deliver_fail_the_strict_check() {
    use scenarios::profile_pki::OcspFlaw;
    let err = ocsp_session(Some(OcspFlaw::Good), true).unwrap_err();
    assert!(err.to_string().contains("V2G20-2416"), "{err}");
}

type Names = Option<Vec<Vec<u8>>>;

/// Both sides handshake and report the authorities the other announced.
fn announced(server: Credentials<'_>, client: Credentials<'_>) -> (Names, Names) {
    let (server, client) = (tls::server_context(&server).unwrap(), tls::client_context(&client).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let thread = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut stream = tls::accept(&server, stream).unwrap();
        // The handshake is done once the client's first byte arrives.
        std::io::Read::read(&mut stream, &mut [0u8; 1]).unwrap();
        tls::peer_authorities(stream.ssl())
    });
    let mut stream = tls::connect(&client, TcpStream::connect(addr).unwrap()).unwrap();
    std::io::Write::write_all(&mut stream, b"x").unwrap();
    let seen_by_client = tls::peer_authorities(stream.ssl());
    (thread.join().unwrap(), seen_by_client)
}

#[test]
fn the_trust_anchors_are_announced_as_certificate_authorities() {
    // V2G20-1006 (the vehicle's ClientHello), 2401 (the charger's CertificateRequest).
    let subject = |p: &Pki| openssl::x509::X509::from_pem(&p.pem).unwrap().subject_name().to_der().unwrap();
    let (ca, secc) = pki(Curve::P521, "SECC");
    let (vca, evcc) = pki(Curve::P521, "EVCC");
    let (by_server, by_client) = announced(credentials(&secc, &vca), credentials(&evcc, &ca));
    assert_eq!(by_server, Some(vec![subject(&ca)]), "what the charger saw in the ClientHello");
    assert_eq!(by_client, Some(vec![subject(&vca)]), "what the vehicle saw in the CertificateRequest");
    // V2G20-3374, 3382: in CPM4PE neither side sends the extension.
    let (by_server, by_client) = announced(credentials(&secc, &vca).without_authorities(), credentials(&evcc, &ca).without_authorities());
    assert_eq!((by_server, by_client), (None, None));
}
