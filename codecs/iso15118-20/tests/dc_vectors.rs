//! Checks the DC codec against EXIficient vectors.

use iso15118_20::common::{MessageHeader, RationalNumber, ResponseCode};
use iso15118_20::dc::*;

fn vector(name: &str) -> std::vec::Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../vectors/{name}.hex"));
    let hex = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {path:?}; run `cargo xtask setup && cargo xtask vectors`"));
    (0..hex.trim().len() / 2)
        .map(|i| u8::from_str_radix(&hex.trim()[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

const H: MessageHeader = MessageHeader { session_id: [0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18], timestamp: 1_700_000_000 };

fn rn(exponent: i8, value: i16) -> RationalNumber {
    RationalNumber { exponent, value }
}

fn req(target_soc: Option<u8>) -> ChargeParameterDiscoveryReq {
    ChargeParameterDiscoveryReq {
        header: H,
        maximum_charge_power: rn(3, 120),
        minimum_charge_power: rn(0, 100),
        maximum_charge_current: rn(0, 200),
        minimum_charge_current: rn(0, 1),
        maximum_voltage: rn(0, 450),
        minimum_voltage: rn(0, 200),
        target_soc,
    }
}

fn res(ramp: Option<RationalNumber>) -> ChargeParameterDiscoveryRes {
    ChargeParameterDiscoveryRes {
        header: H,
        response_code: ResponseCode::Ok,
        maximum_charge_power: rn(3, 150),
        minimum_charge_power: rn(0, 1000),
        maximum_charge_current: rn(0, 300),
        minimum_charge_current: rn(0, -2),
        maximum_voltage: rn(0, 920),
        minimum_voltage: rn(0, 150),
        power_ramp_limitation: ramp,
    }
}

#[test]
fn charge_parameter_discovery_req() {
    for (name, soc) in [("dc_cpd_req", None), ("dc_cpd_req_soc", Some(80))] {
        let want = vector(name);
        let msg = req(soc);
        let mut out = [0u8; 256];
        let n = encode_charge_parameter_discovery_req(&msg, &mut out).unwrap();
        assert_eq!(&out[..n], &want[..], "{name}: encode differs from EXIficient");
        assert_eq!(decode(&want).unwrap(), Message::ChargeParameterDiscoveryReq(msg), "{name}: decode");
    }
}

#[test]
fn charge_parameter_discovery_res() {
    for (name, ramp) in [("dc_cpd_res", None), ("dc_cpd_res_ramp", Some(rn(0, 50)))] {
        let want = vector(name);
        let msg = res(ramp);
        let mut out = [0u8; 256];
        let n = encode_charge_parameter_discovery_res(&msg, &mut out).unwrap();
        assert_eq!(&out[..n], &want[..], "{name}: encode differs from EXIficient");
        assert_eq!(decode(&want).unwrap(), Message::ChargeParameterDiscoveryRes(msg), "{name}: decode");
    }
}

#[test]
fn target_soc_above_100_is_refused() {
    let mut out = [0u8; 256];
    assert!(encode_charge_parameter_discovery_req(&req(Some(101)), &mut out).is_err());
}

#[test]
fn truncated_input_never_panics() {
    for name in ["dc_cpd_req_soc", "dc_cpd_res_ramp"] {
        let full = vector(name);
        for cut in 0..full.len() {
            assert!(decode(&full[..cut]).is_err(), "{name} cut at {cut} decoded");
        }
    }
}
