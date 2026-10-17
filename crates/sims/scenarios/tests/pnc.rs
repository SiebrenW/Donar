//! Plug & Charge authorization end to end on localhost: the vehicle signs the `AuthorizationReq`
//! with its contract key, the charger checks challenge, chain and signature.

mod common;

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use evcc_core::ev::Contract;
use evcc_core::session::{Config, EndReason};
use evcc_linux::Outcome;
use evcc_linux::certificates::Peer;
use evcc_linux::pnc::PemContract;
use evcc_sim::{Behaviour, Limits};
use evse_sim::State as EvseState;
use iso15118_20::common::Authorization;
use iso15118_20::common_types::ResponseCode;
use scenarios::pki::Curve;
use scenarios::profile_pki::{Flaw, profile_pki};

fn fast() -> Config {
    Config { poll_interval_ms: 10, charge_loop_interval_ms: 10, allow_unsecured: true, prefer_pnc: true, ..Config::DC_20 }
}

struct Setup {
    contract: PemContract,
    root_pem: String,
}

fn setup(curve: Curve, flaw: Flaw) -> Setup {
    let (root, identity) = profile_pki(curve, Peer::Contract, flaw);
    Setup { contract: PemContract::new(&identity.chain, &identity.key, None).unwrap(), root_pem: String::from_utf8(root.pem).unwrap() }
}

fn charger(trust: Option<&str>, check_profile: bool, offered: &[Authorization]) -> evse_sim::Config {
    evse_sim::Config {
        offered_authorization: offered.to_vec(),
        pnc: evse_sim::Pnc { trust_pem: trust.map(str::to_string), check_profile },
        ..Default::default()
    }
}

fn start(config: evse_sim::Config) -> (std::net::SocketAddr, thread::JoinHandle<std::io::Result<EvseState>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    (addr, thread::spawn(move || evse_sim::serve_one(&listener, config)))
}

fn behaviour() -> Behaviour {
    Behaviour { connector: evcc_core::ev::DcConnector::Core, stop_after: Some(3), renegotiate_after: None, renegotiate_schedule_after: None, stop_when_rescheduled: false, pause_after: None }
}

fn drive(addr: std::net::SocketAddr, config: Config, contract: Option<Arc<dyn Contract>>) -> (Outcome, common::Recording) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let recording = common::Recording::default();
    let outcome = evcc_sim::run_car_live(&mut stream, false, config, behaviour(), recording.clone(), None, Limits::default(), contract).unwrap();
    (outcome, recording)
}

#[test]
fn a_contract_authorizes_the_session() {
    for curve in [Curve::P521, Curve::Ed448] {
        let s = setup(curve, Flaw::None);
        let (addr, server) = start(charger(Some(&s.root_pem), true, &[Authorization::Eim, Authorization::PnC]));
        let (outcome, log) = drive(addr, fast(), Some(Arc::new(s.contract)));
        assert_eq!(outcome, Outcome::Ended(EndReason::Completed), "{curve:?}");
        assert_eq!(log.count("AuthorizationSelected { service: PnC }"), 1);
        assert_eq!(log.count("AuthorizationSelected { service: Eim }"), 0);
        assert_eq!(server.join().unwrap().unwrap(), EvseState::Stopped);
    }
}

#[test]
fn without_pnc_on_offer_or_a_contract_the_vehicle_authorizes_with_eim() {
    let s = setup(Curve::P521, Flaw::None);
    let (addr, server) = start(charger(Some(&s.root_pem), true, &[Authorization::Eim]));
    let (outcome, log) = drive(addr, fast(), Some(Arc::new(s.contract)));
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(log.count("AuthorizationSelected { service: Eim }"), 1);
    server.join().unwrap().unwrap();

    let (addr, server) = start(charger(None, false, &[Authorization::Eim, Authorization::PnC]));
    let (outcome, log) = drive(addr, fast(), None);
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(log.count("AuthorizationSelected { service: Eim }"), 1);
    server.join().unwrap().unwrap();
}

/// A contract whose signatures are wrong.
struct Forger(PemContract);

impl Contract for Forger {
    fn certificate(&self) -> &[u8] {
        self.0.certificate()
    }
    fn sub_certificate(&self, index: usize) -> Option<&[u8]> {
        self.0.sub_certificate(index)
    }
    fn signature_algorithm(&self) -> &str {
        self.0.signature_algorithm()
    }
    fn digest_algorithm(&self) -> &str {
        self.0.digest_algorithm()
    }
    fn digest(&self, data: &[u8], out: &mut [u8]) -> Option<usize> {
        self.0.digest(data, out)
    }
    fn sign(&self, data: &[u8], out: &mut [u8]) -> Option<usize> {
        let n = self.0.sign(data, out)?;
        out[0] ^= 0x55;
        Some(n)
    }
}

#[test]
fn a_signature_that_does_not_hold_ends_the_session() {
    // V2G20-461: FAILED_SignatureError, which no fallback follows.
    let s = setup(Curve::P521, Flaw::None);
    let (addr, server) = start(charger(Some(&s.root_pem), true, &[Authorization::Eim, Authorization::PnC]));
    let (outcome, _) = drive(addr, fast(), Some(Arc::new(Forger(s.contract))));
    assert_eq!(outcome, Outcome::Ended(EndReason::Rejected(ResponseCode::FailedSignatureError)));
    drop(server);
}

#[test]
fn a_contract_the_charger_cannot_accept_falls_back_to_eim() {
    // V2G20-2212 and 2215 by the charger, V2G20-1583 by the vehicle.
    let cases = [
        (Flaw::Expired, ResponseCode::WarningCertificateExpired, true),
        (Flaw::KeyIdMethod1, ResponseCode::WarningCertificateValidationError, true),
    ];
    for (flaw, code, check_profile) in cases {
        let s = setup(Curve::P521, flaw);
        let (addr, server) = start(charger(Some(&s.root_pem), check_profile, &[Authorization::Eim, Authorization::PnC]));
        let (outcome, log) = drive(addr, fast(), Some(Arc::new(s.contract)));
        assert_eq!(outcome, Outcome::Ended(EndReason::Completed), "{flaw:?}");
        assert_eq!(log.count("AuthorizationSelected { service: PnC }"), 1);
        assert_eq!(log.count("AuthorizationSelected { service: Eim }"), 1);
        assert!(log.count(&format!("{code:?}")) >= 1, "{flaw:?}: {:?}", log.0.borrow());
        server.join().unwrap().unwrap();
        // Without the fallback the session ends with the code.
        let s = setup(Curve::P521, flaw);
        let (addr, server) = start(charger(Some(&s.root_pem), check_profile, &[Authorization::Eim, Authorization::PnC]));
        let (outcome, _) = drive(addr, Config { pnc_fallback_to_eim: false, ..fast() }, Some(Arc::new(s.contract)));
        assert_eq!(outcome, Outcome::Ended(EndReason::Rejected(code)), "{flaw:?}");
        drop(server);
    }
}

#[test]
fn a_chain_from_an_unknown_root_is_not_valid_and_the_profile_check_is_optional() {
    let s = setup(Curve::P521, Flaw::None);
    let other = setup(Curve::P521, Flaw::None);
    let (addr, server) = start(charger(Some(&other.root_pem), true, &[Authorization::Eim, Authorization::PnC]));
    let (outcome, _) = drive(addr, Config { pnc_fallback_to_eim: false, ..fast() }, Some(Arc::new(s.contract)));
    assert_eq!(outcome, Outcome::Ended(EndReason::Rejected(ResponseCode::WarningCertificateValidationError)));
    drop(server);

    // Method 1 key identifiers break the profile but not the path: accepted without the profile check.
    let s = setup(Curve::P521, Flaw::KeyIdMethod1);
    let (addr, server) = start(charger(Some(&s.root_pem), false, &[Authorization::Eim, Authorization::PnC]));
    let (outcome, log) = drive(addr, Config { pnc_fallback_to_eim: false, ..fast() }, Some(Arc::new(s.contract)));
    assert_eq!(outcome, Outcome::Ended(EndReason::Completed));
    assert_eq!(log.count("AuthorizationSelected { service: Eim }"), 0);
    server.join().unwrap().unwrap();
}
