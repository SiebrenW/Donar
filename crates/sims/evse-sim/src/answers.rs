//! What the charger says when it cannot go on, and what it said: the failing response of the
//! right type for any request (V2G20-459, 460: `FAILED_SequenceError`, `FAILED_UnknownSession`),
//! and the message and response code of a frame it sent, for the log and for the check against
//! the tables of response codes (Tables 224 and 226).

use exi::Arena;
use iso15118_20::ResponseMessage;
use iso15118_20::common::{self, AsResAuthorizationModeChoice, Authorization, EimAsResAuthorizationMode};
use iso15118_20::common_types::{Processing, RationalNumber, ResponseCode};
use iso15118_20::ac;
use iso15118_20::dc;
use v2gtp::{Parse, PayloadType, parse};

use crate::evse::{Evse, frame};

/// A decoded request of either message set.
pub enum Request<'a> {
    Common(common::Message<'a>),
    Dc(dc::Message<'a>),
    Ac(ac::Message<'a>),
}

/// Decodes a frame's payload as a request, wherever it belongs.
pub fn decode_request<'a>(payload_type: PayloadType, payload: &[u8], arena: &'a Arena<'_>) -> Option<Request<'a>> {
    let common = || common::decode(payload, arena).ok().map(Request::Common);
    let dc = || dc::decode(payload, arena).ok().map(Request::Dc);
    let ac = || ac::decode(payload, arena).ok().map(Request::Ac);
    match payload_type {
        PayloadType::MAINSTREAM => common(),
        PayloadType::DC_MAINSTREAM => dc(),
        PayloadType::AC_MAINSTREAM => ac(),
        _ => dc().or_else(common),
    }
}

impl Evse {
    /// The failing response of the type that answers `request`, or `None` for a message the
    /// simulator has no response to build for (a response, certificate installation).
    pub(crate) fn failing(&self, request: &Request<'_>, code: ResponseCode) -> Option<Vec<u8>> {
        let header = || self.header();
        let zero = RationalNumber { exponent: 0, value: 0 };
        let main = PayloadType::MAINSTREAM;
        let dcm = PayloadType::DC_MAINSTREAM;
        match request {
            Request::Common(m) => match m {
                common::Message::SessionSetupReq(_) => frame(main, |o| common::SessionSetupRes { header: header(), response_code: code, evseid: &self.config.evse_id }.encode(o)),
                common::Message::AuthorizationSetupReq(_) => frame(main, |o| {
                    common::AuthorizationSetupRes {
                        header: header(),
                        response_code: code,
                        authorization_services: &[Authorization::Eim],
                        certificate_installation_service: false,
                        as_res_authorization_mode: AsResAuthorizationModeChoice::Eim(EimAsResAuthorizationMode {}),
                    }
                    .encode(o)
                }),
                common::Message::AuthorizationReq(_) => frame(main, |o| common::AuthorizationRes { header: header(), response_code: code, evse_processing: Processing::Finished }.encode(o)),
                common::Message::ServiceDiscoveryReq(_) => frame(main, |o| {
                    common::ServiceDiscoveryRes {
                        header: header(),
                        response_code: code,
                        service_renegotiation_supported: false,
                        energy_transfer_service_list: common::ServiceList { service: &[common::Service { service_id: 2, free_service: false }] },
                        vas_list: None,
                    }
                    .encode(o)
                }),
                common::Message::ServiceDetailReq(r) => frame(main, |o| {
                    // At least one parameter set with one parameter (the lists have a minimum).
                    let parameters = [common::Parameter { name: "Connector", choice: common::ParameterChoice::IntValue(1) }];
                    let sets = [common::ParameterSet { parameter_set_id: 1, parameter: &parameters }];
                    common::ServiceDetailRes { header: header(), response_code: code, service_id: r.service_id, service_parameter_list: common::ServiceParameterList { parameter_set: &sets } }.encode(o)
                }),
                common::Message::ServiceSelectionReq(_) => frame(main, |o| common::ServiceSelectionRes { header: header(), response_code: code }.encode(o)),
                common::Message::ScheduleExchangeReq(_) => frame(main, |o| {
                    common::ScheduleExchangeRes {
                        header: header(),
                        response_code: code,
                        evse_processing: Processing::Finished,
                        go_to_pause: None,
                        se_res_control_mode: common::SeResControlModeChoice::Dynamic(common::DynamicSeResControlMode { departure_time: None, minimum_soc: None, target_soc: None, schedule: None }),
                    }
                    .encode(o)
                }),
                common::Message::PowerDeliveryReq(_) => frame(main, |o| common::PowerDeliveryRes { header: header(), response_code: code, evse_status: None }.encode(o)),
                common::Message::SessionStopReq(_) => frame(main, |o| common::SessionStopRes { header: header(), response_code: code }.encode(o)),
                _ => None,
            },
            Request::Dc(m) => match m {
                dc::Message::DcChargeParameterDiscoveryReq(_) => {
                    let res = self.cpd_res(code);
                    frame(dcm, |o| res.encode(o))
                }
                dc::Message::DcCableCheckReq(_) => frame(dcm, |o| dc::DcCableCheckRes { header: header(), response_code: code, evse_processing: Processing::Finished }.encode(o)),
                dc::Message::DcPreChargeReq(_) => frame(dcm, |o| dc::DcPreChargeRes { header: header(), response_code: code, evse_present_voltage: zero }.encode(o)),
                dc::Message::DcChargeLoopReq(_) => frame(dcm, |o| {
                    dc::DcChargeLoopRes {
                        header: header(),
                        response_code: code,
                        evse_status: None,
                        meter_info: None,
                        receipt: None,
                        evse_present_current: zero,
                        evse_present_voltage: zero,
                        evse_power_limit_achieved: false,
                        evse_current_limit_achieved: false,
                        evse_voltage_limit_achieved: false,
                        cl_res_control_mode: dc::ClResControlModeChoice::DynamicDc(dc::DynamicDcClResControlMode {
                            departure_time: None,
                            minimum_soc: None,
                            target_soc: None,
                            ack_max_delay: None,
                            evse_maximum_charge_power: zero,
                            evse_minimum_charge_power: zero,
                            evse_maximum_charge_current: zero,
                            evse_maximum_voltage: zero,
                        }),
                    }
                    .encode(o)
                }),
                dc::Message::DcWeldingDetectionReq(_) => frame(dcm, |o| dc::DcWeldingDetectionRes { header: header(), response_code: code, evse_present_voltage: zero }.encode(o)),
                _ => None,
            },
            Request::Ac(m) => match m {
                ac::Message::AcChargeParameterDiscoveryReq(_) => {
                    let res = self.ac_cpd_res(code);
                    frame(PayloadType::AC_MAINSTREAM, |o| res.encode(o))
                }
                ac::Message::AcChargeLoopReq(_) => frame(PayloadType::AC_MAINSTREAM, |o| {
                    ac::AcChargeLoopRes {
                        header: header(),
                        response_code: code,
                        evse_status: None,
                        meter_info: None,
                        receipt: None,
                        evse_target_frequency: None,
                        cl_res_control_mode: ac::ClResControlModeChoice::DynamicAc(ac::DynamicAcClResControlMode {
                            departure_time: None,
                            minimum_soc: None,
                            target_soc: None,
                            ack_max_delay: None,
                            evse_target_active_power: zero,
                            evse_target_active_power_l2: None,
                            evse_target_active_power_l3: None,
                            evse_target_reactive_power: None,
                            evse_target_reactive_power_l2: None,
                            evse_target_reactive_power_l3: None,
                            evse_present_active_power: None,
                            evse_present_active_power_l2: None,
                            evse_present_active_power_l3: None,
                        }),
                    }
                    .encode(o)
                }),
                _ => None,
            },
        }
    }
}

/// The message and response code of a frame the charger built (a response), and its name.
pub fn response_of(bytes: &[u8]) -> Option<(&'static str, ResponseMessage, ResponseCode)> {
    let Ok(Parse::Frame(f)) = parse(bytes, 1 << 20) else { return None };
    let mut mem = vec![0u8; 16384];
    let arena = Arena::new(&mut mem);
    use ResponseMessage as M;
    if f.payload_type == PayloadType::AC_MAINSTREAM {
        return Some(match ac::decode(f.payload, &arena).ok()? {
            ac::Message::AcChargeParameterDiscoveryRes(r) => ("AC_ChargeParameterDiscoveryRes", M::AcChargeParameterDiscovery, r.response_code),
            ac::Message::AcChargeLoopRes(r) => ("AC_ChargeLoopRes", M::AcChargeLoop, r.response_code),
            _ => return None,
        });
    }
    if f.payload_type != PayloadType::DC_MAINSTREAM
        && let Ok(m) = common::decode(f.payload, &arena) {
            return Some(match m {
                common::Message::SessionSetupRes(r) => ("SessionSetupRes", M::SessionSetup, r.response_code),
                common::Message::AuthorizationSetupRes(r) => ("AuthorizationSetupRes", M::AuthorizationSetup, r.response_code),
                common::Message::AuthorizationRes(r) => ("AuthorizationRes", M::Authorization, r.response_code),
                common::Message::ServiceDiscoveryRes(r) => ("ServiceDiscoveryRes", M::ServiceDiscovery, r.response_code),
                common::Message::ServiceDetailRes(r) => ("ServiceDetailRes", M::ServiceDetail, r.response_code),
                common::Message::ServiceSelectionRes(r) => ("ServiceSelectionRes", M::ServiceSelection, r.response_code),
                common::Message::ScheduleExchangeRes(r) => ("ScheduleExchangeRes", M::ScheduleExchange, r.response_code),
                common::Message::PowerDeliveryRes(r) => ("PowerDeliveryRes", M::PowerDelivery, r.response_code),
                common::Message::SessionStopRes(r) => ("SessionStopRes", M::SessionStop, r.response_code),
                _ => return None,
            });
        }
    Some(match dc::decode(f.payload, &arena).ok()? {
        dc::Message::DcChargeParameterDiscoveryRes(r) => ("DC_ChargeParameterDiscoveryRes", M::DcChargeParameterDiscovery, r.response_code),
        dc::Message::DcCableCheckRes(r) => ("DC_CableCheckRes", M::DcCableCheck, r.response_code),
        dc::Message::DcPreChargeRes(r) => ("DC_PreChargeRes", M::DcPreCharge, r.response_code),
        dc::Message::DcChargeLoopRes(r) => ("DC_ChargeLoopRes", M::DcChargeLoop, r.response_code),
        dc::Message::DcWeldingDetectionRes(r) => ("DC_WeldingDetectionRes", M::DcWeldingDetection, r.response_code),
        _ => return None,
    })
}
