//! The control pilot for a host: where the readings come from.
//!
//! A real host measures the pilot (voltage state and PWM) in hardware and reports every change to the
//! session machine at once ([`PilotSource`]). The test platform has no hardware: [`Wire`] is a cable made of two small
//! files in a directory both simulators can see, one per side, that each side writes and the other
//! side reads, with the same readings a real measurement would give.
//!
//! * `ev`: the state the vehicle holds (`A`, `B`, `C`, `D`, `E`, `F`, or `X` for an invalid voltage),
//! * `evse`: the charger's oscillator: `off` (a constant voltage, 100 %) or the duty cycle in percent.
//!
//! A reading exists once both files do: until the charger has written its oscillator, or while no vehicle
//! is connected, there is none, and nothing is judged.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use control_pilot::{Pwm, Reading, State};

/// A source of readings a host polls between its other work (between socket reads).
pub trait PilotSource {
    /// A reading if the pilot changed since the last call.
    fn poll(&mut self) -> Option<Reading>;
    /// The session machine published a new guard (`Step::pilot`): what the host's own monitor is to judge from now on.
    /// A host that watches the pilot in an interrupt or in hardware arms that monitor here; the default does nothing and
    /// leaves the judging to the machine, which applies the same rule to every reading it is given.
    fn arm(&mut self, _guard: control_pilot::Guard) {}
}

/// How often a host with a pilot source looks for a change while it waits for the socket.
pub const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2);

/// The cable between two simulators.
#[derive(Debug, Clone)]
pub struct Wire {
    dir: PathBuf,
}

impl Wire {
    /// The cable in `dir` (created if it is not there).
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<Wire> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Wire { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The vehicle holds this state (`A` after it let go of the cable).
    pub fn set_ev(&self, state: State) -> io::Result<()> {
        self.write("ev", &[letter(state)])
    }

    /// The charger's oscillator.
    pub fn set_evse(&self, pwm: Pwm) -> io::Result<()> {
        match pwm {
            Pwm::Off => self.write("evse", b"off"),
            Pwm::Duty(d) => self.write("evse", d.to_string().as_bytes()),
        }
    }

    /// What either side measures now, if both are there.
    pub fn reading(&self) -> Option<Reading> {
        let ev = fs::read(self.dir.join("ev")).ok()?;
        let evse = fs::read(self.dir.join("evse")).ok()?;
        let state = match *ev.first()? {
            b'A' => State::A,
            b'B' => State::B,
            b'C' => State::C,
            b'D' => State::D,
            b'E' => State::E,
            b'F' => State::F,
            _ => State::Invalid,
        };
        let text = std::str::from_utf8(&evse).ok()?.trim();
        let pwm = if text == "off" { Pwm::Off } else { Pwm::Duty(text.parse().ok()?) };
        Some(Reading { state, pwm })
    }

    /// The readings of this cable as a [`PilotSource`]: a change is reported once.
    pub fn source(&self) -> WireSource {
        WireSource { wire: self.clone(), last: None, guard: control_pilot::Guard::DISARMED }
    }

    fn write(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        // Written aside and renamed, so a reader never sees half of it.
        let tmp = self.dir.join(format!(".{name}.tmp"));
        fs::write(&tmp, bytes)?;
        fs::rename(tmp, self.dir.join(name))
    }
}

fn letter(state: State) -> u8 {
    match state {
        State::A => b'A',
        State::B => b'B',
        State::C => b'C',
        State::D => b'D',
        State::E => b'E',
        State::F => b'F',
        State::Invalid => b'X',
    }
}

/// See [`Wire::source`].
pub struct WireSource {
    wire: Wire,
    last: Option<Reading>,
    guard: control_pilot::Guard,
}

impl WireSource {
    /// The guard the session machine last published.
    pub fn guard(&self) -> control_pilot::Guard {
        self.guard
    }
}

impl PilotSource for WireSource {
    fn arm(&mut self, guard: control_pilot::Guard) {
        self.guard = guard;
    }

    fn poll(&mut self) -> Option<Reading> {
        let now = self.wire.reading()?;
        (self.last != Some(now)).then(|| {
            self.last = Some(now);
            now
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wire_gives_a_reading_once_both_sides_wrote_and_reports_changes_once() {
        let dir = std::env::temp_dir().join(format!("wire-{}", std::process::id()));
        let wire = Wire::new(&dir).unwrap();
        let mut source = wire.source();
        assert_eq!(source.poll(), None);
        wire.set_ev(State::B).unwrap();
        assert_eq!(source.poll(), None, "the charger has not written yet");
        wire.set_evse(Pwm::Duty(5)).unwrap();
        assert_eq!(source.poll(), Some(Reading::CONNECTED));
        assert_eq!(source.poll(), None, "reported once");
        wire.set_ev(State::C).unwrap();
        assert_eq!(source.poll(), Some(Reading::CHARGING));
        wire.set_evse(Pwm::Off).unwrap();
        assert_eq!(source.poll(), Some(Reading { state: State::C, pwm: Pwm::Off }));
        let _ = fs::remove_dir_all(dir);
    }
}
