//! Revocation status by OCSP (ISO 15118-20:2022 7.7.3, V2G20-1240, 2409 ff., RFC 6960).
//!
//! A relying party needs the status of every certificate of the peer's chain except the root
//! (V2G20-2416). The responses come stapled in the TLS handshake (`status_request`, V2G20-2388);
//! each is checked here: the response is successful, signed by the issuer of the certificate or
//! by a responder whose chain ends in a trusted root (V2G20-2413, 2414), covers the certificate
//! with status *good* (V2G20-2415), and is valid now and for at most one week (V2G20-1021).
//!
//! OpenSSL hands a TLS client one stapled response (for the leaf) whatever the peer sent for
//! the CA certificates, so [`check_revocation`] also takes responses the host got another way
//! (cached, or asked of the responder, V2G20-2411).

use openssl::hash::MessageDigest;
use openssl::ocsp::{OcspCertId, OcspCertStatus, OcspFlag, OcspResponse, OcspResponseStatus};
use openssl::stack::Stack;
use openssl::x509::X509;
use openssl::x509::store::X509StoreBuilder;

use crate::certificates::Violation;

/// The longest validity of a response (V2G20-1021), in seconds.
pub const MAX_VALIDITY_S: i64 = 7 * 24 * 3600;

/// Checks the revocation status of the certificates of `chain` (DER, leaf first, root last)
/// from `responses` (DER `OCSPResponse`s) at `now`; `trust` are the trusted roots. With
/// `require_all`, a missing status for any certificate below the root is a violation
/// (V2G20-2416); without it only the leaf's is. A response that is present has to be good in
/// either case.
pub fn check_revocation(chain: &[Vec<u8>], responses: &[Vec<u8>], trust: &[Vec<u8>], now: i64, require_all: bool) -> Vec<Violation> {
    let mut found = Vec::new();
    let add = |found: &mut Vec<Violation>, depth: usize, requirement: &'static str, message: String| found.push(Violation { depth, requirement, message });
    let certs: Result<Vec<X509>, _> = chain.iter().map(|d| X509::from_der(d)).collect();
    let Ok(certs) = certs else {
        add(&mut found, 0, "X.509", "the chain cannot be parsed".into());
        return found;
    };
    let mut store = X509StoreBuilder::new().expect("store");
    // The profiles make AuthorityInfoAccess critical, which OpenSSL cannot process; the chain
    // itself is held to the profile by `certificates::check_chain` (V2G20-2686).
    let _ = store.set_flags(openssl::x509::verify::X509VerifyFlags::IGNORE_CRITICAL);
    for der in trust {
        if let Ok(c) = X509::from_der(der) {
            let _ = store.add_cert(c);
        }
    }
    let store = store.build();
    let parsed: Vec<OcspResponse> = responses.iter().filter_map(|r| OcspResponse::from_der(r).ok()).collect();
    if parsed.len() != responses.len() {
        add(&mut found, 0, "RFC 6960", "an OCSP response cannot be parsed".into());
    }
    // Certificates a responder may be among: the chain.
    let mut extra = Stack::new().expect("stack");
    for c in &certs {
        let _ = extra.push(c.clone());
    }
    for depth in 0..certs.len().saturating_sub(1) {
        // The root needs no status; a chain of one certificate has no issuer to ask.
        let (cert, issuer) = (&certs[depth], &certs[depth + 1]);
        let Ok(id) = OcspCertId::from_cert(MessageDigest::sha1(), cert, issuer) else { continue };
        let mut answered = false;
        for response in &parsed {
            if response.status() != OcspResponseStatus::SUCCESSFUL {
                add(&mut found, depth, "RFC 6960", format!("the response status is {}, not successful", response.status().as_raw()));
                answered = true;
                continue;
            }
            let Ok(basic) = response.basic() else { continue };
            let Some(status) = basic.find_status(&id) else { continue };
            answered = true;
            if let Err(e) = basic.verify(&extra, &store, OcspFlag::empty()) {
                // V2G20-2413, 2414: signature and responder certificate.
                add(&mut found, depth, "V2G20-2414", format!("the response does not verify: {e}"));
                continue;
            }
            if status.status != OcspCertStatus::GOOD {
                add(&mut found, depth, "V2G20-2415", format!("the certificate status is {}, not good", if status.status == OcspCertStatus::REVOKED { "revoked" } else { "unknown" }));
            }
            let (this, next) = (to_unix(status.this_update), status.next_update().map(to_unix));
            match next {
                None => add(&mut found, depth, "V2G20-1021", "the response has no nextUpdate".into()),
                Some(next) => {
                    if next - this > MAX_VALIDITY_S {
                        add(&mut found, depth, "V2G20-1021", format!("the response is valid for {} s, at most one week is allowed", next - this));
                    }
                    if now > next {
                        add(&mut found, depth, "V2G20-1021", "the response has expired".into());
                    }
                }
            }
            if now < this {
                add(&mut found, depth, "RFC 6960", "the response is from the future".into());
            }
        }
        if !answered && (require_all || depth == 0) {
            add(&mut found, depth, "V2G20-2416", "no OCSP status for the certificate".into());
        }
    }
    found
}

/// Seconds since the epoch of a GeneralizedTime. OpenSSL prints it as `Oct 16 20:00:00 2026 GMT`
/// and has no conversion, so that text is read back as an `Asn1Time`.
fn to_unix(t: &openssl::asn1::Asn1GeneralizedTimeRef) -> i64 {
    let text = t.to_string();
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let parts: Vec<&str> = text.split_whitespace().collect();
    let month = months.iter().position(|m| *m == parts[0]).expect("month") + 1;
    let day: u32 = parts[1].parse().expect("day");
    let clock: Vec<&str> = parts[2].split(':').collect();
    let asn1 = format!("{}{month:02}{day:02}{}{}{}Z", parts[3], clock[0], clock[1], clock[2]);
    let time = openssl::asn1::Asn1Time::from_str_x509(&asn1).expect("time");
    let diff = openssl::asn1::Asn1Time::from_unix(0).expect("epoch").diff(&time).expect("diff");
    i64::from(diff.days) * 86_400 + i64::from(diff.secs)
}
