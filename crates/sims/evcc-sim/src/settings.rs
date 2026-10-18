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
    /// ISO 15118-2 (DC and AC).
    #[serde(rename = "iso15118-2")]
    Iso2,
}

/// What the simulated car charges with under ISO 15118-2.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Iso2Mode {
    /// DC; the inlet (`connector`) decides between `DC_core`, `DC_extended` and `DC_combo_core`.
    #[default]
    Dc,
    /// AC, single phase.
    Ac1,
    /// AC, three phases.
    Ac3,
}

/// The knobs of the ISO 15118-2 side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Iso2 {
    pub mode: Iso2Mode,
    /// The MAC address of the EVCC, its EVCCID.
    pub mac_address: [u8; 6],
    pub send_charging_profile: bool,
    pub welding_detection: bool,
    pub enforce_cpd_limits: bool,
    /// Value-added services (ids from the charger's `ServiceList`) to ask about and select.
    pub value_added: Vec<u16>,
}

impl Default for Iso2 {
    fn default() -> Self {
        Self { mode: Iso2Mode::Dc, mac_address: [0x02, 0x00, 0x5E, 0x00, 0x53, 0x02], send_charging_profile: true, welding_detection: true, enforce_cpd_limits: true, value_added: Vec::new() }
    }
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
    /// What the car can give back in a BPT session (`bpt = true`).
    pub max_discharge_power_w: u32,
    pub min_discharge_power_w: u32,
    pub max_discharge_current_a: u32,
    pub min_discharge_current_a: u32,
    /// The AC services (`ac = true`), per line: what the on-board charger draws, and feeds back
    /// in a BPT session, and how many lines the car has (1 or 3).
    pub ac_max_charge_power_w: u32,
    pub ac_min_charge_power_w: u32,
    pub ac_max_discharge_power_w: u32,
    pub ac_min_discharge_power_w: u32,
    pub ac_phases: u8,
    /// In a BPT session: discharge (a negative `EVTargetEnergyRequest`) until the battery is at
    /// this state of charge (percent), then charge to `target_soc`. Needs the battery model.
    pub discharge_to_soc: Option<u8>,
    /// Scheduled mode: offer this power (watts, fed back) for `energy_offer_s` seconds in the
    /// `EVEnergyOffer`, asking `energy_offer_fee_cents` per kWh (in EUR). Needs `scheduled`.
    pub energy_offer_w: Option<u32>,
    pub energy_offer_s: u32,
    pub energy_offer_fee_cents: u32,
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
            max_discharge_power_w: 11_000,
            min_discharge_power_w: 100,
            max_discharge_current_a: 30,
            min_discharge_current_a: 1,
            ac_max_charge_power_w: 3680,
            ac_min_charge_power_w: 1380,
            ac_max_discharge_power_w: 3680,
            ac_min_discharge_power_w: 500,
            ac_phases: 3,
            discharge_to_soc: None,
            energy_offer_w: None,
            energy_offer_s: 3600,
            energy_offer_fee_cents: 30,
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
    /// ISO 15118-20: select the bidirectional service `DC_BPT` instead of `DC` (`AC_BPT` instead
    /// of `AC` with `ac`).
    pub bpt: bool,
    /// ISO 15118-20 AC: Scheduled control mode (select one of the charger's schedules, follow it)
    /// instead of Dynamic.
    pub scheduled: bool,
    /// Scheduled mode: state `PowerToleranceAcceptance` also when the charger offered no tolerance
    /// (V2G20-1866 asks for it only with one; the Python library insists on it).
    pub state_tolerance_always: bool,
    /// ISO 15118-20: ask the charger for its meter reading in every charge loop answer
    /// (`MeterInfoRequested`, V2G20-1081).
    pub request_meter_info: bool,
    /// ISO 15118-20 Dynamic: select `MobilityNeedsMode` 2, the charger provides the mobility needs
    /// (departure time, minimum and target state of charge), and the vehicle reports its display
    /// parameters in full.
    pub mobility_needs_by_secc: bool,
    /// ISO 15118-20: charge with the AC service (offering the AC schema) instead of DC.
    pub ac: bool,
    /// DIN SPEC 70121.
    pub din: Din,
    /// ISO 15118-2.
    pub iso2: Iso2,
    /// Run SLAC on the interface before looking for a charger.
    pub slac: bool,
    /// Configure a Qualcomm QCA7000 family modem on the interface with the charger's key once SLAC
    /// has matched (CM_SET_KEY); without it the interface is taken as the link already.
    pub qca_modem: bool,
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
            bpt: false,
            ac: false,
            scheduled: false,
            state_tolerance_always: false,
            request_meter_info: false,
            mobility_needs_by_secc: false,
            din: Din::default(),
            iso2: Iso2::default(),
            slac: false,
            slac_repetition_ms: None,
            slac_validation: SlacValidation::Never,
            qca_modem: false,
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

    /// The ISO 15118-2 session configuration these settings call for.
    pub fn iso2_session(&self) -> evcc_core::iso2::Config {
        let mut config = evcc_core::iso2::Config {
            mode: match self.iso2.mode {
                Iso2Mode::Dc => evcc_core::iso2::Mode::Dc,
                Iso2Mode::Ac1 => evcc_core::iso2::Mode::AcSinglePhase,
                Iso2Mode::Ac3 => evcc_core::iso2::Mode::AcThreePhase,
            },
            welding_detection: self.iso2.welding_detection,
            send_charging_profile: self.iso2.send_charging_profile,
            enforce_cpd_limits: self.iso2.enforce_cpd_limits,
            prefer_contract: self.pnc.is_some(),
            // The session config is `Copy` with static lists: the few ids live as long as the process.
            value_added: Box::leak(self.iso2.value_added.clone().into_boxed_slice()),
            ..evcc_core::iso2::Config::ISO2_DC
        };
        if let Some(ms) = self.poll_interval_ms {
            config.poll_interval_ms = ms;
        }
        if let Some(ms) = self.charge_loop_interval_ms {
            config.charge_loop_interval_ms = ms;
        }
        config
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
        let base = match (self.ac, self.bpt) {
            (false, false) => evcc_core::session::Config::DC_20,
            (false, true) => evcc_core::session::Config::DC_BPT_20,
            (true, false) => evcc_core::session::Config::AC_20,
            (true, true) => evcc_core::session::Config::AC_BPT_20,
        };
        let base = if self.ac && self.scheduled { base.scheduled() } else if self.mobility_needs_by_secc { base.mobility_needs_by_secc() } else { base };
        let mut config = evcc_core::session::Config { allow_unsecured: self.tls.is_none(), prefer_pnc: self.pnc.is_some(), state_tolerance_always: self.state_tolerance_always, request_meter_info: self.request_meter_info, ..base };
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
