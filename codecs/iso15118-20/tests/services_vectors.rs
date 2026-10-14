//! Checks the service discovery / detail / selection codecs against EXIficient vectors.

use exi::Arena;
use iso15118_20::common::*;
use iso15118_20::services::*;

fn vector(name: &str) -> std::vec::Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../vectors/{name}.hex"));
    let hex = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {path:?}; run `cargo xtask setup && cargo xtask vectors`"));
    (0..hex.trim().len() / 2)
        .map(|i| u8::from_str_radix(&hex.trim()[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

const H: MessageHeader = MessageHeader { session_id: [0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18], timestamp: 1_700_000_000 };

/// Encodes `msg` with `enc`, compares with the vector, and decodes the vector back.
fn check<'m>(name: &str, msg: Message<'m>, enc: impl FnOnce(&mut [u8]) -> Result<usize, exi::EncodeError>) {
    let want = vector(name);
    let mut out = [0u8; 1024];
    let n = enc(&mut out).unwrap();
    assert_eq!(&out[..n], &want[..], "{name}: encode differs from EXIficient");
    let mut mem = [0u8; 4096];
    let arena = Arena::new(&mut mem);
    assert_eq!(decode(&want, &arena).unwrap(), msg, "{name}: decode");
}

#[test]
fn discovery_req() {
    let m = ServiceDiscoveryReq { header: H, supported_service_ids: None };
    check("service_discovery_req", Message::ServiceDiscoveryReq(m), |o| encode_service_discovery_req(&m, o));
    let m = ServiceDiscoveryReq { header: H, supported_service_ids: Some(&[2, 4]) };
    check("service_discovery_req_ids", Message::ServiceDiscoveryReq(m), |o| encode_service_discovery_req(&m, o));
}

#[test]
fn discovery_res() {
    let dc = [Service { service_id: service_id::DC, free: false }];
    let m = ServiceDiscoveryRes {
        header: H,
        response_code: ResponseCode::Ok,
        service_renegotiation_supported: false,
        energy_transfer_services: &dc,
        vas: None,
    };
    check("service_discovery_res_dc", Message::ServiceDiscoveryRes(m), |o| encode_service_discovery_res(&m, o));
    let two = [Service { service_id: 2, free: false }, Service { service_id: 4, free: true }];
    let vas = [Service { service_id: 1000, free: true }];
    let m = ServiceDiscoveryRes {
        header: H,
        response_code: ResponseCode::Ok,
        service_renegotiation_supported: true,
        energy_transfer_services: &two,
        vas: Some(&vas),
    };
    check("service_discovery_res_two", Message::ServiceDiscoveryRes(m), |o| encode_service_discovery_res(&m, o));
}

/// Lists at their `maxOccurs`: the end-of-list code shrinks to one bit.
#[test]
fn lists_at_their_maximum() {
    let ids: std::vec::Vec<u16> = (1..=16).collect();
    let m = ServiceDiscoveryReq { header: H, supported_service_ids: Some(&ids) };
    check("service_discovery_req_max", Message::ServiceDiscoveryReq(m), |o| encode_service_discovery_req(&m, o));
    let list: std::vec::Vec<Service> = (1..=8).map(|i| Service { service_id: i, free: i % 2 == 0 }).collect();
    let m = ServiceDiscoveryRes {
        header: H,
        response_code: ResponseCode::Ok,
        service_renegotiation_supported: false,
        energy_transfer_services: &list,
        vas: Some(&list),
    };
    check("service_discovery_res_max", Message::ServiceDiscoveryRes(m), |o| encode_service_discovery_res(&m, o));
}

#[test]
fn detail_req() {
    let m = ServiceDetailReq { header: H, service_id: 2 };
    check("service_detail_req", Message::ServiceDetailReq(m), |o| encode_service_detail_req(&m, o));
}

fn int(name: &'static str, v: i32) -> Parameter<'static> {
    Parameter { name, value: ParameterValue::Int(v) }
}

#[test]
fn detail_res() {
    let params = [int("Connector", 1), int("ControlMode", 1), int("MobilityNeedsMode", 1), int("Pricing", 0)];
    let sets = [ParameterSet { id: 1, parameters: &params }];
    let m = ServiceDetailRes { header: H, response_code: ResponseCode::Ok, service_id: 2, parameter_sets: &sets };
    check("service_detail_res_dc", Message::ServiceDetailRes(m), |o| encode_service_detail_res(&m, o));

    let params = [
        Parameter { name: "b", value: ParameterValue::Bool(true) },
        Parameter { name: "y", value: ParameterValue::Byte(-5) },
        Parameter { name: "s", value: ParameterValue::Short(300) },
        Parameter { name: "r", value: ParameterValue::Rational { exponent: 3, value: -20 } },
        Parameter { name: "f", value: ParameterValue::Text("abc") },
    ];
    let sets = [ParameterSet { id: 7, parameters: &params }];
    let m = ServiceDetailRes { header: H, response_code: ResponseCode::Ok, service_id: 2, parameter_sets: &sets };
    check("service_detail_res_kinds", Message::ServiceDetailRes(m), |o| encode_service_detail_res(&m, o));
}

/// Repeated names (local hits) and a text equal to a name (global hit, then local hit).
#[test]
fn detail_res_string_table_hits() {
    let text = |name, v| Parameter { name, value: ParameterValue::Text(v) };
    let one = [int("Connector", 1), int("Pricing", 0), text("Note", "Pricing")];
    let two = [int("Connector", 2), int("Pricing", 1), text("Note", "Pricing"), text("Other", "Note")];
    let sets = [ParameterSet { id: 1, parameters: &one }, ParameterSet { id: 2, parameters: &two }];
    let m = ServiceDetailRes { header: H, response_code: ResponseCode::Ok, service_id: 2, parameter_sets: &sets };
    check("service_detail_res_hits", Message::ServiceDetailRes(m), |o| encode_service_detail_res(&m, o));
}

#[test]
fn selection() {
    let sel = SelectedService { service_id: 2, parameter_set_id: 1 };
    let m = ServiceSelectionReq { header: H, energy_transfer_service: sel, vas: None };
    check("service_selection_req", Message::ServiceSelectionReq(m), |o| encode_service_selection_req(&m, o));
    let vas = [SelectedService { service_id: 1000, parameter_set_id: 3 }];
    let m = ServiceSelectionReq { header: H, energy_transfer_service: sel, vas: Some(&vas) };
    check("service_selection_req_vas", Message::ServiceSelectionReq(m), |o| encode_service_selection_req(&m, o));
    for (name, code) in [("service_selection_res", ResponseCode::Ok), ("service_selection_res_failed", ResponseCode::FailedServiceSelectionInvalid)] {
        let m = ServiceSelectionRes { header: H, response_code: code };
        check(name, Message::ServiceSelectionRes(m), |o| encode_service_selection_res(&m, o));
    }
}

#[test]
fn truncated_input_never_panics() {
    for name in ["service_discovery_res_two", "service_detail_res_kinds", "service_selection_req_vas", "service_discovery_req_ids"] {
        let full = vector(name);
        for cut in 0..full.len() {
            let mut mem = [0u8; 4096];
            let arena = Arena::new(&mut mem);
            assert!(decode(&full[..cut], &arena).is_err(), "{name} cut at {cut} decoded");
        }
    }
}
