//! Choosing among the charger's `SAScheduleTuple`s (ISO 15118-2): the first is the default; a vehicle that can compare
//! them may pick another. A tuple has a maximum power over time (`PMaxSchedule`) and, optionally, a `SalesTariff`: price
//! levels over time (`EPriceLevel`, 0 the cheapest).
//!
//! The comparison estimates what the energy the vehicle needs would cost if it were charged with the tuple's power
//! (at most what the vehicle takes) under its price levels. A tuple that cannot deliver the energy before departure loses
//! against one that can. Without price levels on every tuple the cost is not compared and the default stays.

use iso15118_2::types::{SaScheduleTuple, TimeIntervalChoice};

use super::convert::number;

/// What a tuple would do for the vehicle's needs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Estimate {
    /// Energy delivered by the departure, in Wh (at most what is needed).
    pub delivered_wh: f64,
    /// Price levels (plus one, so level 0 still counts) times kWh; `None` without a sales tariff.
    pub cost: Option<f64>,
}

fn start(interval: &TimeIntervalChoice) -> f64 {
    let TimeIntervalChoice::Relative(r) = interval;
    f64::from(r.start)
}

/// The price level in force `at` seconds in; `None` where the tuple has no price levels.
fn level(tuple: &SaScheduleTuple<'_>, at: f64) -> Option<f64> {
    let tariff = tuple.sales_tariff.as_ref()?;
    let mut level = None;
    for entry in tariff.sales_tariff_entry {
        if start(&entry.time_interval) <= at {
            level = entry.e_price_level.map(f64::from).or(level);
        }
    }
    level.or_else(|| tariff.sales_tariff_entry.iter().find_map(|e| e.e_price_level.map(f64::from)))
}

pub(super) fn estimate(tuple: &SaScheduleTuple<'_>, need_wh: f64, max_watts: f64, departure_s: f64) -> Estimate {
    let entries = tuple.p_max_schedule.p_max_schedule_entry;
    let mut delivered = 0.0;
    let mut cost = 0.0;
    let priced = tuple.sales_tariff.is_some();
    for (i, e) in entries.iter().enumerate() {
        let from = start(&e.time_interval);
        // An entry lasts until the next starts; the last one until the departure.
        let until = entries.get(i + 1).map_or(departure_s.max(from), |n| start(&n.time_interval)).min(departure_s);
        if until <= from || delivered >= need_wh {
            continue;
        }
        let watts = number(e.p_max).min(max_watts).max(0.0);
        // Within the entry the price level changes where the sales tariff has a new entry: energy is delivered, and priced,
        // in the pieces between.
        let mut t = from;
        while t < until && delivered < need_wh {
            let next = tuple.sales_tariff.as_ref().and_then(|s| s.sales_tariff_entry.iter().map(|e| start(&e.time_interval)).filter(|&b| b > t).reduce(f64::min)).map_or(until, |b| b.min(until));
            let wh = (watts * (next - t) / 3600.0).min(need_wh - delivered);
            delivered += wh;
            cost += wh / 1000.0 * (level(tuple, t).unwrap_or(0.0) + 1.0);
            t = next;
        }
    }
    Estimate { delivered_wh: delivered, cost: priced.then_some(cost) }
}

/// At most this many tuples are looked at (the vehicle asks for few, `max_schedule_tuples`).
const MAX_TUPLES: usize = 8;

/// Index of the tuple to use: the first that delivers the need before departure at the least cost; if none does, the one that
/// delivers most. The default (the first) stays where costs cannot be compared.
pub(super) fn choose(tuples: &[SaScheduleTuple<'_>], need_wh: f64, max_watts: f64, departure_s: f64) -> usize {
    let tuples = &tuples[..tuples.len().min(MAX_TUPLES)];
    let mut estimates = [Estimate { delivered_wh: 0.0, cost: None }; MAX_TUPLES];
    for (e, t) in estimates.iter_mut().zip(tuples) {
        *e = estimate(t, need_wh, max_watts, departure_s);
    }
    let estimates = &estimates[..tuples.len()];
    let comparable = estimates.iter().all(|e| e.cost.is_some());
    let enough = |e: &Estimate| e.delivered_wh >= need_wh * 0.999;
    let mut best = 0;
    for (i, e) in estimates.iter().enumerate().skip(1) {
        let b = &estimates[best];
        let better = match (enough(e), enough(b)) {
            (true, false) => true,
            (false, true) => false,
            (true, true) => comparable && e.cost < b.cost,
            (false, false) => e.delivered_wh > b.delivered_wh + 1e-6 || (comparable && (e.delivered_wh - b.delivered_wh).abs() <= 1e-6 && e.cost < b.cost),
        };
        if better {
            best = i;
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::boxed::Box;

    use iso15118_2::types::{PMaxSchedule, PMaxScheduleEntry, PhysicalValue, RelativeTimeInterval, SalesTariff, SalesTariffEntry, UnitSymbol};

    fn w(x: f64) -> PhysicalValue {
        PhysicalValue::from_f64(x, UnitSymbol::W)
    }

    fn entry(start: u32, watts: f64) -> PMaxScheduleEntry {
        PMaxScheduleEntry { time_interval: TimeIntervalChoice::Relative(RelativeTimeInterval { start, duration: None }), p_max: w(watts) }
    }

    fn priced(start: u32, level: u8) -> SalesTariffEntry<'static> {
        SalesTariffEntry { time_interval: TimeIntervalChoice::Relative(RelativeTimeInterval { start, duration: None }), e_price_level: Some(level), consumption_cost: &[] }
    }

    #[test]
    fn the_cheaper_tuple_wins_when_both_deliver_and_the_default_stays_without_prices() {
        // 10 kWh needed, 11 kW, 8 hours: the first tuple is expensive for the first four hours, the second only in the last four.
        let full = [entry(0, 11_000.0)];
        let dear_first = [priced(0, 3), priced(14_400, 0)];
        let dear_last = [priced(0, 0), priced(14_400, 3)];
        let tuple = |id: u8, entries: &'static [PMaxScheduleEntry], tariff: Option<&'static [SalesTariffEntry<'static>]>| SaScheduleTuple {
            sa_schedule_tuple_id: id,
            p_max_schedule: PMaxSchedule { p_max_schedule_entry: entries },
            sales_tariff: tariff.map(|e| SalesTariff { id: None, sales_tariff_id: id, sales_tariff_description: None, num_e_price_levels: Some(4), sales_tariff_entry: e }),
        };
        let full: &'static [PMaxScheduleEntry] = Box::leak(Box::new(full));
        let a: &'static [SalesTariffEntry<'static>] = Box::leak(Box::new(dear_first));
        let b: &'static [SalesTariffEntry<'static>] = Box::leak(Box::new(dear_last));
        let tuples = [tuple(1, full, Some(a)), tuple(2, full, Some(b))];
        assert_eq!(choose(&tuples, 10_000.0, 11_000.0, 28_800.0), 1);
        // Without sales tariffs nothing is compared.
        let bare = [tuple(1, full, None), tuple(2, full, None)];
        assert_eq!(choose(&bare, 10_000.0, 11_000.0, 28_800.0), 0);
    }

    #[test]
    fn a_tuple_that_delivers_the_energy_beats_a_cheaper_one_that_does_not() {
        let slow: &'static [PMaxScheduleEntry] = Box::leak(Box::new([entry(0, 1_000.0)]));
        let fast: &'static [PMaxScheduleEntry] = Box::leak(Box::new([entry(0, 11_000.0)]));
        let cheap: &'static [SalesTariffEntry<'static>] = Box::leak(Box::new([priced(0, 0)]));
        let dear: &'static [SalesTariffEntry<'static>] = Box::leak(Box::new([priced(0, 3)]));
        let tuple = |id: u8, entries: &'static [PMaxScheduleEntry], tariff: &'static [SalesTariffEntry<'static>]| SaScheduleTuple {
            sa_schedule_tuple_id: id,
            p_max_schedule: PMaxSchedule { p_max_schedule_entry: entries },
            sales_tariff: Some(SalesTariff { id: None, sales_tariff_id: id, sales_tariff_description: None, num_e_price_levels: Some(4), sales_tariff_entry: tariff }),
        };
        // 10 kWh in 2 hours: only 2 kWh at 1 kW, all of it at 11 kW.
        let tuples = [tuple(1, slow, cheap), tuple(2, fast, dear)];
        assert_eq!(choose(&tuples, 10_000.0, 11_000.0, 7_200.0), 1);
    }
}
