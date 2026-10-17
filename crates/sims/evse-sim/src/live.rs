//! The live side of the simulated charger for a test platform: the measurements it reports in
//! the charge loop, modelled or set by hand, the state of its session, and the config the
//! next session will use (what the control socket changes).

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sim_control::{Endpoint, EventLog, Metric};

use crate::config::{Config, Limits};

struct Inner {
    /// For the next session.
    next: Config,
    /// The limits of the session in progress; the charger offers and reports these, and they can
    /// be changed while it runs.
    limits: Limits,
    /// The session in progress (or the last one).
    used: Option<Config>,
    /// Its settings as changed during the session, and a change the charger has not taken yet.
    session_config: Config,
    patch: Option<Config>,
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
    events: EventLog,
}

impl EvseLive {
    pub fn new(config: Config) -> Arc<EvseLive> {
        Arc::new(EvseLive {
            inner: Mutex::new(Inner {
                limits: config.limits,
                session_config: config.clone(),
                next: config,
                used: None,
                patch: None,
                voltage: Metric::new(400.0),
                current: Metric::new(0.0),
                max_power: Metric::new(0.0),
                state: "Waiting".into(),
                sessions: 0,
                ev_voltage: 0.0,
                ev_max_current: 0.0,
                ev_max_power: 0.0,
            }),
            events: EventLog::new(4000),
        })
    }

    /// Notes something the charger did, for the timeline of the test platform.
    pub fn event(&self, line: &str) {
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let kind = if line.starts_with("state ") {
            "state"
        } else if line.starts_with("rx ") || line.starts_with("tx ") {
            // The wire is on the vehicle's side of the timeline.
            return;
        } else {
            "note"
        };
        self.events.push(json!({ "src": "evse", "t": now_ms, "kind": kind, "text": line }));
    }

    /// A change of the session's settings made during the session, once.
    pub fn take_patch(&self) -> Option<Config> {
        self.inner.lock().unwrap().patch.take()
    }

    /// The config for a session that starts now.
    pub fn begin_session(&self) -> Config {
        let mut g = self.inner.lock().unwrap();
        g.sessions += 1;
        g.used = Some(g.next.clone());
        g.limits = g.next.limits;
        g.session_config = g.next.clone();
        g.patch = None;
        // Nothing is delivered before the charge loop.
        g.voltage.effective = 0.0;
        g.current.effective = 0.0;
        g.max_power.effective = 0.0;
        g.next.clone()
    }

    /// The limits now.
    pub fn limits(&self) -> Limits {
        self.inner.lock().unwrap().limits
    }

    /// The state of the session. What the charger reports as delivered follows it: the values of
    /// the charge loop are only there while it runs. The current is zero once it ends (the
    /// stop, V2G20-1194), the voltage and the power limit once the session is over.
    pub fn set_state(&self, state: impl std::fmt::Debug) {
        let mut g = self.inner.lock().unwrap();
        // A state of the machine, or a name: Debug of a string has quotes.
        g.state = format!("{state:?}").trim_matches('"').to_string();
        if g.state != "ChargeLoop" {
            g.current.effective = 0.0;
        }
        if matches!(g.state.as_str(), "Waiting" | "Stopped" | "Paused" | "Closed") {
            g.voltage.effective = 0.0;
            g.max_power.effective = 0.0;
        }
    }

    /// The values of a charge loop answer to a request with these values: present voltage and
    /// current, and the maximum power. Modelled: the voltage the vehicle reports, the current
    /// both sides' limits allow, the configured power limit.
    pub fn charge_loop(&self, ev_voltage: f64, ev_max_current: f64, ev_max_power: f64) -> (f64, f64, f64) {
        let mut g = self.inner.lock().unwrap();
        let limits = g.limits;
        g.ev_voltage = ev_voltage;
        g.ev_max_current = ev_max_current;
        g.ev_max_power = ev_max_power;
        let voltage = g.voltage.resolve(ev_voltage);
        let current = g.current.resolve(modelled_current(voltage, ev_max_current, ev_max_power, &limits));
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
            "limits": g.used.as_ref().map(|_| g.limits),
            "config": g.used.as_ref().map(|_| &g.session_config),
            "measurements": { "voltage": g.voltage, "current": g.current, "max_power": g.max_power },
            "ev_reports": { "voltage": g.ev_voltage, "max_current": g.ev_max_current, "max_power": g.ev_max_power },
        })
    }

    fn events(&self, cursor: u64) -> (Vec<Value>, u64) {
        self.events.since(cursor)
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
            // Settings of the session in progress (`{"values": {"cable_check_code": ...}}`), from the
            // next message on: whatever a setting file can say, for the keys given.
            Some("patch") => {
                let values = cmd.get("values").and_then(Value::as_object).ok_or("values: an object")?;
                let mut config = serde_json::to_value(&g.session_config).map_err(|e| e.to_string())?;
                for (key, value) in values {
                    let slot = config.get_mut(key).ok_or_else(|| format!("no such setting: {key}"))?;
                    *slot = value.clone();
                }
                let config: Config = serde_json::from_value(config).map_err(|e| e.to_string())?;
                g.session_config = config.clone();
                g.patch = Some(config);
                Ok(())
            }
            // The limits of the session in progress, from the next message on.
            Some("limits") => {
                let key = cmd.get("key").and_then(Value::as_str).ok_or("key: a limit name")?;
                let mut limits = serde_json::to_value(g.limits).map_err(|e| e.to_string())?;
                let slot = limits.get_mut(key).ok_or_else(|| format!("no such limit: {key}"))?;
                *slot = cmd.get("value").cloned().unwrap_or(Value::Null);
                g.limits = serde_json::from_value(limits).map_err(|e| format!("{key}: {e}"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_delivered_outside_the_charge_loop() {
        let live = EvseLive::new(Config::default());
        live.begin_session();
        live.set_state("ChargeLoop");
        let (v, i, p) = live.charge_loop(400.0, 100.0, 50_000.0);
        assert_eq!((v, i, p), (400.0, 100.0, 150_000.0));
        let effective = |live: &EvseLive| {
            let s = live.snapshot();
            let m = &s["measurements"];
            [m["voltage"]["effective"].as_f64().unwrap(), m["current"]["effective"].as_f64().unwrap(), m["max_power"]["effective"].as_f64().unwrap()]
        };
        assert_eq!(effective(&live), [400.0, 100.0, 150_000.0]);
        // The stop: the current is gone, the voltage stays until the session is over.
        live.set_state("AwaitWeldingReq");
        assert_eq!(effective(&live), [400.0, 0.0, 150_000.0]);
        live.set_state("Waiting");
        assert_eq!(effective(&live), [0.0, 0.0, 0.0]);
        // The next session starts from nothing too, whatever the last values were.
        live.set_state("ChargeLoop");
        live.charge_loop(400.0, 100.0, 50_000.0);
        live.begin_session();
        assert_eq!(effective(&live), [0.0, 0.0, 0.0]);
    }
}
