//! The live side of the simulated charger for a test platform: the measurements it reports in
//! the charge loop, modelled or set by hand, the state of its session, and the config the
//! next session will use (what the control socket changes).

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sim_control::{Endpoint, Metric};

use crate::config::{Config, Limits};

struct Inner {
    /// For the next session.
    next: Config,
    /// The session in progress (or the last one).
    used: Option<Config>,
    voltage: Metric,
    current: Metric,
    max_power: Metric,
    state: String,
    sessions: u32,
    /// What the vehicle reported in its last charge loop request.
    ev_voltage: f64,
    ev_max_current: f64,
    ev_max_power: f64,
}

pub struct EvseLive {
    inner: Mutex<Inner>,
}

impl EvseLive {
    pub fn new(config: Config) -> Arc<EvseLive> {
        Arc::new(EvseLive {
            inner: Mutex::new(Inner {
                next: config,
                used: None,
                voltage: Metric::new(400.0),
                current: Metric::new(0.0),
                max_power: Metric::new(0.0),
                state: "Waiting".into(),
                sessions: 0,
                ev_voltage: 0.0,
                ev_max_current: 0.0,
                ev_max_power: 0.0,
            }),
        })
    }

    /// The config for a session that starts now.
    pub fn begin_session(&self) -> Config {
        let mut g = self.inner.lock().unwrap();
        g.sessions += 1;
        g.used = Some(g.next.clone());
        g.next.clone()
    }

    pub fn set_state(&self, state: impl std::fmt::Debug) {
        self.inner.lock().unwrap().state = format!("{state:?}");
    }

    /// The values of a charge loop answer to a request with these values: present voltage and
    /// current, and the maximum power. Modelled: the voltage the vehicle reports, the current
    /// both sides' limits allow, the configured power limit.
    pub fn charge_loop(&self, ev_voltage: f64, ev_max_current: f64, ev_max_power: f64, limits: &Limits) -> (f64, f64, f64) {
        let mut g = self.inner.lock().unwrap();
        g.ev_voltage = ev_voltage;
        g.ev_max_current = ev_max_current;
        g.ev_max_power = ev_max_power;
        let voltage = g.voltage.resolve(ev_voltage);
        let current = g.current.resolve(modelled_current(voltage, ev_max_current, ev_max_power, limits));
        let power = g.max_power.resolve(f64::from(limits.max_charge_power_w));
        (voltage, current, power)
    }
}

/// The current both sides' limits allow at `voltage`.
pub(crate) fn modelled_current(voltage: f64, ev_max_current: f64, ev_max_power: f64, limits: &Limits) -> f64 {
    let power = ev_max_power.min(f64::from(limits.max_charge_power_w));
    ev_max_current.min(f64::from(limits.max_charge_current_a)).min(power / voltage.max(1.0)).max(0.0)
}

impl Endpoint for EvseLive {
    fn snapshot(&self) -> Value {
        let g = self.inner.lock().unwrap();
        json!({
            "state": g.state,
            "sessions": g.sessions,
            "limits": g.used.as_ref().map(|c| c.limits),
            "measurements": { "voltage": g.voltage, "current": g.current, "max_power": g.max_power },
            "ev_reports": { "voltage": g.ev_voltage, "max_current": g.ev_max_current, "max_power": g.ev_max_power },
        })
    }

    fn command(&self, cmd: Value) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap();
        match cmd.get("cmd").and_then(Value::as_str) {
            // The settings of the next session; the one in progress keeps its own.
            Some("config") => {
                let session = cmd.get("session").cloned().ok_or("session: missing")?;
                g.next = serde_json::from_value(session).map_err(|e| format!("session: {e}"))?;
                Ok(())
            }
            Some("measure") => {
                let metric = match cmd.get("key").and_then(Value::as_str) {
                    Some("voltage") => &mut g.voltage,
                    Some("current") => &mut g.current,
                    Some("max_power") => &mut g.max_power,
                    other => return Err(format!("no such measurement: {other:?}")),
                };
                metric.apply(&cmd);
                Ok(())
            }
            other => Err(format!("unknown command {other:?}")),
        }
    }
}
