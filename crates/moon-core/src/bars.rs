use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::error::CoreError;

/// One daily OHLCV bar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub date: NaiveDate,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub adj_close: f64,
    pub volume: f64,
}

impl Bar {
    /// `adj_close / close`.
    pub fn adj_factor(&self) -> f64 {
        self.adj_close / self.close
    }

    /// Open scaled by the adjustment factor.
    pub fn adj_open(&self) -> f64 {
        self.open * self.adj_factor()
    }
}

/// Bars sorted ascending by date, with unique dates.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceSeries {
    bars: Vec<Bar>,
}

fn valid_price(x: f64) -> bool {
    x.is_finite() && x > 0.0
}

impl PriceSeries {
    /// Sorts by date and keeps the last bar of duplicate dates.
    pub fn new(mut bars: Vec<Bar>) -> Result<Self, CoreError> {
        for b in &bars {
            if !valid_price(b.close) || !valid_price(b.adj_close) {
                return Err(CoreError::InvalidSeries(format!(
                    "precio no positivo o no finito el {}",
                    b.date
                )));
            }
        }
        // Stable sort keeps input order within equal dates; the last one wins.
        bars.sort_by_key(|b| b.date);
        let mut out: Vec<Bar> = Vec::with_capacity(bars.len());
        for b in bars {
            match out.last_mut() {
                Some(last) if last.date == b.date => *last = b,
                _ => out.push(b),
            }
        }
        Ok(Self { bars: out })
    }

    pub fn bars(&self) -> &[Bar] {
        &self.bars
    }

    pub fn is_empty(&self) -> bool {
        self.bars.is_empty()
    }

    pub fn first_date(&self) -> Option<NaiveDate> {
        self.bars.first().map(|b| b.date)
    }

    pub fn last_date(&self) -> Option<NaiveDate> {
        self.bars.last().map(|b| b.date)
    }

    /// Bars with `date <= date`.
    pub fn up_to(&self, date: NaiveDate) -> &[Bar] {
        let n = self.bars.partition_point(|b| b.date <= date);
        &self.bars[..n]
    }

    pub fn bar_on(&self, date: NaiveDate) -> Option<&Bar> {
        self.bars
            .binary_search_by_key(&date, |b| b.date)
            .ok()
            .map(|i| &self.bars[i])
    }

    pub fn on_or_before(&self, date: NaiveDate) -> Option<&Bar> {
        self.up_to(date).last()
    }
}

/// Last bar of each calendar month as `(date, adj_close)`.
pub fn monthly_closes(bars: &[Bar]) -> Vec<(NaiveDate, f64)> {
    let mut out: Vec<(NaiveDate, f64)> = Vec::new();
    for b in bars {
        match out.last_mut() {
            Some(last) if (last.0.year(), last.0.month()) == (b.date.year(), b.date.month()) => {
                *last = (b.date, b.adj_close);
            }
            _ => out.push((b.date, b.adj_close)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::d;

    fn bar(date: NaiveDate, close: f64) -> Bar {
        Bar {
            date,
            open: close,
            high: close,
            low: close,
            close,
            adj_close: close,
            volume: 1.0,
        }
    }

    #[test]
    fn series_sorts_and_dedups() {
        let s = PriceSeries::new(vec![
            bar(d(2024, 1, 3), 3.0),
            bar(d(2024, 1, 2), 2.0),
            bar(d(2024, 1, 3), 4.0),
        ])
        .unwrap();
        assert_eq!(s.bars().len(), 2);
        assert_eq!(s.first_date(), Some(d(2024, 1, 2)));
        assert_eq!(s.last_date(), Some(d(2024, 1, 3)));
        assert_eq!(s.bar_on(d(2024, 1, 3)).unwrap().close, 4.0);
        assert!(!s.is_empty());
    }

    #[test]
    fn series_rejects_non_positive() {
        assert!(PriceSeries::new(vec![bar(d(2024, 1, 2), 0.0)]).is_err());
        assert!(PriceSeries::new(vec![bar(d(2024, 1, 2), -1.0)]).is_err());
        assert!(PriceSeries::new(vec![bar(d(2024, 1, 2), f64::NAN)]).is_err());
        let mut b = bar(d(2024, 1, 2), 1.0);
        b.adj_close = f64::INFINITY;
        assert!(PriceSeries::new(vec![b]).is_err());
    }

    #[test]
    fn up_to_is_inclusive() {
        let s = PriceSeries::new(vec![
            bar(d(2024, 1, 2), 1.0),
            bar(d(2024, 1, 3), 2.0),
            bar(d(2024, 1, 5), 3.0),
        ])
        .unwrap();
        assert_eq!(s.up_to(d(2024, 1, 3)).len(), 2);
        assert_eq!(s.up_to(d(2024, 1, 1)).len(), 0);
        assert_eq!(s.on_or_before(d(2024, 1, 4)).unwrap().close, 2.0);
        assert!(s.bar_on(d(2024, 1, 4)).is_none());
    }

    #[test]
    fn monthly_closes_takes_last_bar_of_month() {
        let bars = vec![
            bar(d(2024, 1, 30), 1.0),
            bar(d(2024, 1, 31), 2.0),
            bar(d(2024, 2, 28), 3.0),
            bar(d(2024, 2, 29), 4.0),
        ];
        assert_eq!(
            monthly_closes(&bars),
            vec![(d(2024, 1, 31), 2.0), (d(2024, 2, 29), 4.0)]
        );
    }

    #[test]
    fn adj_open_scales_by_factor() {
        let b = Bar {
            date: d(2024, 1, 2),
            open: 90.0,
            high: 100.0,
            low: 80.0,
            close: 100.0,
            adj_close: 50.0,
            volume: 1.0,
        };
        assert_eq!(b.adj_factor(), 0.5);
        assert_eq!(b.adj_open(), 45.0);
    }
}
