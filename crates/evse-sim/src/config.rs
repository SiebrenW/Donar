//! The simulator's knobs.

use iso15118_20::common::*;
use iso15118_20::common_types::ResponseCode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub evse_id: String,
    pub session_id: [u8; 8],
    /// Answer the SupportedAppProtocolReq with `FailedNoNegotiation` regardless of the offer.
    pub reject_protocol: bool,
    /// Response code for `SessionSetupRes`.
    pub setup_code: ResponseCode,
    /// Never answer a `SessionSetupReq` (the EV should time out).
    pub silent_on_setup: bool,
    /// How many `AuthorizationRes` carry `Ongoing` before one says `Finished`.
    pub authorization_ongoing_responses: u32,
    /// Never finish authorization: answer `Ongoing` forever (the EV's ongoing timeout should fire).
    pub authorization_never_finishes: bool,
    /// Response code of the `AuthorizationRes` that would finish authorization.
    pub authorization_code: ResponseCode,
    /// Authorization services offered in `AuthorizationSetupRes`.
    pub offered_authorization: Vec<Authorization>,
    /// Energy transfer services offered in `ServiceDiscoveryRes`.
    pub energy_services: Vec<u16>,
    /// Parameter sets answered for every offered service: `(id, [(name, integer value)])`.
    pub parameter_sets: Vec<(u16, Vec<(String, i32)>)>,
    /// Response code of the `ServiceSelectionRes` for a valid selection.
    pub selection_code: ResponseCode,
    /// Response code of `DC_ChargeParameterDiscoveryRes`.
    pub cpd_code: ResponseCode,
    /// How many `ScheduleExchangeRes` carry `Ongoing` before one says `Finished`.
    pub schedule_ongoing_responses: u32,
    /// Response code of the `ScheduleExchangeRes` that finishes the exchange.
    pub schedule_code: ResponseCode,
    /// How many `DC_CableCheckRes` carry `Ongoing` before one says `Finished`.
    pub cable_check_ongoing_responses: u32,
    /// Response code of the `DC_CableCheckRes` that finishes the check.
    pub cable_check_code: ResponseCode,
    /// How many `DC_PreChargeReq` the EVSE answers before the EV considers the voltage reached
    /// (the simulator only reports the target voltage once this many arrived).
    pub precharge_requests_to_settle: u32,
    /// Response code of the `PowerDeliveryRes` to `Start`.
    pub power_delivery_code: ResponseCode,
    /// After this many `DC_ChargeLoopRes` the EVSE asks the EV to terminate (`EVSENotification`).
    pub terminate_after_loops: Option<u32>,
    /// Response code of the `DC_WeldingDetectionRes`.
    pub welding_code: ResponseCode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            evse_id: "DE*SIM*E0001".into(),
            session_id: [0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18],
            reject_protocol: false,
            setup_code: ResponseCode::OkNewSessionEstablished,
            silent_on_setup: false,
            authorization_ongoing_responses: 0,
            authorization_never_finishes: false,
            authorization_code: ResponseCode::Ok,
            offered_authorization: vec![Authorization::Eim],
            energy_services: vec![2],
            // Connector 1, ControlMode 2 (Dynamic), MobilityNeedsMode 1, Pricing 0; the
            // numbers are those of ISO 15118-20:2022 Table 207.
            parameter_sets: vec![(
                1,
                vec![
                    ("Connector".into(), 1),
                    ("ControlMode".into(), 2),
                    ("MobilityNeedsMode".into(), 1),
                    ("Pricing".into(), 0),
                ],
            )],
            selection_code: ResponseCode::Ok,
            cpd_code: ResponseCode::Ok,
            schedule_ongoing_responses: 0,
            schedule_code: ResponseCode::Ok,
            cable_check_ongoing_responses: 0,
            cable_check_code: ResponseCode::Ok,
            precharge_requests_to_settle: 1,
            power_delivery_code: ResponseCode::Ok,
            terminate_after_loops: None,
            welding_code: ResponseCode::Ok,
        }
    }
}

