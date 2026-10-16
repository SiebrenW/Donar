//! The simulator's knobs.

use std::collections::BTreeMap;

use iso15118_20::common::*;
use iso15118_20::common_types::ResponseCode;
use serde::{Deserialize, Serialize};

/// Response codes and authorization services appear in files under their schema names
/// (`FAILED_SequenceError`, `EIM`).
macro_rules! schema_names {
    ($module:ident, $ty:ty) => {
        mod $module {
            use super::*;
            use serde::{Deserializer, Serializer, de::Error};

            #[allow(dead_code)] // `Authorization` is only written as a list, see `codes`
            pub fn serialize<S: Serializer>(value: &$ty, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(value.xml_name())
            }

            pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<$ty, D::Error> {
                let name = String::deserialize(d)?;
                <$ty>::ALL
                    .into_iter()
                    .find(|v| v.xml_name() == name)
                    .ok_or_else(|| D::Error::custom(format!("unknown {}: {name}", stringify!($ty))))
            }
        }
    };
}

schema_names!(code, ResponseCode);
schema_names!(authorization, Authorization);

mod codes {
    use super::*;
    use serde::{Deserializer, Serializer, ser::SerializeSeq};

    pub fn serialize<S: Serializer>(values: &[Authorization], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(values.len()))?;
        for v in values {
            seq.serialize_element(v.xml_name())?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Authorization>, D::Error> {
        #[derive(Deserialize)]
        struct One(#[serde(with = "authorization")] Authorization);
        Ok(Vec::<One>::deserialize(d)?
            .into_iter()
            .map(|One(a)| a)
            .collect())
    }
}

/// Everything a simulated charger process reads from its settings file (`evse-sim --config`):
/// the session knobs and the SLAC responder's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Answer SLAC on the interface (the responder follows `slac`).
    pub slac_responder: bool,
    pub session: Config,
    pub slac: crate::slac::Config,
}

/// A parameter set offered in `ServiceDetailRes`: the numbers of ISO 15118-20:2022 Table 207
/// by parameter name (`Connector`, `ControlMode`, `MobilityNeedsMode`, `Pricing`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfferedSet {
    pub id: u16,
    pub values: BTreeMap<String, i32>,
}

impl OfferedSet {
    pub fn new<'a>(id: u16, values: impl IntoIterator<Item = (&'a str, i32)>) -> Self {
        Self {
            id,
            values: values
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub evse_id: String,
    pub session_id: [u8; 8],
    /// Answer the SupportedAppProtocolReq with `FailedNoNegotiation` regardless of the offer.
    pub reject_protocol: bool,
    /// Response code for `SessionSetupRes`.
    #[serde(with = "code")]
    pub setup_code: ResponseCode,
    /// Never answer a `SessionSetupReq` (the EV should time out).
    pub silent_on_setup: bool,
    /// How many `AuthorizationRes` carry `Ongoing` before one says `Finished`.
    pub authorization_ongoing_responses: u32,
    /// Never finish authorization: answer `Ongoing` forever (the EV's ongoing timeout should fire).
    pub authorization_never_finishes: bool,
    /// Response code of the `AuthorizationRes` that would finish authorization.
    #[serde(with = "code")]
    pub authorization_code: ResponseCode,
    /// Authorization services offered in `AuthorizationSetupRes`.
    #[serde(with = "codes")]
    pub offered_authorization: Vec<Authorization>,
    /// Energy transfer services offered in `ServiceDiscoveryRes`.
    pub energy_services: Vec<u16>,
    /// Parameter sets answered for every offered service.
    pub parameter_sets: Vec<OfferedSet>,
    /// Response code of the `ServiceSelectionRes` for a valid selection.
    #[serde(with = "code")]
    pub selection_code: ResponseCode,
    /// Response code of `DC_ChargeParameterDiscoveryRes`.
    #[serde(with = "code")]
    pub cpd_code: ResponseCode,
    /// How many `ScheduleExchangeRes` carry `Ongoing` before one says `Finished`.
    pub schedule_ongoing_responses: u32,
    /// Response code of the `ScheduleExchangeRes` that finishes the exchange.
    #[serde(with = "code")]
    pub schedule_code: ResponseCode,
    /// How many `DC_CableCheckRes` carry `Ongoing` before one says `Finished`.
    pub cable_check_ongoing_responses: u32,
    /// Response code of the `DC_CableCheckRes` that finishes the check.
    #[serde(with = "code")]
    pub cable_check_code: ResponseCode,
    /// How many `DC_PreChargeReq` the EVSE answers before the EV considers the voltage reached
    /// (the simulator only reports the target voltage once this many arrived).
    pub precharge_requests_to_settle: u32,
    /// Response code of the `PowerDeliveryRes` to `Start`.
    #[serde(with = "code")]
    pub power_delivery_code: ResponseCode,
    /// After this many `DC_ChargeLoopRes` the EVSE asks the EV to terminate (`EVSENotification`).
    pub terminate_after_loops: Option<u32>,
    /// After this many `DC_ChargeLoopRes` the EVSE asks the EV to pause (`EVSENotification`
    /// Pause, `NotificationMaxDelay` 60 s, V2G20-1850) and repeats it until the EV reacts.
    pub pause_after_loops: Option<u32>,
    /// Accept `PowerDeliveryReq(Standby)`; otherwise answer `WARNING_StandbyNotAllowed`
    /// (V2G20-1387) and stop asking for the pause.
    pub standby_allowed: bool,
    /// How many `DC_ChargeLoopRes` a standby period lasts before the EVSE sends `ExitStandby`
    /// (V2G20-1854), repeated until the EV leaves it.
    pub standby_loops: u32,
    /// `ServiceRenegotiationSupported` in `ServiceDiscoveryRes`.
    pub service_renegotiation_supported: bool,
    /// After this many `DC_ChargeLoopRes` the EVSE asks the EV for a service renegotiation
    /// (`EVSENotification`, V2G20-1651) and repeats it until the EV reacts; once per session.
    /// Sent even when not supported, to see the EV ignore it (V2G20-1652).
    pub service_renegotiation_after_loops: Option<u32>,
    /// Offer service renegotiation but answer `SessionStopReq(ServiceRenegotiation)` with
    /// `FAILED_NoServiceRenegotiationSupported` (V2G20-1955).
    pub refuse_service_renegotiation: bool,
    /// Response code of the `DC_WeldingDetectionRes`.
    #[serde(with = "code")]
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
            parameter_sets: vec![OfferedSet::new(
                1,
                [
                    ("Connector", 1),
                    ("ControlMode", 2),
                    ("MobilityNeedsMode", 1),
                    ("Pricing", 0),
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
            service_renegotiation_supported: false,
            service_renegotiation_after_loops: None,
            refuse_service_renegotiation: false,
            pause_after_loops: None,
            standby_allowed: true,
            standby_loops: 2,
            welding_code: ResponseCode::Ok,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_default_config_survives_toml() {
        let text = toml::to_string(&Config::default()).unwrap();
        assert_eq!(toml::from_str::<Config>(&text).unwrap(), Config::default());
    }

    #[test]
    fn a_file_names_only_what_differs() {
        let config: Config = toml::from_str(
            r#"
            setup_code = "FAILED_SequenceError"
            offered_authorization = ["EIM", "PnC"]
            terminate_after_loops = 4
            [[parameter_sets]]
            id = 7
            values = { Connector = 2, ControlMode = 2, MobilityNeedsMode = 1 }
            "#,
        )
        .unwrap();
        assert_eq!(config.setup_code, ResponseCode::FailedSequenceError);
        assert_eq!(
            config.offered_authorization,
            [Authorization::Eim, Authorization::PnC]
        );
        assert_eq!(config.terminate_after_loops, Some(4));
        assert_eq!(
            config.parameter_sets,
            [OfferedSet::new(
                7,
                [
                    ("Connector", 2),
                    ("ControlMode", 2),
                    ("MobilityNeedsMode", 1)
                ]
            )]
        );
        assert_eq!(config.evse_id, Config::default().evse_id);
    }

    #[test]
    fn unknown_keys_and_names_are_errors() {
        assert!(toml::from_str::<Config>("setup_cod = \"OK\"").is_err());
        assert!(toml::from_str::<Config>("setup_code = \"Nope\"").is_err());
    }
}
