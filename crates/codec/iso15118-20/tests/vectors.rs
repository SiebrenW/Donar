//! Checks the generated codec byte for byte against reference vectors made by EXIficient
//! (`cargo xtask vectors`), in both directions.

use exi::Arena;
use iso15118_20::common::{self, *};
use iso15118_20::common_types::{MessageHeader, Processing, RationalNumber, ResponseCode};
use iso15118_20::ac;
use iso15118_20::dc;

fn vector(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../../data/vectors/{name}.hex"));
    let hex = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing {path:?}; run `cargo xtask setup && cargo xtask vectors`"));
    (0..hex.trim().len() / 2).map(|i| u8::from_str_radix(&hex.trim()[2 * i..2 * i + 2], 16).unwrap()).collect()
}

const TS: u64 = 1_700_000_000;
const SESSION: [u8; 8] = [0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18];

fn hdr() -> MessageHeader<'static> {
    MessageHeader::new(SESSION, TS)
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
        header: MessageHeader::new([0; 8], TS),
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
            header: MessageHeader::new([1, 2, 3, 4, 5, 6, 7, 8], TS),
            response_code: code,
            evseid: evse,
        };
        check(&format!("session_setup_res_{name}"), Message::SessionSetupRes(m), |o| m.encode(o));
    }
}

/// An empty string element has no characters event: EXIficient codes it as the second-level
/// `EE` of the element (`Enc::string_element`). Checked in every position that matters: only
/// element, first and second of two optional elements, both.
#[test]
fn empty_string_elements() {
    let m = SessionSetupRes {
        header: MessageHeader::new([1, 2, 3, 4, 5, 6, 7, 8], TS),
        response_code: ResponseCode::FailedWrongChargeParameter,
        evseid: "",
    };
    check("session_setup_res_empty", Message::SessionSetupRes(m), |o| m.encode(o));
    for (name, code, explanation) in [
        ("empty_code", Some(""), Some("abc")),
        ("empty_explanation", Some("abc"), Some("")),
        ("empty_both", Some(""), Some("")),
        ("empty_expl_only", None, Some("")),
    ] {
        let m = SessionStopReq {
            header: hdr(),
            charging_session: ChargingSession::Terminate,
            ev_termination_code: code,
            ev_termination_explanation: explanation,
        };
        check(&format!("session_stop_req_{name}"), Message::SessionStopReq(m), |o| m.encode(o));
    }
}

/// Another encoder may write the empty string as a `CH` event with a zero-length value.
#[test]
fn an_empty_string_as_a_character_event_is_accepted() {
    let reference = vector("session_stop_req_empty_code");
    // The escape and the 3-bit EE code (4 bits) become CH (0), the length 0 + 2 as an unsigned
    // integer (8 bits) and EE (0), at the sixth bit after the 17th octet (the header ends there).
    let mut bits: Vec<bool> = reference.iter().flat_map(|b| (0..8).rev().map(move |i| b >> i & 1 == 1)).collect();
    let at = 17 * 8 + 6;
    assert_eq!(bits[at..at + 4], [true, false, false, false]);
    let ch: Vec<bool> = "0000000100".chars().map(|c| c == '1').collect();
    bits.splice(at..at + 4, ch);
    while !bits.len().is_multiple_of(8) {
        bits.push(false);
    }
    let bytes: Vec<u8> = bits.chunks(8).map(|c| c.iter().fold(0, |a, &b| a << 1 | u8::from(b))).collect();
    let mut mem = [0u8; 4096];
    let arena = Arena::new(&mut mem);
    let Message::SessionStopReq(m) = common::decode(&bytes, &arena).unwrap() else { panic!() };
    assert_eq!(m.ev_termination_code, Some(""));
    assert_eq!(m.ev_termination_explanation, Some("abc"));
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

/// Scheduled control mode: the energy offer, the schedule tuples with their prices, the selection.
#[test]
fn scheduled_control_mode() {
    let m = ScheduleExchangeReq {
        header: hdr(),
        maximum_supporting_points: 12,
        se_req_control_mode: SeReqControlModeChoice::Scheduled(ScheduledSeReqControlMode {
            departure_time: None,
            ev_target_energy_request: None,
            ev_maximum_energy_request: None,
            ev_minimum_energy_request: None,
            ev_energy_offer: None,
        }),
    };
    check("schedule_exchange_req_scheduled_min", Message::ScheduleExchangeReq(m), |o| m.encode(o));
    let ev_entries = [EvPowerScheduleEntry { duration: 3600, power: rn(3, 7) }];
    let rule = [EvPriceRule { energy_fee: rn(-2, 30), power_range_start: rn(0, 0) }];
    let stacks = [EvPriceRuleStack { duration: 3600, ev_price_rule: &rule }];
    let m = ScheduleExchangeReq {
        header: hdr(),
        maximum_supporting_points: 1024,
        se_req_control_mode: SeReqControlModeChoice::Scheduled(ScheduledSeReqControlMode {
            departure_time: Some(28800),
            ev_target_energy_request: Some(rn(3, 40)),
            ev_maximum_energy_request: Some(rn(3, 60)),
            ev_minimum_energy_request: Some(rn(0, 0)),
            ev_energy_offer: Some(EvEnergyOffer {
                ev_power_schedule: EvPowerSchedule { time_anchor: TS, ev_power_schedule_entries: EvPowerScheduleEntryList { ev_power_schedule_entry: &ev_entries } },
                ev_absolute_price_schedule: EvAbsolutePriceSchedule {
                    time_anchor: TS,
                    currency: "EUR",
                    price_algorithm: "urn:iso:std:iso:15118:-20:PriceAlgorithm:1-Power",
                    ev_price_rule_stacks: EvPriceRuleStackList { ev_price_rule_stack: &stacks },
                },
            }),
        }),
    };
    check("schedule_exchange_req_scheduled_offer", Message::ScheduleExchangeReq(m), |o| m.encode(o));

    let entry = |duration: u32, power: i16, l2: Option<i16>, l3: Option<i16>| PowerScheduleEntry { duration, power: rn(3, power), power_l2: l2.map(|p| rn(3, p)), power_l3: l3.map(|p| rn(3, p)) };
    let first = [entry(3600, 7, None, None)];
    let second = [entry(1800, 11, Some(4), Some(4)), entry(1800, 2, None, None)];
    let third = [entry(3600, 3, None, None)];
    let power = |entries: &'static [PowerScheduleEntry], available, tolerance| PowerSchedule {
        time_anchor: TS,
        available_energy: available,
        power_tolerance: tolerance,
        power_schedule_entries: PowerScheduleEntryList { power_schedule_entry: entries },
    };
    let levels = [PriceLevelScheduleEntry { duration: 1800, price_level: 1 }, PriceLevelScheduleEntry { duration: 1800, price_level: 2 }];
    let price_rule = [PriceRule { energy_fee: rn(-2, 25), parking_fee: None, parking_fee_period: None, carbon_dioxide_emission: None, renewable_generation_percentage: None, power_range_start: rn(0, 0) }];
    let price_stacks = [PriceRuleStack { duration: 3600, price_rule: &price_rule }];
    let tuples = [
        ScheduleTuple { schedule_tuple_id: 1, charging_schedule: ChargingSchedule { power_schedule: power(Box::leak(Box::new(first)), None, None), schedule: None }, discharging_schedule: None },
        ScheduleTuple {
            schedule_tuple_id: 2,
            charging_schedule: ChargingSchedule {
                power_schedule: power(Box::leak(Box::new(second)), Some(rn(3, 30)), Some(rn(0, 5))),
                schedule: Some(ScheduleChoice::PriceLevel(PriceLevelSchedule {
                    id: None,
                    time_anchor: TS,
                    price_schedule_id: 5,
                    price_schedule_description: Some("night"),
                    number_of_price_levels: 2,
                    price_level_schedule_entries: PriceLevelScheduleEntryList { price_level_schedule_entry: &levels },
                })),
            },
            discharging_schedule: Some(ChargingSchedule {
                power_schedule: power(Box::leak(Box::new(third)), None, None),
                schedule: Some(ScheduleChoice::AbsolutePrice(AbsolutePriceSchedule {
                    id: None,
                    time_anchor: TS,
                    price_schedule_id: 6,
                    price_schedule_description: None,
                    currency: "EUR",
                    language: "eng",
                    price_algorithm: "urn:iso:std:iso:15118:-20:PriceAlgorithm:1-Power",
                    minimum_cost: None,
                    maximum_cost: None,
                    tax_rules: None,
                    price_rule_stacks: PriceRuleStackList { price_rule_stack: &price_stacks },
                    overstay_rules: None,
                    additional_selected_services: None,
                })),
            }),
        },
    ];
    let m = ScheduleExchangeRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        evse_processing: Processing::Finished,
        go_to_pause: None,
        se_res_control_mode: SeResControlModeChoice::Scheduled(ScheduledSeResControlMode { schedule_tuple: &tuples }),
    };
    check("schedule_exchange_res_scheduled", Message::ScheduleExchangeRes(m), |o| m.encode(o));

    let entries = [PowerScheduleEntry { duration: 3600, power: rn(3, 7), power_l2: None, power_l3: None }];
    let m = PowerDeliveryReq {
        header: hdr(),
        ev_processing: Processing::Finished,
        charge_progress: ChargeProgress::Start,
        ev_power_profile: Some(EvPowerProfile {
            time_anchor: TS,
            evppt_control_mode: EvpptControlModeChoice::Scheduled(ScheduledEvpptControlMode {
                selected_schedule_tuple_id: 2,
                power_tolerance_acceptance: Some(PowerToleranceAcceptance::PowerToleranceConfirmed),
            }),
            ev_power_profile_entries: EvPowerProfileEntryList { ev_power_profile_entry: &entries },
        }),
        bpt_channel_selection: None,
    };
    check("power_delivery_req_start_scheduled", Message::PowerDeliveryReq(m), |o| m.encode(o));
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

fn charge_loop_req() -> dc::DcChargeLoopReq<'static> {
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
fn dc_bpt() {
    let m = dc::DcChargeParameterDiscoveryReq {
        header: hdr(),
        dc_cpd_req_energy_transfer_mode: dc::DcCpdReqEnergyTransferModeChoice::Bpt(dc::BptDcCpdReqEnergyTransferMode {
            ev_maximum_charge_power: rn(3, 120),
            ev_minimum_charge_power: rn(0, 100),
            ev_maximum_charge_current: rn(0, 200),
            ev_minimum_charge_current: rn(0, 1),
            ev_maximum_voltage: rn(0, 450),
            ev_minimum_voltage: rn(0, 200),
            target_soc: Some(80),
            ev_maximum_discharge_power: rn(3, 11),
            ev_minimum_discharge_power: rn(0, 100),
            ev_maximum_discharge_current: rn(0, 30),
            ev_minimum_discharge_current: rn(0, 1),
        }),
    };
    check_dc("dc_bpt_cpd_req", dc::Message::DcChargeParameterDiscoveryReq(m), |o| m.encode(o));
    let m = dc::DcChargeParameterDiscoveryRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        dc_cpd_res_energy_transfer_mode: dc::DcCpdResEnergyTransferModeChoice::Bpt(dc::BptDcCpdResEnergyTransferMode {
            evse_maximum_charge_power: rn(3, 150),
            evse_minimum_charge_power: rn(0, 1000),
            evse_maximum_charge_current: rn(0, 300),
            evse_minimum_charge_current: rn(0, -2),
            evse_maximum_voltage: rn(0, 920),
            evse_minimum_voltage: rn(0, 150),
            evse_power_ramp_limitation: Some(rn(0, 50)),
            evse_maximum_discharge_power: rn(3, 12),
            evse_minimum_discharge_power: rn(0, 200),
            evse_maximum_discharge_current: rn(0, 40),
            evse_minimum_discharge_current: rn(0, 2),
        }),
    };
    check_dc("dc_bpt_cpd_res", dc::Message::DcChargeParameterDiscoveryRes(m), |o| m.encode(o));

    let m = dc::DcChargeLoopReq {
        cl_req_control_mode: dc::ClReqControlModeChoice::BptDynamicDc(dc::BptDynamicDcClReqControlMode {
            departure_time: None,
            ev_target_energy_request: rn(3, 40),
            ev_maximum_energy_request: rn(3, 60),
            ev_minimum_energy_request: rn(0, 0),
            ev_maximum_charge_power: rn(3, 100),
            ev_minimum_charge_power: rn(0, 100),
            ev_maximum_charge_current: rn(0, 200),
            ev_maximum_voltage: rn(0, 450),
            ev_minimum_voltage: rn(0, 200),
            ev_maximum_discharge_power: rn(3, 11),
            ev_minimum_discharge_power: rn(0, 100),
            ev_maximum_discharge_current: rn(0, 30),
            ev_maximum_v2x_energy_request: Some(rn(3, 20)),
            ev_minimum_v2x_energy_request: Some(rn(0, 500)),
        }),
        ..charge_loop_req()
    };
    check_dc("dc_bpt_charge_loop_req", dc::Message::DcChargeLoopReq(m), |o| m.encode(o));
    let m = dc::DcChargeLoopRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        evse_status: None,
        meter_info: None,
        receipt: None,
        evse_present_current: rn(0, 120),
        evse_present_voltage: rn(0, 398),
        evse_power_limit_achieved: false,
        evse_current_limit_achieved: true,
        evse_voltage_limit_achieved: false,
        cl_res_control_mode: dc::ClResControlModeChoice::BptDynamicDc(dc::BptDynamicDcClResControlMode {
            departure_time: None,
            minimum_soc: None,
            target_soc: None,
            ack_max_delay: None,
            evse_maximum_charge_power: rn(3, 150),
            evse_minimum_charge_power: rn(0, 100),
            evse_maximum_charge_current: rn(0, 300),
            evse_maximum_voltage: rn(0, 920),
            evse_maximum_discharge_power: rn(3, 12),
            evse_minimum_discharge_power: rn(0, 200),
            evse_maximum_discharge_current: rn(0, 40),
            evse_minimum_voltage: rn(0, 150),
        }),
    };
    check_dc("dc_bpt_charge_loop_res", dc::Message::DcChargeLoopRes(m), |o| m.encode(o));
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
    for (name, progress, channel) in [
        ("power_delivery_req_start", ChargeProgress::Start, None),
        ("power_delivery_req_start_bpt", ChargeProgress::Start, Some(ChannelSelection::Discharge)),
        ("power_delivery_req_stop", ChargeProgress::Stop, None),
    ] {
        let m = PowerDeliveryReq {
            header: hdr(),
            ev_processing: Processing::Finished,
            charge_progress: progress,
            ev_power_profile: Some(EvPowerProfile {
                time_anchor: TS,
                evppt_control_mode: EvpptControlModeChoice::Dynamic(DynamicEvpptControlMode {}),
                ev_power_profile_entries: EvPowerProfileEntryList { ev_power_profile_entry: &entries },
            }),
            bpt_channel_selection: channel,
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
        "certificate_installation_req_serials",
        "session_stop_req_empty_code",
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
    let common_names = ["session_setup_res_ok", "authorization_setup_res_pnc_eim_listed", "service_discovery_res_max", "service_detail_res_hits", "service_selection_req_vas", "certificate_installation_req_serials", "session_stop_req_empty_both"];
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

// ---------------------------------------------------------------- certificate serial numbers

/// `xs:integer` is of any size: a certificate serial number is up to 20 octets (RFC 5280), a
/// native integer cannot hold it. Values around the 7-bit group, octet and 20-octet boundaries,
/// negative ones (the magnitude minus one is coded) and one far beyond 20 octets, against
/// EXIficient.
#[test]
fn root_certificate_ids_with_serial_numbers_of_any_size() {
    use iso15118_20::common_types::ListOfRootCertificateIds;
    use xmldsig::X509IssuerSerial;
    let max20 = [0xffu8; 20];
    let mut high20 = [0u8; 20];
    high20[0] = 0x80;
    let max40 = [0xffu8; 40];
    let serials = [
        exi::Integer::unsigned(&[1]),
        exi::Integer::ZERO,
        exi::Integer::new(true, &[5]),
        exi::Integer::unsigned(&max20),
        exi::Integer::unsigned(&high20),
        exi::Integer::new(true, &max20),
        exi::Integer::unsigned(&max40),
        exi::Integer::unsigned(&[0xff]),
        exi::Integer::unsigned(&[1, 0]),
        exi::Integer::new(true, &[0x80]),
        exi::Integer::new(true, &[1, 0]),
    ];
    let names: Vec<String> = (0..serials.len()).map(|i| format!("CN=Root {i},O=V2G")).collect();
    let ids: Vec<X509IssuerSerial<'_>> = serials
        .iter()
        .zip(&names)
        .map(|(&x509_serial_number, n)| X509IssuerSerial { x509_issuer_name: n, x509_serial_number })
        .collect();
    let m = CertificateInstallationReq {
        header: hdr(),
        oem_provisioning_certificate_chain: SignedCertificateChain { id: "id1", certificate: &[1, 2, 3, 4, 5], sub_certificates: None },
        list_of_root_certificate_ids: ListOfRootCertificateIds { root_certificate_id: &ids },
        maximum_contract_certificate_chains: 3,
        prioritized_emaids: None,
    };
    let want = vector("certificate_installation_req_serials");
    let mut out = [0u8; 2048];
    let n = m.encode(&mut out).unwrap();
    let mut mem = [0u8; 8192];
    let arena = Arena::new(&mut mem);
    let decoded = common::decode(&want, &arena);
    assert_eq!(&out[..n], &want[..], "encode differs from EXIficient");
    let Message::CertificateInstallationReq(got) = decoded.unwrap() else { panic!() };
    assert_eq!(got, m);
    // The 20-octet serial number as the 21 content octets of a DER INTEGER (a leading zero keeps it positive).
    let mut der = [0xffu8; 21];
    der[0] = 0;
    assert_eq!(exi::Integer::unsigned(&der), serials[3]);
}

// ---------------------------------------------------------------- header signature

const CANONICAL: &str = "http://www.w3.org/TR/canonical-exi/";
const ECDSA512: &str = "http://www.w3.org/2001/04/xmldsig-more#ecdsa-sha512";
const SHA512: &str = "http://www.w3.org/2001/04/xmlenc#sha512";

fn bytes(range: std::ops::Range<u8>) -> Vec<u8> {
    range.collect()
}

/// The message header's `Signature` (V2G20-771 profile) against EXIficient: the algorithm
/// strings come from the string table the second time, and the optional ids shift the event codes.
#[test]
fn header_signatures() {
    use xmldsig::{CanonicalizationMethod, DigestMethod, Reference, Signature, SignatureMethod, SignatureValue, SignedInfo, Transform, Transforms};
    let (d1, d2, value) = (bytes(0..64), bytes(64..128), bytes(0..132));
    let transform = [Transform { algorithm: CANONICAL }];
    let first = Reference { id: None, uri: Some("#id1"), transforms: Some(Transforms { transform: &transform }), digest_method: DigestMethod { algorithm: SHA512 }, digest_value: &d1 };
    let second = Reference { id: Some("r2"), uri: Some("#id2"), transforms: None, digest_method: DigestMethod { algorithm: SHA512 }, digest_value: &d2 };
    let (canonical, ecdsa) = (CanonicalizationMethod { algorithm: CANONICAL }, SignatureMethod { algorithm: ECDSA512 });
    let plain = Signature {
        id: None,
        signed_info: SignedInfo { id: None, canonicalization_method: canonical, signature_method: ecdsa, reference: &[first] },
        signature_value: SignatureValue { id: None, value: &value },
    };
    let ids = Signature {
        id: Some("sig"),
        signed_info: SignedInfo { id: Some("si"), canonicalization_method: canonical, signature_method: ecdsa, reference: &[first, second] },
        signature_value: SignatureValue { id: Some("sv"), value: &value },
    };
    for (name, signature) in [("header_signature", plain), ("header_signature_two_references", ids)] {
        let mut header = hdr();
        header.signature = Some(signature);
        let m = SessionStopReq { header, charging_session: ChargingSession::Terminate, ev_termination_code: None, ev_termination_explanation: None };
        let want = vector(name);
        let mut out = [0u8; 2048];
        let n = m.encode(&mut out).unwrap();
        assert_eq!(&out[..n], &want[..], "{name}: encode differs from EXIficient");
        let mut mem = [0u8; 8192];
        let arena = Arena::new(&mut mem);
        assert_eq!(common::decode(&want, &arena).unwrap(), Message::SessionStopReq(m), "{name}: decode");
    }
}

/// A signature cut short never decodes, and damaged ones never panic.
#[test]
fn truncated_and_mutated_signatures_never_panic() {
    let want = vector("header_signature");
    // A stream cut inside the signature never decodes.
    for cut in 10..want.len() {
        let mut mem = [0u8; 8192];
        let arena = Arena::new(&mut mem);
        assert!(common::decode(&want[..cut], &arena).is_err(), "cut at {cut}");
    }
    // Flipping bits anywhere never panics.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    for _ in 0..3000 {
        let mut copy = want.clone();
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let i = (state as usize) % copy.len();
        copy[i] ^= 1 << ((state >> 40) % 8);
        let mut mem = [0u8; 8192];
        let arena = Arena::new(&mut mem);
        let _ = common::decode(&copy, &arena);
    }
}

// ---------------------------------------------------------------- signed fragments

/// The parts of a message that are signed are EXI fragments (V2G20-1449): the start event is
/// chosen among all element declarations of the schema, and the stream ends with the end of the
/// document. EXIficient's fragment encoding of the OEM provisioning certificate chain and of a
/// `SignedInfo`.
#[test]
fn signed_fragments() {
    use iso15118_20::signature::{self, encode_fragment};
    use xmldsig::{CanonicalizationMethod, DigestMethod, Reference, SignatureMethod, SignedInfo, Transform, Transforms};
    let chain = SignedCertificateChain { id: "id1", certificate: &[1, 2, 3, 4, 5], sub_certificates: None };
    let mut out = [0u8; 256];
    let n = encode_fragment(&mut out, signature::fragment::OEM_PROVISIONING_CERTIFICATE_CHAIN, |e| chain.encode_content(e)).unwrap();
    assert_eq!(&out[..n], &vector("fragment_signed_certificate_chain")[..]);

    let digest = bytes(0..64);
    let transform = [Transform { algorithm: CANONICAL }];
    let reference = Reference { id: None, uri: Some("#id1"), transforms: Some(Transforms { transform: &transform }), digest_method: DigestMethod { algorithm: SHA512 }, digest_value: &digest };
    let (canonical, ecdsa) = (CanonicalizationMethod { algorithm: CANONICAL }, SignatureMethod { algorithm: ECDSA512 });
    let info = SignedInfo { id: None, canonicalization_method: canonical, signature_method: ecdsa, reference: &[reference] };
    let mut out = [0u8; 512];
    let n = info.encode_fragment(&mut out).unwrap();
    assert_eq!(&out[..n], &vector("fragment_signed_info")[..]);
}

// ---------------------------------------------------------------- AC

fn check_ac(name: &str, msg: ac::Message<'_>, enc: impl FnOnce(&mut [u8]) -> Result<usize, exi::EncodeError>) {
    let want = vector(name);
    let mut out = [0u8; 2048];
    let n = enc(&mut out).unwrap_or_else(|e| panic!("{name}: encode failed: {e:?}"));
    assert_eq!(&out[..n], &want[..], "{name}: encode differs from EXIficient");
    let mut mem = [0u8; 8192];
    let arena = Arena::new(&mut mem);
    assert_eq!(ac::decode(&want, &arena).unwrap_or_else(|e| panic!("{name}: {e:?}")), msg, "{name}: decode");
}

#[test]
fn ac_charge_parameter_discovery() {
    let base = ac::AcCpdReqEnergyTransferMode {
        ev_maximum_charge_power: rn(3, 11),
        ev_maximum_charge_power_l2: None,
        ev_maximum_charge_power_l3: None,
        ev_minimum_charge_power: rn(0, 1400),
        ev_minimum_charge_power_l2: None,
        ev_minimum_charge_power_l3: None,
    };
    let m = ac::AcChargeParameterDiscoveryReq { header: hdr(), ac_cpd_req_energy_transfer_mode: ac::AcCpdReqEnergyTransferModeChoice::Base(base) };
    check_ac("ac_cpd_req", ac::Message::AcChargeParameterDiscoveryReq(m), |o| m.encode(o));
    let three = ac::AcCpdReqEnergyTransferMode { ev_maximum_charge_power_l2: Some(rn(3, 11)), ev_maximum_charge_power_l3: Some(rn(3, 11)), ..base };
    let m = ac::AcChargeParameterDiscoveryReq { header: hdr(), ac_cpd_req_energy_transfer_mode: ac::AcCpdReqEnergyTransferModeChoice::Base(three) };
    check_ac("ac_cpd_req_3ph", ac::Message::AcChargeParameterDiscoveryReq(m), |o| m.encode(o));
    let m = ac::AcChargeParameterDiscoveryReq {
        header: hdr(),
        ac_cpd_req_energy_transfer_mode: ac::AcCpdReqEnergyTransferModeChoice::Bpt(ac::BptAcCpdReqEnergyTransferMode {
            ev_maximum_charge_power: rn(3, 11),
            ev_maximum_charge_power_l2: None,
            ev_maximum_charge_power_l3: None,
            ev_minimum_charge_power: rn(0, 1400),
            ev_minimum_charge_power_l2: None,
            ev_minimum_charge_power_l3: None,
            ev_maximum_discharge_power: rn(3, 11),
            ev_maximum_discharge_power_l2: Some(rn(3, 4)),
            ev_maximum_discharge_power_l3: None,
            ev_minimum_discharge_power: rn(0, 500),
            ev_minimum_discharge_power_l2: None,
            ev_minimum_discharge_power_l3: None,
        }),
    };
    check_ac("ac_bpt_cpd_req", ac::Message::AcChargeParameterDiscoveryReq(m), |o| m.encode(o));

    let res = ac::AcCpdResEnergyTransferMode {
        evse_maximum_charge_power: rn(3, 11),
        evse_maximum_charge_power_l2: None,
        evse_maximum_charge_power_l3: None,
        evse_minimum_charge_power: rn(0, 1400),
        evse_minimum_charge_power_l2: None,
        evse_minimum_charge_power_l3: None,
        evse_nominal_frequency: rn(0, 50),
        maximum_power_asymmetry: None,
        evse_power_ramp_limitation: None,
        evse_present_active_power: None,
        evse_present_active_power_l2: None,
        evse_present_active_power_l3: None,
    };
    let m = ac::AcChargeParameterDiscoveryRes { header: hdr(), response_code: ResponseCode::Ok, ac_cpd_res_energy_transfer_mode: ac::AcCpdResEnergyTransferModeChoice::Base(res) };
    check_ac("ac_cpd_res", ac::Message::AcChargeParameterDiscoveryRes(m), |o| m.encode(o));
    let full = ac::AcCpdResEnergyTransferMode {
        evse_maximum_charge_power: rn(3, 22),
        evse_maximum_charge_power_l2: Some(rn(3, 7)),
        evse_maximum_charge_power_l3: Some(rn(3, 7)),
        maximum_power_asymmetry: Some(rn(0, 100)),
        evse_power_ramp_limitation: Some(rn(0, 50)),
        evse_present_active_power: Some(rn(3, 5)),
        ..res
    };
    let m = ac::AcChargeParameterDiscoveryRes { header: hdr(), response_code: ResponseCode::Ok, ac_cpd_res_energy_transfer_mode: ac::AcCpdResEnergyTransferModeChoice::Base(full) };
    check_ac("ac_cpd_res_full", ac::Message::AcChargeParameterDiscoveryRes(m), |o| m.encode(o));
    let m = ac::AcChargeParameterDiscoveryRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        ac_cpd_res_energy_transfer_mode: ac::AcCpdResEnergyTransferModeChoice::Bpt(ac::BptAcCpdResEnergyTransferMode {
            evse_maximum_charge_power: rn(3, 11),
            evse_maximum_charge_power_l2: None,
            evse_maximum_charge_power_l3: None,
            evse_minimum_charge_power: rn(0, 1400),
            evse_minimum_charge_power_l2: None,
            evse_minimum_charge_power_l3: None,
            evse_nominal_frequency: rn(0, 50),
            maximum_power_asymmetry: None,
            evse_power_ramp_limitation: None,
            evse_present_active_power: None,
            evse_present_active_power_l2: None,
            evse_present_active_power_l3: None,
            evse_maximum_discharge_power: rn(3, 11),
            evse_maximum_discharge_power_l2: None,
            evse_maximum_discharge_power_l3: None,
            evse_minimum_discharge_power: rn(0, 500),
            evse_minimum_discharge_power_l2: None,
            evse_minimum_discharge_power_l3: None,
        }),
    };
    check_ac("ac_bpt_cpd_res", ac::Message::AcChargeParameterDiscoveryRes(m), |o| m.encode(o));
}

fn dynamic_request() -> ac::DynamicAcClReqControlMode {
    ac::DynamicAcClReqControlMode {
        departure_time: None,
        ev_target_energy_request: rn(3, 40),
        ev_maximum_energy_request: rn(3, 60),
        ev_minimum_energy_request: rn(0, 0),
        ev_maximum_charge_power: rn(3, 11),
        ev_maximum_charge_power_l2: None,
        ev_maximum_charge_power_l3: None,
        ev_minimum_charge_power: rn(0, 1400),
        ev_minimum_charge_power_l2: None,
        ev_minimum_charge_power_l3: None,
        ev_present_active_power: rn(3, 7),
        ev_present_active_power_l2: None,
        ev_present_active_power_l3: None,
        ev_present_reactive_power: rn(0, 200),
        ev_present_reactive_power_l2: None,
        ev_present_reactive_power_l3: None,
    }
}

#[test]
fn ac_charge_loop_request() {
    let loop_req = |meter_info_requested, mode| ac::AcChargeLoopReq { header: hdr(), display_parameters: None, meter_info_requested, cl_req_control_mode: mode };
    let dynamic = dynamic_request();
    let m = loop_req(false, ac::ClReqControlModeChoice::DynamicAc(dynamic));
    check_ac("ac_charge_loop_req_dynamic", ac::Message::AcChargeLoopReq(m), |o| m.encode(o));
    let three = ac::DynamicAcClReqControlMode {
        ev_maximum_charge_power_l2: Some(rn(3, 11)),
        ev_maximum_charge_power_l3: Some(rn(3, 11)),
        ev_present_active_power_l2: Some(rn(3, 7)),
        ev_present_active_power_l3: Some(rn(3, 7)),
        ev_present_reactive_power_l2: Some(rn(0, 200)),
        ev_present_reactive_power_l3: Some(rn(0, 200)),
        ..dynamic
    };
    let m = loop_req(true, ac::ClReqControlModeChoice::DynamicAc(three));
    check_ac("ac_charge_loop_req_dynamic_3ph", ac::Message::AcChargeLoopReq(m), |o| m.encode(o));
    let m = loop_req(
        false,
        ac::ClReqControlModeChoice::BptDynamicAc(ac::BptDynamicAcClReqControlMode {
            departure_time: dynamic.departure_time,
            ev_target_energy_request: dynamic.ev_target_energy_request,
            ev_maximum_energy_request: dynamic.ev_maximum_energy_request,
            ev_minimum_energy_request: dynamic.ev_minimum_energy_request,
            ev_maximum_charge_power: dynamic.ev_maximum_charge_power,
            ev_maximum_charge_power_l2: None,
            ev_maximum_charge_power_l3: None,
            ev_minimum_charge_power: dynamic.ev_minimum_charge_power,
            ev_minimum_charge_power_l2: None,
            ev_minimum_charge_power_l3: None,
            ev_present_active_power: dynamic.ev_present_active_power,
            ev_present_active_power_l2: None,
            ev_present_active_power_l3: None,
            ev_present_reactive_power: dynamic.ev_present_reactive_power,
            ev_present_reactive_power_l2: None,
            ev_present_reactive_power_l3: None,
            ev_maximum_discharge_power: rn(3, 11),
            ev_maximum_discharge_power_l2: None,
            ev_maximum_discharge_power_l3: None,
            ev_minimum_discharge_power: rn(0, 500),
            ev_minimum_discharge_power_l2: None,
            ev_minimum_discharge_power_l3: None,
            ev_maximum_v2x_energy_request: Some(rn(3, 20)),
            ev_minimum_v2x_energy_request: Some(rn(0, 500)),
        }),
    );
    check_ac("ac_charge_loop_req_bpt", ac::Message::AcChargeLoopReq(m), |o| m.encode(o));
    let scheduled = ac::ScheduledAcClReqControlMode {
        ev_target_energy_request: Some(rn(3, 40)),
        ev_maximum_energy_request: None,
        ev_minimum_energy_request: None,
        ev_maximum_charge_power: Some(rn(3, 11)),
        ev_maximum_charge_power_l2: None,
        ev_maximum_charge_power_l3: None,
        ev_minimum_charge_power: None,
        ev_minimum_charge_power_l2: None,
        ev_minimum_charge_power_l3: None,
        ev_present_active_power: rn(3, 7),
        ev_present_active_power_l2: None,
        ev_present_active_power_l3: None,
        ev_present_reactive_power: Some(rn(0, 100)),
        ev_present_reactive_power_l2: None,
        ev_present_reactive_power_l3: None,
    };
    let m = loop_req(false, ac::ClReqControlModeChoice::ScheduledAc(scheduled));
    check_ac("ac_charge_loop_req_scheduled", ac::Message::AcChargeLoopReq(m), |o| m.encode(o));
    let m = loop_req(
        false,
        ac::ClReqControlModeChoice::BptScheduledAc(ac::BptScheduledAcClReqControlMode {
            ev_target_energy_request: scheduled.ev_target_energy_request,
            ev_maximum_energy_request: None,
            ev_minimum_energy_request: None,
            ev_maximum_charge_power: scheduled.ev_maximum_charge_power,
            ev_maximum_charge_power_l2: None,
            ev_maximum_charge_power_l3: None,
            ev_minimum_charge_power: None,
            ev_minimum_charge_power_l2: None,
            ev_minimum_charge_power_l3: None,
            ev_present_active_power: scheduled.ev_present_active_power,
            ev_present_active_power_l2: None,
            ev_present_active_power_l3: None,
            ev_present_reactive_power: None,
            ev_present_reactive_power_l2: None,
            ev_present_reactive_power_l3: None,
            ev_maximum_discharge_power: Some(rn(3, 11)),
            ev_maximum_discharge_power_l2: None,
            ev_maximum_discharge_power_l3: None,
            ev_minimum_discharge_power: Some(rn(0, 500)),
            ev_minimum_discharge_power_l2: None,
            ev_minimum_discharge_power_l3: None,
        }),
    );
    check_ac("ac_charge_loop_req_scheduled_bpt", ac::Message::AcChargeLoopReq(m), |o| m.encode(o));
}

#[test]
fn ac_charge_loop_response() {
    let loop_res = |frequency, mode| ac::AcChargeLoopRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        evse_status: None,
        meter_info: None,
        receipt: None,
        evse_target_frequency: frequency,
        cl_res_control_mode: mode,
    };
    let dynamic = ac::DynamicAcClResControlMode {
        departure_time: None,
        minimum_soc: None,
        target_soc: None,
        ack_max_delay: None,
        evse_target_active_power: rn(3, 7),
        evse_target_active_power_l2: Some(rn(3, 7)),
        evse_target_active_power_l3: None,
        evse_target_reactive_power: Some(rn(0, 100)),
        evse_target_reactive_power_l2: None,
        evse_target_reactive_power_l3: None,
        evse_present_active_power: Some(rn(3, 6)),
        evse_present_active_power_l2: None,
        evse_present_active_power_l3: None,
    };
    let m = loop_res(Some(rn(0, 50)), ac::ClResControlModeChoice::DynamicAc(dynamic));
    check_ac("ac_charge_loop_res_dynamic", ac::Message::AcChargeLoopRes(m), |o| m.encode(o));
    let minimal = ac::DynamicAcClResControlMode {
        evse_target_active_power_l2: None,
        evse_target_reactive_power: None,
        evse_present_active_power: None,
        ..dynamic
    };
    let m = loop_res(None, ac::ClResControlModeChoice::DynamicAc(minimal));
    check_ac("ac_charge_loop_res_dynamic_min", ac::Message::AcChargeLoopRes(m), |o| m.encode(o));
    let m = loop_res(
        None,
        ac::ClResControlModeChoice::ScheduledAc(ac::ScheduledAcClResControlMode {
            evse_target_active_power: Some(rn(3, 7)),
            evse_target_active_power_l2: None,
            evse_target_active_power_l3: None,
            evse_target_reactive_power: None,
            evse_target_reactive_power_l2: None,
            evse_target_reactive_power_l3: None,
            evse_present_active_power: Some(rn(3, 6)),
            evse_present_active_power_l2: None,
            evse_present_active_power_l3: None,
        }),
    );
    check_ac("ac_charge_loop_res_scheduled", ac::Message::AcChargeLoopRes(m), |o| m.encode(o));
    let m = loop_res(
        None,
        ac::ClResControlModeChoice::BptDynamicAc(ac::BptDynamicAcClResControlMode {
            departure_time: None,
            minimum_soc: None,
            target_soc: None,
            ack_max_delay: None,
            evse_target_active_power: rn(3, -7),
            evse_target_active_power_l2: None,
            evse_target_active_power_l3: None,
            evse_target_reactive_power: None,
            evse_target_reactive_power_l2: None,
            evse_target_reactive_power_l3: None,
            evse_present_active_power: None,
            evse_present_active_power_l2: None,
            evse_present_active_power_l3: None,
        }),
    );
    check_ac("ac_charge_loop_res_bpt", ac::Message::AcChargeLoopRes(m), |o| m.encode(o));
}
