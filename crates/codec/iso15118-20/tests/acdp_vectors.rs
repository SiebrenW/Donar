//! The ACDP messages byte for byte against EXIficient (`cargo xtask vectors`), in both directions.

use exi::Arena;
use iso15118_20::acdp::*;
use iso15118_20::common_types::{MessageHeader, Processing, RationalNumber, ResponseCode};

fn vector(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../../data/vectors/{name}.hex"));
    let hex = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing {path:?}; run `cargo xtask setup && cargo xtask vectors`"));
    (0..hex.trim().len() / 2).map(|i| u8::from_str_radix(&hex.trim()[2 * i..2 * i + 2], 16).unwrap()).collect()
}

fn hdr() -> MessageHeader<'static> {
    MessageHeader::new([0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18], 1_700_000_000)
}

fn check(name: &str, msg: Message<'_>, enc: impl FnOnce(&mut [u8]) -> Result<usize, exi::EncodeError>) {
    let want = vector(name);
    let mut out = [0u8; 512];
    let n = enc(&mut out).unwrap_or_else(|e| panic!("{name}: encode failed: {e:?}"));
    assert_eq!(&out[..n], &want[..], "{name}: encode differs from EXIficient");
    let mut mem = [0u8; 2048];
    let arena = Arena::new(&mut mem);
    assert_eq!(decode(&want, &arena).unwrap_or_else(|e| panic!("{name}: {e:?}")), msg, "{name}: decode");
}

#[test]
fn vehicle_positioning() {
    let req = AcdpVehiclePositioningReq { header: hdr(), ev_mobility_status: false, ev_positioning_support: true };
    check("acdp_vehicle_positioning_req", Message::AcdpVehiclePositioningReq(req), |o| req.encode(o));
    let req = AcdpVehiclePositioningReq { ev_mobility_status: true, ..req };
    check("acdp_vehicle_positioning_req_immobilized", Message::AcdpVehiclePositioningReq(req), |o| req.encode(o));
    let res = AcdpVehiclePositioningRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        evse_processing: Processing::Ongoing,
        evse_positioning_support: true,
        ev_relative_x_deviation: -120,
        ev_relative_y_deviation: 85,
        contact_window_xc: 150,
        contact_window_yc: 100,
        ev_in_charge_position: false,
    };
    check("acdp_vehicle_positioning_res", Message::AcdpVehiclePositioningRes(res), |o| res.encode(o));
    let res = AcdpVehiclePositioningRes {
        evse_processing: Processing::Finished,
        evse_positioning_support: false,
        ev_relative_x_deviation: 0,
        ev_relative_y_deviation: 0,
        contact_window_xc: 0,
        contact_window_yc: 0,
        ev_in_charge_position: true,
        ..res
    };
    check("acdp_vehicle_positioning_res_finished", Message::AcdpVehiclePositioningRes(res), |o| res.encode(o));
}

/// The disconnect messages are the connect messages under other element names (the schema gives them the connect types).
#[test]
fn connect_and_disconnect() {
    let req = AcdpConnectReq { header: hdr(), ev_electrical_charging_device_status: ElectricalChargingDeviceStatus::StateB };
    check("acdp_connect_req", Message::AcdpConnectReq(req), |o| req.encode(o));
    let res = AcdpConnectRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        evse_processing: Processing::Ongoing,
        evse_electrical_charging_device_status: ElectricalChargingDeviceStatus::StateB,
        evse_mechanical_charging_device_status: MechanicalChargingDeviceStatus::Moving,
    };
    check("acdp_connect_res", Message::AcdpConnectRes(res), |o| res.encode(o));
    let req = AcdpConnectReq { ev_electrical_charging_device_status: ElectricalChargingDeviceStatus::StateA, ..req };
    check("acdp_disconnect_req", Message::AcdpDisconnectReq(req), |o| req.encode_as_acdp_disconnect_req(o));
    let res = AcdpConnectRes {
        evse_processing: Processing::Finished,
        evse_electrical_charging_device_status: ElectricalChargingDeviceStatus::StateA,
        evse_mechanical_charging_device_status: MechanicalChargingDeviceStatus::Home,
        ..res
    };
    check("acdp_disconnect_res", Message::AcdpDisconnectRes(res), |o| res.encode_as_acdp_disconnect_res(o));
}

#[test]
fn system_status() {
    let status = EvTechnicalStatus {
        ev_ready_to_charge: false,
        ev_immobilization_request: true,
        ev_immobilized: Some(true),
        evwlan_strength: Some(RationalNumber { exponent: 0, value: -66 }),
        evcp_status: Some(CpStatus::StateC),
        evsoc: Some(74),
        ev_error_code: Some(ErrorCode::FailedRess),
        ev_timeout: Some(false),
    };
    let req = AcdpSystemStatusReq { header: hdr(), ev_technical_status: status };
    check("acdp_system_status_req", Message::AcdpSystemStatusReq(req), |o| req.encode(o));
    let minimal = EvTechnicalStatus {
        ev_ready_to_charge: true,
        ev_immobilization_request: false,
        ev_immobilized: None,
        evwlan_strength: None,
        evcp_status: None,
        evsoc: None,
        ev_error_code: None,
        ev_timeout: None,
    };
    let req = AcdpSystemStatusReq { ev_technical_status: minimal, ..req };
    check("acdp_system_status_req_min", Message::AcdpSystemStatusReq(req), |o| req.encode(o));
    let res = AcdpSystemStatusRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        evse_mechanical_charging_device_status: MechanicalChargingDeviceStatus::EndPosition,
        evse_ready_to_charge: true,
        evse_isolation_status: IsolationStatus::Safe,
        evse_disabled: false,
        evse_utility_interrupt_event: false,
        evse_emergency_shutdown: false,
        evse_malfunction: false,
        ev_in_charge_position: true,
        ev_association_status: true,
    };
    check("acdp_system_status_res", Message::AcdpSystemStatusRes(res), |o| res.encode(o));
}
