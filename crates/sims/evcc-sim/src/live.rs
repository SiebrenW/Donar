//! The live side of the simulated vehicle for a test platform: the phases of a session, a hold
//! at the start of each (the gate of `evcc_linux::run_gated`), the measurements that are either
//! modelled or set by hand, and the commands and state of the control socket.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use evcc_core::session::{AcChargeLoopStatus, ChargeLoopStatus, State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sim_control::{Endpoint, EventLog, Metric};

use crate::settings::Limits;

/// The battery the model charges.
pub(crate) const CAPACITY_WH: f64 = 60_000.0;
/// The battery voltage the model reports.
const NOMINAL_VOLTAGE: f64 = 400.0;

/// A session in coarse steps. The vehicle holds before the first message of a phase it was not
/// asked to enter yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Before the session: SLAC, SDP, the connection.
    Connecting,
    Setup,
    Authorization,
    Service,
    ChargeParameters,
    ScheduleExchange,
    CableCheck,
    PreCharge,
    PowerDelivery,
    ChargeLoop,
    /// The orderly stop: current down, `PowerDeliveryReq(Stop)`, contactors, welding detection,
    /// `SessionStopReq`.
    Stop,
    Ended,
}

impl Phase {
    pub fn of(state: State) -> Phase {
        use State::*;
        match state {
            Idle => Phase::Connecting,
            // The positioning of an ACDP vehicle is part of the setup, before the authorization.
            AwaitSupportedAppProtocolRes | AwaitSessionSetupRes | AwaitVehiclePositioningRes => Phase::Setup,
            AwaitAuthorizationSetupRes | AwaitAuthorizationRes | AwaitContractAuthenticationRes | AwaitPaymentDetailsRes => Phase::Authorization,
            AwaitServiceDiscoveryRes | AwaitServiceDetailRes | AwaitServiceSelectionRes | AwaitServicePaymentSelectionRes => Phase::Service,
            AwaitChargeParameterDiscoveryRes | AwaitDutyCycle => Phase::ChargeParameters,
            AwaitScheduleExchangeRes => Phase::ScheduleExchange,
            AwaitControlPilotC | AwaitCableCheckRes => Phase::CableCheck,
            AwaitPreChargeRes | AwaitContactors => Phase::PreCharge,
            AwaitPowerDeliveryRes => Phase::PowerDelivery,
            AwaitChargeLoopRes | AwaitPowerDeliveryStandbyRes | AwaitCurrentDemandRes | AwaitChargingStatusRes | AwaitMeteringReceiptRes => Phase::ChargeLoop,
            AwaitCurrentStopped | AwaitPowerDeliveryStopRes | AwaitContactorsOpened | AwaitControlPilotB | AwaitWeldingDetectionRes | AwaitSessionStopRes => Phase::Stop,
            Ended => Phase::Ended,
        }
    }
}

struct Inner {
    limits: Limits,
    voltage: Metric,
    soc: Metric,
    /// The state of charge of the battery model, percent.
    model_soc: f64,
    /// The discharge down to `limits.discharge_to_soc` is over.
    discharge_done: bool,
    updated: Option<Instant>,
    time_scale: f64,
    stop_at_target_soc: bool,
    /// What the charger reported in the last charge loop answer.
    seen_voltage: f64,
    seen_current: f64,
    /// The active power per line the charger last asked of the on-board charger (AC), watts.
    ac_power: [f64; 3],
    /// The charger's `EVSEPowerRampLimitation`, percent of the maximum power per minute (0: none), and when the
    /// AC power was last moved.
    ramp_percent: f64,
    ac_power_at: Option<Instant>,
    loops: u32,
    phase: Phase,
    state: Option<State>,
    held: bool,
    /// The phase to run to; the vehicle holds at its start.
    target: Phase,
    /// Whether to go on in `target` (its messages, repeated ones included, keep flowing) or to
    /// hold before it.
    stay: bool,
    /// Run the whole session without holds, and charge until the vehicle itself is done.
    free_run: bool,
    real_timers: bool,
    stop_asked: bool,
    closed: bool,
    result: Option<String>,
    action: Option<Action>,
    /// The control pilot cable shared with the simulated charger, and what the session machine last asked the host to watch.
    wire: Option<evcc_linux::pilot::Wire>,
    guard: Option<control_pilot::Guard>,
}

/// Something the vehicle does in the charge loop when asked, once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Pause,
    RenegotiateServices,
    RenegotiateSchedule,
    /// An emergency of the vehicle's own: its pilot leaves state C and it drops the connection.
    PilotEmergency,
}

pub struct EvLive {
    inner: Mutex<Inner>,
    changed: Condvar,
    events: EventLog,
}

impl Inner {
    fn discharging(&self) -> bool {
        self.limits.discharge_to_soc.is_some() && !self.discharge_done
    }

    /// The battery takes `power_w` for the time since the last answer (the model); true when the
    /// vehicle should stop now: it was asked to advance to the stop, or the target state of
    /// charge is reached.
    fn take_energy(&mut self, power_w: f64) -> bool {
        let g = self;
        g.loops += 1;
        let now = Instant::now();
        if let Some(before) = g.updated.replace(now) {
            let hours = now.duration_since(before).as_secs_f64() * g.time_scale / 3600.0;
            let energy_wh = power_w * hours;
            g.model_soc = (g.model_soc + energy_wh / CAPACITY_WH * 100.0).clamp(0.0, 100.0);
        }
        if g.limits.discharge_to_soc.is_some_and(|floor| g.model_soc <= f64::from(floor)) {
            g.discharge_done = true;
        }
        // The shown state of charge follows the model, also where the protocol never asks for it.
        let modelled = g.model_soc;
        g.soc.resolve(modelled);
        let reached = g.stop_at_target_soc && !g.discharging() && g.model_soc >= f64::from(g.limits.target_soc);
        let stop = reached || (g.target >= Phase::Stop && !g.free_run);
        if stop && !g.stop_asked {
            g.stop_asked = true;
            return true;
        }
        false
    }

    /// Whether entering `phase` is a place to hold: beyond the target, or at it when the
    /// vehicle is to hold before it.
    fn holds_at(&self, phase: Phase) -> bool {
        phase > self.target || (phase == self.target && !self.stay)
    }
}

impl EvLive {
    /// A session with `limits`, held before its first message.
    pub fn new(limits: Limits, start_soc: u8, time_scale: u32, stop_at_target_soc: bool) -> Arc<EvLive> {
        Arc::new(EvLive {
            inner: Mutex::new(Inner {
                limits,
                voltage: Metric::new(NOMINAL_VOLTAGE),
                soc: Metric::new(f64::from(start_soc)),
                model_soc: f64::from(start_soc),
                discharge_done: false,
                updated: None,
                time_scale: f64::from(time_scale.max(1)),
                stop_at_target_soc,
                seen_voltage: 0.0,
                seen_current: 0.0,
                ac_power: [0.0; 3],
                ramp_percent: 0.0,
                ac_power_at: None,
                loops: 0,
                phase: Phase::Connecting,
                state: None,
                held: false,
                target: Phase::Setup,
                stay: false,
                free_run: false,
                real_timers: false,
                stop_asked: false,
                closed: false,
                result: None,
                action: None,
                wire: None,
                guard: None,
            }),
            changed: Condvar::new(),
            events: EventLog::new(4000),
        })
    }

    /// What the vehicle was asked to do in the charge loop, once.
    /// The control pilot cable of the session (the host's end of it).
    pub fn set_wire(&self, wire: evcc_linux::pilot::Wire) {
        self.inner.lock().unwrap().wire = Some(wire);
    }

    /// What the session machine last asked the host to watch on the pilot.
    pub fn set_guard(&self, guard: control_pilot::Guard) {
        self.inner.lock().unwrap().guard = Some(guard);
    }

    pub fn take_action(&self) -> Option<Action> {
        self.inner.lock().unwrap().action.take()
    }

    /// Notes a record of the session for the timeline of the test platform.
    pub fn record(&self, record: &evcc_core::log::Record<'_>) {
        use evcc_core::log::Record;
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        let phase = self.inner.lock().unwrap().phase;
        let mut event = match record {
            Record::MsgTx { msg } => json!({ "src": "ev", "t": now_ms, "kind": "tx", "msg": format!("{msg:?}") }),
            Record::MsgRx { msg } => json!({ "src": "ev", "t": now_ms, "kind": "rx", "msg": format!("{msg:?}") }),
            Record::Response { msg, code } => json!({ "src": "ev", "t": now_ms, "kind": "response", "msg": format!("{msg:?}"), "code": code.xml_name() }),
            Record::DinResponse { msg, code } => json!({ "src": "ev", "t": now_ms, "kind": "response", "msg": format!("{msg:?}"), "code": code.xml_name() }),
            Record::Iso2Response { msg, code } => json!({ "src": "ev", "t": now_ms, "kind": "response", "msg": format!("{msg:?}"), "code": code.xml_name() }),
            Record::StateChange { from, to } => json!({ "src": "ev", "t": now_ms, "kind": "state", "text": format!("{from:?} -> {to:?}") }),
            // Raw payloads are for the log.
            Record::RxPayload { .. } => return,
            // The charger broke a rule: shown as such.
            Record::Violation { .. } | Record::ResponseCodeNotAllowed { .. } => json!({ "src": "ev", "t": now_ms, "kind": "violation", "text": format!("{record:?}") }),
            other => json!({ "src": "ev", "t": now_ms, "kind": "note", "text": format!("{other:?}") }),
        };
        event["phase"] = json!(phase);
        self.events.push(event);
    }

    pub fn battery_voltage(&self) -> f64 {
        self.inner.lock().unwrap().voltage.resolve(NOMINAL_VOLTAGE)
    }

    pub fn present_soc(&self) -> f64 {
        let mut g = self.inner.lock().unwrap();
        let modelled = g.model_soc;
        g.soc.resolve(modelled)
    }

    /// Whether the vehicle is in the discharge part of its session (`limits.discharge_to_soc`).
    pub fn discharging(&self) -> bool {
        self.inner.lock().unwrap().discharging()
    }

    /// The vehicle's limits of this session.
    pub fn limits(&self) -> Limits {
        self.inner.lock().unwrap().limits
    }

    /// A charge loop answer arrived: the battery takes what the charger delivers. True when
    /// the vehicle should stop now: it was asked to advance to the stop, or the target state
    /// of charge is reached.
    pub fn charge_loop(&self, status: &ChargeLoopStatus) -> bool {
        let mut g = self.inner.lock().unwrap();
        g.seen_voltage = status.present_voltage.to_f64();
        g.seen_current = status.present_current.to_f64();
        let power = g.seen_voltage * g.seen_current;
        g.take_energy(power)
    }

    /// An AC charge loop answer arrived: the on-board charger draws (or feeds back, when
    /// negative) the target power, which the battery takes. As for DC, true when the vehicle
    /// should stop now.
    pub fn ac_charge_loop(&self, status: &AcChargeLoopStatus) -> bool {
        let mut g = self.inner.lock().unwrap();
        let p = status.target_active_power;
        let target = [Some(p.l1), p.l2, p.l3].map(|v| v.map_or(0.0, iso15118_20::common_types::RationalNumber::to_f64));
        let now = Instant::now();
        let elapsed = g.ac_power_at.replace(now).map_or(0.0, |t| now.duration_since(t).as_secs_f64());
        // V2G20-1831: changes of a power that comes from the vehicle's own schedule are held to the ramp limit;
        // a target the charger names overrides it (V2G20-1832).
        let watts = if status.target_from_schedule && g.ramp_percent > 0.0 {
            let step = g.ramp_percent / 100.0 * f64::from(g.limits.ac_max_charge_power_w) * elapsed / 60.0;
            let mut now_power = g.ac_power;
            for (present, target) in now_power.iter_mut().zip(target) {
                *present += (target - *present).clamp(-step, step);
            }
            now_power
        } else {
            target
        };
        g.ac_power = watts;
        // Shown like a DC charge: the nominal voltage and the current the power comes to.
        g.seen_voltage = NOMINAL_VOLTAGE;
        g.seen_current = watts.iter().sum::<f64>() / NOMINAL_VOLTAGE;
        g.take_energy(watts.iter().sum())
    }

    /// For tests: lets the battery model's clock run `by` further, instead of sleeping (a slow machine sleeps longer than asked).
    #[cfg(test)]
    fn shift_clock(&self, by: std::time::Duration) {
        let mut g = self.inner.lock().unwrap();
        g.updated = g.updated.and_then(|t| t.checked_sub(by));
    }

    /// The charger's power ramp limit from its charge parameters, percent of the maximum power per minute.
    pub fn set_ramp_limit(&self, percent: f64) {
        self.inner.lock().unwrap().ramp_percent = percent;
    }

    /// The active power per line the charger last asked for, which the on-board charger now
    /// draws (watts; negative when it feeds back).
    pub fn ac_present_power(&self) -> [f64; 3] {
        self.inner.lock().unwrap().ac_power
    }

    /// Called with every state the session enters, before its frame goes out. Blocks while the
    /// vehicle is held; true when the wait is not to count against the timers.
    pub fn gate(&self, state: State) -> bool {
        let mut g = self.inner.lock().unwrap();
        g.state = Some(state);
        let phase = Phase::of(state);
        if phase == g.phase {
            return false;
        }
        g.phase = phase;
        if !g.holds_at(phase) || g.closed {
            return false;
        }
        g.held = true;
        while g.holds_at(g.phase) && !g.closed {
            g = self.changed.wait(g).unwrap();
        }
        g.held = false;
        !g.real_timers
    }

    pub fn finished(&self, result: String) {
        let mut g = self.inner.lock().unwrap();
        g.phase = Phase::Ended;
        g.held = false;
        g.result = Some(result);
    }

    /// Releases a held session for good (the simulation is closing).
    pub fn close(&self) {
        self.inner.lock().unwrap().closed = true;
        self.changed.notify_all();
    }
}

impl Endpoint for EvLive {
    fn snapshot(&self) -> Value {
        let g = self.inner.lock().unwrap();
        json!({
            "phase": g.phase,
            "held": g.held,
            "target": g.target,
            "stay": g.stay,
            "free_run": g.free_run,
            "state": g.state.map(|s| format!("{s:?}")),
            "real_timers": g.real_timers,
            "limits": g.limits,
            "measurements": { "voltage": g.voltage, "soc": g.soc },
            "evse_reports": { "voltage": g.seen_voltage, "current": g.seen_current },
            "model_soc": g.model_soc,
            "loops": g.loops,
            "result": g.result,
            "pilot": { "reading": sim_control::pilot_json::reading(g.wire.as_ref().and_then(evcc_linux::pilot::Wire::reading)), "guard": sim_control::pilot_json::guard(g.guard) },
        })
    }

    fn events(&self, cursor: u64) -> (Vec<Value>, u64) {
        self.events.since(cursor)
    }

    fn command(&self, cmd: Value) -> Result<(), String> {
        let mut g = self.inner.lock().unwrap();
        match cmd.get("cmd").and_then(Value::as_str) {
            Some("advance") => {
                let to: Phase = serde_json::from_value(cmd.get("to").cloned().unwrap_or_default()).map_err(|e| format!("to: {e}"))?;
                if to <= Phase::Connecting {
                    return Err("nothing before the session to advance to".into());
                }
                g.target = to;
                g.stay = cmd.get("stay").and_then(Value::as_bool).unwrap_or(false);
                g.free_run = false;
            }
            // No holds from now on; the charge goes on until the vehicle is done or is told to stop.
            Some("run") => {
                g.target = Phase::Ended;
                g.stay = false;
                g.free_run = true;
            }
            Some("stop") => {
                g.target = Phase::Stop;
                g.stay = false;
                g.free_run = false;
            }
            // The vehicle's limits, from the next message on (the charge loop carries them).
            Some("limits") => {
                let key = cmd.get("key").and_then(Value::as_str).ok_or("key: a limit name")?;
                let mut limits = serde_json::to_value(g.limits).map_err(|e| e.to_string())?;
                let slot = limits.get_mut(key).ok_or_else(|| format!("no such limit: {key}"))?;
                *slot = cmd.get("value").cloned().unwrap_or(Value::Null);
                g.limits = serde_json::from_value(limits).map_err(|e| format!("{key}: {e}"))?;
                if key == "discharge_to_soc" {
                    g.discharge_done = false;
                }
            }
            // Something to do in the charge loop, once.
            Some("event") => {
                g.action = Some(match cmd.get("name").and_then(Value::as_str) {
                    Some("pause") => Action::Pause,
                    Some("renegotiate_services") => Action::RenegotiateServices,
                    Some("renegotiate_schedule") => Action::RenegotiateSchedule,
                    Some("pilot_emergency") => Action::PilotEmergency,
                    other => return Err(format!("no such event: {other:?}")),
                });
            }
            // The vehicle changes the state it holds on the control pilot (A to F): a real vehicle's pilot goes where its hardware takes it.
            Some("pilot") => {
                let state = sim_control::pilot_json::state(cmd.get("state").and_then(Value::as_str).unwrap_or(""))?;
                g.wire.as_ref().ok_or("there is no control pilot wire in this simulation")?.set_ev(state).map_err(|e| e.to_string())?;
            }
            Some("real_timers") => g.real_timers = cmd.get("on").and_then(Value::as_bool).ok_or("on: a boolean")?,
            Some("measure") => {
                let metric = match cmd.get("key").and_then(Value::as_str) {
                    Some("voltage") => &mut g.voltage,
                    Some("soc") => &mut g.soc,
                    other => return Err(format!("no such measurement: {other:?}")),
                };
                metric.apply(&cmd);
            }
            other => return Err(format!("unknown command {other:?}")),
        }
        drop(g);
        self.changed.notify_all();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iso15118_20::common_types::RationalNumber;

    fn status(volts: f64, amps: f64) -> ChargeLoopStatus {
        let rn = RationalNumber::from_f64;
        ChargeLoopStatus {
            present_current: rn(amps),
            present_voltage: rn(volts),
            power_limit_achieved: false,
            current_limit_achieved: false,
            voltage_limit_achieved: false,
            maximum_charge_power: rn(100_000.0),
            maximum_charge_current: rn(200.0),
            maximum_voltage: rn(450.0),
            meter: None,
            mobility_needs: None,
        }
    }

    #[test]
    fn the_battery_charges_with_what_the_charger_delivers_and_stops_at_the_target() {
        // Real time, 1:1: 40 kW into 60 kWh takes 150 s from 20 % to 30 %. The clock is moved, not waited for.
        let live = EvLive::new(Limits { target_soc: 30, ..Limits::default() }, 20, 1, true);
        live.command(json!({ "cmd": "advance", "to": "charge_loop" })).unwrap();
        // The first answer only starts the clock.
        assert!(!live.charge_loop(&status(400.0, 100.0)));
        live.shift_clock(std::time::Duration::from_secs(40));
        let stop = live.charge_loop(&status(400.0, 100.0));
        assert!(live.present_soc() > 20.0);
        assert!(!stop && live.present_soc() < 30.0, "{}", live.present_soc());
        // Far past the target it asks once, not at every answer.
        live.shift_clock(std::time::Duration::from_secs(1000));
        assert!(live.charge_loop(&status(400.0, 100.0)));
        assert!(!live.charge_loop(&status(400.0, 100.0)));
    }

    #[test]
    fn a_free_run_charges_until_the_target_and_a_stop_command_ends_it() {
        let live = EvLive::new(Limits { target_soc: 90, ..Limits::default() }, 20, 1, true);
        live.command(json!({ "cmd": "run" })).unwrap();
        assert!(!live.charge_loop(&status(400.0, 100.0)), "the target phase does not stop a free run");
        live.command(json!({ "cmd": "advance", "to": "ended" })).unwrap();
        assert!(live.charge_loop(&status(400.0, 100.0)), "advancing to the end stops the charge");
    }

    #[test]
    fn advancing_to_the_stop_stops_the_charge_and_a_hand_set_soc_wins() {
        let live = EvLive::new(Limits::default(), 20, 1, false);
        assert!(!live.charge_loop(&status(400.0, 100.0)));
        live.command(json!({ "cmd": "advance", "to": "stop" })).unwrap();
        assert!(live.charge_loop(&status(400.0, 100.0)));
        live.command(json!({ "cmd": "measure", "key": "soc", "value": 55 })).unwrap();
        assert_eq!(live.present_soc(), 55.0);
        assert!(live.command(json!({ "cmd": "measure", "key": "nope" })).is_err());
        assert!(live.command(json!({ "cmd": "advance", "to": "connecting" })).is_err());
    }

    #[test]
    fn a_schedule_change_is_held_to_the_ramp_limit_and_a_named_target_is_not() {
        use evcc_core::ev::Phases;
        let live = EvLive::new(Limits::default(), 20, 1, false);
        // 10 % of 3680 W per line per minute: about 6 W in a hundredth of a second is nothing.
        live.set_ramp_limit(10.0);
        let rn = RationalNumber::from_f64;
        let status = |from_schedule| AcChargeLoopStatus {
            target_active_power: Phases::three(rn(3680.0)),
            target_reactive_power: [None; 3],
            present_active_power: [None; 3],
            target_frequency: None,
            target_from_schedule: from_schedule,
            meter: None,
            mobility_needs: None,
        };
        live.ac_charge_loop(&status(true));
        std::thread::sleep(std::time::Duration::from_millis(20));
        live.ac_charge_loop(&status(true));
        let held = live.ac_present_power()[0];
        assert!(held < 100.0, "{held}");
        live.ac_charge_loop(&status(false));
        assert_eq!(live.ac_present_power()[0], 3680.0);
    }

    #[test]
    fn staying_in_a_phase_passes_its_entry_and_holds_at_the_next() {
        let live = EvLive::new(Limits::default(), 20, 1, false);
        live.command(json!({ "cmd": "advance", "to": "setup", "stay": true })).unwrap();
        assert!(!live.gate(State::AwaitSupportedAppProtocolRes), "the entry of the phase we stay in is not held");
        let other = live.clone();
        let held = std::thread::spawn(move || other.gate(State::AwaitAuthorizationSetupRes));
        let started = Instant::now();
        while !live.snapshot()["held"].as_bool().unwrap() {
            assert!(started.elapsed().as_secs() < 5, "never held");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(live.snapshot()["phase"], "authorization");
        live.command(json!({ "cmd": "advance", "to": "authorization", "stay": true })).unwrap();
        assert!(held.join().unwrap());
    }

    #[test]
    fn limits_can_be_changed_while_the_session_runs() {
        let live = EvLive::new(Limits::default(), 20, 1, false);
        live.command(json!({ "cmd": "limits", "key": "max_charge_current_a", "value": 55 })).unwrap();
        assert_eq!(live.limits().max_charge_current_a, 55);
        assert!(live.command(json!({ "cmd": "limits", "key": "no_such", "value": 1 })).is_err());
        assert!(live.command(json!({ "cmd": "limits", "key": "max_charge_current_a", "value": "high" })).is_err());
        assert_eq!(live.limits().max_charge_current_a, 55);
    }

    #[test]
    fn the_gate_holds_at_the_target_phase_until_advanced() {
        let live = EvLive::new(Limits::default(), 20, 1, false);
        let other = live.clone();
        let held = std::thread::spawn(move || other.gate(State::AwaitSupportedAppProtocolRes));
        let started = Instant::now();
        while !live.snapshot()["held"].as_bool().unwrap() {
            assert!(started.elapsed().as_secs() < 5, "never held");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // Phases before the target are passed without a hold; the target is held at.
        live.command(json!({ "cmd": "advance", "to": "authorization" })).unwrap();
        assert!(held.join().unwrap(), "frozen timers by default");
        assert!(!live.gate(State::AwaitSessionSetupRes), "same phase: no hold");
    }
}
