//! What an `evcc-sim` process reads from its settings file (`--config`) and the command line.

use evcc_core::ev::DcConnector;
use evcc_core::log::Level;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The protocol the simulated car speaks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum Protocol {
    /// ISO 15118-20 (DC, Dynamic control mode).
    #[default]
    #[serde(rename = "iso15118-20")]
    Iso20,
    /// DIN SPEC 70121 (DC, external payment).
    #[serde(rename = "din70121")]
    Din,
}

/// The knobs of the DIN SPEC 70121 side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Din {
    /// The MAC address of the EVCC, its EVCCID (six octets).
    pub mac_address: [u8; 6],
    /// Send a `ChargingProfile` that follows the charger's schedule with `PowerDeliveryReq`.
    pub send_charging_profile: bool,
    /// Do the welding detection after the energy transfer (optional for the EVCC).
    pub welding_detection: bool,
    /// Send `ServiceCategory` `EVCharging` in `ServiceDiscoveryReq`.
    pub service_category: bool,
    /// Keep the charge loop's limits within those announced in the charge parameter discovery.
    pub enforce_cpd_limits: bool,
}

impl Default for Din {
    fn default() -> Self {
        // A locally administered address, as a car without a registered one has.
        Self { mac_address: [0x02, 0x00, 0x5E, 0x00, 0x53, 0x01], send_charging_profile: true, welding_detection: true, service_category: false, enforce_cpd_limits: true }
    }
}

/// The inlet the simulated car claims (value of the service parameter `Connector`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Connector {
    #[default]
    Core,
    Extended,
    Dual2,
    Dual4,
}

impl From<Connector> for DcConnector {
    fn from(c: Connector) -> Self {
        match c {
            Connector::Core => DcConnector::Core,
            Connector::Extended => DcConnector::Extended,
            Connector::Dual2 => DcConnector::Dual2,
            Connector::Dual4 => DcConnector::Dual4,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SlacValidation {
    #[default]
    Never,
    Ambiguous,
    Always,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    #[default]
    Info,
    Debug,
    Trace,
}

impl From<LogLevel> for Level {
    fn from(l: LogLevel) -> Self {
        match l {
            LogLevel::Error => Level::Error,
            LogLevel::Info => Level::Info,
            LogLevel::Debug => Level::Debug,
            LogLevel::Trace => Level::Trace,
        }
    }
}

/// The vehicle's contract for Plug & Charge: PEM files with the contract certificate chain
/// (the contract certificate first, then its sub-CAs; no root) and the contract's private key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Contract {
    pub chain: String,
    pub key: String,
    pub key_password: Option<String>,
}

/// What the vehicle tells the charger about its needs in `DC_ChargeParameterDiscoveryReq`,
/// `ScheduleExchangeReq` and the charge loop. Whole units, so that settings compare and
/// survive a round trip through files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_charge_power_w: u32,
    pub min_charge_power_w: u32,
    pub max_charge_current_a: u32,
    pub min_charge_current_a: u32,
    pub max_voltage_v: u32,
    pub min_voltage_v: u32,
    /// `TargetSOC` of the charge parameters and the schedule exchange (percent).
    pub target_soc: u8,
    /// `MinimumSOC` of the schedule exchange (percent).
    pub min_soc: Option<u8>,
    /// `DepartureTime`: seconds from the schedule exchange.
    pub departure_time_s: u32,
    pub target_energy_wh: u32,
    pub max_energy_wh: u32,
    pub min_energy_wh: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_charge_power_w: 100_000,
            min_charge_power_w: 100,
            max_charge_current_a: 200,
            min_charge_current_a: 1,
            max_voltage_v: 450,
            min_voltage_v: 200,
            target_soc: 80,
            min_soc: None,
            departure_time_s: 3600,
            target_energy_wh: 40_000,
            max_energy_wh: 60_000,
            min_energy_wh: 0,
        }
    }
}

/// The TLS versions a simulator speaks.
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

/// TLS with mutual authentication; the files are PEM.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Tls {
    pub chain: String,
    pub key: String,
    pub trust: String,
    pub key_password: Option<String>,
    /// Also accept the ISO 15118-2 certificate profile (secp256r1).
    pub backwards_compatible: bool,
    pub versions: TlsVersions,
    /// Check the charger's certificate chain against the SECC profiles of Annex B.
    pub certificate_checks: CertificateChecks,
    /// Check the revocation status of the charger's certificates with the OCSP response it
    /// staples (V2G20-1240, 2409 ff.).
    pub ocsp_checks: CertificateChecks,
    /// Ask for a status for every certificate below the root (V2G20-2416). OpenSSL delivers only
    /// the leaf's stapled response, so with a sub-CA this fails; without it only the leaf's is needed.
    pub ocsp_require_all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// The protocol to offer the charger.
    pub protocol: Protocol,
    /// DIN SPEC 70121.
    pub din: Din,
    /// Run SLAC on the interface before looking for a charger.
    pub slac: bool,
    /// `TT_matching_repetition` (default 10 s): how long SLAC keeps trying.
    pub slac_repetition_ms: Option<u32>,
    /// When SLAC validates the charger by control pilot toggles; the toggles are written to
    /// `control_pilot_file` for the simulated charger to count.
    pub slac_validation: SlacValidation,
    /// Toggles per validation (1 to 3); 0: chosen per run.
    pub slac_toggles: u8,
    /// Where the simulated control pilot leaves one octet per toggle (a path relative to the
    /// artifacts directory in a scenario).
    pub control_pilot_file: Option<String>,
    /// Without it the session is unsecured (SDP asks for `NoTls`).
    pub tls: Option<Tls>,
    /// Charge loop rounds before the car stops; 0 charges until the charger ends the session.
    pub stop_after: u32,
    pub connector: Connector,
    /// Ask for a service renegotiation after this many charge loop rounds (once; ignored by
    /// the session unless the charger offered it).
    pub renegotiate_after: Option<u32>,
    /// Ask for a schedule renegotiation (a side stream next to the charge loop) after this many
    /// charge loop rounds, once.
    pub renegotiate_schedule_after: Option<u32>,
    /// Stop as soon as a schedule renegotiation has finished.
    pub stop_when_rescheduled: bool,
    /// Pause the session (`SessionStopReq` Pause, connection closed) after this many charge
    /// loop rounds.
    pub pause_after: Option<u32>,
    /// Resume a paused session on a new connection after this many milliseconds; without it
    /// the process ends with the pause.
    pub resume_after_ms: Option<u32>,
    /// Answer an `EVSENotification` Pause with a pause of the session instead of a standby period.
    pub pause_on_notification: bool,
    pub log: LogLevel,
    /// Overrides of the session timing (milliseconds), for quicker runs.
    pub poll_interval_ms: Option<u32>,
    pub charge_loop_interval_ms: Option<u32>,
    /// The vehicle's needs, as sent to the charger.
    pub limits: Limits,
    /// A contract for Plug & Charge: the vehicle uses it when the charger offers PnC, and EIM
    /// otherwise. A scenario names none: the runner supplies the files.
    pub pnc: Option<Contract>,
    /// State of charge (percent) at the start of the session, for the battery model.
    pub start_soc: u8,
    /// How much faster than real time the battery model runs (1: real time).
    pub time_scale: u32,
    /// Ask to stop when the state of charge reaches `limits.target_soc`.
    pub stop_at_target_soc: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            protocol: Protocol::Iso20,
            din: Din::default(),
            slac: false,
            slac_repetition_ms: None,
            slac_validation: SlacValidation::Never,
            slac_toggles: 0,
            control_pilot_file: None,
            tls: None,
            stop_after: 3,
            connector: Connector::Core,
            renegotiate_after: None,
            renegotiate_schedule_after: None,
            stop_when_rescheduled: false,
            pause_after: None,
            resume_after_ms: None,
            pause_on_notification: false,
            log: LogLevel::Info,
            poll_interval_ms: None,
            charge_loop_interval_ms: None,
            limits: Limits::default(),
            pnc: None,
            start_soc: 20,
            time_scale: 1,
            stop_at_target_soc: false,
        }
    }
}

impl Settings {
    pub fn slac_config(&self) -> evcc_core::slac::Config {
        let mut config = evcc_core::slac::Config::DEFAULT;
        if let Some(ms) = self.slac_repetition_ms {
            config.repetition_ms = ms;
        }
        config.validation = match self.slac_validation {
            SlacValidation::Never => evcc_core::slac::Validation::Never,
            SlacValidation::Ambiguous => evcc_core::slac::Validation::Ambiguous,
            SlacValidation::Always => evcc_core::slac::Validation::Always,
        };
        config.toggles = self.slac_toggles;
        config
    }

    pub fn stop_after(&self) -> Option<u32> {
        (self.stop_after > 0).then_some(self.stop_after)
    }

    /// The DIN SPEC 70121 session configuration these settings call for.
    pub fn din_session(&self) -> evcc_core::din::Config {
        let mut config = evcc_core::din::Config {
            welding_detection: self.din.welding_detection,
            send_charging_profile: self.din.send_charging_profile,
            service_category: self.din.service_category,
            enforce_cpd_limits: self.din.enforce_cpd_limits,
            ..evcc_core::din::Config::DIN
        };
        if let Some(ms) = self.poll_interval_ms {
            config.poll_interval_ms = ms;
        }
        if let Some(ms) = self.charge_loop_interval_ms {
            config.charge_loop_interval_ms = ms;
        }
        config
    }

    /// The session configuration these settings call for.
    pub fn session(&self) -> evcc_core::session::Config {
        let mut config = evcc_core::session::Config {
            allow_unsecured: self.tls.is_none(),
            prefer_pnc: self.pnc.is_some(),
            ..evcc_core::session::Config::DC_20
        };
        if let Some(ms) = self.poll_interval_ms {
            config.poll_interval_ms = ms;
        }
        if let Some(ms) = self.charge_loop_interval_ms {
            config.charge_loop_interval_ms = ms;
        }
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_toml_and_a_file_names_only_what_differs() {
        let all = Settings {
            tls: Some(Tls {
                chain: "c".into(),
                key: "k".into(),
                trust: "t".into(),
                key_password: Some("p".into()),
                backwards_compatible: true,
                versions: TlsVersions::Tls13And12,
                certificate_checks: CertificateChecks::Warn,
                ocsp_checks: CertificateChecks::Off,
                ocsp_require_all: false,
            }),
            stop_after: 0,
            ..Default::default()
        };
        let text = toml::to_string(&all).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), all);
        assert_eq!(all.stop_after(), None);
        let s: Settings =
            toml::from_str("slac = true\nconnector = \"extended\"\nlog = \"error\"").unwrap();
        assert!(
            s.slac
                && s.connector == Connector::Extended
                && s.log == LogLevel::Error
                && s.stop_after == 3
        );
        assert!(toml::from_str::<Settings>("slack = true").is_err());
    }
}
