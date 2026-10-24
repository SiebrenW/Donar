//! The control pilot (CP) as the V2G cores see it.
//!
//! The host measures the CP (voltage state and PWM) and reports every change as a [`Reading`]
//! at the moment it happens: the standards give the reaction times in milliseconds (IEC 61851-23
//! CC.4.3: the EVSE triggers an emergency shutdown in 10 ms, the EV in 50 ms). The rules for what
//! a reading means during a session are here, shared by the vehicle and the charger cores:
//! [`EvWatch`] and [`EvseWatch`] return a [`Verdict`]; the cores tell the watch which phase the
//! session is in (where the CP is *meant* to change) and act on the verdict.
//!
//! What the standards say, as far as it is used:
//! * IEC 61851-23:2023 CC.4.3: an unintended transition from state C/D to B/A, any other or an
//!   unknown state is an emergency shutdown, for the EVSE (10 ms) and for the EV (50 ms).
//! * CC.3.4.3 scenario 2: the EVSE turns off its CP oscillator (static 100 %) while the EV is in
//!   state C/D: the EV triggers an emergency shutdown in 50 ms. After an error shutdown the
//!   oscillator is turned off as part of the sequence (CC.3.4.2, t203), which is no emergency.
//! * DIN SPEC 70121 V2G-DC-647, 655, 656 and ISO 15118-2 V2G2-737: the EV monitors the duty cycle
//!   for changes during the session; after the charge parameter discovery a duty cycle other than
//!   5 % ends the session (CP to B, terminate, close the connection); state E or F closes the
//!   connection.
//! * IEC 61851-1 Table A.5: a static 100 % duty cycle is state x1, "the EV supply equipment will not
//!   deliver the energy". In an AC session that is a stop of the supply, not an emergency (and
//!   ISO 15118-20 V2G20-930 NOTE 1 allows a session to start with it); in a DC session the
//!   standards above make it an emergency.
#![no_std]

/// The state of the control pilot circuit (IEC 61851-1 Table A.4), from the voltage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Not connected.
    A,
    /// Connected, vehicle not ready.
    B,
    /// Vehicle ready to charge.
    C,
    /// Vehicle ready, ventilation required.
    D,
    /// Error (no power at the EVSE, short circuit to PE).
    E,
    /// The EVSE signals a fault.
    F,
    /// A voltage outside every state, or no valid reading.
    Invalid,
}

/// The oscillator of the EVSE: the PWM duty cycle in percent, or no oscillation (a constant
/// voltage, which is a duty cycle of 100 % and an oscillator that is off).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pwm {
    Off,
    /// The measured duty cycle in whole percent; the host rounds.
    Duty(u8),
}

impl Pwm {
    /// 5 % (digital communication) as the vehicle measures it: the charger aims at 5 %, and a measurement from 3 to 7 % is
    /// 5 % (the capacitance and the inductance of the line change the measured duty cycle; V2G20-1408).
    pub fn is_five_percent(self) -> bool {
        matches!(self, Pwm::Duty(3..=7))
    }
}

impl Pwm {
    /// A nominal duty cycle (10 to 95 %), as a charger offers it to a vehicle that may charge by IEC 61851-1 alone and
    /// speaks ISO 15118 as well (ISO 15118-2 V2G2-930).
    pub fn is_nominal(self) -> bool {
        matches!(self, Pwm::Duty(10..=95))
    }
}

/// What the host measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub state: State,
    pub pwm: Pwm,
}

impl Reading {
    /// State C or D with a 5 % oscillator: what a vehicle that charges with HLC sees.
    pub const CHARGING: Reading = Reading { state: State::C, pwm: Pwm::Duty(5) };
    /// State B with a 5 % oscillator: connected, communication may start.
    pub const CONNECTED: Reading = Reading { state: State::B, pwm: Pwm::Duty(5) };

    pub fn is_ready(self) -> bool {
        matches!(self.state, State::C | State::D)
    }
}

/// What a change of the control pilot means to the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to do.
    Fine,
    /// An AC supply stopped (a static duty cycle, state x1): stop the charge in an orderly way.
    SupplyStopped,
    /// The connection is gone or the state is E, F or A where nothing was charging: end the session and
    /// close the connection.
    Fault,
    /// An emergency shutdown: the session ends at once, nothing more is sent, the connection is closed.
    /// (The host opens the contactors and sets the pilot by itself, it does not wait for the core.)
    Emergency,
}

/// Who is judging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Vehicle,
    Charger,
}

/// What the session machine wants watched *right now*: the host arms its own fast monitor with it (an interrupt, a
/// comparator, a loop of a few hundred microseconds) and reacts to an emergency itself, within the milliseconds the
/// standards give, without a trip through the session machine. The machine publishes a new guard in
/// `Step::pilot` whenever the expectation changes (the pilot is meant to go to C, an orderly stop begins, the
/// session ends); [`Guard::judge`] is the rule, the same one the machine applies when it is told a reading with
/// `Event::Pilot`.
///
/// Every field is a plain condition, so a monitor in hardware or in an ISR can implement the guard without this
/// crate. [`Guard::DISARMED`] (nothing wrong, whatever the pilot does) is the guard before and after a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guard {
    pub side: Side,
    /// State C or D has to hold: any other state is an emergency (IEC 61851-23 CC.4.3). For the vehicle it holds from its
    /// own change to C until it asks for B; for the charger from the moment it saw C or D until the stop.
    pub hold_ready: bool,
    /// The oscillator has to be on at 5 % (DC): another duty cycle or an oscillator that is off is an emergency
    /// (DIN V2G-DC-656, IEC 61851-23 CC.3.4.3). Vehicle only; the charger makes the pilot.
    pub hold_five_percent: bool,
    /// AC: a constant duty cycle (state x1) is the supply stopping, no emergency: stop the charge in order.
    pub static_stops_supply: bool,
    /// State A, E, F or an invalid voltage while no emergency is judged ends the session and closes the connection
    /// (DIN V2G-DC-655, ISO 15118-2 V2G2-737). On while a session is under way.
    pub close_on_fault: bool,
}

impl Guard {
    /// Nothing is judged.
    pub const DISARMED: Guard = Guard { side: Side::Vehicle, hold_ready: false, hold_five_percent: false, static_stops_supply: false, close_on_fault: false };

    pub const fn disarmed(side: Side) -> Guard {
        Guard { side, ..Guard::DISARMED }
    }

    /// Whether anything is judged.
    pub fn is_armed(&self) -> bool {
        self.hold_ready || self.hold_five_percent || self.static_stops_supply || self.close_on_fault
    }

    /// What `reading` means under this guard.
    pub fn judge(&self, r: Reading) -> Verdict {
        let fault_state = matches!(r.state, State::A | State::E | State::F);
        if self.hold_ready && !r.is_ready() {
            // The vehicle takes E, F and A for the end of the connection (DIN V2G-DC-655); the charger, which had the
            // vehicle in C or D, for an emergency.
            return if self.side == Side::Vehicle && self.close_on_fault && fault_state { Verdict::Fault } else { Verdict::Emergency };
        }
        if self.close_on_fault && fault_state {
            return Verdict::Fault;
        }
        if self.hold_five_percent && !r.pwm.is_five_percent() {
            return Verdict::Emergency;
        }
        if self.static_stops_supply && r.pwm == Pwm::Off {
            return Verdict::SupplyStopped;
        }
        Verdict::Fine
    }
}

/// The vehicle's view.
#[derive(Debug, Clone, Copy, Default)]
pub struct EvWatch {
    last: Option<Reading>,
    /// The vehicle itself holds the pilot in C or D (from its request until it asks for B).
    ready: bool,
    /// The charge parameter discovery was answered: the oscillator is meant to be on.
    discovered: bool,
    /// The session is being shut down (by the vehicle, or after the charger said so): the pilot is allowed
    /// to change and the oscillator to go off.
    stopping: bool,
    ac: bool,
}

impl EvWatch {
    pub const fn new(ac: bool) -> Self {
        Self { last: None, ready: false, discovered: false, stopping: false, ac }
    }

    /// The vehicle holds C or D (or no longer does).
    pub fn set_ready(&mut self, ready: bool) {
        self.ready = ready;
    }

    /// The charge parameter discovery was answered.
    pub fn set_discovered(&mut self, discovered: bool) {
        self.discovered = discovered;
    }

    /// The shutdown has begun: the pilot may change.
    pub fn set_stopping(&mut self, stopping: bool) {
        self.stopping = stopping;
    }

    pub fn last(&self) -> Option<Reading> {
        self.last
    }

    /// What is to be watched now. `active` is whether a session is under way (before `Start` and after the end nothing is).
    pub fn guard(&self, active: bool) -> Guard {
        let judged = active && !self.stopping;
        Guard {
            side: Side::Vehicle,
            hold_ready: judged && self.ready,
            hold_five_percent: judged && self.discovered && !self.ac,
            static_stops_supply: judged && self.discovered && self.ac,
            close_on_fault: active,
        }
    }

    /// Takes a reading (the host tells the machine what it measured) and judges it by the guard.
    pub fn reading(&mut self, r: Reading) -> Verdict {
        self.last = Some(r);
        self.guard(true).judge(r)
    }
}

/// The charger's view.
#[derive(Debug, Clone, Copy, Default)]
pub struct EvseWatch {
    last: Option<Reading>,
    /// The vehicle was in C or D: from here on a change away from it needs a reason.
    was_ready: bool,
    /// B is what the stop sequence leads to (the power delivery stop was answered, a `SessionStopReq`
    /// came, or the charger itself began the shutdown).
    stopping: bool,
    /// An AC session: the vehicle changing from C to B is how it stops the charge (IEC 61851-1), no emergency; the emergency
    /// rule of IEC 61851-23 CC.4.3 is DC's.
    ac: bool,
}

impl EvseWatch {
    pub const fn new() -> Self {
        Self { last: None, was_ready: false, stopping: false, ac: false }
    }

    /// The session is an AC one (known once the charge parameters or the service are chosen).
    pub fn set_ac(&mut self, ac: bool) {
        self.ac = ac;
    }

    /// The shutdown has begun (the power delivery stop is answered, `SessionStopReq` is in, or the charger
    /// signalled the end): B, and a missing oscillator, are meant now.
    pub fn set_stopping(&mut self) {
        self.stopping = true;
    }

    pub fn stopping(&self) -> bool {
        self.stopping
    }

    pub fn last(&self) -> Option<Reading> {
        self.last
    }

    /// What is to be watched now. `active` is whether a session is under way.
    pub fn guard(&self, active: bool) -> Guard {
        Guard {
            side: Side::Charger,
            hold_ready: active && self.was_ready && !self.stopping && !self.ac,
            hold_five_percent: false,
            static_stops_supply: false,
            close_on_fault: active && !self.stopping,
        }
    }

    /// CC.4.3: the transition from C/D to anything else that nothing explains is an emergency; E, F and A
    /// before the vehicle was ever ready end the session.
    pub fn reading(&mut self, r: Reading) -> Verdict {
        self.last = Some(r);
        if r.is_ready() {
            self.was_ready = true;
        }
        self.guard(true).judge(r)
    }

    /// V2G20-917, 918 (and V2G2-917, 918): the cable check is answered OK when the pilot is C or D, `FAILED`
    /// when it is not. Without a reading (the host does not report the pilot) it is.
    pub fn cable_check_may_pass(&self) -> bool {
        self.last.is_none_or(Reading::is_ready)
    }

    /// V2G20-920 to 922: after the stop the pilot is B before welding detection or session stop is answered OK.
    pub fn stop_may_pass(&self) -> bool {
        self.last.is_none_or(|r| r.state == State::B)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const B5: Reading = Reading::CONNECTED;
    const C5: Reading = Reading::CHARGING;
    const E: Reading = Reading { state: State::E, pwm: Pwm::Off };
    const C_OFF: Reading = Reading { state: State::C, pwm: Pwm::Off };
    const B_OFF: Reading = Reading { state: State::B, pwm: Pwm::Off };

    #[test]
    fn a_measurement_from_3_to_7_percent_is_five_percent() {
        for d in 0..=100u8 {
            assert_eq!(Pwm::Duty(d).is_five_percent(), (3..=7).contains(&d), "{d} %");
        }
        assert!(!Pwm::Off.is_five_percent());
        // The vehicle's DC guard takes the whole window and nothing else.
        let g = EvWatch { discovered: true, ..EvWatch::new(false) }.guard(true);
        assert_eq!(g.judge(Reading { state: State::B, pwm: Pwm::Duty(3) }), Verdict::Fine);
        assert_eq!(g.judge(Reading { state: State::B, pwm: Pwm::Duty(7) }), Verdict::Fine);
        assert_eq!(g.judge(Reading { state: State::B, pwm: Pwm::Duty(2) }), Verdict::Emergency);
        assert_eq!(g.judge(Reading { state: State::B, pwm: Pwm::Duty(8) }), Verdict::Emergency);
    }

    #[test]
    fn nominal_is_10_to_95_percent() {
        for d in 0..=100u8 {
            assert_eq!(Pwm::Duty(d).is_nominal(), (10..=95).contains(&d), "{d} %");
        }
        assert!(!Pwm::Off.is_nominal());
    }

    #[test]
    fn the_vehicle_tolerates_what_it_causes_itself() {
        let mut w = EvWatch::new(false);
        assert_eq!(w.reading(B5), Verdict::Fine);
        w.set_discovered(true);
        w.set_ready(true);
        assert_eq!(w.reading(C5), Verdict::Fine);
        // The vehicle asks for B at the stop: from then on B is meant.
        w.set_ready(false);
        w.set_stopping(true);
        assert_eq!(w.reading(B5), Verdict::Fine);
        assert_eq!(w.reading(B_OFF), Verdict::Fine, "the oscillator goes off as part of the shutdown");
    }

    #[test]
    fn a_dc_vehicle_stops_when_the_charger_turns_the_oscillator_off_or_the_state_leaves_c() {
        let mut w = EvWatch::new(false);
        w.set_discovered(true);
        w.set_ready(true);
        assert_eq!(w.reading(C_OFF), Verdict::Emergency);
        let mut w = EvWatch::new(false);
        w.set_discovered(true);
        w.set_ready(true);
        assert_eq!(w.reading(Reading { state: State::C, pwm: Pwm::Duty(50) }), Verdict::Emergency, "V2G-DC-656: not 5 %");
        let mut w = EvWatch::new(false);
        w.set_ready(true);
        assert_eq!(w.reading(B5), Verdict::Emergency, "C to B nobody asked for");
        let mut w = EvWatch::new(false);
        w.set_ready(true);
        assert_eq!(w.reading(Reading { state: State::Invalid, pwm: Pwm::Duty(5) }), Verdict::Emergency);
    }

    #[test]
    fn before_the_charge_parameters_the_duty_cycle_is_not_judged() {
        let mut w = EvWatch::new(false);
        assert_eq!(w.reading(B_OFF), Verdict::Fine);
        assert_eq!(w.reading(Reading { state: State::B, pwm: Pwm::Duty(50) }), Verdict::Fine);
    }

    #[test]
    fn e_f_and_a_close_the_connection_at_any_time() {
        for state in [State::E, State::F, State::A] {
            let mut w = EvWatch::new(false);
            assert_eq!(w.reading(Reading { state, pwm: Pwm::Off }), Verdict::Fault);
        }
        let mut w = EvWatch::new(true);
        w.set_stopping(true);
        assert_eq!(w.reading(E), Verdict::Fault, "also while stopping");
    }

    #[test]
    fn an_ac_vehicle_stops_in_order_when_the_supply_goes_away() {
        let mut w = EvWatch::new(true);
        w.set_discovered(true);
        w.set_ready(true);
        assert_eq!(w.reading(C_OFF), Verdict::SupplyStopped, "state x1 is not an emergency");
        assert_eq!(w.reading(Reading { state: State::C, pwm: Pwm::Duty(30) }), Verdict::Fine, "a current limit by PWM");
        assert_eq!(w.reading(B5), Verdict::Emergency, "but C to B is, in AC as well");
    }

    #[test]
    fn the_charger_sees_an_emergency_in_a_change_from_c_that_nothing_explains() {
        let mut w = EvseWatch::new();
        assert_eq!(w.reading(B5), Verdict::Fine);
        assert_eq!(w.reading(C5), Verdict::Fine);
        assert_eq!(w.reading(B5), Verdict::Emergency);
        let mut w = EvseWatch::new();
        w.reading(C5);
        assert_eq!(w.reading(E), Verdict::Emergency);
        // After the stop B is what is meant.
        let mut w = EvseWatch::new();
        w.reading(C5);
        w.set_stopping();
        assert_eq!(w.reading(B5), Verdict::Fine);
    }

    #[test]
    fn in_an_ac_session_the_vehicle_leaving_c_is_no_emergency_for_the_charger() {
        let mut w = EvseWatch::new();
        w.set_ac(true);
        assert_eq!(w.reading(C5), Verdict::Fine);
        assert_eq!(w.reading(B5), Verdict::Fine, "IEC 61851-1: the vehicle stops the charge by going to B");
        assert!(!w.guard(true).hold_ready);
        // E, F and A still end the session.
        assert_eq!(w.reading(E), Verdict::Fault);
    }

    #[test]
    fn the_charger_ends_the_session_when_the_cable_goes_before_anything_charged() {
        let mut w = EvseWatch::new();
        assert_eq!(w.reading(Reading { state: State::A, pwm: Pwm::Duty(5) }), Verdict::Fault);
        let mut w = EvseWatch::new();
        assert_eq!(w.reading(B5), Verdict::Fine);
        assert_eq!(w.reading(E), Verdict::Fault);
    }

    #[test]
    fn the_guard_is_the_rule_the_machine_applies() {
        // The host arms its own monitor with the guard; the machine judges a reading by the same rule.
        let readings = [
            Reading::CONNECTED,
            Reading::CHARGING,
            Reading { state: State::C, pwm: Pwm::Off },
            Reading { state: State::C, pwm: Pwm::Duty(50) },
            Reading { state: State::B, pwm: Pwm::Off },
            Reading { state: State::Invalid, pwm: Pwm::Duty(5) },
            Reading { state: State::E, pwm: Pwm::Off },
            Reading { state: State::A, pwm: Pwm::Duty(5) },
        ];
        for ac in [false, true] {
            for (ready, discovered, stopping) in [(false, false, false), (true, false, false), (true, true, false), (false, true, false), (true, true, true)] {
                let mut w = EvWatch::new(ac);
                w.set_ready(ready);
                w.set_discovered(discovered);
                w.set_stopping(stopping);
                let guard = w.guard(true);
                for r in readings {
                    assert_eq!(guard.judge(r), w.clone().reading(r), "vehicle ac={ac} ready={ready} discovered={discovered} stopping={stopping} {r:?}");
                }
            }
        }
        for (was_ready, stopping) in [(false, false), (true, false), (true, true)] {
            let mut w = EvseWatch::new();
            if was_ready {
                w.reading(Reading::CHARGING);
            }
            if stopping {
                w.set_stopping();
            }
            let guard = w.guard(true);
            for r in readings {
                assert_eq!(guard.judge(r), w.clone().reading(r), "charger was_ready={was_ready} stopping={stopping} {r:?}");
            }
        }
    }

    #[test]
    fn the_guard_is_armed_for_the_phases_of_the_session_and_not_outside() {
        let mut w = EvWatch::new(false);
        assert!(!w.guard(false).is_armed(), "no session");
        assert!(w.guard(true).close_on_fault && !w.guard(true).hold_ready, "a session: only the fault states");
        w.set_discovered(true);
        assert!(w.guard(true).hold_five_percent && !w.guard(true).hold_ready);
        w.set_ready(true);
        assert!(w.guard(true).hold_ready, "C is held from the vehicle's own change");
        w.set_stopping(true);
        let g = w.guard(true);
        assert!(!g.hold_ready && !g.hold_five_percent && g.close_on_fault, "the shutdown may change the pilot");
        assert!(!w.guard(false).is_armed());
        let mut c = EvseWatch::new();
        assert!(!c.guard(true).hold_ready);
        c.reading(Reading::CHARGING);
        assert!(c.guard(true).hold_ready, "once the charger saw C it holds");
        c.set_stopping();
        assert!(!c.guard(true).hold_ready, "the stop makes B meant");
    }

    #[test]
    fn the_charger_waits_for_the_pilot_the_standards_name() {
        let mut w = EvseWatch::new();
        assert!(w.cable_check_may_pass() && w.stop_may_pass(), "no reading, no check");
        w.reading(B5);
        assert!(!w.cable_check_may_pass(), "V2G20-918: the cable check wants C or D");
        w.reading(C5);
        assert!(w.cable_check_may_pass());
        assert!(!w.stop_may_pass(), "V2G20-922: after the stop B");
        w.set_stopping();
        w.reading(B5);
        assert!(w.stop_may_pass());
    }
}
