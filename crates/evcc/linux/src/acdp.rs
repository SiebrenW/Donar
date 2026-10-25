//! The physical world of an ACDP charging point for the simulators: where the vehicle is. The vehicle simulator writes it,
//! the charger simulator's pairing and positioning device (PPD) reads it, like the control pilot [`crate::pilot::Wire`]: one
//! small file in a directory both read and write, written aside and renamed so a reader never sees half of it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Where the vehicle is and who it says it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VehiclePosition {
    /// The EVID the vehicle's marker carries (the tags on the roof of a bus, ISO 15118-20 Annex E).
    pub evid: String,
    /// The contact interface of the vehicle against the pantograph's, in millimetres: x in the driving direction (negative
    /// while the vehicle approaches, Annex D), y lateral.
    pub x_mm: i32,
    pub y_mm: i32,
}

/// The directory both simulators of a scenario use.
#[derive(Debug, Clone)]
pub struct World {
    dir: PathBuf,
}

impl World {
    pub fn new(dir: impl Into<PathBuf>) -> io::Result<World> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(World { dir })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The vehicle is here now.
    pub fn set_vehicle(&self, position: &VehiclePosition) -> io::Result<()> {
        // Written aside and renamed, so a reader never sees half of it.
        let tmp = self.dir.join(".vehicle.tmp");
        fs::write(&tmp, format!("{} {} {}\n", position.evid, position.x_mm, position.y_mm))?;
        fs::rename(tmp, self.dir.join("vehicle"))
    }

    /// The vehicle's position, if it wrote one. A read that finds nothing is repeated a few times: on some file systems (a bind
    /// mount of the host's directory into the container) a reader can miss the file for a moment while the writer replaces it,
    /// and what reads the position, the PPD, must not take that for a vehicle that left.
    pub fn vehicle(&self) -> Option<VehiclePosition> {
        for attempt in 0..5 {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            if let Some(position) = self.read_vehicle() {
                return Some(position);
            }
        }
        None
    }

    fn read_vehicle(&self) -> Option<VehiclePosition> {
        let text = fs::read_to_string(self.dir.join("vehicle")).ok()?;
        let mut parts = text.split_whitespace();
        let evid = parts.next()?.to_string();
        let x_mm = parts.next()?.parse().ok()?;
        let y_mm = parts.next()?.parse().ok()?;
        Some(VehiclePosition { evid, x_mm, y_mm })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_vehicle_position_goes_through_the_directory() {
        let dir = std::env::temp_dir().join(format!("acdp-world-{}", std::process::id()));
        let world = World::new(&dir).unwrap();
        assert_eq!(world.vehicle(), None, "nothing written yet");
        let position = VehiclePosition { evid: "BUS42".into(), x_mm: -1200, y_mm: 35 };
        world.set_vehicle(&position).unwrap();
        assert_eq!(world.vehicle(), Some(position));
        world.set_vehicle(&VehiclePosition { evid: "BUS42".into(), x_mm: 4, y_mm: -2 }).unwrap();
        assert_eq!(world.vehicle().map(|p| (p.x_mm, p.y_mm)), Some((4, -2)));
        let _ = fs::remove_dir_all(dir);
    }
}
