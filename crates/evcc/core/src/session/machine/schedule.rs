//! The charging schedule the vehicle selected in Scheduled control mode: a copy of the first
//! entries of the chosen tuple's power schedule, and where in it the session is.

use crate::ev::Phases;
use iso15118_20::common::ScheduleTuple;
use crate::rational::Rational;

/// How many entries are kept: `MaximumSupportingPoints` of the session (12 by default) at most.
pub(super) const MAX_ENTRIES: usize = 16;

const ZERO: Rational = Rational { exponent: 0, value: 0 };

#[derive(Debug, Clone, Copy)]
pub(super) struct Schedule {
    /// SECC time, microseconds, when the first entry starts: the schedule's `TimeAnchor`, or, when the
    /// charger names none (zero), the time it was received.
    start: u64,
    len: usize,
    /// `(duration in seconds, power per line)`.
    entries: [(u32, Phases); MAX_ENTRIES],
}

impl Schedule {
    pub const NONE: Schedule = Schedule { start: 0, len: 0, entries: [(0, Phases { l1: ZERO, l2: None, l3: None }); MAX_ENTRIES] };

    /// The charging schedule of `tuple`, received at SECC time `now`.
    pub fn of(tuple: &ScheduleTuple<'_>, now: u64) -> Schedule {
        let power = &tuple.charging_schedule.power_schedule;
        let mut s = Schedule::NONE;
        s.start = if power.time_anchor == 0 { now } else { power.time_anchor };
        for e in power.power_schedule_entries.power_schedule_entry.iter().take(MAX_ENTRIES) {
            s.entries[s.len] = (e.duration, Phases { l1: e.power.into(), l2: e.power_l2.map(Into::into), l3: e.power_l3.map(Into::into) });
            s.len += 1;
        }
        s
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The entry active at SECC time `now`: its index and the seconds left of it. Before the start it
    /// is the first, after the end the last (with nothing left).
    fn position(&self, now: u64) -> Option<(usize, u32)> {
        if self.len == 0 {
            return None;
        }
        let mut left = now.saturating_sub(self.start) / 1_000_000;
        for (i, (duration, _)) in self.entries[..self.len].iter().enumerate() {
            if left < u64::from(*duration) {
                return Some((i, *duration - left as u32));
            }
            left -= u64::from(*duration);
        }
        Some((self.len - 1, 0))
    }

    /// The power the schedule allows at `now`.
    pub fn power_at(&self, now: u64) -> Option<Phases> {
        self.position(now).map(|(i, _)| self.entries[i].1)
    }

    /// What is left of the schedule at `now`: the active entry (what is left of it) and the ones
    /// after, written to `out`; the number of entries.
    pub fn remaining(&self, now: u64, out: &mut [(u32, Phases)]) -> usize {
        let Some((first, left)) = self.position(now) else { return 0 };
        let mut n = 0;
        for i in first..self.len {
            let Some(slot) = out.get_mut(n) else { break };
            *slot = (if i == first { left.max(1) } else { self.entries[i].0 }, self.entries[i].1);
            n += 1;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rn(value: i16) -> Rational {
        Rational { exponent: 3, value }
    }

    fn schedule(start: u64) -> Schedule {
        let mut s = Schedule::NONE;
        s.start = start;
        for (duration, kw) in [(100, 7), (50, 3)] {
            s.entries[s.len] = (duration, Phases::single(rn(kw)));
            s.len += 1;
        }
        s
    }

    #[test]
    fn the_entry_in_force_follows_the_time_from_the_anchor() {
        let s = schedule(10_000_000);
        let kw = |now| s.power_at(now).map(|p| p.l1.value);
        // Before the start the first entry holds, at the end the last one.
        assert_eq!((kw(0), kw(10_000_000), kw(109_999_999)), (Some(7), Some(7), Some(7)));
        assert_eq!((kw(110_000_000), kw(159_000_000), kw(900_000_000)), (Some(3), Some(3), Some(3)));
        assert_eq!(Schedule::NONE.power_at(5), None);
    }

    #[test]
    fn what_is_left_starts_with_the_rest_of_the_entry_in_force() {
        let s = schedule(0);
        let mut out = [(0, Phases::single(rn(0))); 4];
        assert_eq!(s.remaining(30_000_000, &mut out), 2);
        assert_eq!((out[0].0, out[0].1.l1.value, out[1].0, out[1].1.l1.value), (70, 7, 50, 3));
        // Past the end the last entry has nothing left, but a profile needs a second.
        assert_eq!(s.remaining(1_000_000_000, &mut out), 1);
        assert_eq!((out[0].0, out[0].1.l1.value), (1, 3));
        // A short buffer takes what fits.
        assert_eq!(s.remaining(0, &mut out[..1]), 1);
    }
}
