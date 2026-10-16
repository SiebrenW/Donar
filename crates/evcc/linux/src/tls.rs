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
use std::net::TcpStream;

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
}

impl<'a> Credentials<'a> {
    pub fn new(chain_pem: &'a [u8], key_pem: &'a [u8], trust_pem: &'a [u8]) -> Self {
        Self { chain_pem, key_pem, trust_pem, key_password: None, backwards_compatible: false, versions: Versions::Tls13 }
    }

    /// The versions to speak. TLS 1.2 uses the ISO 15118-2 certificate profile, so it also
    /// switches [`Credentials::backwards_compatible`] on.
    pub fn versions(mut self, versions: Versions) -> Self {
        self.versions = versions;
        self.backwards_compatible |= versions != Versions::Tls13;
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
    b.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);
    Ok(b)
}

/// A client (EVCC) context.
pub fn client_context(c: &Credentials<'_>) -> Result<SslContext, ErrorStack> {
    Ok(context(SslMethod::tls_client(), c)?.build())
}

/// A server (SECC) context, for the EVSE side of tests.
pub fn server_context(c: &Credentials<'_>) -> Result<SslContext, ErrorStack> {
    Ok(context(SslMethod::tls_server(), c)?.build())
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
pub fn connect(context: &SslContext, stream: TcpStream) -> io::Result<SslStream<TcpStream>> {
    finish(Ssl::new(context).map_err(other)?.connect(stream))
}

/// Performs the server handshake on an accepted socket.
pub fn accept(context: &SslContext, stream: TcpStream) -> io::Result<SslStream<TcpStream>> {
    finish(Ssl::new(context).map_err(other)?.accept(stream))
}
