//! Choosing among the charger's schedule tuples (ISO 15118-20 8.3.4.3.6, V2G20-297/298: the first tuple
//! is the default; a vehicle that can compare tuples may pick another).
//!
//! The comparison is an estimate of what the energy the vehicle needs would cost if it were charged
//! with each tuple's power schedule (at most what the vehicle takes), under the tuple's price schedule.
//! A tuple that cannot deliver the energy before departure loses against one that can. Price schedules
//! are comparable when they are of the same kind (absolute prices in the same currency, or price
//! levels); tuples that are not comparable leave the default.

use iso15118_20::common::{ChargingSchedule, ScheduleChoice, ScheduleTuple};

use super::config::rational_to_volts as value;

/// What a tuple would do for the vehicle's needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Estimate {
    /// Energy delivered by the departure, in Wh (at most what is needed).
    pub delivered_wh: f32,
    /// What it costs: in the currency of the price schedule, or in price levels times kWh.
    pub cost: f32,
    kind: Kind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Absolute([u8; 3], usize),
    Level,
    Unpriced,
}

/// The price in force `at` seconds into the schedule, for energy drawn at `watts`: absolute fee per kWh, or
/// the price level plus one (so level 0 still counts).
fn price(schedule: &ScheduleChoice<'_>, at: f32, watts: f32) -> f32 {
    match schedule {
        ScheduleChoice::AbsolutePrice(a) => {
            let mut left = at;
            let stacks = a.price_rule_stacks.price_rule_stack;
            let Some(mut stack) = stacks.first() else { return 0.0 };
            for s in stacks {
                stack = s;
                if left < s.duration as f32 {
                    break;
                }
                left -= s.duration as f32;
            }
            // The rules are ordered by the start of their power range: the last one that starts below the power.
            let mut fee = 0.0;
            for rule in stack.price_rule {
                if value(rule.power_range_start) <= watts {
                    fee = value(rule.energy_fee);
                }
            }
            fee
        }
        ScheduleChoice::PriceLevel(l) => {
            let mut left = at;
            let entries = l.price_level_schedule_entries.price_level_schedule_entry;
            let mut level = entries.first().map_or(0, |e| e.price_level);
            for e in entries {
                level = e.price_level;
                if left < e.duration as f32 {
                    break;
                }
                left -= e.duration as f32;
            }
            f32::from(level) + 1.0
        }
    }
}

fn kind(schedule: &ChargingSchedule<'_>) -> Kind {
    match &schedule.schedule {
        Some(ScheduleChoice::AbsolutePrice(a)) => {
            let mut c = [0u8; 3];
            let bytes = a.currency.as_bytes();
            let n = bytes.len().min(3);
            c[..n].copy_from_slice(&bytes[..n]);
            Kind::Absolute(c, n)
        }
        Some(ScheduleChoice::PriceLevel(_)) => Kind::Level,
        None => Kind::Unpriced,
    }
}

/// Estimates `tuple` for a vehicle that needs `need_wh`, takes at most `max_watts` (all lines together) and leaves
/// after `departure_s` seconds from now.
pub(super) fn estimate(tuple: &ScheduleTuple<'_>, need_wh: f32, max_watts: f32, departure_s: f32) -> Estimate {
    let schedule = &tuple.charging_schedule;
    let (mut delivered, mut cost, mut t) = (0.0f32, 0.0f32, 0.0f32);
    for entry in schedule.power_schedule.power_schedule_entries.power_schedule_entry {
        let watts = (value(entry.power) + entry.power_l2.map_or(0.0, value) + entry.power_l3.map_or(0.0, value)).clamp(0.0, max_watts);
        let seconds = (entry.duration as f32).min((departure_s - t).max(0.0));
        let wh = (watts * seconds / 3600.0).min(need_wh - delivered).max(0.0);
        let fee = schedule.schedule.as_ref().map_or(0.0, |p| price(p, t, watts));
        delivered += wh;
        cost += wh / 1000.0 * fee;
        t += entry.duration as f32;
        if delivered >= need_wh || t >= departure_s {
            break;
        }
    }
    Estimate { delivered_wh: delivered, cost, kind: kind(schedule) }
}

/// The tuple to select: the cheapest of those that deliver the need by departure; when none does, the one that
/// delivers most; the first where the price schedules cannot be compared. Returns its index.
pub(super) fn choose(tuples: &[ScheduleTuple<'_>], need_wh: f32, max_watts: f32, departure_s: f32) -> usize {
    let Some(first) = tuples.first() else { return 0 };
    let base = estimate(first, need_wh, max_watts, departure_s);
    if base.kind == Kind::Unpriced {
        return 0;
    }
    let mut best = (0usize, base);
    for (i, tuple) in tuples.iter().enumerate().skip(1) {
        let e = estimate(tuple, need_wh, max_watts, departure_s);
        // Only prices of one kind (and currency) are compared.
        if e.kind != base.kind {
            continue;
        }
        let enough = |x: &Estimate| x.delivered_wh >= need_wh * 0.99;
        let better = match (enough(&e), enough(&best.1)) {
            (true, false) => true,
            (false, true) => false,
            (true, true) => e.cost < best.1.cost,
            (false, false) => e.delivered_wh > best.1.delivered_wh,
        };
        if better {
            best = (i, e);
        }
    }
    best.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use iso15118_20::common::{AbsolutePriceSchedule, PowerSchedule, PowerScheduleEntry, PowerScheduleEntryList, PriceRule, PriceRuleStack, PriceRuleStackList};
    use iso15118_20::common_types::RationalNumber;

    fn rn(exponent: i8, value: i16) -> RationalNumber {
        RationalNumber { exponent, value }
    }

    /// A tuple of the given power entries under the given prices (EUR).
    fn tuple(id: u32, entries: &'static [PowerScheduleEntry], stacks: &'static [PriceRuleStack<'static>]) -> ScheduleTuple<'static> {
        ScheduleTuple {
            schedule_tuple_id: id,
            charging_schedule: ChargingSchedule {
                power_schedule: PowerSchedule { time_anchor: 0, available_energy: None, power_tolerance: None, power_schedule_entries: PowerScheduleEntryList { power_schedule_entry: entries } },
                schedule: Some(ScheduleChoice::AbsolutePrice(AbsolutePriceSchedule {
                    id: None,
                    time_anchor: 0,
                    price_schedule_id: id,
                    price_schedule_description: None,
                    currency: "EUR",
                    language: "eng",
                    price_algorithm: "",
                    minimum_cost: None,
                    maximum_cost: None,
                    tax_rules: None,
                    price_rule_stacks: PriceRuleStackList { price_rule_stack: stacks },
                    overstay_rules: None,
                    additional_selected_services: None,
                })),
            },
            discharging_schedule: None,
        }
    }

    const fn entry(seconds: u32, kw: i16) -> PowerScheduleEntry {
        PowerScheduleEntry { duration: seconds, power: RationalNumber { exponent: 3, value: kw }, power_l2: None, power_l3: None }
    }
    const fn rule(cents: i16) -> PriceRule {
        PriceRule { energy_fee: RationalNumber { exponent: -2, value: cents }, parking_fee: None, parking_fee_period: None, carbon_dioxide_emission: None, renewable_generation_percentage: None, power_range_start: RationalNumber { exponent: 0, value: 0 } }
    }

    static FAST: [PowerScheduleEntry; 1] = [entry(3600, 11)];
    static SLOW: [PowerScheduleEntry; 1] = [entry(36_000, 3)];
    static DEAR: [PriceRule; 1] = [rule(40)];
    static CHEAP: [PriceRule; 1] = [rule(20)];
    static DEAR_STACK: [PriceRuleStack<'static>; 1] = [PriceRuleStack { duration: 36_000, price_rule: &DEAR }];
    static CHEAP_STACK: [PriceRuleStack<'static>; 1] = [PriceRuleStack { duration: 36_000, price_rule: &CHEAP }];

    #[test]
    fn the_cheapest_tuple_that_delivers_in_time_wins() {
        let tuples = [tuple(1, &FAST, &DEAR_STACK), tuple(2, &SLOW, &CHEAP_STACK)];
        // 10 kWh by tomorrow: the slow cheap tuple delivers 30 kWh in ten hours.
        assert_eq!(choose(&tuples, 10_000.0, 11_000.0, 36_000.0), 1);
        // In an hour only the fast one gets there (3 kW would deliver 3 kWh).
        assert_eq!(choose(&tuples, 10_000.0, 11_000.0, 3600.0), 0);
        // And when neither does, the one that delivers more.
        assert_eq!(choose(&tuples, 50_000.0, 11_000.0, 3600.0), 0);
    }

    #[test]
    fn tuples_without_prices_leave_the_default() {
        let mut tuples = [tuple(1, &FAST, &DEAR_STACK), tuple(2, &SLOW, &CHEAP_STACK)];
        tuples[0].charging_schedule.schedule = None;
        assert_eq!(choose(&tuples, 10_000.0, 11_000.0, 36_000.0), 0);
        assert_eq!(choose(&[], 1.0, 1.0, 1.0), 0);
    }
}
