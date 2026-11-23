//! Performance metrics computed on a unitized NAV (time-weighted returns).

use chrono::{Datelike, NaiveDate};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::backtest::{EquityPoint, Trade};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    pub start: NaiveDate,
    pub end: NaiveDate,
    pub years: f64,
    pub total_return: f64,
    pub cagr: f64,
    pub volatility: f64,
    pub sharpe: f64,
    pub max_drawdown: f64,
    pub calmar: f64,
    pub best_month: f64,
    pub worst_month: f64,
    pub positive_months: f64,
    pub turnover: f64,
    pub total_fees: Decimal,
    pub trades: usize,
    pub time_in_refuge: f64,
    pub final_equity: Decimal,
    pub net_contributions: Decimal,
}

const TRADING_DAYS: f64 = 252.0;

fn zeros(start: NaiveDate, end: NaiveDate) -> Metrics {
    Metrics {
        start,
        end,
        years: 0.0,
        total_return: 0.0,
        cagr: 0.0,
        volatility: 0.0,
        sharpe: 0.0,
        max_drawdown: 0.0,
        calmar: 0.0,
        best_month: 0.0,
        worst_month: 0.0,
        positive_months: 0.0,
        turnover: 0.0,
        total_fees: Decimal::ZERO,
        trades: 0,
        time_in_refuge: 0.0,
        final_equity: Decimal::ZERO,
        net_contributions: Decimal::ZERO,
    }
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        0.0
    } else {
        xs.iter().sum::<f64>() / xs.len() as f64
    }
}

fn stdev_sample(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    let var = xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (xs.len() - 1) as f64;
    var.sqrt()
}

/// Metrics from a NAV series alone; trading fields are zero.
pub fn nav_metrics(navs: &[(NaiveDate, f64)], risk_free_rate: f64) -> Metrics {
    let (Some(first), Some(last)) = (navs.first(), navs.last()) else {
        return zeros(NaiveDate::default(), NaiveDate::default());
    };
    let mut m = zeros(first.0, last.0);
    if navs.len() < 2 || (first.1 <= 0.0 || first.1.is_nan()) {
        return m;
    }
    let returns: Vec<f64> = navs.windows(2).map(|w| w[1].1 / w[0].1 - 1.0).collect();
    let years = (last.0 - first.0).num_days() as f64 / 365.25;
    m.years = years;
    m.total_return = last.1 / first.1 - 1.0;
    m.cagr = if years > 0.0 {
        (last.1 / first.1).powf(1.0 / years) - 1.0
    } else {
        0.0
    };
    let sd = stdev_sample(&returns);
    m.volatility = sd * TRADING_DAYS.sqrt();
    let rf_d = (1.0 + risk_free_rate).powf(1.0 / TRADING_DAYS) - 1.0;
    m.sharpe = if sd > 0.0 {
        (mean(&returns) - rf_d) / sd * TRADING_DAYS.sqrt()
    } else {
        0.0
    };
    let mut peak = f64::MIN;
    let mut mdd = 0.0_f64;
    for &(_, nav) in navs {
        peak = peak.max(nav);
        if peak > 0.0 {
            mdd = mdd.max(1.0 - nav / peak);
        }
    }
    m.max_drawdown = mdd;
    m.calmar = if mdd > 0.0 { m.cagr / mdd } else { 0.0 };

    // Month-end navs, each relative to the previous month-end (the first to the first point).
    let mut month_ends: Vec<((i32, u32), f64)> = Vec::new();
    for &(date, nav) in navs {
        let key = (date.year(), date.month());
        match month_ends.last_mut() {
            Some(last) if last.0 == key => last.1 = nav,
            _ => month_ends.push((key, nav)),
        }
    }
    let mut prev = first.1;
    let mut monthly: Vec<f64> = Vec::with_capacity(month_ends.len());
    for (_, nav) in &month_ends {
        monthly.push(nav / prev - 1.0);
        prev = *nav;
    }
    if !monthly.is_empty() {
        m.best_month = monthly.iter().copied().fold(f64::MIN, f64::max);
        m.worst_month = monthly.iter().copied().fold(f64::MAX, f64::min);
        m.positive_months =
            monthly.iter().filter(|r| **r > 0.0).count() as f64 / monthly.len() as f64;
    }
    m
}

pub fn compute_metrics(points: &[EquityPoint], trades: &[Trade], risk_free_rate: f64) -> Metrics {
    let navs: Vec<(NaiveDate, f64)> = points.iter().map(|p| (p.date, p.nav)).collect();
    let mut m = nav_metrics(&navs, risk_free_rate);
    if points.len() < 2 {
        return m;
    }
    m.total_fees = trades.iter().map(|t| t.fees).sum();
    m.trades = trades.len();
    m.time_in_refuge = mean(&points.iter().map(|p| p.refuge_weight).collect::<Vec<_>>());
    let mean_equity = mean(
        &points
            .iter()
            .map(|p| crate::money::dec_to_f64(p.equity))
            .collect::<Vec<_>>(),
    );
    let traded: f64 = trades
        .iter()
        .map(|t| crate::money::dec_to_f64(t.value.abs()))
        .sum();
    m.turnover = if mean_equity > 0.0 && m.years > 0.0 {
        traded / 2.0 / mean_equity / m.years
    } else {
        0.0
    };
    m.final_equity = points[points.len() - 1].equity;
    m.net_contributions = points.iter().map(|p| p.flow).sum::<Decimal>();
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::d;

    #[test]
    fn metrics_known_drawdown() {
        let navs = [
            (d(2024, 1, 1), 1.0),
            (d(2024, 1, 2), 1.2),
            (d(2024, 1, 3), 0.9),
            (d(2024, 1, 4), 1.1),
        ];
        let m = nav_metrics(&navs, 0.0);
        assert!((m.max_drawdown - 0.25).abs() < 1e-12);
        let flat: Vec<_> = (1..=5).map(|i| (d(2024, 1, i), 1.0)).collect();
        let m = nav_metrics(&flat, 0.0);
        assert_eq!(m.sharpe, 0.0);
        assert_eq!(m.volatility, 0.0);
        assert_eq!(m.calmar, 0.0);
    }

    #[test]
    fn metrics_cagr_one_year() {
        let navs = [(d(2023, 1, 1), 1.0), (d(2024, 1, 1), 1.1)];
        let m = nav_metrics(&navs, 0.0);
        assert!((m.cagr - 0.10).abs() < 0.001, "cagr {}", m.cagr);
        assert!((m.total_return - 0.1).abs() < 1e-12);
    }

    #[test]
    fn metrics_degenerate_inputs_do_not_panic() {
        assert_eq!(nav_metrics(&[], 0.0).trades, 0);
        let m = nav_metrics(&[(d(2024, 1, 1), 1.0)], 0.0);
        assert_eq!(m.total_return, 0.0);
        let m = compute_metrics(&[], &[], 0.0);
        assert_eq!(m.final_equity, Decimal::ZERO);
    }

    #[test]
    fn monthly_stats() {
        let navs = [
            (d(2024, 1, 31), 1.0),
            (d(2024, 2, 29), 1.1),
            (d(2024, 3, 29), 0.99),
        ];
        let m = nav_metrics(&navs, 0.0);
        assert!((m.best_month - 0.1).abs() < 1e-12);
        assert!((m.worst_month + 0.1).abs() < 1e-12);
        assert!((m.positive_months - 1.0 / 3.0).abs() < 1e-12);
    }
}
