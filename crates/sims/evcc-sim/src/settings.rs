//! What an `evcc-sim` process reads from its settings file (`--config`) and the command line.

use evcc_core::ev::DcConnector;
use evcc_core::log::Level;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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

/// TLS 1.3 with mutual authentication; the files are PEM.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    pub chain: String,
    pub key: String,
    pub trust: String,
    pub key_password: Option<String>,
    /// Also accept the ISO 15118-2 certificate profile (secp256r1).
    pub backwards_compatible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
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

    /// The session configuration these settings call for.
    pub fn session(&self) -> evcc_core::session::Config {
        let mut config = evcc_core::session::Config {
            allow_unsecured: self.tls.is_none(),
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
