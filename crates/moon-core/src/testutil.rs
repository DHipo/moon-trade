//! Test helpers shared with other crates through the `test-util` feature.
#![allow(clippy::unwrap_used)]

use chrono::{Datelike, Duration, NaiveDate, Weekday};

use crate::asset::{AssetId, AssetSpec, Currency, ExecSpec, Market, Role};
use crate::bars::{Bar, PriceSeries};

pub fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn flat_bar(date: NaiveDate, close: f64) -> Bar {
    Bar {
        date,
        open: close,
        high: close,
        low: close,
        close,
        adj_close: close,
        volume: 1000.0,
    }
}

fn is_weekday(date: NaiveDate) -> bool {
    !matches!(date.weekday(), Weekday::Sat | Weekday::Sun)
}

/// One bar per weekday starting at `start` (or the next weekday).
pub fn daily_series(start: NaiveDate, closes: &[f64]) -> PriceSeries {
    let mut date = start;
    let mut bars = Vec::with_capacity(closes.len());
    for &c in closes {
        while !is_weekday(date) {
            date += Duration::days(1);
        }
        bars.push(flat_bar(date, c));
        date += Duration::days(1);
    }
    PriceSeries::new(bars).unwrap()
}

/// One bar on the last weekday of each month, starting at the given month.
pub fn monthly_series(start_year: i32, start_month: u32, closes: &[f64]) -> PriceSeries {
    let (mut y, mut m) = (start_year, start_month);
    let mut bars = Vec::with_capacity(closes.len());
    for &c in closes {
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        let mut date = d(ny, nm, 1) - Duration::days(1);
        while !is_weekday(date) {
            date -= Duration::days(1);
        }
        bars.push(flat_bar(date, c));
        (y, m) = (ny, nm);
    }
    PriceSeries::new(bars).unwrap()
}

/// Test asset spec. `CASH` refuge has neither signal symbol nor exec;
/// canaries and benchmarks have no exec.
pub fn spec(id: &str, role: Role) -> AssetSpec {
    let is_cash = id == AssetId::CASH;
    let exec = match role {
        Role::Risky | Role::Refuge if !is_cash => Some(ExecSpec {
            symbol: id.to_string(),
            market: Market::Bcba,
            currency: Currency::Ars,
            lot_size: 1,
        }),
        _ => None,
    };
    AssetSpec {
        id: AssetId::new(id),
        role,
        signal_symbol: if is_cash { None } else { Some(id.to_string()) },
        cedear_ratio: 1.0,
        exec,
    }
}
