//! TLS for V2GTP over TCP with OpenSSL (ISO 15118-20:2022, 7.7).
//!
//! The profile is TLS 1.3 only with mutual authentication (V2G20-1264). Cipher suites are
//! `TLS_AES_256_GCM_SHA384` then `TLS_CHACHA20_POLY1305_SHA256` (Table 6), key exchange groups
//! `secp521r1` then `x448` and signatures `ecdsa_secp521r1_sha512` / `ed448` (Table 7,
//! V2G20-2674, 2319, 2320). Backwards compatibility with ISO 15118-2 (V2G20-2054 ff.) is opt-in:
//! [`Credentials::versions`] with [`Versions::Tls13And12`] also offers TLS 1.2 with the suite
//! `TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA256` after the TLS 1.3 ones (the static-ECDH suite of
//! Table 7 is not in OpenSSL), and [`Credentials::backwards_compatible`] adds the ISO 15118-2
//! certificate profile (secp256r1 keys and signatures, V2G20-2056, 2057), which TLS 1.2 needs.
//!
//! The peer certificate chain is verified against the trust anchors but not against a host
//! name: a SECC is found by SDP, not by name.

use std::io;
use std::net::{Ipv6Addr, SocketAddr, SocketAddrV6, TcpStream, UdpSocket};
use std::sync::Arc;

use openssl::error::ErrorStack;
use openssl::pkey::PKey;
use openssl::ssl::{
    HandshakeError, Ssl, SslContext, SslContextBuilder, SslMethod, SslStream, SslVerifyMode, SslVersion,
};
use openssl::x509::X509;
use openssl::x509::store::X509StoreBuilder;

/// Table 6.
pub const CIPHER_SUITES: &str = "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256";
/// Table 7.
pub const GROUPS: &str = "secp521r1:x448";
/// V2G20-2674 (preferred) and V2G20-2319.
pub const SIGNATURE_ALGORITHMS: &str = "ecdsa_secp521r1_sha512:ed448";
/// The TLS 1.2 suite of ISO 15118-2 (Table 7) that OpenSSL has: ECDHE with an ECDSA certificate.
pub const CIPHER_LIST_TLS12: &str = "ECDHE-ECDSA-AES128-SHA256";
/// Appended for ISO 15118-2 certificates (secp256r1) when backwards compatibility is wanted.
pub const GROUPS_LEGACY: &str = ":secp256r1";
pub const SIGNATURE_ALGORITHMS_LEGACY: &str = ":ecdsa_secp256r1_sha256";

/// The protocol versions a context speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Versions {
    /// TLS 1.3 only: what ISO 15118-20 asks.
    #[default]
    Tls13,
    /// TLS 1.3, and TLS 1.2 for a peer that speaks only that (an ISO 15118-2 charger).
    Tls13And12,
    /// TLS 1.2 only: plays an ISO 15118-2 peer, for tests.
    Tls12,
}

/// Where the secrets of a test platform's TLS sessions go, for a packet capture to decrypt them.
///
/// The dSPACE V2Gshark plugin for Wireshark reads the lines of an NSS key log (`CLIENT_RANDOM ...`
/// for TLS 1.2, the `*_TRAFFIC_SECRET` lines for TLS 1.3) from UDP packets with a source port in
/// 49152 to 65535 that one of the partners sends in a test environment. Each line is one packet,
/// sent to the all-nodes group of the interface so that a capture on the cable sees it.
/// **Only for simulators and tests**: it gives away the key of the session.
#[derive(Clone)]
pub struct Disclosure {
    socket: Arc<UdpSocket>,
    to: SocketAddr,
}

/// The destination port of the disclosure packets (nothing listens there).
pub const DISCLOSURE_PORT: u16 = 50000;

impl Disclosure {
    /// Discloses on the interface with this index.
    pub fn on_interface(ifindex: u32) -> io::Result<Self> {
        // The source port has to be in the dynamic range the dissector looks at.
        let start = 49152 + (std::process::id() % 16000) as u16;
        let mut last = None;
        for port in (start..=65535).chain(49152..start) {
            match UdpSocket::bind((Ipv6Addr::UNSPECIFIED, port)) {
                Ok(socket) => {
                    let to = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1), DISCLOSURE_PORT, 0, ifindex));
                    return Ok(Self { socket: Arc::new(socket), to });
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::other("no free port")))
    }

    fn send(&self, line: &str) {
        let _ = self.socket.send_to(line.as_bytes(), self.to);
    }
}

/// Our certificate chain (leaf first) and its private key, both PEM, and the trust anchors
/// (PEM, one or more certificates) the peer's chain must end in.
pub struct Credentials<'a> {
    pub chain_pem: &'a [u8],
    pub key_pem: &'a [u8],
    pub trust_pem: &'a [u8],
    /// Passphrase of an encrypted private key.
    pub key_password: Option<&'a [u8]>,
    /// Also accept secp256r1 certificates and key exchange (the ISO 15118-2 profile).
    pub backwards_compatible: bool,
    pub versions: Versions,
    /// An OCSP response (DER) to staple to the certificate of a server context, for the
    /// peer's `status_request` (V2G20-2388).
    pub ocsp_response: Option<&'a [u8]>,
    /// Gives the session secrets away (test platforms only).
    pub disclosure: Option<Disclosure>,
}

impl<'a> Credentials<'a> {
    pub fn new(chain_pem: &'a [u8], key_pem: &'a [u8], trust_pem: &'a [u8]) -> Self {
        Self { chain_pem, key_pem, trust_pem, key_password: None, backwards_compatible: false, versions: Versions::Tls13, ocsp_response: None, disclosure: None }
    }

    /// Staples this OCSP response (DER, for the leaf certificate) to a server's certificate when the
    /// client asks for the status.
    pub fn ocsp_response(mut self, response: &'a [u8]) -> Self {
        self.ocsp_response = Some(response);
        self
    }

    /// The versions to speak. TLS 1.2 uses the ISO 15118-2 certificate profile, so it also
    /// switches [`Credentials::backwards_compatible`] on.
    pub fn versions(mut self, versions: Versions) -> Self {
        self.versions = versions;
        self.backwards_compatible |= versions != Versions::Tls13;
        self
    }

    /// Sends the secrets of every session to the capture (see [`Disclosure`]).
    pub fn disclose_secrets(mut self, disclosure: Disclosure) -> Self {
        self.disclosure = Some(disclosure);
        self
    }

    pub fn key_password(mut self, password: &'a [u8]) -> Self {
        self.key_password = Some(password);
        self
    }

    pub fn backwards_compatible(mut self) -> Self {
        self.backwards_compatible = true;
        self
    }
}

fn context(role: SslMethod, c: &Credentials<'_>) -> Result<SslContextBuilder, ErrorStack> {
    let mut b = SslContextBuilder::new(role)?;
    let (min, max) = match c.versions {
        Versions::Tls13 => (SslVersion::TLS1_3, SslVersion::TLS1_3),
        Versions::Tls13And12 => (SslVersion::TLS1_2, SslVersion::TLS1_3),
        Versions::Tls12 => (SslVersion::TLS1_2, SslVersion::TLS1_2),
    };
    b.set_min_proto_version(Some(min))?;
    b.set_max_proto_version(Some(max))?;
    b.set_ciphersuites(CIPHER_SUITES)?;
    b.set_cipher_list(CIPHER_LIST_TLS12)?;
    if c.backwards_compatible {
        b.set_groups_list(&format!("{GROUPS}{GROUPS_LEGACY}"))?;
        b.set_sigalgs_list(&format!("{SIGNATURE_ALGORITHMS}{SIGNATURE_ALGORITHMS_LEGACY}"))?;
    } else {
        b.set_groups_list(GROUPS)?;
        b.set_sigalgs_list(SIGNATURE_ALGORITHMS)?;
    }
    if let Some(d) = c.disclosure.clone() {
        b.set_keylog_callback(move |_, line| d.send(line));
    }
    let mut chain = X509::stack_from_pem(c.chain_pem)?.into_iter();
    let leaf = chain.next().ok_or_else(ErrorStack::get)?;
    b.set_certificate(&leaf)?;
    for cert in chain {
        b.add_extra_chain_cert(cert)?;
    }
    let key = match c.key_password {
        Some(password) => PKey::private_key_from_pem_passphrase(c.key_pem, password)?,
        None => PKey::private_key_from_pem(c.key_pem)?,
    };
    b.set_private_key(&key)?;
    b.check_private_key()?;
    let mut store = X509StoreBuilder::new()?;
    for cert in X509::stack_from_pem(c.trust_pem)? {
        store.add_cert(cert)?;
    }
    b.set_cert_store(store.build());
    // The server requests our certificate (V2G20-1264) and each side insists on the other's.
    b.set_verify_callback(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT, tolerate_profile_extensions);
    Ok(b)
}

/// The profiles of Annex B make AuthorityInfoAccess and CRLDistributionPoints critical, which
/// OpenSSL does not process during path validation and therefore reports as an unhandled critical
/// extension. Revocation is checked with OCSP and CRLs apart from the path validation, so
/// those two are let through; any other critical extension OpenSSL does not know still fails.
fn tolerate_profile_extensions(ok: bool, ctx: &mut openssl::x509::X509StoreContextRef) -> bool {
    use x509_parser::prelude::*;
    if ok || ctx.error().as_raw() != 34 /* X509_V_ERR_UNHANDLED_CRITICAL_EXTENSION */ {
        return ok;
    }
    let Some(der) = ctx.current_cert().and_then(|c| c.to_der().ok()) else { return false };
    let Ok((_, cert)) = X509Certificate::from_der(&der) else { return false };
    let tolerated = ["1.3.6.1.5.5.7.1.1", "2.5.29.31"];
    let only_those = cert.extensions().iter().filter(|e| e.critical).all(|e| {
        let oid = e.oid.to_id_string();
        // The critical extensions OpenSSL handles itself are not what raised the error.
        tolerated.contains(&oid.as_str()) || ["2.5.29.15", "2.5.29.19", "2.5.29.37", "2.5.29.17", "2.5.29.30", "2.5.29.36", "2.5.29.32"].contains(&oid.as_str())
    });
    if only_those {
        ctx.set_error(openssl::x509::X509VerifyResult::OK);
    }
    only_those
}

/// A client (EVCC) context.
pub fn client_context(c: &Credentials<'_>) -> Result<SslContext, ErrorStack> {
    Ok(context(SslMethod::tls_client(), c)?.build())
}

/// A server (SECC) context, for the EVSE side of tests.
pub fn server_context(c: &Credentials<'_>) -> Result<SslContext, ErrorStack> {
    let mut b = context(SslMethod::tls_server(), c)?;
    if let Some(response) = c.ocsp_response {
        let response = response.to_vec();
        b.set_status_callback(move |ssl| {
            ssl.set_ocsp_status(&response)?;
            Ok(true)
        })?;
    }
    Ok(b.build())
}

fn other(e: impl std::error::Error + Send + Sync + 'static) -> io::Error {
    io::Error::other(e)
}

fn finish<S: std::io::Read + std::io::Write>(r: Result<SslStream<S>, HandshakeError<S>>) -> io::Result<SslStream<S>> {
    r.map_err(|e| match e {
        HandshakeError::SetupFailure(e) => other(e),
        HandshakeError::Failure(m) => other(m.into_error()),
        HandshakeError::WouldBlock(m) => other(m.into_error()),
    })
}

/// Performs the client handshake on a connected socket.
///
/// The ClientHello carries `status_request` (V2G20-2388), so that a SECC with a response for its
/// certificate staples it; see [`check_peer_ocsp`].
pub fn connect(context: &SslContext, stream: TcpStream) -> io::Result<SslStream<TcpStream>> {
    let mut ssl = Ssl::new(context).map_err(other)?;
    ssl.set_status_type(openssl::ssl::StatusType::OCSP).map_err(other)?;
    finish(ssl.connect(stream))
}

/// Performs the server handshake on an accepted socket.
pub fn accept(context: &SslContext, stream: TcpStream) -> io::Result<SslStream<TcpStream>> {
    finish(Ssl::new(context).map_err(other)?.accept(stream))
}

/// What to do with a peer certificate chain that does not meet the Annex B profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CertificateChecks {
    /// Do not check.
    #[default]
    Off,
    /// Check and report, go on.
    Warn,
    /// Check; a chain with violations ends the connection.
    Enforce,
}

/// Checks the chain the peer presented against the profiles of `peer` (see
/// [`crate::certificates`]) after the handshake. Every violation goes to `report`; with
/// [`CertificateChecks::Enforce`] there is an error when there was one.
pub fn check_peer_certificates<S>(stream: &SslStream<S>, peer: crate::certificates::Peer, policy: CertificateChecks, mut report: impl FnMut(&crate::certificates::Violation)) -> io::Result<()> {
    if policy == CertificateChecks::Off {
        return Ok(());
    }
    let found = crate::certificates::check_peer(stream.ssl(), peer);
    found.iter().for_each(&mut report);
    match (policy, found.first()) {
        (CertificateChecks::Enforce, Some(first)) => Err(io::Error::new(io::ErrorKind::InvalidData, format!("the peer's certificates do not meet the profile: {first} (and {} more)", found.len() - 1))),
        _ => Ok(()),
    }
}

/// Checks the revocation status of the peer's chain with the OCSP response stapled in the
/// handshake (and `more` that the host has from elsewhere), against `trust_pem` and the rules of
/// [`crate::ocsp`]. `require_all` asks for a status for every certificate below the root
/// (V2G20-2416); OpenSSL hands over only the leaf's stapled response, so without more responses
/// that fails for any chain with a sub-CA.
pub fn check_peer_ocsp<S>(
    stream: &SslStream<S>,
    trust_pem: &[u8],
    more: &[Vec<u8>],
    policy: CertificateChecks,
    require_all: bool,
    mut report: impl FnMut(&crate::certificates::Violation),
) -> io::Result<()> {
    if policy == CertificateChecks::Off {
        return Ok(());
    }
    let ssl = stream.ssl();
    let chain: Vec<Vec<u8>> = ssl.verified_chain().map(|c| c.iter().filter_map(|x| x.to_der().ok()).collect()).unwrap_or_default();
    let trust: Vec<Vec<u8>> = X509::stack_from_pem(trust_pem).map_err(other)?.iter().filter_map(|c| c.to_der().ok()).collect();
    let mut responses: Vec<Vec<u8>> = ssl.ocsp_status().map(|r| vec![r.to_vec()]).unwrap_or_default();
    responses.extend_from_slice(more);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let found = crate::ocsp::check_revocation(&chain, &responses, &trust, now, require_all);
    found.iter().for_each(&mut report);
    match (policy, found.first()) {
        (CertificateChecks::Enforce, Some(first)) => Err(io::Error::new(io::ErrorKind::InvalidData, format!("the revocation status of the peer's certificates is not good: {first} (and {} more)", found.len() - 1))),
        _ => Ok(()),
    }
}
