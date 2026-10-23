//! The charger's control pilot doing what the matching machine asks ([`evse_core::slac::PilotRequest`]), on the simulated cable.

use std::thread::{self, JoinHandle};
use std::time::Duration;

use control_pilot::State;
use evcc_linux::pilot::{Oscillator, Wire};
use evse_core::slac::PilotRequest;

/// The oscillator the pilot returns to after a cycle when it was off or in an error state before.
const RESUME_DUTY: u8 = 5;

/// Carries out `request` on `wire`; blocks for the time of the sequence. `x1_ms` is how long the oscillator stays off where the
/// sequence goes through X1.
///
/// An error cycle (V2G3-M07-04, 05, 07 to 09, M06-07): through X1 first when the oscillator is on, then state E for the time asked
/// (at least `T_step_EF`), then the oscillator on again at the duty cycle it had (5 % if it had none). The state change from
/// E to B is what starts the matching process again. X1 alone (V2G3-M06-07 after the retries) turns the oscillator off.
pub fn run(wire: &Wire, request: PilotRequest, x1_ms: u32) -> std::io::Result<()> {
    match request {
        PilotRequest::X1 => wire.set_evse_signal(Oscillator::Off),
        PilotRequest::ErrorCycle { via_x1, ms } => {
            let resume = match wire.evse() {
                Some(Oscillator::Duty(d)) => Oscillator::Duty(d),
                _ => Oscillator::Duty(RESUME_DUTY),
            };
            if via_x1 {
                wire.set_evse_signal(Oscillator::Off)?;
                thread::sleep(Duration::from_millis(x1_ms.into()));
            }
            wire.set_evse_signal(Oscillator::Error(State::E))?;
            thread::sleep(Duration::from_millis(ms.into()));
            wire.set_evse_signal(resume)
        }
    }
}

/// The charger changes from 5 % to the nominal duty cycle `duty` (V2G3-M06-05, 06, 08): with a link (the matching is done or under way)
/// through X1 for `x1_ms`, which tells the vehicle that the duty cycle is going to change; without one through state E for `ef_ms`
/// (at least `T_step_EF`), which is how a vehicle that has not seen any communication starts over. Also the sequence after a positive
/// EIM when no matching is running (M06-08: E/F, then X1 or X2 as nominal). The charger's policy for when to change is the host's.
pub fn to_nominal(wire: &Wire, duty: u8, linked: bool, x1_ms: u32, ef_ms: u32) -> std::io::Result<()> {
    if linked {
        wire.set_evse_signal(Oscillator::Off)?;
        thread::sleep(Duration::from_millis(x1_ms.into()));
    } else {
        wire.set_evse_signal(Oscillator::Error(State::E))?;
        thread::sleep(Duration::from_millis(ef_ms.into()));
    }
    wire.set_evse_signal(Oscillator::Duty(duty))
}

/// [`run`] in a thread of its own, so that the matching goes on being served.
pub fn spawn(wire: Wire, request: PilotRequest, x1_ms: u32) -> JoinHandle<()> {
    thread::spawn(move || {
        if let Err(e) = run(&wire, request, x1_ms) {
            eprintln!("pilot: {request:?}: {e}");
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    use control_pilot::{Pwm, Reading};

    use super::*;

    /// What a vehicle in state B sees on the cable, with the time.
    fn watch(wire: Wire, for_ms: u64) -> JoinHandle<Vec<(u64, Reading)>> {
        thread::spawn(move || {
            let (start, mut source, mut seen) = (Instant::now(), wire.source(), Vec::new());
            while start.elapsed() < Duration::from_millis(for_ms) {
                use evcc_linux::pilot::PilotSource;
                if let Some(r) = source.poll() {
                    seen.push((start.elapsed().as_millis() as u64, r));
                }
                thread::sleep(Duration::from_millis(1));
            }
            seen
        })
    }

    fn cable(name: &str) -> Wire {
        let wire = Wire::new(std::env::temp_dir().join(format!("sequence-{name}-{}", std::process::id()))).unwrap();
        wire.set_ev(State::B).unwrap();
        wire.set_evse(Pwm::Duty(5)).unwrap();
        wire
    }

    #[test]
    fn an_error_cycle_goes_through_x1_and_e_and_comes_back_at_the_duty_cycle_it_had() {
        let wire = cable("cycle");
        wire.set_evse(Pwm::Duty(53)).unwrap();
        let seen = watch(wire.clone(), 600);
        thread::sleep(Duration::from_millis(50));
        run(&wire, PilotRequest::ErrorCycle { via_x1: true, ms: 150 }, 100).unwrap();
        let seen = seen.join().unwrap();
        let states: Vec<Reading> = seen.iter().map(|(_, r)| *r).collect();
        assert_eq!(
            states,
            vec![
                Reading { state: State::B, pwm: Pwm::Duty(53) },
                Reading { state: State::B, pwm: Pwm::Off },
                Reading { state: State::E, pwm: Pwm::Off },
                Reading { state: State::B, pwm: Pwm::Duty(53) },
            ]
        );
        let at = |i: usize| seen[i].0;
        assert!(at(2) - at(1) >= 95, "X1 for the time given");
        assert!(at(3) - at(2) >= 145, "E for at least the time asked");
    }

    #[test]
    fn a_cycle_from_x1_goes_through_e_only_and_resumes_at_five_percent() {
        let wire = cable("x1");
        wire.set_evse(Pwm::Off).unwrap();
        let seen = watch(wire.clone(), 400);
        thread::sleep(Duration::from_millis(50));
        run(&wire, PilotRequest::ErrorCycle { via_x1: false, ms: 80 }, 100).unwrap();
        let states: Vec<Reading> = seen.join().unwrap().into_iter().map(|(_, r)| r).collect();
        assert_eq!(
            states,
            vec![Reading { state: State::B, pwm: Pwm::Off }, Reading { state: State::E, pwm: Pwm::Off }, Reading::CONNECTED]
        );
    }

    #[test]
    fn the_change_to_a_nominal_duty_cycle_goes_through_x1_with_a_link_and_through_e_without() {
        let wire = cable("nominal");
        let seen = watch(wire.clone(), 500);
        thread::sleep(Duration::from_millis(30));
        to_nominal(&wire, 53, true, 60, 60).unwrap();
        thread::sleep(Duration::from_millis(30));
        wire.set_evse(Pwm::Duty(5)).unwrap();
        thread::sleep(Duration::from_millis(30));
        to_nominal(&wire, 53, false, 60, 70).unwrap();
        let states: Vec<Reading> = seen.join().unwrap().into_iter().map(|(_, r)| r).collect();
        let b = |pwm| Reading { state: State::B, pwm };
        assert_eq!(
            states,
            vec![b(Pwm::Duty(5)), b(Pwm::Off), b(Pwm::Duty(53)), b(Pwm::Duty(5)), Reading { state: State::E, pwm: Pwm::Off }, b(Pwm::Duty(53))]
        );
    }

    #[test]
    fn x1_turns_the_oscillator_off_and_leaves_it() {
        let wire = cable("off");
        run(&wire, PilotRequest::X1, 0).unwrap();
        assert_eq!(wire.reading(), Some(Reading { state: State::B, pwm: Pwm::Off }));
    }

    #[test]
    fn the_thread_runs_the_same() {
        let wire = cable("thread");
        let done = Arc::new(Mutex::new(false));
        let flag = done.clone();
        let handle = spawn(wire.clone(), PilotRequest::ErrorCycle { via_x1: false, ms: 50 }, 0);
        thread::spawn(move || {
            handle.join().unwrap();
            *flag.lock().unwrap() = true;
        });
        thread::sleep(Duration::from_millis(20));
        assert_eq!(wire.reading().map(|r| r.state), Some(State::E));
        thread::sleep(Duration::from_millis(150));
        assert!(*done.lock().unwrap());
        assert_eq!(wire.reading(), Some(Reading::CONNECTED));
    }
}
