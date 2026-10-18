//! The scenario file format (TOML) and its checks. A scenario names two participants, a
//! charger (SECC) and a vehicle (EVCC), joined by one cable, with the settings of each, the
//! cable's impairments and what must hold afterwards.
//!
//! ```toml
//! name = "dc-tls"
//! description = "A DC session over TLS 1.3 with freshly made P-521 certificates."
//! timeout_s = 60            # for the vehicle to finish
//! pki = "p521"              # p521 | ed448 | p256 | partner (the SECC's own test PKI)
//!
//! [impairment]              # optional, applied in both directions
//! delay_ms = 20
//! loss_percent = 0.5
//!
//! [secc]
//! kind = "evse-sim"         # | "iso15118-python"
//! tls = true                # the runner puts the generated certificates into the settings
//! [secc.settings.session]   # evse_sim::Settings
//! terminate_after_loops = 4
//!
//! [evcc]
//! kind = "evcc-sim"
//! tls = true
//! pnc = false               # true: a contract certificate chain for Plug & Charge, made for the run
//! [evcc.settings]           # evcc_sim::Settings; [evcc.settings.tls] takes only options
//!                           # (versions, backwards_compatible), the runner makes the files
//! slac = true
//!
//! [expect]
//! evcc_exit = 0             # default 0
//! secc_exit = 0             # not checked when absent
//! evcc_log = ["session: Ended(Completed)"]   # lines that must appear
//! secc_log = ["session: Stopped"]
//! ```

use std::io;
use std::path::Path;

use serde::Deserialize;

use crate::pki::Curve;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_timeout")]
    pub timeout_s: u64,
    /// Where the certificates come from when a side uses TLS; by default generated on P-521,
    /// or the partner's own when the SECC is an external implementation.
    pub pki: Option<PkiSource>,
    pub impairment: Option<Impairment>,
    pub secc: Participant,
    pub evcc: Participant,
    #[serde(default)]
    pub expect: Expect,
}

fn default_timeout() -> u64 {
    60
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PkiSource {
    P521,
    Ed448,
    P256,
    Partner,
}

impl PkiSource {
    pub fn curve(self) -> Option<Curve> {
        match self {
            PkiSource::P521 => Some(Curve::P521),
            PkiSource::Ed448 => Some(Curve::Ed448),
            PkiSource::P256 => Some(Curve::P256),
            PkiSource::Partner => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Impairment {
    pub delay_ms: Option<u32>,
    pub loss_percent: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Kind {
    #[serde(rename = "evcc-sim")]
    EvccSim,
    #[serde(rename = "evse-sim")]
    EvseSim,
    /// The SECC of the open-source Python `iso15118` library.
    #[serde(rename = "iso15118-python")]
    Iso15118Python,
    /// The EVSE side of SLAC of the open-source `pyslac` library (no charging session behind it).
    #[serde(rename = "pyslac")]
    PySlac,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Participant {
    pub kind: Kind,
    #[serde(default)]
    pub tls: bool,
    /// Plug & Charge: the runner makes a contract chain and key for the vehicle (and the charger
    /// trusts its root); for the vehicle only.
    #[serde(default)]
    pub pnc: bool,
    /// For the external charger: the protocols it speaks, as its own setting names them
    /// (`ISO_15118_20_DC` by default, or `DIN_SPEC_70121`).
    #[serde(default)]
    pub protocols: Option<String>,
    /// For the external charger: its `AUTH_MODES` (`EIM` by default, `PNC`, or both separated by commas).
    #[serde(default)]
    pub auth_modes: Option<String>,
    /// The participant's settings file as a table (`evcc_sim::Settings`, `evse_sim::Settings`).
    #[serde(default)]
    pub settings: toml::Table,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Expect {
    pub evcc_exit: i32,
    pub secc_exit: Option<i32>,
    pub evcc_log: Vec<String>,
    pub secc_log: Vec<String>,
}

impl Scenario {
    pub fn parse(text: &str) -> io::Result<Scenario> {
        let scenario: Scenario = toml::from_str(text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        scenario.check()?;
        Ok(scenario)
    }

    pub fn load(path: &Path) -> io::Result<Scenario> {
        let text = std::fs::read_to_string(path)?;
        Scenario::parse(&text).map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))
    }

    fn check(&self) -> io::Result<()> {
        let bad = |message: String| Err(io::Error::new(io::ErrorKind::InvalidData, format!("scenario {}: {message}", self.name)));
        if self.evcc.kind != Kind::EvccSim {
            return bad("the vehicle (evcc) must be an evcc-sim".into());
        }
        if self.secc.kind == Kind::EvccSim {
            return bad("the charger (secc) cannot be an evcc-sim".into());
        }
        if self.secc.pnc {
            return bad("`pnc = true` belongs to the vehicle; the charger's `[secc.settings.session]` says what it offers".into());
        }
        if self.evcc.settings.contains_key("pnc") {
            return bad("Plug & Charge is switched on with `pnc = true` on the vehicle and the runner supplies the files".into());
        }
        // The vehicle's `tls` table may set options (`versions`, `backwards_compatible`) but not
        // name files: the runner supplies those.
        let names_files = |t: &toml::Table| t.get("tls").and_then(toml::Value::as_table).is_some_and(|t| ["chain", "key", "trust", "key_password"].iter().any(|k| t.contains_key(*k)));
        if self.evcc.settings.get("tls").is_some_and(|t| !t.is_table()) || names_files(&self.evcc.settings) || self.secc.settings.contains_key("tls") {
            return bad("TLS is switched on with `tls = true` on the participant and the runner supplies the files; the vehicle's `[evcc.settings.tls]` takes only options".into());
        }
        // The settings must be what the participant reads, so that mistakes show when the
        // file is loaded and not in the middle of a run.
        let valid = match self.evcc.settings.clone().try_into::<evcc_sim::Settings>() {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("evcc settings: {e}")),
        }
        .and_then(|()| match self.secc.kind {
            Kind::EvseSim => self.secc.settings.clone().try_into::<evse_sim::Settings>().map(drop).map_err(|e| format!("secc settings: {e}")),
            _ if self.secc.settings.is_empty() => Ok(()),
            _ => Err("an external charger takes no settings".into()),
        });
        if let Err(message) = valid {
            return bad(message);
        }
        if self.pki == Some(PkiSource::Partner) && self.secc.kind == Kind::EvseSim {
            return bad("pki = \"partner\" needs an external charger".into());
        }
        Ok(())
    }

    /// Whether the run needs certificates.
    pub fn uses_tls(&self) -> bool {
        self.evcc.tls || self.secc.tls
    }

    pub fn pki(&self) -> PkiSource {
        self.pki.unwrap_or(if self.secc.kind == Kind::Iso15118Python { PkiSource::Partner } else { PkiSource::P521 })
    }

    pub fn evcc_settings(&self) -> evcc_sim::Settings {
        self.evcc.settings.clone().try_into().expect("checked on load")
    }

    pub fn secc_settings(&self) -> evse_sim::Settings {
        self.secc.settings.clone().try_into().expect("checked on load")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        name = "x"
        [secc]
        kind = "evse-sim"
        [evcc]
        kind = "evcc-sim"
    "#;

    #[test]
    fn a_minimal_scenario_gets_defaults() {
        let s = Scenario::parse(MINIMAL).unwrap();
        assert_eq!((s.timeout_s, s.expect.evcc_exit, s.expect.secc_exit), (60, 0, None));
        assert!(!s.uses_tls());
        assert_eq!(s.pki(), PkiSource::P521);
    }

    #[test]
    fn mistakes_are_reported_when_the_file_is_loaded() {
        let with = |extra: &str| Scenario::parse(&format!("{MINIMAL}\n{extra}")).unwrap_err().to_string();
        assert!(with("[expect]\nevcc_exitt = 1").contains("evcc_exitt"));
        assert!(Scenario::parse(&MINIMAL.replace("evse-sim", "evcc-sim")).unwrap_err().to_string().contains("cannot be an evcc-sim"));
        assert!(Scenario::parse(&MINIMAL.replace("[evcc]", "[evcc.settings]\nslack = true\n[evcc]")).is_err());
        assert!(Scenario::parse(&format!("{MINIMAL}\n[secc.settings.session]\nsetup_code = \"Nope\"")).is_err());
        assert!(Scenario::parse(&format!("{MINIMAL}\n[evcc.settings.tls]\nchain = \"x\"")).unwrap_err().to_string().contains("tls = true"));
    }
}
