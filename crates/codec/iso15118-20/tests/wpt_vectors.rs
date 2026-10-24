//! The WPT messages byte for byte against EXIficient (`cargo xtask vectors`), in both directions. Only the codec exists
//! for WPT; the session machines do not use it yet.

use exi::Arena;
use iso15118_20::common_types::{MessageHeader, Processing, RationalNumber, ResponseCode};
use iso15118_20::wpt::*;

fn vector(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../../data/vectors/{name}.hex"));
    let hex = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("missing {path:?}; run `cargo xtask setup && cargo xtask vectors`"));
    (0..hex.trim().len() / 2).map(|i| u8::from_str_radix(&hex.trim()[2 * i..2 * i + 2], 16).unwrap()).collect()
}

fn hdr() -> MessageHeader<'static> {
    MessageHeader::new([0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18], 1_700_000_000)
}

fn rn(exponent: i8, value: i16) -> RationalNumber {
    RationalNumber { exponent, value }
}

fn check(name: &str, msg: Message<'_>, enc: impl FnOnce(&mut [u8]) -> Result<usize, exi::EncodeError>) {
    let want = vector(name);
    let mut out = [0u8; 1024];
    let n = enc(&mut out).unwrap_or_else(|e| panic!("{name}: encode failed: {e:?}"));
    assert_eq!(&out[..n], &want[..], "{name}: encode differs from EXIficient");
    let mut mem = [0u8; 8192];
    let arena = Arena::new(&mut mem);
    assert_eq!(decode(&want, &arena).unwrap_or_else(|e| panic!("{name}: {e:?}")), msg, "{name}: decode");
}

const ONE_TWO_THREE: &[u8] = &[1, 2, 3];
const VENDOR: &[&[u8]] = &[ONE_TWO_THREE];
const VENDOR_TWO: &[&[u8]] = &[ONE_TWO_THREE, &[4, 5, 6, 7]];

#[test]
fn charge_parameter_discovery() {
    let req = WptChargeParameterDiscoveryReq {
        header: hdr(),
        evpc_max_receivable_power: rn(3, 11),
        sd_max_ground_clearence: 160,
        sd_min_ground_clearence: 100,
        evpc_natural_frequency: rn(3, 85),
        evpc_device_local_control: false,
        vendor_specific_data_container: &[],
    };
    check("wpt_cpd_req", Message::WptChargeParameterDiscoveryReq(req), |o| req.encode(o));
    let req = WptChargeParameterDiscoveryReq { evpc_device_local_control: true, vendor_specific_data_container: VENDOR_TWO, ..req };
    check("wpt_cpd_req_vendor", Message::WptChargeParameterDiscoveryReq(req), |o| req.encode(o));
    let res = WptChargeParameterDiscoveryRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        pd_input_power_class: WptPowerClass::MfWpt3,
        sd_min_output_power: rn(3, 2),
        sd_max_output_power: rn(3, 11),
        sd_max_ground_clearance_support: 180,
        sd_min_ground_clearance_support: 90,
        pd_min_coil_current: rn(0, 5),
        pd_max_coil_current: rn(0, 40),
        sd_manufacturer_specific_data_container: VENDOR,
    };
    check("wpt_cpd_res", Message::WptChargeParameterDiscoveryRes(res), |o| res.encode(o));
}

fn spec(id: u32, position: (i16, i16, i16), orientation: (i16, i16, i16)) -> WptTxRxSpecData {
    let xyz = |(coord_x, coord_y, coord_z)| WptCoordinateXyz { coord_x, coord_y, coord_z };
    WptTxRxSpecData { tx_rx_identifier: id, tx_rx_position: xyz(position), tx_rx_orientation: xyz(orientation) }
}

const FINE: &[WptFinePositioningMethod] = &[WptFinePositioningMethod::Manual, WptFinePositioningMethod::LfTxEv];
const PAIRING: &[WptPairingMethod] = &[WptPairingMethod::ExternalConfirmation, WptPairingMethod::Lpe];
const ALIGNMENT: &[WptAlignmentCheckMethod] = &[WptAlignmentCheckMethod::PowerCheck, WptAlignmentCheckMethod::Proprietary];

#[test]
fn fine_positioning_setup() {
    let tx = [spec(1, (100, -200, 0), (0, 0, 1)), spec(2, (-100, 200, 10), (0, 1, 0))];
    let pulses = [WptTxRxPulseOrder { index_number: 1, tx_rx_identifier: 1 }, WptTxRxPulseOrder { index_number: 2, tx_rx_identifier: 2 }];
    let req = WptFinePositioningSetupReq {
        header: hdr(),
        ev_processing: Processing::Ongoing,
        ev_device_fine_positioning_method_list: WptFinePositioningMethodList { wpt_fine_positioning_method: FINE },
        ev_device_pairing_method_list: WptPairingMethodList { wpt_pairing_method: PAIRING },
        ev_device_alignment_check_method_list: WptAlignmentCheckMethodList { wpt_alignment_check_method: ALIGNMENT },
        natural_offset: 50,
        vendor_specific_data_container: VENDOR_TWO,
        lf_system_setup_data: Some(WptLfSystemSetupData {
            setup_data: SetupDataChoice::LfTransmitter(WptLfTransmitterData {
                number_of_transmitters: 2,
                signal_frequency: rn(3, 125),
                tx_spec_data: &tx,
                tx_package_spec_data: Some(WptTxRxPackageSpecData { pulse_sequence_order: &pulses, pulse_separation_time: 5, pulse_duration: 10, package_separation_time: 50 }),
            }),
        }),
    };
    check("wpt_fine_positioning_setup_req", Message::WptFinePositioningSetupReq(req), |o| req.encode(o));
    let req = WptFinePositioningSetupReq {
        header: hdr(),
        ev_processing: Processing::Finished,
        ev_device_fine_positioning_method_list: WptFinePositioningMethodList { wpt_fine_positioning_method: &[WptFinePositioningMethod::Proprietary] },
        ev_device_pairing_method_list: WptPairingMethodList { wpt_pairing_method: &[WptPairingMethod::Optical] },
        ev_device_alignment_check_method_list: WptAlignmentCheckMethodList { wpt_alignment_check_method: &[WptAlignmentCheckMethod::Lpe] },
        natural_offset: 0,
        vendor_specific_data_container: &[],
        lf_system_setup_data: None,
    };
    check("wpt_fine_positioning_setup_req_min", Message::WptFinePositioningSetupReq(req), |o| req.encode(o));
    let rx = [spec(7, (1, 2, 3), (0, 0, 1)), spec(8, (4, 5, 6), (1, 0, 0))];
    let res = WptFinePositioningSetupRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        primary_device_fine_positioning_method_list: WptFinePositioningMethodList { wpt_fine_positioning_method: FINE },
        primary_device_pairing_method_list: WptPairingMethodList { wpt_pairing_method: PAIRING },
        primary_device_alignment_check_method_list: WptAlignmentCheckMethodList { wpt_alignment_check_method: ALIGNMENT },
        natural_offset: 75,
        vendor_specific_data_container: &[],
        lf_system_setup_data: Some(WptLfSystemSetupData { setup_data: SetupDataChoice::LfReceiver(WptLfReceiverData { number_of_receivers: 2, rx_spec_data: &rx }) }),
    };
    check("wpt_fine_positioning_setup_res", Message::WptFinePositioningSetupRes(res), |o| res.encode(o));
}

#[test]
fn fine_positioning() {
    let rx = WptLfDataPackageList {
        num_packages: 1,
        wpt_lf_data_package: WptLfDataPackage {
            package_index: 0,
            data: DataChoice::LfRx(WptLfRxDataList {
                wpt_lf_rx_data_list: WptLfRxData { rx_identifier: 3, rssi_data: WptLfRxRssiList { rssi_data_list: WptLfRxRssi { tx_identifier: 1, rssi: rn(0, -40) } } },
            }),
        },
    };
    let req = WptFinePositioningReq {
        header: hdr(),
        ev_processing: Processing::Ongoing,
        ev_result_code: WptEvResult::EvResultSuccess,
        vendor_specific_data_container: VENDOR,
        wpt_lf_data_package_list: Some(rx),
    };
    check("wpt_fine_positioning_req", Message::WptFinePositioningReq(req), |o| req.encode(o));
    let req = WptFinePositioningReq {
        ev_processing: Processing::Finished,
        ev_result_code: WptEvResult::EvResultUnknown,
        vendor_specific_data_container: &[],
        wpt_lf_data_package_list: None,
        ..req
    };
    check("wpt_fine_positioning_req_min", Message::WptFinePositioningReq(req), |o| req.encode(o));
    let tx = WptLfDataPackageList {
        num_packages: 2,
        wpt_lf_data_package: WptLfDataPackage {
            package_index: 1,
            data: DataChoice::LfTx(WptLfTxDataList { wpt_lf_tx_data_list: WptLfTxData { tx_identifier: 2, eirp: rn(-3, 20) } }),
        },
    };
    let res = WptFinePositioningRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        evse_processing: Processing::Ongoing,
        vendor_specific_data_container: &[],
        wpt_lf_data_package_list: Some(tx),
    };
    check("wpt_fine_positioning_res", Message::WptFinePositioningRes(res), |o| res.encode(o));
}

#[test]
fn pairing() {
    let req = WptPairingReq {
        header: hdr(),
        ev_processing: Processing::Ongoing,
        observed_id_code: Some(4711),
        ev_result_code: WptEvResult::EvResultFailed,
        vendor_specific_data_container: VENDOR,
    };
    check("wpt_pairing_req", Message::WptPairingReq(req), |o| req.encode(o));
    let req = WptPairingReq {
        ev_processing: Processing::Finished,
        observed_id_code: None,
        ev_result_code: WptEvResult::EvResultSuccess,
        vendor_specific_data_container: &[],
        ..req
    };
    check("wpt_pairing_req_min", Message::WptPairingReq(req), |o| req.encode(o));
    let alternatives = [
        AlternativeSecc { ssid: Some("depot-7"), bssid: Some("AABBCCDDEEFF"), ip_address: Some("fe80::1"), port: Some(50000) },
        AlternativeSecc { ssid: None, bssid: None, ip_address: None, port: Some(50001) },
    ];
    let res = WptPairingRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        evse_processing: Processing::Finished,
        observed_id_code: Some(4711),
        alternative_secc_list: Some(AlternativeSeccList { alternative_secc: &alternatives }),
        vendor_specific_data_container: VENDOR,
    };
    check("wpt_pairing_res", Message::WptPairingRes(res), |o| res.encode(o));
    let res = WptPairingRes {
        evse_processing: Processing::Ongoing,
        observed_id_code: None,
        alternative_secc_list: None,
        vendor_specific_data_container: &[],
        ..res
    };
    check("wpt_pairing_res_min", Message::WptPairingRes(res), |o| res.encode(o));
}

#[test]
fn alignment_check() {
    let req = WptAlignmentCheckReq {
        header: hdr(),
        ev_processing: Processing::Ongoing,
        target_coil_current: Some(rn(0, 12)),
        ev_result_code: WptEvResult::EvResultSuccess,
        vendor_specific_data_container: &[],
    };
    check("wpt_alignment_check_req", Message::WptAlignmentCheckReq(req), |o| req.encode(o));
    let req = WptAlignmentCheckReq {
        ev_processing: Processing::Finished,
        target_coil_current: None,
        ev_result_code: WptEvResult::EvResultFailed,
        vendor_specific_data_container: VENDOR,
        ..req
    };
    check("wpt_alignment_check_req_min", Message::WptAlignmentCheckReq(req), |o| req.encode(o));
    let res = WptAlignmentCheckRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        evse_processing: Processing::Finished,
        power_transmitted: Some(rn(3, 3)),
        supply_device_current: Some(rn(0, 8)),
        vendor_specific_data_container: &[],
    };
    check("wpt_alignment_check_res", Message::WptAlignmentCheckRes(res), |o| res.encode(o));
    let res = WptAlignmentCheckRes { evse_processing: Processing::Ongoing, power_transmitted: None, supply_device_current: None, ..res };
    check("wpt_alignment_check_res_min", Message::WptAlignmentCheckRes(res), |o| res.encode(o));
}

#[test]
fn charge_loop() {
    let req = WptChargeLoopReq {
        header: hdr(),
        display_parameters: None,
        meter_info_requested: false,
        evpc_power_request: rn(3, 11),
        evpc_power_output: rn(3, 10),
        evpc_charge_diagnostics: WptEvpcChargeDiagnostics::EvpcNoIssue,
        evpc_operating_frequency: Some(rn(3, 85)),
        evpc_power_control_parameter: Some(WptEvpcPowerControlParameter {
            evpc_coil_current_request: rn(0, 20),
            evpc_coil_current_information: rn(0, 19),
            evpc_current_output_information: rn(0, 25),
            evpc_voltage_output_information: rn(0, 400),
        }),
        manufacturer_specific_data_container: VENDOR,
    };
    check("wpt_charge_loop_req", Message::WptChargeLoopReq(req), |o| req.encode(o));
    let req = WptChargeLoopReq {
        meter_info_requested: true,
        evpc_power_request: rn(0, 0),
        evpc_power_output: rn(0, 0),
        evpc_charge_diagnostics: WptEvpcChargeDiagnostics::EvpcAnomalyDetected,
        evpc_operating_frequency: None,
        evpc_power_control_parameter: None,
        manufacturer_specific_data_container: &[],
        ..req
    };
    check("wpt_charge_loop_req_min", Message::WptChargeLoopReq(req), |o| req.encode(o));
    let res = WptChargeLoopRes {
        header: hdr(),
        response_code: ResponseCode::Ok,
        evse_status: None,
        meter_info: None,
        receipt: None,
        evpc_power_request: rn(3, 11),
        sd_power_input: Some(rn(3, 12)),
        spc_max_output_power_limit: rn(3, 11),
        spc_min_output_power_limit: rn(3, 1),
        spc_charge_diagnostics: WptSpcChargeDiagnostics::SpcfodDetected,
        spc_operating_frequency: Some(rn(3, 85)),
        spc_power_control_parameter: Some(WptSpcPowerControlParameter { spc_primary_device_coil_current_information: rn(0, 22) }),
        manufacturer_specific_data_container: VENDOR,
    };
    check("wpt_charge_loop_res", Message::WptChargeLoopRes(res), |o| res.encode(o));
    let res = WptChargeLoopRes {
        evpc_power_request: rn(0, 0),
        sd_power_input: None,
        spc_min_output_power_limit: rn(0, 0),
        spc_charge_diagnostics: WptSpcChargeDiagnostics::SpcNoIssue,
        spc_operating_frequency: None,
        spc_power_control_parameter: None,
        manufacturer_specific_data_container: &[],
        ..res
    };
    check("wpt_charge_loop_res_min", Message::WptChargeLoopRes(res), |o| res.encode(o));
}
