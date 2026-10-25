//! The pantograph of the simulated ACDP charger: it takes `pantograph_move_ms` to go down and as long to go up, and the state is
//! written into the world the vehicle simulator shares ([`evcc_linux::acdp::World`]), where the vehicle reads whether its contacts
//! have a connection (the control pilot state `ACDP_Connect` reports is B only while the pantograph is down).

use std::io;
use std::time::{Duration, Instant};

use iso15118_20::acdp::MechanicalChargingDeviceStatus as Mech;

use crate::config::{Acdp, PantographFault};

pub(crate) struct Pantograph {
    state: Mech,
    /// Moving to this position, since this moment.
    moving: Option<(Mech, Instant)>,
    move_for: Duration,
    fault: PantographFault,
    world: evcc_linux::acdp::World,
}

impl Pantograph {
    pub(crate) fn new(acdp: &Acdp) -> io::Result<Self> {
        let dir = acdp.world.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "an ACDP charger needs the world directory (acdp.world)"))?;
        let p = Self { state: Mech::Home, moving: None, move_for: Duration::from_millis(acdp.pantograph_move_ms.into()), fault: acdp.pantograph_fault, world: evcc_linux::acdp::World::new(dir)? };
        p.publish()?;
        Ok(p)
    }

    /// Asked to be at `to` (down: `EndPosition`, up: `Home`): it starts to move unless it is there or on its way.
    pub(crate) fn go(&mut self, to: Mech, now: Instant) -> Mech {
        self.update(now);
        if self.state != to && self.moving.is_none_or(|(target, _)| target != to) {
            self.state = Mech::Moving;
            self.moving = Some((to, now));
            let _ = self.publish();
        }
        self.state
    }

    /// Where it is now: the movement ends after `move_for`, unless the pantograph is stuck.
    pub(crate) fn update(&mut self, now: Instant) -> Mech {
        if let Some((to, since)) = self.moving {
            let stuck = match to {
                Mech::EndPosition => self.fault == PantographFault::StuckLowering,
                _ => self.fault == PantographFault::StuckRaising,
            };
            if !stuck && now.duration_since(since) >= self.move_for {
                self.state = to;
                self.moving = None;
                let _ = self.publish();
            }
        }
        self.state
    }

    fn publish(&self) -> io::Result<()> {
        self.world.set_pantograph(match self.state {
            Mech::Home => "Home",
            Mech::Moving => "Moving",
            Mech::EndPosition => "EndPosition",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pantograph(fault: PantographFault, name: &str) -> Pantograph {
        let dir = std::env::temp_dir().join(format!("pantograph-{}-{name}", std::process::id()));
        Pantograph::new(&Acdp { pantograph_move_ms: 100, pantograph_fault: fault, world: Some(dir.display().to_string()), ..Acdp::default() }).unwrap()
    }

    #[test]
    fn it_moves_for_the_set_time_and_stops_at_the_end_position() {
        let mut p = pantograph(PantographFault::None, "moves");
        let t0 = Instant::now();
        assert_eq!(p.go(Mech::EndPosition, t0), Mech::Moving);
        assert_eq!(p.update(t0 + Duration::from_millis(60)), Mech::Moving);
        assert_eq!(p.go(Mech::EndPosition, t0 + Duration::from_millis(80)), Mech::Moving, "asking again does not restart the movement");
        assert_eq!(p.update(t0 + Duration::from_millis(100)), Mech::EndPosition);
        assert_eq!(p.go(Mech::Home, t0 + Duration::from_millis(120)), Mech::Moving);
        assert_eq!(p.update(t0 + Duration::from_millis(230)), Mech::Home);
        assert_eq!(p.world.pantograph().as_deref(), Some("Home"));
    }

    #[test]
    fn a_stuck_pantograph_never_arrives() {
        let mut down = pantograph(PantographFault::StuckLowering, "stuck-down");
        let t0 = Instant::now();
        down.go(Mech::EndPosition, t0);
        assert_eq!(down.update(t0 + Duration::from_secs(60)), Mech::Moving);
        let mut up = pantograph(PantographFault::StuckRaising, "stuck-up");
        up.go(Mech::EndPosition, t0);
        assert_eq!(up.update(t0 + Duration::from_millis(100)), Mech::EndPosition, "lowering works");
        up.go(Mech::Home, t0 + Duration::from_millis(120));
        assert_eq!(up.update(t0 + Duration::from_secs(60)), Mech::Moving);
    }
}
