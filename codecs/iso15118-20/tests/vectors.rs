//! Checks the generated codec byte for byte against reference vectors made by EXIficient
//! (`cargo xtask vectors`), in both directions.

use exi::Arena;
use iso15118_20::common::{self, *};
use iso15118_20::common_types::{MessageHeader, Processing, RationalNumber, ResponseCode};
use iso15118_20::dc;

fn vector(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../vectors/{name}.hex"));
    let hex = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {path:?}; run `cargo xtask setup && cargo xtask vectors`"));
    (0..hex.trim().len() / 2).map(|i| u8::from_str_radix(&hex.trim()[2 * i..2 * i + 2], 16).unwrap()).collect()
}

const TS: u64 = 1_700_000_000;
const SESSION: [u8; 8] = [0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18];

fn hdr() -> MessageHeader {
    MessageHeader { session_id: SESSION, time_stamp: TS }
}

fn rn(exponent: i8, value: i16) -> RationalNumber {
    RationalNumber { exponent, value }
}

/// Encodes with `enc`, compares with the vector, decodes the vector and compares with `msg`.
fn check(name: &str, msg: common::Message<'_>, enc: impl FnOnce(&mut [u8]) -> Result<usize, exi::EncodeError>) {
    let want = vector(name);
    let mut out = [0u8; 2048];
    let n = enc(&mut out).unwrap_or_else(|e| panic!("{name}: encode failed: {e:?}"));
    assert_eq!(&out[..n], &want[..], "{name}: encode differs from EXIficient");
    let mut mem = [0u8; 8192];
    let arena = Arena::new(&mut mem);
    assert_eq!(common::decode(&want, &arena).unwrap_or_else(|e| panic!("{name}: {e:?}")), msg, "{name}: decode");
}

fn check_dc(name: &str, msg: dc::Message<'_>, enc: impl FnOnce(&mut [u8]) -> Result<usize, exi::EncodeError>) {
    let want = vector(name);
    let mut out = [0u8; 2048];
    let n = enc(&mut out).unwrap_or_else(|e| panic!("{name}: encode failed: {e:?}"));
    assert_eq!(&out[..n], &want[..], "{name}: encode differs from EXIficient");
    let mut mem = [0u8; 8192];
    let arena = Arena::new(&mut mem);
    assert_eq!(dc::decode(&want, &arena).unwrap_or_else(|e| panic!("{name}: {e:?}")), msg, "{name}: decode");
}

// ---------------------------------------------------------------- session setup

#[test]
fn session_setup_req() {
    let m = SessionSetupReq {
        header: MessageHeader { session_id: [0; 8], time_stamp: TS },
        evccid: "WMIV1234567890ABCDEX",
    };
    check("session_setup_req", Message::SessionSetupReq(m), |o| m.encode(o));
}

#[test]
fn session_setup_res() {
    for (name, code, evse) in [
        ("ok", ResponseCode::Ok, "DE*EVSE*0001"),
        ("joined", ResponseCode::OkOldSessionJoined, "DE*EVSE*0001"),
        ("unknown", ResponseCode::FailedUnknownSession, "DE*EVSE*0002"),
        ("wrongcp", ResponseCode::FailedWrongChargeParameter, "X"),
    ] {
        let m = SessionSetupRes {
            header: MessageHeader { session_id: [1, 2, 3, 4, 5, 6, 7, 8], time_stamp: TS },
            response_code: code,
            evseid: evse,
        };
        check(&format!("session_setup_res_{name}"), Message::SessionSetupRes(m), |o| m.encode(o));
    }
}

/// Open question (see PLAN.md): EXIficient's reference for an empty string element uses an
/// event code this codec does not understand yet, so both directions must fail loudly instead
/// of producing a wrong stream.
#[test]
fn empty_string_is_unsupported_not_wrong() {
    let m = SessionSetupReq { header: hdr(), evccid: "" };
    assert!(m.encode(&mut [0u8; 64]).is_err());
    let reference = vector("session_setup_res_empty");
    let mut mem = [0u8; 512];
    let arena = Arena::new(&mut mem);
    assert!(common::decode(&reference, &arena).is_err());
}

#[test]
fn identifier_longer_than_the_schema_allows_is_rejected() {
    let long = "x".repeat(256);
    let m = SessionSetupReq { header: hdr(), evccid: &long };
    assert!(m.encode(&mut [0u8; 1024]).is_err());
}

// ---------------------------------------------------------------- authorization

#[test]
fn authorization_setup() {
    let m = AuthorizationSetupReq { header: hdr() };
    check("authorization_setup_req", Message::AuthorizationSetupReq(m), |o| m.encode(o));
    use Authorization::{Eim, PnC};
    for (name, services, cert) in [
        ("eim", &[Eim][..], false),
        ("eim_cert", &[Eim][..], true),
        ("eim_pnc_listed", &[Eim, PnC][..], false),
        ("pnc_eim_listed", &[PnC, Eim][..], true),
    ] {
        let m = AuthorizationSetupRes {
            header: hdr(),
            response_code: ResponseCode::Ok,
            authorization_services: services,
            certificate_installation_service: cert,
            as_res_authorization_mode: AsResAuthorizationModeChoice::Eim(EimAsResAuthorizationMode {}),
        };
        check(&format!("authorization_setup_res_{name}"), Message::AuthorizationSetupRes(m), |o| m.encode(o));
    }
}

#[test]
fn authorization() {
    let m = AuthorizationReq {
        header: hdr(),
        selected_authorization_service: Authorization::Eim,
        a_req_authorization_mode: AReqAuthorizationModeChoice::Eim(EimAReqAuthorizationMode {}),
    };
    check("authorization_req_eim", Message::AuthorizationReq(m), |o| m.encode(o));
    for (name, code, p) in [
        ("finished", ResponseCode::Ok, Processing::Finished),
        ("ongoing", ResponseCode::Ok, Processing::Ongoing),
        ("waiting", ResponseCode::Ok, Processing::OngoingWaitingForCustomerInteraction),
        ("failed", ResponseCode::WarningEimAuthorizationFailure, Processing::Finished),
    ] {
        let m = AuthorizationRes { header: hdr(), response_code: code, evse_processing: p };
        check(&format!("authorization_res_{name}"), Message::AuthorizationRes(m), |o| m.encode(o));
    }
}

// ---------------------------------------------------------------- services

#[test]
fn service_discovery() {
    let m = ServiceDiscoveryReq { header: hdr(), supported_service_ids: None };
    check("service_discovery_req", Message::ServiceDiscoveryReq(m), |o| m.encode(o));
    let m = ServiceDiscoveryReq { header: hdr(), supported_service_ids: Some(ServiceIdList { service_id: &[2, 4] }) };
    check("service_discovery_req_ids", Message::ServiceDiscoveryReq(m), |o| m.encode(o));
    let ids: Vec<u16> = (1..=16).collect();
    let m = ServiceDiscoveryReq { header: hdr(), supported_service_ids: Some(ServiceIdList { service_id: &ids }) };
    check("service_discovery_req_max", Message::ServiceDiscoveryReq(m), |o| m.encode(o));

    let dc_only = [Service { service_id: 2, free_service: false }];
    let m = ServiceDiscoveryRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        service_renegotiation_supported: false,
        energy_transfer_service_list: ServiceList { service: &dc_only },
        vas_list: None,
    };
    check("service_discovery_res_dc", Message::ServiceDiscoveryRes(m), |o| m.encode(o));

    let two = [Service { service_id: 2, free_service: false }, Service { service_id: 4, free_service: true }];
    let vas = [Service { service_id: 1000, free_service: true }];
    let m = ServiceDiscoveryRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        service_renegotiation_supported: true,
        energy_transfer_service_list: ServiceList { service: &two },
        vas_list: Some(ServiceList { service: &vas }),
    };
    check("service_discovery_res_two", Message::ServiceDiscoveryRes(m), |o| m.encode(o));

    // Lists at their maxOccurs: the end-of-list code shrinks to one bit.
    let eight: Vec<Service> = (1..=8).map(|i| Service { service_id: i, free_service: i % 2 == 0 }).collect();
    let m = ServiceDiscoveryRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        service_renegotiation_supported: false,
        energy_transfer_service_list: ServiceList { service: &eight },
        vas_list: Some(ServiceList { service: &eight }),
    };
    check("service_discovery_res_max", Message::ServiceDiscoveryRes(m), |o| m.encode(o));
}

fn int(name: &'static str, v: i32) -> Parameter<'static> {
    Parameter { name, choice: ParameterChoice::IntValue(v) }
}

#[test]
fn service_detail() {
    let m = ServiceDetailReq { header: hdr(), service_id: 2 };
    check("service_detail_req", Message::ServiceDetailReq(m), |o| m.encode(o));

    let params = [int("Connector", 1), int("ControlMode", 1), int("MobilityNeedsMode", 1), int("Pricing", 0)];
    let sets = [ParameterSet { parameter_set_id: 1, parameter: &params }];
    let m = ServiceDetailRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        service_id: 2,
        service_parameter_list: ServiceParameterList { parameter_set: &sets },
    };
    check("service_detail_res_dc", Message::ServiceDetailRes(m), |o| m.encode(o));

    let params = [
        Parameter { name: "b", choice: ParameterChoice::BoolValue(true) },
        Parameter { name: "y", choice: ParameterChoice::ByteValue(-5) },
        Parameter { name: "s", choice: ParameterChoice::ShortValue(300) },
        Parameter { name: "r", choice: ParameterChoice::RationalNumber(rn(3, -20)) },
        Parameter { name: "f", choice: ParameterChoice::FiniteString("abc") },
    ];
    let sets = [ParameterSet { parameter_set_id: 7, parameter: &params }];
    let m = ServiceDetailRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        service_id: 2,
        service_parameter_list: ServiceParameterList { parameter_set: &sets },
    };
    check("service_detail_res_kinds", Message::ServiceDetailRes(m), |o| m.encode(o));

    // Repeated names (local hits) and a text equal to a name (global hit).
    let text = |name, v| Parameter { name, choice: ParameterChoice::FiniteString(v) };
    let one = [int("Connector", 1), int("Pricing", 0), text("Note", "Pricing")];
    let two = [int("Connector", 2), int("Pricing", 1), text("Note", "Pricing"), text("Other", "Note")];
    let sets = [ParameterSet { parameter_set_id: 1, parameter: &one }, ParameterSet { parameter_set_id: 2, parameter: &two }];
    let m = ServiceDetailRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        service_id: 2,
        service_parameter_list: ServiceParameterList { parameter_set: &sets },
    };
    check("service_detail_res_hits", Message::ServiceDetailRes(m), |o| m.encode(o));
}

#[test]
fn service_selection() {
    let sel = SelectedService { service_id: 2, parameter_set_id: 1 };
    let m = ServiceSelectionReq { header: hdr(), selected_energy_transfer_service: sel, selected_vas_list: None };
    check("service_selection_req", Message::ServiceSelectionReq(m), |o| m.encode(o));
    let vas = [SelectedService { service_id: 1000, parameter_set_id: 3 }];
    let m = ServiceSelectionReq {
        header: hdr(),
        selected_energy_transfer_service: sel,
        selected_vas_list: Some(SelectedServiceList { selected_service: &vas }),
    };
    check("service_selection_req_vas", Message::ServiceSelectionReq(m), |o| m.encode(o));
    for (name, code) in [("service_selection_res", ResponseCode::Ok), ("service_selection_res_failed", ResponseCode::FailedServiceSelectionInvalid)] {
        let m = ServiceSelectionRes { header: hdr(), response_code: code };
        check(name, Message::ServiceSelectionRes(m), |o| m.encode(o));
    }
}

// ---------------------------------------------------------------- schedule exchange

#[test]
fn schedule_exchange_dynamic() {
    let energy = |departure_time, min_soc, target_soc, v2x: Option<(RationalNumber, RationalNumber)>| {
        SeReqControlModeChoice::Dynamic(DynamicSeReqControlMode {
            departure_time,
            minimum_soc: min_soc,
            target_soc,
            ev_target_energy_request: rn(3, 40),
            ev_maximum_energy_request: rn(3, 60),
            ev_minimum_energy_request: rn(0, 0),
            ev_maximum_v2x_energy_request: v2x.map(|v| v.0),
            ev_minimum_v2x_energy_request: v2x.map(|v| v.1),
        })
    };
    let m = ScheduleExchangeReq {
        header: hdr(),
        maximum_supporting_points: 12,
        se_req_control_mode: energy(3600, None, None, None),
    };
    check("schedule_exchange_req_min", Message::ScheduleExchangeReq(m), |o| m.encode(o));
    let m = ScheduleExchangeReq {
        header: hdr(),
        maximum_supporting_points: 1024,
        se_req_control_mode: energy(86400, Some(20), Some(80), Some((rn(3, 10), rn(0, -5)))),
    };
    check("schedule_exchange_req_full", Message::ScheduleExchangeReq(m), |o| m.encode(o));

    let m = ScheduleExchangeRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        evse_processing: Processing::Finished,
        go_to_pause: None,
        se_res_control_mode: SeResControlModeChoice::Dynamic(DynamicSeResControlMode {
            departure_time: None,
            minimum_soc: None,
            target_soc: None,
            schedule: None,
        }),
    };
    check("schedule_exchange_res_min", Message::ScheduleExchangeRes(m), |o| m.encode(o));
    let m = ScheduleExchangeRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        evse_processing: Processing::Ongoing,
        go_to_pause: Some(true),
        se_res_control_mode: SeResControlModeChoice::Dynamic(DynamicSeResControlMode {
            departure_time: Some(7200),
            minimum_soc: Some(10),
            target_soc: Some(90),
            schedule: None,
        }),
    };
    check("schedule_exchange_res_full", Message::ScheduleExchangeRes(m), |o| m.encode(o));
}

// ---------------------------------------------------------------- DC

#[test]
fn dc_charge_parameter_discovery() {
    let mode = |soc| dc::DcCpdReqEnergyTransferMode {
        ev_maximum_charge_power: rn(3, 120),
        ev_minimum_charge_power: rn(0, 100),
        ev_maximum_charge_current: rn(0, 200),
        ev_minimum_charge_current: rn(0, 1),
        ev_maximum_voltage: rn(0, 450),
        ev_minimum_voltage: rn(0, 200),
        target_soc: soc,
    };
    for (name, soc) in [("dc_cpd_req", None), ("dc_cpd_req_soc", Some(80))] {
        let m = dc::DcChargeParameterDiscoveryReq {
            header: hdr(),
            dc_cpd_req_energy_transfer_mode: dc::DcCpdReqEnergyTransferModeChoice::Base(mode(soc)),
        };
        check_dc(name, dc::Message::DcChargeParameterDiscoveryReq(m), |o| m.encode(o));
    }
    let res = |ramp| dc::DcCpdResEnergyTransferMode {
        evse_maximum_charge_power: rn(3, 150),
        evse_minimum_charge_power: rn(0, 1000),
        evse_maximum_charge_current: rn(0, 300),
        evse_minimum_charge_current: rn(0, -2),
        evse_maximum_voltage: rn(0, 920),
        evse_minimum_voltage: rn(0, 150),
        evse_power_ramp_limitation: ramp,
    };
    for (name, ramp) in [("dc_cpd_res", None), ("dc_cpd_res_ramp", Some(rn(0, 50)))] {
        let m = dc::DcChargeParameterDiscoveryRes {
            header: hdr(),
            response_code: ResponseCode::Ok,
            dc_cpd_res_energy_transfer_mode: dc::DcCpdResEnergyTransferModeChoice::Base(res(ramp)),
        };
        check_dc(name, dc::Message::DcChargeParameterDiscoveryRes(m), |o| m.encode(o));
    }
}

fn charge_loop_req() -> dc::DcChargeLoopReq {
    dc::DcChargeLoopReq {
        header: hdr(),
        display_parameters: None,
        meter_info_requested: false,
        ev_present_voltage: rn(0, 400),
        cl_req_control_mode: dc::ClReqControlModeChoice::DynamicDc(dc::DynamicDcClReqControlMode {
            departure_time: None,
            ev_target_energy_request: rn(3, 40),
            ev_maximum_energy_request: rn(3, 60),
            ev_minimum_energy_request: rn(0, 0),
            ev_maximum_charge_power: rn(3, 100),
            ev_minimum_charge_power: rn(0, 100),
            ev_maximum_charge_current: rn(0, 200),
            ev_maximum_voltage: rn(0, 450),
            ev_minimum_voltage: rn(0, 200),
        }),
    }
}

#[test]
fn dc_charge_loop() {
    let m = charge_loop_req();
    check_dc("dc_charge_loop_req", dc::Message::DcChargeLoopReq(m), |o| m.encode(o));
    for (name, status) in [
        ("dc_charge_loop_res", None),
        (
            "dc_charge_loop_res_terminate",
            Some(iso15118_20::common_types::EvseStatus {
                notification_max_delay: 5,
                evse_notification: iso15118_20::common_types::EvseNotification::Terminate,
            }),
        ),
    ] {
        let m = dc::DcChargeLoopRes {
            header: hdr(),
            response_code: ResponseCode::Ok,
            evse_status: status,
            meter_info: None,
            receipt: None,
            evse_present_current: rn(0, 120),
            evse_present_voltage: rn(0, 398),
            evse_power_limit_achieved: false,
            evse_current_limit_achieved: true,
            evse_voltage_limit_achieved: false,
            cl_res_control_mode: dc::ClResControlModeChoice::DynamicDc(dc::DynamicDcClResControlMode {
                departure_time: None,
                minimum_soc: None,
                target_soc: None,
                ack_max_delay: None,
                evse_maximum_charge_power: rn(3, 150),
                evse_minimum_charge_power: rn(0, 100),
                evse_maximum_charge_current: rn(0, 300),
                evse_maximum_voltage: rn(0, 920),
            }),
        };
        check_dc(name, dc::Message::DcChargeLoopRes(m), |o| m.encode(o));
    }
}

#[test]
fn dc_welding_detection() {
    for (name, p) in [("dc_welding_req_ongoing", Processing::Ongoing), ("dc_welding_req_finished", Processing::Finished)] {
        let m = dc::DcWeldingDetectionReq { header: hdr(), ev_processing: p };
        check_dc(name, dc::Message::DcWeldingDetectionReq(m), |o| m.encode(o));
    }
    let m = dc::DcWeldingDetectionRes { header: hdr(), response_code: ResponseCode::Ok, evse_present_voltage: rn(0, 12) };
    check_dc("dc_welding_res", dc::Message::DcWeldingDetectionRes(m), |o| m.encode(o));
}

#[test]
fn power_delivery_and_session_stop() {
    let entries = [PowerScheduleEntry { duration: 3600, power: rn(3, 100), power_l2: None, power_l3: None }];
    for (name, progress) in [("power_delivery_req_start", ChargeProgress::Start), ("power_delivery_req_stop", ChargeProgress::Stop)] {
        let m = PowerDeliveryReq {
            header: hdr(),
            ev_processing: Processing::Finished,
            charge_progress: progress,
            ev_power_profile: Some(EvPowerProfile {
                time_anchor: TS,
                evppt_control_mode: EvpptControlModeChoice::Dynamic(DynamicEvpptControlMode {}),
                ev_power_profile_entries: EvPowerProfileEntryList { ev_power_profile_entry: &entries },
            }),
            bpt_channel_selection: None,
        };
        check(name, Message::PowerDeliveryReq(m), |o| m.encode(o));
    }
    let m = PowerDeliveryRes { header: hdr(), response_code: ResponseCode::Ok, evse_status: None };
    check("power_delivery_res", Message::PowerDeliveryRes(m), |o| m.encode(o));
    let m = PowerDeliveryRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        evse_status: Some(iso15118_20::common_types::EvseStatus {
            notification_max_delay: 1,
            evse_notification: iso15118_20::common_types::EvseNotification::Pause,
        }),
    };
    check("power_delivery_res_status", Message::PowerDeliveryRes(m), |o| m.encode(o));

    let m = SessionStopReq { header: hdr(), charging_session: ChargingSession::Terminate, ev_termination_code: None, ev_termination_explanation: None };
    check("session_stop_req", Message::SessionStopReq(m), |o| m.encode(o));
    let m = SessionStopReq {
        header: hdr(),
        charging_session: ChargingSession::Terminate,
        ev_termination_code: Some("urn:iso:std:iso:15118:-20:EVTerminationCode:Test"),
        ev_termination_explanation: None,
    };
    check("session_stop_req_code", Message::SessionStopReq(m), |o| m.encode(o));
    let m = SessionStopRes { header: hdr(), response_code: ResponseCode::Ok };
    check("session_stop_res", Message::SessionStopRes(m), |o| m.encode(o));
}

#[test]
fn target_soc_above_100_is_refused() {
    let m = dc::DcChargeParameterDiscoveryReq {
        header: hdr(),
        dc_cpd_req_energy_transfer_mode: dc::DcCpdReqEnergyTransferModeChoice::Base(dc::DcCpdReqEnergyTransferMode {
            ev_maximum_charge_power: rn(0, 1),
            ev_minimum_charge_power: rn(0, 1),
            ev_maximum_charge_current: rn(0, 1),
            ev_minimum_charge_current: rn(0, 1),
            ev_maximum_voltage: rn(0, 1),
            ev_minimum_voltage: rn(0, 1),
            target_soc: Some(101),
        }),
    };
    assert!(m.encode(&mut [0u8; 256]).is_err());
}

// ---------------------------------------------------------------- robustness

#[test]
fn truncated_input_never_panics_and_never_decodes() {
    for name in [
        "session_setup_req",
        "session_setup_res_ok",
        "authorization_setup_res_pnc_eim_listed",
        "service_discovery_res_max",
        "service_detail_res_kinds",
        "service_detail_res_hits",
        "service_selection_req_vas",
    ] {
        let full = vector(name);
        for cut in 0..full.len() {
            let mut mem = [0u8; 8192];
            let arena = Arena::new(&mut mem);
            assert!(common::decode(&full[..cut], &arena).is_err(), "{name} cut at {cut} decoded");
        }
    }
    for name in ["dc_cpd_req_soc", "dc_cpd_res_ramp"] {
        let full = vector(name);
        for cut in 0..full.len() {
            let mut mem = [0u8; 8192];
            let arena = Arena::new(&mut mem);
            assert!(dc::decode(&full[..cut], &arena).is_err(), "{name} cut at {cut} decoded");
        }
    }
}

#[test]
fn a_small_arena_is_an_error_not_a_panic() {
    let full = vector("service_detail_res_kinds");
    let mut mem = [0u8; 16];
    let arena = Arena::new(&mut mem);
    assert_eq!(common::decode(&full, &arena), Err(exi::DecodeError::ArenaFull));
}

/// Flips bits and bytes of valid streams: whatever comes out, decoding must return, not panic
/// or run away (a hostile EVSE controls these bytes).
#[test]
fn mutated_streams_never_panic() {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let common_names = ["session_setup_res_ok", "authorization_setup_res_pnc_eim_listed", "service_discovery_res_max", "service_detail_res_hits", "service_selection_req_vas"];
    for name in common_names {
        let original = vector(name);
        for _ in 0..2000 {
            let mut bytes = original.clone();
            for _ in 0..=(next() % 3) {
                let i = (next() as usize) % bytes.len();
                bytes[i] ^= 1 << (next() % 8);
            }
            if next() % 4 == 0 {
                bytes.truncate((next() as usize) % bytes.len());
            }
            let mut mem = [0u8; 4096];
            let arena = Arena::new(&mut mem);
            let _ = common::decode(&bytes, &arena);
        }
    }
    for name in ["dc_cpd_req_soc", "dc_cpd_res_ramp"] {
        let original = vector(name);
        for _ in 0..2000 {
            let mut bytes = original.clone();
            let i = (next() as usize) % bytes.len();
            bytes[i] ^= 1 << (next() % 8);
            let mut mem = [0u8; 4096];
            let arena = Arena::new(&mut mem);
            let _ = dc::decode(&bytes, &arena);
        }
    }
}
