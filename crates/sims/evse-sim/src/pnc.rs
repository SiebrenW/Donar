//! The charger's side of the Plug & Charge authorization (ISO 15118-20 8.3.4.3.3, V2G20-2564 and
//! 2565): the challenge, the contract chain and the signature of the `AuthorizationReq`.

use evcc_linux::certificates::{Peer, check_chain};
use evcc_linux::xmlsig;
use iso15118_20::common::PnCAReqAuthorizationMode;
use iso15118_20::common_types::{MessageHeader, ResponseCode};
use iso15118_20::signature::encode_fragment;
use openssl::x509::X509;

use crate::config::Pnc;

/// What the checks of a PnC `AuthorizationReq` found.
pub fn verify(policy: &Pnc, challenge: [u8; 16], header: &MessageHeader<'_>, mode: &PnCAReqAuthorizationMode<'_>) -> ResponseCode {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let chain = &mode.contract_certificate_chain;
    let mut ders: Vec<Vec<u8>> = vec![chain.certificate.to_vec()];
    ders.extend(chain.sub_certificates.certificate.iter().map(|c| c.to_vec()));
    let Ok(leaf) = X509::from_der(chain.certificate) else { return ResponseCode::WarningCertificateValidationError };
    // The chain, against the trust anchors when there are any.
    let mut expires_soon = false;
    if let Some(trust) = &policy.trust_pem {
        let Ok(roots) = X509::stack_from_pem(trust.as_bytes()) else { return ResponseCode::WarningCertificateValidationError };
        let last = X509::from_der(ders.last().expect("one certificate")).expect("parsed above or a sub-CA");
        let Some(root) = roots.iter().find(|r| r.subject_name().to_der().ok() == last.issuer_name().to_der().ok()) else {
            return ResponseCode::WarningCertificateValidationError;
        };
        ders.push(root.to_der().expect("DER"));
        let found = check_chain(&ders, Peer::Contract, now);
        // Path validation always; the profiles when asked for.
        let relevant = found.iter().filter(|v| policy.check_profile || matches!(v.requirement, "V2G20-1001" | "V2G20-1004" | "RFC 5280" | "X.509"));
        // The first finding decides the response code.
        if let Some(v) = relevant.into_iter().next() {
            return match v.message.as_str() {
                "expired" => ResponseCode::WarningCertificateExpired,
                "not valid yet" => ResponseCode::WarningCertificateNotYetValid,
                _ => ResponseCode::WarningCertificateValidationError,
            };
        }
        // V2G20-2218: valid, but not for much longer.
        expires_soon = openssl::asn1::Asn1Time::from_unix(now + 14 * 86_400).is_ok_and(|soon| leaf.not_after() < soon);
    }
    // V2G20-2565: the challenge of this session.
    if mode.gen_challenge != challenge {
        return ResponseCode::WarningChallengeInvalid;
    }
    // V2G20-1063: signed with the contract key; V2G20-461 for a signature that does not hold.
    let (Some(signature), Ok(key)) = (&header.signature, leaf.public_key()) else { return ResponseCode::FailedSignatureError };
    let mut fragment = vec![0u8; 1 << 15];
    let Ok(length) = encode_fragment(&mut fragment, iso15118_20::signature::fragment::PNC_A_REQ_AUTHORIZATION_MODE, |e| mode.encode_content(e)) else {
        return ResponseCode::FailedSignatureError;
    };
    let uri = format!("#{}", mode.id);
    if xmlsig::verify(&key, signature, &[(uri.as_str(), &fragment[..length])]).is_err() {
        return ResponseCode::FailedSignatureError;
    }
    if expires_soon { ResponseCode::OkCertificateExpiresSoon } else { ResponseCode::Ok }
}
