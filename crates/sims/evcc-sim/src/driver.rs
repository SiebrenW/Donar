//! The driver of a simulated ACDP vehicle (a bus): follows a scripted approach to the pantograph, publishes where the contact
//! interface is into the world the charger's pairing and positioning device (PPD) reads, brakes at the end of the approach and
//! judges whether the vehicle is in position, from the charger's guidance or, without it, by its own means.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use evcc_linux::acdp::{VehiclePosition, World};

use crate::settings::{Acdp, ApproachPoint};

/// The scripted path, linear between its points, still after the last one.
#[derive(Clone)]
struct Approach {
    started: Instant,
    points: Vec<ApproachPoint>,
}

impl Approach {
    fn at(&self, elapsed_ms: u32) -> (i32, i32) {
        let (Some(first), Some(last)) = (self.points.first(), self.points.last()) else { return (0, 0) };
        if elapsed_ms <= first.t_ms {
            return (first.x_mm, first.y_mm);
        }
        if elapsed_ms >= last.t_ms {
            return (last.x_mm, last.y_mm);
        }
        let i = self.points.iter().rposition(|p| p.t_ms <= elapsed_ms).unwrap_or(0);
        let (a, b) = (&self.points[i], &self.points[i + 1]);
        let (span, into) = (i64::from(b.t_ms - a.t_ms).max(1), i64::from(elapsed_ms - a.t_ms));
        let lerp = |from: i32, to: i32| (i64::from(from) + (i64::from(to) - i64::from(from)) * into / span) as i32;
        (lerp(a.x_mm, b.x_mm), lerp(a.y_mm, b.y_mm))
    }

    /// The signal strength of the last point passed that names one.
    fn wlan_at(&self, elapsed_ms: u32, default: i16) -> i16 {
        self.points.iter().take_while(|p| p.t_ms <= elapsed_ms).filter_map(|p| p.wlan_dbm).last().unwrap_or(default)
    }

    fn elapsed_ms(&self) -> u32 {
        self.started.elapsed().as_millis().min(u128::from(u32::MAX)) as u32
    }
}

pub struct Driver {
    world: World,
    approach: Approach,
    settings: Acdp,
    /// What the charger last said: it guides the vehicle, and the vehicle is within the contact windows.
    guided: AtomicBool,
    guided_in_position: AtomicBool,
    stop: Arc<AtomicBool>,
}

impl Driver {
    /// Starts the approach: from now the position is published every 20 ms until the driver is dropped.
    pub fn start(settings: &Acdp) -> io::Result<Arc<Driver>> {
        let dir = settings.world.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "an ACDP vehicle needs the world directory (acdp.world)"))?;
        let world = World::new(dir)?;
        let approach = Approach { started: Instant::now(), points: settings.approach.clone() };
        let stop = Arc::new(AtomicBool::new(false));
        let driver = Arc::new(Driver { world: world.clone(), approach: approach.clone(), settings: settings.clone(), guided: AtomicBool::new(false), guided_in_position: AtomicBool::new(false), stop: stop.clone() });
        let evid = settings.evid.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let (x_mm, y_mm) = approach.at(approach.elapsed_ms());
                let _ = world.set_vehicle(&VehiclePosition { evid: evid.clone(), x_mm, y_mm });
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Ok(driver)
    }

    /// Whether the contacts have a connection: the pantograph is down. Through them the vehicle sees the control pilot (B), before
    /// that nothing (A).
    pub fn pantograph_down(&self) -> bool {
        self.world.pantograph().as_deref() == Some("EndPosition")
    }

    /// The radio's signal strength now, dBm.
    pub fn wlan_dbm(&self) -> i16 {
        self.approach.wlan_at(self.approach.elapsed_ms(), self.settings.wlan_dbm)
    }

    pub fn evid(&self) -> &str {
        &self.settings.evid
    }

    pub fn positioning_support(&self) -> bool {
        self.settings.positioning_support
    }

    /// The vehicle's contact interface against the pantograph's, now.
    pub fn position(&self) -> (i32, i32) {
        self.approach.at(self.approach.elapsed_ms())
    }

    /// What the charger's last answer said (`Notice::AcdpPosition`).
    pub fn guidance(&self, charger_guides: bool, in_position: bool) {
        self.guided.store(charger_guides, Ordering::Relaxed);
        self.guided_in_position.store(in_position, Ordering::Relaxed);
    }

    /// Positioned and braked: the approach is over for `brake_ms`, and the vehicle is in position by the charger's guidance, or by
    /// the driver's own judgement when the charger does not guide.
    pub fn immobilized(&self) -> bool {
        let Some(last) = self.approach.points.last() else { return true };
        if self.approach.elapsed_ms() < last.t_ms.saturating_add(self.settings.brake_ms) {
            return false;
        }
        if self.guided.load(Ordering::Relaxed) {
            return self.guided_in_position.load(Ordering::Relaxed);
        }
        let (x, y) = self.position();
        x.unsigned_abs() <= u32::from(self.settings.own_window_x_mm.unsigned_abs()) && y.unsigned_abs() <= u32::from(self.settings.own_window_y_mm.unsigned_abs())
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(t_ms: u32, x_mm: i32, y_mm: i32) -> ApproachPoint {
        ApproachPoint { t_ms, x_mm, y_mm, wlan_dbm: None }
    }

    #[test]
    fn the_approach_is_linear_between_its_points_and_still_outside_them() {
        let a = Approach { started: Instant::now(), points: vec![p(1000, -3000, 200), p(2000, -1000, 0), p(3000, 0, 0)] };
        assert_eq!(a.at(0), (-3000, 200), "before the first point");
        assert_eq!(a.at(1500), (-2000, 100));
        assert_eq!(a.at(2000), (-1000, 0));
        assert_eq!(a.at(2500), (-500, 0));
        assert_eq!(a.at(9000), (0, 0), "after the last");
        assert_eq!(Approach { started: Instant::now(), points: vec![] }.at(5), (0, 0));
    }

    #[test]
    fn the_signal_strength_is_the_last_one_a_point_named() {
        let mut points = vec![p(0, -9000, 0), p(1000, -3000, 0), p(2000, -100, 0)];
        points[1].wlan_dbm = Some(-75);
        points[2].wlan_dbm = Some(-88);
        let a = Approach { started: Instant::now(), points };
        assert_eq!(a.wlan_at(500, -62), -62, "before any point that names one");
        assert_eq!(a.wlan_at(1500, -62), -75);
        assert_eq!(a.wlan_at(2500, -62), -88);
    }
}
