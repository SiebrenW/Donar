//! The pairing and positioning device (PPD) of the simulated ACDP charger: it reads the vehicle's marker and, with an
//! optical device, its position, from the world the vehicle simulator writes ([`evcc_linux::acdp::World`]).

use std::io;

use evcc_linux::acdp::{VehiclePosition, World};
use evse_core::secc::AcdpPositioning;

use crate::config::{Acdp, Ppd as Kind};

pub(crate) struct Ppd<'a> {
    acdp: &'a Acdp,
    world: World,
}

impl<'a> Ppd<'a> {
    pub(crate) fn new(acdp: &'a Acdp) -> io::Result<Self> {
        let dir = acdp.world.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "an ACDP charger needs the world directory (acdp.world)"))?;
        Ok(Self { acdp, world: World::new(dir)? })
    }

    /// The vehicle the device reads: its marker is inside the detection zone.
    pub(crate) fn detect(&self) -> Option<VehiclePosition> {
        let v = self.world.vehicle()?;
        (v.x_mm.unsigned_abs() <= self.acdp.detection_zone_x_mm && v.y_mm.unsigned_abs() <= self.acdp.detection_zone_y_mm).then_some(v)
    }

    /// What goes into `ACDP_VehiclePositioningRes` for the vehicle that was paired: no association when the device no longer reads
    /// its EVID (it left the zone, or another vehicle's marker is read).
    pub(crate) fn positioning(&self, paired_evid: &str) -> AcdpPositioning {
        let Some(v) = self.detect().filter(|v| v.evid == paired_evid) else {
            return AcdpPositioning { association: false, ..AcdpPositioning::UNGUIDED };
        };
        match self.acdp.ppd {
            // The basic device knows only that the marker is in the zone (Annex E.2, NOTE 2 of 8.3.4.7.5.1).
            Kind::Rfid => AcdpPositioning { association: true, positioning_support: false, x_deviation_mm: 0, y_deviation_mm: 0, window_x_mm: 0, window_y_mm: 0, in_charge_position: true },
            Kind::Optical => {
                let (x, y) = (v.x_mm.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16, v.y_mm.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16);
                // The contact matching condition (Annex D): both deviations inside their windows.
                let in_position = x.unsigned_abs() < self.acdp.contact_window_x_mm.unsigned_abs() && y.unsigned_abs() < self.acdp.contact_window_y_mm.unsigned_abs();
                AcdpPositioning {
                    association: true,
                    positioning_support: true,
                    x_deviation_mm: x,
                    y_deviation_mm: y,
                    window_x_mm: self.acdp.contact_window_x_mm,
                    window_y_mm: self.acdp.contact_window_y_mm,
                    in_charge_position: in_position,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acdp(ppd: Kind, name: &str) -> Acdp {
        let dir = std::env::temp_dir().join(format!("ppd-{}-{name}", std::process::id()));
        Acdp { ppd, world: Some(dir.display().to_string()), ..Acdp::default() }
    }

    fn put(acdp: &Acdp, evid: &str, x_mm: i32, y_mm: i32) {
        World::new(acdp.world.as_ref().unwrap()).unwrap().set_vehicle(&VehiclePosition { evid: evid.into(), x_mm, y_mm }).unwrap();
    }

    #[test]
    fn the_device_reads_the_marker_only_inside_its_zone() {
        let a = acdp(Kind::Rfid, "zone");
        let ppd = Ppd::new(&a).unwrap();
        assert!(ppd.detect().is_none(), "nothing there");
        put(&a, "BUS42", -8000, 0);
        assert!(ppd.detect().is_none(), "too far away");
        put(&a, "BUS42", -2900, 1400);
        assert_eq!(ppd.detect().map(|v| v.evid), Some("BUS42".into()));
        put(&a, "BUS42", -2900, 1600);
        assert!(ppd.detect().is_none(), "the next lane");
    }

    #[test]
    fn the_basic_device_only_says_that_the_marker_is_there_and_the_optical_one_measures() {
        let rfid = acdp(Kind::Rfid, "rfid");
        put(&rfid, "BUS42", -1000, 80);
        let seen = Ppd::new(&rfid).unwrap().positioning("BUS42");
        assert!(seen.association && !seen.positioning_support && seen.in_charge_position && seen.x_deviation_mm == 0);

        let optical = acdp(Kind::Optical, "optical");
        let ppd = Ppd::new(&optical).unwrap();
        put(&optical, "BUS42", -1000, 80);
        let seen = ppd.positioning("BUS42");
        assert!(seen.positioning_support && !seen.in_charge_position, "1 m short of the window");
        assert_eq!((seen.x_deviation_mm, seen.y_deviation_mm, seen.window_x_mm, seen.window_y_mm), (-1000, 80, 150, 100));
        put(&optical, "BUS42", 40, -30);
        assert!(ppd.positioning("BUS42").in_charge_position);
        put(&optical, "BUS42", 150, 0);
        assert!(!ppd.positioning("BUS42").in_charge_position, "the window is open, the edge is out (Annex D: less than)");
    }

    #[test]
    fn the_association_is_lost_when_the_marker_leaves_or_another_one_is_read() {
        let a = acdp(Kind::Optical, "association");
        let ppd = Ppd::new(&a).unwrap();
        put(&a, "BUS42", -500, 0);
        assert!(ppd.positioning("BUS42").association);
        put(&a, "BUS43", -500, 0);
        assert!(!ppd.positioning("BUS42").association, "another vehicle took its place");
        put(&a, "BUS42", -9000, 0);
        assert!(!ppd.positioning("BUS42").association, "it left");
    }
}
