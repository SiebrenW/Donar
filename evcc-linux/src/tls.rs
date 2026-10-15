//! TLS for V2GTP over TCP with OpenSSL (ISO 15118-20:2022, 7.7).
//!
//! The profile is TLS 1.3 only with mutual authentication (V2G20-1264). Cipher suites are
//! `TLS_AES_256_GCM_SHA384` then `TLS_CHACHA20_POLY1305_SHA256` (Table 6), key exchange groups
//! `secp521r1` then `x448` and signatures `ecdsa_secp521r1_sha512` / `ed448` (Table 7,
//! V2G20-2674, 2319, 2320). Backwards compatibility with ISO 15118-2 (TLS 1.2, V2G20-2054
//! ff.) is not offered yet.
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

/// Our certificate chain (leaf first) and its private key, both PEM, and the trust anchors
/// (PEM, one or more certificates) the peer's chain must end in.
pub struct Credentials<'a> {
    pub chain_pem: &'a [u8],
    pub key_pem: &'a [u8],
    pub trust_pem: &'a [u8],
}

fn context(role: SslMethod, c: &Credentials<'_>) -> Result<SslContextBuilder, ErrorStack> {
    let mut b = SslContextBuilder::new(role)?;
    b.set_min_proto_version(Some(SslVersion::TLS1_3))?;
    b.set_max_proto_version(Some(SslVersion::TLS1_3))?;
    b.set_ciphersuites(CIPHER_SUITES)?;
    b.set_groups_list(GROUPS)?;
    b.set_sigalgs_list(SIGNATURE_ALGORITHMS)?;
    let mut chain = X509::stack_from_pem(c.chain_pem)?.into_iter();
    let leaf = chain.next().ok_or_else(ErrorStack::get)?;
    b.set_certificate(&leaf)?;
    for cert in chain {
        b.add_extra_chain_cert(cert)?;
    }
    let key = PKey::private_key_from_pem(c.key_pem)?;
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
