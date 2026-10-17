//! The charger's side of the Plug & Charge authorization (ISO 15118-20 8.3.4.3.3, V2G20-2564 and
//! 2565): the challenge, the contract chain and the signature of the `AuthorizationReq`.

use evcc_linux::certificates::{Peer, check_chain};
use evcc_linux::xmlsig;
use iso15118_20::common::PnCAReqAuthorizationMode;
use iso15118_20::common_types::{MessageHeader, ResponseCode};
use iso15118_20::signature::encode_fragment;
use openssl::x509::X509;

use crate::config::Pnc;

/// Why a contract chain is not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainFault {
    Expired,
    NotYetValid,
    Invalid,
}

/// The contract certificate of `ders` (leaf first, root not sent) checked against the trust anchors
/// of the policy, if there are any; and whether it expires within two weeks (V2G20-2218).
pub fn check_contract_chain(policy: &Pnc, ders: &[Vec<u8>]) -> Result<(X509, bool), ChainFault> {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let mut ders = ders.to_vec();
    let leaf = X509::from_der(ders.first().ok_or(ChainFault::Invalid)?).map_err(|_| ChainFault::Invalid)?;
    let mut expires_soon = false;
    if let Some(trust) = &policy.trust_pem {
        let roots = X509::stack_from_pem(trust.as_bytes()).map_err(|_| ChainFault::Invalid)?;
        let last = X509::from_der(ders.last().expect("one certificate")).map_err(|_| ChainFault::Invalid)?;
        let root = roots.iter().find(|r| r.subject_name().to_der().ok() == last.issuer_name().to_der().ok()).ok_or(ChainFault::Invalid)?;
        ders.push(root.to_der().expect("DER"));
        let found = check_chain(&ders, Peer::Contract, now);
        // Path validation always; the profiles when asked for.
        let relevant = found.iter().filter(|v| policy.check_profile || matches!(v.requirement, "V2G20-1001" | "V2G20-1004" | "RFC 5280" | "X.509"));
        // The first finding decides.
        if let Some(v) = relevant.into_iter().next() {
            return Err(match v.message.as_str() {
                "expired" => ChainFault::Expired,
                "not valid yet" => ChainFault::NotYetValid,
                _ => ChainFault::Invalid,
            });
        }
        expires_soon = openssl::asn1::Asn1Time::from_unix(now + 14 * 86_400).is_ok_and(|soon| leaf.not_after() < soon);
    }
    Ok((leaf, expires_soon))
}

/// What the checks of a PnC `AuthorizationReq` of ISO 15118-20 found.
pub fn verify(policy: &Pnc, challenge: [u8; 16], header: &MessageHeader<'_>, mode: &PnCAReqAuthorizationMode<'_>) -> ResponseCode {
    let chain = &mode.contract_certificate_chain;
    let mut ders: Vec<Vec<u8>> = vec![chain.certificate.to_vec()];
    ders.extend(chain.sub_certificates.certificate.iter().map(|c| c.to_vec()));
    // The chain, against the trust anchors when there are any.
    let (leaf, expires_soon) = match check_contract_chain(policy, &ders) {
        Ok(ok) => ok,
        Err(ChainFault::Expired) => return ResponseCode::WarningCertificateExpired,
        Err(ChainFault::NotYetValid) => return ResponseCode::WarningCertificateNotYetValid,
        Err(ChainFault::Invalid) => return ResponseCode::WarningCertificateValidationError,
    };
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

/// The checks of an ISO 15118-2 `MeteringReceiptReq`: the signature in the header over the element
/// with the contract key, and the reading being the one the charger sent.
pub fn verify_iso2_receipt(contract: &X509, signature: Option<&iso15118_2::signature::Signature<'_>>, request: &iso15118_2::body::MeteringReceiptReq<'_>, sent_reading: Option<u64>) -> iso15118_2::types::ResponseCode {
    use iso15118_2::types::ResponseCode as C;
    let (Some(signature), Ok(key), Some(id)) = (signature, contract.public_key(), request.id) else { return C::FailedMeteringSignatureNotValid };
    let mut fragment = vec![0u8; 1 << 12];
    let Ok(length) = iso15118_2::signature::encode_fragment(&mut fragment, iso15118_2::signature::fragment::METERING_RECEIPT_REQ, |e| request.encode_content(e)) else {
        return C::FailedMeteringSignatureNotValid;
    };
    let uri = format!("#{id}");
    if xmlsig::verify(&key, signature, &[(uri.as_str(), &fragment[..length])]).is_err() {
        return C::FailedMeteringSignatureNotValid;
    }
    // The vehicle's copy of the meter info has to be the charger's.
    if request.meter_info.meter_reading != sent_reading {
        return C::FailedMeteringSignatureNotValid;
    }
    C::Ok
}

/// The checks of an ISO 15118-2 `AuthorizationReq` for a contract: the challenge of this session
/// (V2G2-... `FAILED_ChallengeInvalid`) and the signature in the header over the element, made with the
/// key of the contract certificate seen in `PaymentDetailsReq`.
pub fn verify_iso2(challenge: [u8; 16], contract: &X509, signature: Option<&iso15118_2::signature::Signature<'_>>, request: &iso15118_2::body::AuthorizationReq<'_>) -> iso15118_2::types::ResponseCode {
    use iso15118_2::types::ResponseCode as C;
    if request.gen_challenge != Some(challenge) {
        return C::FailedChallengeInvalid;
    }
    let (Some(signature), Ok(key), Some(id)) = (signature, contract.public_key(), request.id) else { return C::FailedSignatureError };
    let mut fragment = vec![0u8; 1 << 12];
    let Ok(length) = iso15118_2::signature::encode_fragment(&mut fragment, iso15118_2::signature::fragment::AUTHORIZATION_REQ, |e| request.encode_content(e)) else {
        return C::FailedSignatureError;
    };
    let uri = format!("#{id}");
    if xmlsig::verify(&key, signature, &[(uri.as_str(), &fragment[..length])]).is_err() {
        return C::FailedSignatureError;
    }
    C::Ok
}
