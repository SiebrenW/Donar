//! The simulator's knobs.

use std::collections::BTreeMap;

use iso15118_20::common::*;
use iso15118_20::common_types::ResponseCode;
use schemars::JsonSchema;
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum TlsVersions {
    /// TLS 1.3 only (ISO 15118-20).
    #[default]
    #[serde(rename = "1.3")]
    Tls13,
    /// TLS 1.3, and TLS 1.2 with `ECDHE-ECDSA-AES128-SHA256` for an ISO 15118-2 peer; implies
    /// `backwards_compatible`.
    #[serde(rename = "1.3+1.2")]
    Tls13And12,
    /// TLS 1.2 only: plays an ISO 15118-2 peer; implies `backwards_compatible`.
    #[serde(rename = "1.2")]
    Tls12,
}

impl From<TlsVersions> for evcc_linux::tls::Versions {
    fn from(v: TlsVersions) -> Self {
        match v {
            TlsVersions::Tls13 => Self::Tls13,
            TlsVersions::Tls13And12 => Self::Tls13And12,
            TlsVersions::Tls12 => Self::Tls12,
        }
    }
}

/// What to do with a peer certificate chain that does not meet the profiles of ISO 15118-20 Annex B.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CertificateChecks {
    #[default]
    Off,
    /// Check and log the violations, go on.
    Warn,
    /// Check; a violation ends the connection.
    Enforce,
}

impl From<CertificateChecks> for evcc_linux::tls::CertificateChecks {
    fn from(v: CertificateChecks) -> Self {
        match v {
            CertificateChecks::Off => Self::Off,
            CertificateChecks::Warn => Self::Warn,
            CertificateChecks::Enforce => Self::Enforce,
        }
    }
}

/// Everything a simulated charger process reads from its settings file (`evse-sim --config`):
/// the session knobs and the SLAC responder's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Answer SLAC on the interface (the responder follows `slac`).
    pub slac_responder: bool,
    /// The TLS versions to speak (when the charger has certificates).
    pub tls_versions: TlsVersions,
    /// Also accept secp256r1 certificates (the ISO 15118-2 profile).
    pub backwards_compatible: bool,
    /// Check the vehicle's certificate chain against the vehicle profiles of Annex B.
    pub certificate_checks: CertificateChecks,
    /// The simulated control pilot the vehicle toggles: one octet per toggle is appended to
    /// this file (a path relative to the artifacts directory in a scenario). The charger counts
    /// them when the vehicle asks for the validation by BCB-Toggle.
    pub control_pilot_file: Option<String>,
    /// Serve this many sessions (connections) one after the other; 0 for ever.
    pub sessions: u32,
    pub session: Config,
    pub slac: crate::slac::Config,
}

impl Default for Settings {
    fn default() -> Self {
        Self { slac_responder: false, tls_versions: TlsVersions::Tls13, backwards_compatible: false, certificate_checks: CertificateChecks::Off, control_pilot_file: None, sessions: 1, session: Config::default(), slac: Default::default() }
    }
}

/// Names for the values of the service parameters (ISO 15118-20:2022 Table 207) that a file may
/// use instead of the numbers, and that are written back as names: `Connector` (the names of
/// the vehicle's `connector` setting), `ControlMode`, `MobilityNeedsMode` and `Pricing`.
const NAMED_VALUES: &[(&str, &[(&str, i32)])] = &[
    ("Connector", &[("core", 1), ("extended", 2), ("dual2", 3), ("dual4", 4)]),
    ("ControlMode", &[("scheduled", 1), ("dynamic", 2)]),
    ("MobilityNeedsMode", &[("evcc", 1), ("evcc_and_secc", 2)]),
    ("Pricing", &[("none", 0), ("absolute", 1), ("levels", 2)]),
];

/// The names of an AC service's `Connector` (Table 205). A set of an AC service is told apart by
/// its `EVSENominalVoltage`, which no DC set has.
const AC_CONNECTORS: &[(&str, i32)] = &[("single_phase", 1), ("three_phase", 2)];

fn names_of(parameter: &str, ac: bool) -> Option<&'static [(&'static str, i32)]> {
    if ac && parameter == "Connector" {
        return Some(AC_CONNECTORS);
    }
    NAMED_VALUES.iter().find(|(p, _)| *p == parameter).map(|(_, n)| *n)
}

/// The name of a parameter value of a DC set, if it has one.
pub fn value_name(parameter: &str, value: i32) -> Option<&'static str> {
    named(parameter, value, false)
}

fn named(parameter: &str, value: i32, ac: bool) -> Option<&'static str> {
    names_of(parameter, ac)?.iter().find(|(_, v)| *v == value).map(|(n, _)| *n)
}

mod parameter_values {
    use super::*;
    use serde::{Deserializer, Serializer, de::Error, ser::SerializeMap};

    /// The names where there are some, the numbers otherwise.
    pub fn serialize<S: Serializer>(values: &BTreeMap<String, i32>, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(values.len()))?;
        let ac = values.contains_key("EVSENominalVoltage");
        for (parameter, value) in values {
            match named(parameter, *value, ac) {
                Some(name) => map.serialize_entry(parameter, name)?,
                None => map.serialize_entry(parameter, value)?,
            }
        }
        map.end()
    }

    /// Numbers, or the names of [`NAMED_VALUES`].
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, i32>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Value {
            Number(i32),
            Name(String),
        }
        let raw = BTreeMap::<String, Value>::deserialize(d)?;
        let ac = raw.contains_key("EVSENominalVoltage");
        let mut out = BTreeMap::new();
        for (parameter, value) in raw {
            let number = match value {
                Value::Number(n) => n,
                Value::Name(name) => {
                    let names = names_of(&parameter, ac).ok_or_else(|| D::Error::custom(format!("{parameter} has no named values, use a number")))?;
                    names
                        .iter()
                        .find(|(n, _)| *n == name)
                        .map(|(_, v)| *v)
                        .ok_or_else(|| D::Error::custom(format!("{parameter}: unknown value {name:?}; one of {}", names.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", "))))?
                }
            };
            out.insert(parameter, number);
        }
        Ok(out)
    }
}

/// A parameter set offered in `ServiceDetailRes`: the values of ISO 15118-20:2022 Table 207
/// by parameter name (`Connector`, `ControlMode`, `MobilityNeedsMode`, `Pricing`). Values are
/// written and read by name where there is one (`Connector = "extended"`, the names of the
/// vehicle's `connector` setting); numbers are accepted too.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OfferedSet {
    pub id: u16,
    #[serde(with = "parameter_values")]
    #[schemars(with = "BTreeMap<String, serde_json::Value>")]
    pub values: BTreeMap<String, i32>,
}

impl OfferedSet {
    /// `id 2: Connector=extended(2), ControlMode=dynamic(2)`, for logs.
    pub fn describe(&self) -> String {
        let ac = self.values.contains_key("EVSENominalVoltage");
        let values: Vec<String> = self
            .values
            .iter()
            .map(|(k, v)| match named(k, *v, ac) {
                Some(name) => format!("{k}={name}({v})"),
                None => format!("{k}={v}"),
            })
            .collect();
        format!("set {}: {}", self.id, values.join(", "))
    }

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

/// What the charger offers in `DC_ChargeParameterDiscoveryRes` and the charge loop. Whole units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_charge_power_w: u32,
    pub min_charge_power_w: u32,
    pub max_charge_current_a: u32,
    pub min_charge_current_a: u32,
    pub max_voltage_v: u32,
    pub min_voltage_v: u32,
    /// `EVSEPowerRampLimitation` in percent per minute (8.3.5.5.2), when the charger limits how
    /// fast the power may change.
    pub power_ramp_limitation_percent_per_min: Option<u32>,
    /// What the charger takes back from the vehicle in a BPT session (service 6).
    pub max_discharge_power_w: u32,
    pub min_discharge_power_w: u32,
    pub max_discharge_current_a: u32,
    pub min_discharge_current_a: u32,
    /// The AC services (1 and 5), per phase: what the charger delivers (and takes back) on each
    /// line.
    pub ac_max_charge_power_w: u32,
    pub ac_min_charge_power_w: u32,
    pub ac_max_discharge_power_w: u32,
    pub ac_min_discharge_power_w: u32,
    /// Lines the charger has: 1, or 3 (then every power is also given for lines 2 and 3).
    pub ac_phases: u8,
    pub ac_nominal_frequency_hz: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_charge_power_w: 150_000,
            min_charge_power_w: 1000,
            max_charge_current_a: 300,
            min_charge_current_a: 2,
            max_voltage_v: 920,
            min_voltage_v: 150,
            power_ramp_limitation_percent_per_min: None,
            max_discharge_power_w: 11_000,
            min_discharge_power_w: 100,
            max_discharge_current_a: 30,
            min_discharge_current_a: 1,
            ac_max_charge_power_w: 3680,
            ac_min_charge_power_w: 1380,
            ac_max_discharge_power_w: 3680,
            ac_min_discharge_power_w: 500,
            ac_phases: 3,
            ac_nominal_frequency_hz: 50,
        }
    }
}

/// How the charger treats the contract of a Plug & Charge authorization (offered with
/// `offered_authorization = ["EIM", "PnC"]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Pnc {
    /// PEM trust anchors (the eMSP roots) for the contract certificate chain. The `evse-sim`
    /// command fills it from `--pnc-trust FILE`. Without any the chain is not validated; the
    /// challenge and the signature are always checked.
    pub trust_pem: Option<String>,
    /// Hold the chain to the profiles of Annex B (tables B.9, B.10) and not only to path
    /// validation: validity and signatures.
    pub check_profile: bool,
}

/// A protocol the charger speaks; it picks the one the vehicle prefers of those both have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Protocol {
    /// ISO 15118-20 (DC, Dynamic control mode).
    #[serde(rename = "iso15118-20")]
    Iso20,
    /// DIN SPEC 70121 (DC, external payment).
    #[serde(rename = "din70121")]
    Din,
    /// ISO 15118-2 (DC and AC).
    #[serde(rename = "iso15118-2")]
    Iso2,
}

/// An energy transfer mode of ISO 15118-2 the charger offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Iso2Mode {
    #[serde(rename = "AC_single_phase_core")]
    AcSinglePhaseCore,
    #[serde(rename = "AC_three_phase_core")]
    AcThreePhaseCore,
    #[serde(rename = "DC_core")]
    DcCore,
    #[serde(rename = "DC_extended")]
    DcExtended,
    #[serde(rename = "DC_combo_core")]
    DcComboCore,
}

/// The knobs of the ISO 15118-2 side (the other knobs of [`Config`] apply as well: `limits` for DC,
/// `setup_code`, `authorization_*`, `selection_code`, `cpd_code`, `cable_check_*`,
/// `precharge_requests_to_settle`, `power_delivery_code`, `welding_code`, `silent`, `session_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Iso2 {
    /// The energy transfer modes of `ServiceDiscoveryRes`.
    pub modes: Vec<Iso2Mode>,
    pub evse_id: String,
    /// Also offer the payment option `Contract` (Plug & Charge).
    pub offer_contract_payment: bool,
    /// How many `ChargeParameterDiscoveryRes` carry `Ongoing` before one says `Finished`.
    pub cpd_ongoing_responses: u32,
    /// After this many `CurrentDemandRes` or `ChargingStatusRes` the charger asks the vehicle to stop
    /// (`EVSE_Shutdown` for DC, the notification `StopCharging` for AC).
    pub shutdown_after_loops: Option<u32>,
    /// AC: `EVSENominalVoltage` (V) and `EVSEMaxCurrent` (A).
    pub ac_nominal_voltage_v: u32,
    pub ac_max_current_a: u32,
    /// After this many loop answers the charger asks for new charge parameters (the notification
    /// `ReNegotiation`), once.
    pub renegotiate_after_loops: Option<u32>,
    /// A contract session: every this many loop answers carry a meter reading and ask for a signed
    /// `MeteringReceiptReq`.
    pub receipt_every_loops: Option<u32>,
    /// Value-added services (ids) listed in `ServiceDiscoveryRes`; `ServiceDetailReq` for one gets
    /// two parameter sets (port 80 and 443).
    pub value_added_services: Vec<u16>,
    /// Offer two schedule tuples (11 kW) with sales tariffs: tuple 1 is dear in the first four hours (price level 3, then 0),
    /// tuple 2 is cheap in the first four hours (0, then 3). A vehicle that compares them takes tuple 2.
    pub sales_tariffs: bool,
}

impl Default for Iso2 {
    fn default() -> Self {
        Self {
            modes: vec![Iso2Mode::AcSinglePhaseCore, Iso2Mode::AcThreePhaseCore, Iso2Mode::DcCore, Iso2Mode::DcExtended, Iso2Mode::DcComboCore],
            evse_id: "DE*ABC*E1234".into(),
            offer_contract_payment: false,
            cpd_ongoing_responses: 0,
            shutdown_after_loops: None,
            ac_nominal_voltage_v: 230,
            ac_max_current_a: 32,
            renegotiate_after_loops: None,
            receipt_every_loops: None,
            value_added_services: Vec::new(),
            sales_tariffs: false,
        }
    }
}

/// The energy transfer type of DIN SPEC 70121 the charger offers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum DinEnergyTransfer {
    /// The core pins of an IEC 62196-3 configuration CC or DD connector.
    #[serde(rename = "DC_core")]
    DcCore,
    /// The extended pins of a configuration EE or FF connector (a CCS charger).
    #[default]
    #[serde(rename = "DC_extended")]
    DcExtended,
}

/// The knobs of the DIN SPEC 70121 side (the other knobs of [`Config`] apply to it as well:
/// `limits`, `setup_code`, `authorization_*`, `selection_code`, `cpd_code`, `cable_check_*`,
/// `precharge_requests_to_settle`, `power_delivery_code`, `welding_code`, `silent`, `session_id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Din {
    /// The `EnergyTransferType` of `ServiceDiscoveryRes`.
    pub energy_transfer: DinEnergyTransfer,
    /// The EVSE id in the format of DIN SPEC 91286 (`49*89*6360`), sent as hexBinary (V2G-DC-621).
    pub evse_id: String,
    /// Also offer the payment option `Contract` (DIN SPEC 70121 defines `ExternalPayment` only).
    pub offer_contract_payment: bool,
    /// How many `ChargeParameterDiscoveryRes` carry `Ongoing` before one says `Finished`.
    pub cpd_ongoing_responses: u32,
    /// After this many `CurrentDemandRes` the charger asks the vehicle to stop with the status
    /// code `EVSE_Shutdown` (V2G-DC-650).
    pub shutdown_after_loops: Option<u32>,
}

impl Default for Din {
    fn default() -> Self {
        Self { energy_transfer: DinEnergyTransfer::default(), evse_id: "49*89*6360".into(), offer_contract_payment: false, cpd_ongoing_responses: 0, shutdown_after_loops: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The protocols the charger speaks.
    pub protocols: Vec<Protocol>,
    /// DIN SPEC 70121.
    pub din: Din,
    /// ISO 15118-2.
    pub iso2: Iso2,
    /// Plug & Charge.
    pub pnc: Pnc,
    /// Answer a charge loop request whose limits break V2G20-2198 or 2199 with `FAILED`; without
    /// it the violation is only noted.
    pub fail_on_limit_violation: bool,
    /// Receive but do not answer anything (to see the vehicle time out). A test platform can
    /// switch it on and off during a session.
    pub silent: bool,
    /// The limits offered to the vehicle.
    pub limits: Limits,
    pub evse_id: String,
    pub session_id: [u8; 8],
    /// Answer the SupportedAppProtocolReq with `FailedNoNegotiation` regardless of the offer.
    pub reject_protocol: bool,
    /// Response code for `SessionSetupRes`.
    #[serde(with = "code")]
    #[schemars(with = "String")]
    pub setup_code: ResponseCode,
    /// Never answer a `SessionSetupReq` (the EV should time out).
    pub silent_on_setup: bool,
    /// How many `AuthorizationRes` carry `Ongoing` before one says `Finished`.
    pub authorization_ongoing_responses: u32,
    /// Never finish authorization: answer `Ongoing` forever (the EV's ongoing timeout should fire).
    pub authorization_never_finishes: bool,
    /// Response code of the `AuthorizationRes` that would finish authorization.
    #[serde(with = "code")]
    #[schemars(with = "String")]
    pub authorization_code: ResponseCode,
    /// Authorization services offered in `AuthorizationSetupRes`.
    #[serde(with = "codes")]
    #[schemars(with = "Vec<String>")]
    pub offered_authorization: Vec<Authorization>,
    /// Energy transfer services offered in `ServiceDiscoveryRes`.
    pub energy_services: Vec<u16>,
    /// Parameter sets answered for every offered DC service.
    pub parameter_sets: Vec<OfferedSet>,
    /// Parameter sets answered for the AC services (Table 205: `Connector` 1 single phase, 2 three
    /// phase, `EVSENominalVoltage`, `ControlMode`, `MobilityNeedsMode`, `Pricing`).
    pub ac_parameter_sets: Vec<OfferedSet>,
    /// The charger's meter: its id, whether `MeterInfo` goes into every charge loop answer (V2G20-1833;
    /// otherwise when the vehicle asks, V2G20-1081), and the notification `MeteringConfirmation`
    /// (Plug & Charge only, V2G20-2704) with a receipt, every so many charge loop answers.
    pub meter_id: String,
    pub meter_info_always: bool,
    pub metering_confirmation_every_loops: Option<u32>,
    pub receipt_every_loops: Option<u32>,
    /// Scheduled control mode: offer prices with the schedules (tuple 1: 0.40 per kWh, tuple 2: 0.20), so that a
    /// vehicle that compares them has something to choose from.
    pub schedule_prices: bool,
    /// With `MobilityNeedsMode` 2 the charger provides the mobility needs (a driver told it on a display
    /// there): the departure time in seconds from the answer, the minimum and the target state of charge
    /// in percent, named in the `ScheduleExchangeRes`. Left out when `None`. Parameter sets with that
    /// mode are offered after the others (DC ids 5 to 8, AC ids 5 and 6).
    pub mobility_departure_s: Option<u32>,
    /// The charger has no power: `GoToPause` in the answer to the schedule exchange (V2G20-2121), ISO 15118-20 only.
    pub no_power: bool,
    pub mobility_minimum_soc: Option<u8>,
    pub mobility_target_soc: Option<u8>,
    /// Scheduled control mode: how long one entry of an offered power schedule lasts, in seconds.
    pub schedule_entry_s: u32,
    /// Response code of the `ServiceSelectionRes` for a valid selection.
    #[serde(with = "code")]
    #[schemars(with = "String")]
    pub selection_code: ResponseCode,
    /// Response code of `DC_ChargeParameterDiscoveryRes`.
    #[serde(with = "code")]
    #[schemars(with = "String")]
    pub cpd_code: ResponseCode,
    /// How many `ScheduleExchangeRes` carry `Ongoing` before one says `Finished`.
    pub schedule_ongoing_responses: u32,
    /// Response code of the `ScheduleExchangeRes` that finishes the exchange.
    #[serde(with = "code")]
    #[schemars(with = "String")]
    pub schedule_code: ResponseCode,
    /// How many `DC_CableCheckRes` carry `Ongoing` before one says `Finished`.
    pub cable_check_ongoing_responses: u32,
    /// Response code of the `DC_CableCheckRes` that finishes the check.
    #[serde(with = "code")]
    #[schemars(with = "String")]
    pub cable_check_code: ResponseCode,
    /// How many `DC_PreChargeReq` the EVSE answers before the EV considers the voltage reached
    /// (the simulator only reports the target voltage once this many arrived).
    pub precharge_requests_to_settle: u32,
    /// Response code of the `PowerDeliveryRes` to `Start`.
    #[serde(with = "code")]
    #[schemars(with = "String")]
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
    /// After this many `DC_ChargeLoopRes` the EVSE asks the EV for a schedule renegotiation
    /// (`EVSENotification`, V2G20-1845) and repeats it until the exchange on the side stream
    /// is done; once per session.
    pub schedule_renegotiation_after_loops: Option<u32>,
    /// Never answer a message on the schedule renegotiation side stream (the EV should time out).
    pub side_stream_silent: bool,
    /// Do not recognise the id of a paused session in `SessionSetupReq`: answer with a new
    /// session (V2G20-2547) and make the EV authorize again.
    pub forget_paused_sessions: bool,
    /// Response code of the `DC_WeldingDetectionRes`.
    #[serde(with = "code")]
    #[schemars(with = "String")]
    pub welding_code: ResponseCode,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            protocols: vec![Protocol::Iso20, Protocol::Din, Protocol::Iso2],
            din: Din::default(),
            iso2: Iso2::default(),
            pnc: Pnc::default(),
            silent: false,
            fail_on_limit_violation: false,
            limits: Limits::default(),
            evse_id: "DE*SIM*E0001".into(),
            session_id: [0xA1, 0xB2, 0xC3, 0xD4, 0xE5, 0xF6, 0x07, 0x18],
            reject_protocol: false,
            setup_code: ResponseCode::OkNewSessionEstablished,
            silent_on_setup: false,
            authorization_ongoing_responses: 0,
            authorization_never_finishes: false,
            authorization_code: ResponseCode::Ok,
            offered_authorization: vec![Authorization::Eim],
            energy_services: vec![1, 2, 5, 6],
            // One set per inlet (Connector 1 to 4), ControlMode 2 (Dynamic), MobilityNeedsMode 1,
            // Pricing 0, so that any vehicle finds its own; the numbers are those of
            // ISO 15118-20:2022 Table 207.
            // Then the same with MobilityNeedsMode 2 (ids 5 to 8, V2G20-2663: Dynamic only).
            parameter_sets: [1, 2]
                .into_iter()
                .flat_map(|mobility| (1..=4).map(move |connector| (mobility, connector)))
                .enumerate()
                .map(|(i, (mobility, connector))| OfferedSet::new(i as u16 + 1, [("Connector", connector), ("ControlMode", 2), ("MobilityNeedsMode", mobility), ("Pricing", 0)]))
                .collect(),
            // Dynamic control for each connection (ids 1 and 2), then Scheduled (3 and 4).
            ac_parameter_sets: [(2, 1), (1, 1), (2, 2)]
                .into_iter()
                .flat_map(|(mode, mobility)| (1..=2).map(move |connector| (mode, mobility, connector)))
                .enumerate()
                .map(|(i, (mode, mobility, connector))| OfferedSet::new(i as u16 + 1, [("Connector", connector), ("EVSENominalVoltage", 400), ("ControlMode", mode), ("MobilityNeedsMode", mobility), ("Pricing", 0)]))
                .collect(),
            meter_id: "DE*SIM*M0001".to_string(),
            meter_info_always: false,
            metering_confirmation_every_loops: None,
            receipt_every_loops: None,
            no_power: false,
            mobility_departure_s: None,
            mobility_minimum_soc: None,
            mobility_target_soc: None,
            schedule_prices: false,
            schedule_entry_s: 3600,
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
            schedule_renegotiation_after_loops: None,
            side_stream_silent: false,
            forget_paused_sessions: false,
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
    fn parameter_values_may_be_named() {
        let set: OfferedSet = toml::from_str("id = 3\nvalues = { Connector = \"extended\", ControlMode = \"dynamic\", Pricing = 0 }").unwrap();
        assert_eq!(set, OfferedSet::new(3, [("Connector", 2), ("ControlMode", 2), ("Pricing", 0)]));
        assert_eq!(set.describe(), "set 3: Connector=extended(2), ControlMode=dynamic(2), Pricing=none(0)");
        // Written back by name, with a number for a value that has none.
        let json = serde_json::to_string(&OfferedSet::new(1, [("Connector", 1), ("Pricing", 2), ("Other", 9)])).unwrap();
        assert_eq!(json, r#"{"id":1,"values":{"Connector":"core","Other":9,"Pricing":"levels"}}"#);
        let e = toml::from_str::<OfferedSet>("id = 1\nvalues = { Connector = \"big\" }").unwrap_err().to_string();
        assert!(e.contains("core, extended, dual2, dual4"), "{e}");
        assert!(toml::from_str::<OfferedSet>("id = 1\nvalues = { Pricing = \"free\" }").is_err());
        assert!(toml::from_str::<OfferedSet>("id = 1\nvalues = { Other = \"x\" }").is_err());
    }

    #[test]
    fn unknown_keys_and_names_are_errors() {
        assert!(toml::from_str::<Config>("setup_cod = \"OK\"").is_err());
        assert!(toml::from_str::<Config>("setup_code = \"Nope\"").is_err());
    }
}
