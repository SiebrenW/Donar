//! What an `evcc-sim` process reads from its settings file (`--config`) and the command line.

use evcc_core::ev::DcConnector;
use evcc_core::log::Level;
use serde::{Deserialize, Serialize};

/// The inlet the simulated car claims (value of the service parameter `Connector`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
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

/// TLS 1.3 with mutual authentication; the files are PEM.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    pub chain: String,
    pub key: String,
    pub trust: String,
    pub key_password: Option<String>,
    /// Also accept the ISO 15118-2 certificate profile (secp256r1).
    pub backwards_compatible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Run SLAC on the interface before looking for a charger.
    pub slac: bool,
    /// `TT_matching_repetition` (default 10 s): how long SLAC keeps trying.
    pub slac_repetition_ms: Option<u32>,
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
    pub log: LogLevel,
    /// Overrides of the session timing (milliseconds), for quicker runs.
    pub poll_interval_ms: Option<u32>,
    pub charge_loop_interval_ms: Option<u32>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            slac: false,
            slac_repetition_ms: None,
            tls: None,
            stop_after: 3,
            connector: Connector::Core,
            renegotiate_after: None,
            renegotiate_schedule_after: None,
            stop_when_rescheduled: false,
            log: LogLevel::Info,
            poll_interval_ms: None,
            charge_loop_interval_ms: None,
        }
    }
}

impl Settings {
    pub fn slac_config(&self) -> evcc_core::slac::Config {
        let mut config = evcc_core::slac::Config::DEFAULT;
        if let Some(ms) = self.slac_repetition_ms {
            config.repetition_ms = ms;
        }
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
