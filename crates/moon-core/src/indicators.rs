//! Technical indicators over `f64` series.

use chrono::NaiveDate;

use crate::bars::Bar;

/// Mean of the last `n` values; `None` if `n == 0` or there are fewer than `n` values.
pub fn sma(values: &[f64], n: usize) -> Option<f64> {
    if n == 0 || values.len() < n {
        return None;
    }
    let tail = &values[values.len() - n..];
    Some(tail.iter().sum::<f64>() / n as f64)
}

/// `last / value `months` earlier - 1`; needs more than `months` points.
pub fn trailing_return(monthly: &[(NaiveDate, f64)], months: usize) -> Option<f64> {
    if monthly.len() <= months {
        return None;
    }
    let last = monthly[monthly.len() - 1].1;
    let base = monthly[monthly.len() - 1 - months].1;
    Some(last / base - 1.0)
}

/// Keller's 13612W momentum: `(12*r1 + 4*r3 + 2*r6 + r12) / 4`; needs 13 points.
pub fn momentum_13612w(monthly: &[(NaiveDate, f64)]) -> Option<f64> {
    let r1 = trailing_return(monthly, 1)?;
    let r3 = trailing_return(monthly, 3)?;
    let r6 = trailing_return(monthly, 6)?;
    let r12 = trailing_return(monthly, 12)?;
    Some((12.0 * r1 + 4.0 * r3 + 2.0 * r6 + r12) / 4.0)
}

/// Sample standard deviation of the last `lookback` simple `adj_close` returns, annualized
/// with `sqrt(252)`.
pub fn annualized_volatility(bars: &[Bar], lookback: usize) -> Option<f64> {
    if lookback < 2 || bars.len() < lookback + 1 {
        return None;
    }
    let window = &bars[bars.len() - lookback - 1..];
    let returns: Vec<f64> = window
        .windows(2)
        .map(|w| w[1].adj_close / w[0].adj_close - 1.0)
        .collect();
    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;
    let var = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    Some(var.sqrt() * 252.0_f64.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{d, daily_series, monthly_series};

    fn monthly(closes: &[f64]) -> Vec<(NaiveDate, f64)> {
        crate::bars::monthly_closes(monthly_series(2023, 1, closes).bars())
    }

    #[test]
    fn sma_and_returns() {
        assert_eq!(sma(&[1., 2., 3., 4.], 2), Some(3.5));
        assert_eq!(sma(&[1.], 2), None);
        assert_eq!(sma(&[1.], 0), None);
        let m = monthly(&[100., 110., 121.]);
        assert!((trailing_return(&m, 2).unwrap() - 0.21).abs() < 1e-12);
        assert_eq!(trailing_return(&m, 3), None);
    }

    #[test]
    fn momentum_13612w_known_value() {
        let closes: Vec<f64> = (0..13).map(|i| 100.0 * 1.01_f64.powi(i)).collect();
        let m = monthly(&closes);
        let r = |k: i32| 1.01_f64.powi(k) - 1.0;
        let expected = (12.0 * r(1) + 4.0 * r(3) + 2.0 * r(6) + r(12)) / 4.0;
        assert!((momentum_13612w(&m).unwrap() - expected).abs() < 1e-12);
        assert_eq!(momentum_13612w(&m[1..]), None);
    }

    #[test]
    fn volatility_of_constant_series_is_zero() {
        let s = daily_series(d(2024, 1, 1), &[100.0; 30]);
        assert!(annualized_volatility(s.bars(), 20).unwrap().abs() < 1e-12);
    }

    #[test]
    fn volatility_none_without_history() {
        let s = daily_series(d(2024, 1, 1), &[100.0; 10]);
        assert_eq!(annualized_volatility(s.bars(), 20), None);
        assert_eq!(annualized_volatility(s.bars(), 1), None);
        assert_eq!(annualized_volatility(s.bars(), 10), None);
        assert!(annualized_volatility(s.bars(), 9).is_some());
    }
}
