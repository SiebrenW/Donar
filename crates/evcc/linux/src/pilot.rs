//! The control pilot for a host: where the readings come from.
//!
//! A real host measures the pilot (voltage state and PWM) in hardware and reports every change to the
//! session machine at once ([`PilotSource`]). The test platform has no hardware: [`Wire`] is a cable made of two small
//! files in a directory both simulators can see, one per side, that each side writes and the other
//! side reads, with the same readings a real measurement would give.
//!
//! * `ev`: the state the vehicle holds (`A`, `B`, `C`, `D`, `E`, `F`, or `X` for an invalid voltage),
//! * `evse`: the charger's oscillator: `off` (a constant voltage, 100 %) or the duty cycle in percent; or `E` or `F`, the
//!   charger holding the pilot in an error state (it pulls the line to 0 V or -12 V, whatever the vehicle does).
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

/// What the charger puts on the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Oscillator {
    /// A constant voltage (X1).
    Off,
    /// The oscillator at this duty cycle in percent (X2).
    Duty(u8),
    /// State E or F, which the vehicle sees whatever state it holds.
    Error(State),
}

impl Oscillator {
    fn text(self) -> String {
        match self {
            Oscillator::Off => "off".into(),
            Oscillator::Duty(d) => d.to_string(),
            Oscillator::Error(State::F) => "F".into(),
            Oscillator::Error(_) => "E".into(),
        }
    }

    fn parse(text: &str) -> Option<Oscillator> {
        Some(match text.trim() {
            "off" => Oscillator::Off,
            "E" => Oscillator::Error(State::E),
            "F" => Oscillator::Error(State::F),
            d => Oscillator::Duty(d.parse().ok()?),
        })
    }
}

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
        self.set_evse_signal(match pwm {
            Pwm::Off => Oscillator::Off,
            Pwm::Duty(d) => Oscillator::Duty(d),
        })
    }

    /// What the charger puts on the line, including an error state.
    pub fn set_evse_signal(&self, signal: Oscillator) -> io::Result<()> {
        self.write("evse", signal.text().as_bytes())
    }

    /// What the charger puts on the line now, if it wrote anything.
    pub fn evse(&self) -> Option<Oscillator> {
        Oscillator::parse(std::str::from_utf8(&fs::read(self.dir.join("evse")).ok()?).ok()?)
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
        Some(match Oscillator::parse(std::str::from_utf8(&evse).ok()?)? {
            Oscillator::Off => Reading { state, pwm: Pwm::Off },
            Oscillator::Duty(d) => Reading { state, pwm: Pwm::Duty(d) },
            // The charger's error state is what both sides measure.
            Oscillator::Error(state) => Reading { state, pwm: Pwm::Off },
        })
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
        // The charger holds the line in E: the vehicle's state does not matter.
        wire.set_evse_signal(Oscillator::Error(State::E)).unwrap();
        assert_eq!(source.poll(), Some(Reading { state: State::E, pwm: Pwm::Off }));
        assert_eq!(wire.evse(), Some(Oscillator::Error(State::E)));
        wire.set_evse_signal(Oscillator::Duty(5)).unwrap();
        assert_eq!(source.poll(), Some(Reading::CHARGING));
        let _ = fs::remove_dir_all(dir);
    }
}
