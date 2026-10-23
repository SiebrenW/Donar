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
    run_with(wire, request, x1_ms, sleep)
}

/// [`run`] with the waiting handed in (a test records what is on the cable at every wait instead of watching the clock).
fn run_with(wire: &Wire, request: PilotRequest, x1_ms: u32, mut wait: impl FnMut(u32)) -> std::io::Result<()> {
    match request {
        PilotRequest::X1 => wire.set_evse_signal(Oscillator::Off),
        PilotRequest::ErrorCycle { via_x1, ms } => {
            let resume = match wire.evse() {
                Some(Oscillator::Duty(d)) => Oscillator::Duty(d),
                _ => Oscillator::Duty(RESUME_DUTY),
            };
            if via_x1 {
                wire.set_evse_signal(Oscillator::Off)?;
                wait(x1_ms);
            }
            wire.set_evse_signal(Oscillator::Error(State::E))?;
            wait(ms);
            wire.set_evse_signal(resume)
        }
    }
}

/// The charger changes from 5 % to the nominal duty cycle `duty` (V2G3-M06-05, 06, 08): with a link (the matching is done or under way)
/// through X1 for `x1_ms`, which tells the vehicle that the duty cycle is going to change; without one through state E for `ef_ms`
/// (at least `T_step_EF`), which is how a vehicle that has not seen any communication starts over. Also the sequence after a positive
/// EIM when no matching is running (M06-08: E/F, then X1 or X2 as nominal). The charger's policy for when to change is the host's.
pub fn to_nominal(wire: &Wire, duty: u8, linked: bool, x1_ms: u32, ef_ms: u32) -> std::io::Result<()> {
    to_nominal_with(wire, duty, linked, x1_ms, ef_ms, sleep)
}

fn to_nominal_with(wire: &Wire, duty: u8, linked: bool, x1_ms: u32, ef_ms: u32, mut wait: impl FnMut(u32)) -> std::io::Result<()> {
    if linked {
        wire.set_evse_signal(Oscillator::Off)?;
        wait(x1_ms);
    } else {
        wire.set_evse_signal(Oscillator::Error(State::E))?;
        wait(ef_ms);
    }
    wire.set_evse_signal(Oscillator::Duty(duty))
}

fn sleep(ms: u32) {
    thread::sleep(Duration::from_millis(ms.into()));
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
    use control_pilot::{Pwm, Reading};

    use super::*;

    fn cable(name: &str) -> Wire {
        let wire = Wire::new(std::env::temp_dir().join(format!("sequence-{name}-{}", std::process::id()))).unwrap();
        wire.set_ev(State::B).unwrap();
        wire.set_evse(Pwm::Duty(5)).unwrap();
        wire
    }

    fn b(pwm: Pwm) -> Reading {
        Reading { state: State::B, pwm }
    }

    const E: Reading = Reading { state: State::E, pwm: Pwm::Off };

    /// What the vehicle (in state B) sees on the cable at every wait of `sequence`, with the length of the wait.
    fn trace(wire: &Wire, sequence: impl FnOnce(&mut dyn FnMut(u32))) -> Vec<(Reading, u32)> {
        let mut seen = Vec::new();
        sequence(&mut |ms| seen.push((wire.reading().unwrap(), ms)));
        seen
    }

    #[test]
    fn an_error_cycle_goes_through_x1_and_e_and_comes_back_at_the_duty_cycle_it_had() {
        let wire = cable("cycle");
        wire.set_evse(Pwm::Duty(53)).unwrap();
        let seen = trace(&wire, |wait| run_with(&wire, PilotRequest::ErrorCycle { via_x1: true, ms: 4000 }, 3000, wait).unwrap());
        // X1 for the time given, then E for the time asked, then back.
        assert_eq!(seen, vec![(b(Pwm::Off), 3000), (E, 4000)]);
        assert_eq!(wire.reading(), Some(b(Pwm::Duty(53))));
    }

    #[test]
    fn a_cycle_from_x1_goes_through_e_only_and_resumes_at_five_percent() {
        let wire = cable("x1");
        wire.set_evse(Pwm::Off).unwrap();
        let seen = trace(&wire, |wait| run_with(&wire, PilotRequest::ErrorCycle { via_x1: false, ms: 80 }, 100, wait).unwrap());
        assert_eq!(seen, vec![(E, 80)]);
        assert_eq!(wire.reading(), Some(Reading::CONNECTED));
    }

    #[test]
    fn the_change_to_a_nominal_duty_cycle_goes_through_x1_with_a_link_and_through_e_without() {
        let wire = cable("nominal");
        let seen = trace(&wire, |wait| to_nominal_with(&wire, 53, true, 60, 70, wait).unwrap());
        assert_eq!(seen, vec![(b(Pwm::Off), 60)]);
        assert_eq!(wire.reading(), Some(b(Pwm::Duty(53))));
        wire.set_evse(Pwm::Duty(5)).unwrap();
        let seen = trace(&wire, |wait| to_nominal_with(&wire, 53, false, 60, 70, wait).unwrap());
        assert_eq!(seen, vec![(E, 70)]);
        assert_eq!(wire.reading(), Some(b(Pwm::Duty(53))));
    }

    #[test]
    fn x1_turns_the_oscillator_off_and_leaves_it() {
        let wire = cable("off");
        run(&wire, PilotRequest::X1, 0).unwrap();
        assert_eq!(wire.reading(), Some(b(Pwm::Off)));
    }

    #[test]
    fn the_thread_does_the_cycle_and_ends() {
        let wire = cable("thread");
        spawn(wire.clone(), PilotRequest::ErrorCycle { via_x1: false, ms: 20 }, 0).join().unwrap();
        assert_eq!(wire.reading(), Some(Reading::CONNECTED));
    }
}
