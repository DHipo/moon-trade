//! Pure scheduling logic: which jobs are due at a given instant. All times are ART.

use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};
use moon_core::time::art_datetime;
use serde::{Deserialize, Serialize};

use crate::config::ScheduleConfig;

/// Minimum wait between two attempts of the monthly rebalance.
const MONTHLY_RETRY_MINUTES: i64 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Job {
    MonthlyRebalance,
    RiskCheck,
    DailyClose,
    WeeklyReport,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScheduleState {
    pub last_risk_check: Option<DateTime<Utc>>,
    /// Month of the last completed monthly rebalance, as `YYYY-MM`.
    pub last_monthly: Option<String>,
    pub last_monthly_attempt: Option<DateTime<Utc>>,
    pub last_daily_close: Option<NaiveDate>,
    pub last_weekly: Option<NaiveDate>,
}

/// Monday to Friday and not a configured holiday.
pub fn is_trading_day(d: NaiveDate, holidays: &[NaiveDate]) -> bool {
    !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) && !holidays.contains(&d)
}

/// The last trading day strictly before `d`.
pub fn previous_trading_day(d: NaiveDate, holidays: &[NaiveDate]) -> NaiveDate {
    let mut day = d;
    loop {
        day -= Duration::days(1);
        if is_trading_day(day, holidays) {
            return day;
        }
    }
}

fn month_key(d: NaiveDate) -> String {
    d.format("%Y-%m").to_string()
}

/// Jobs due at `now`, in the order Monthly, RiskCheck, DailyClose, WeeklyReport.
/// Risk checks run even when paused (emergency exits stay active).
pub fn due_jobs(
    now: DateTime<Utc>,
    cfg: &ScheduleConfig,
    state: &ScheduleState,
    paused: bool,
) -> Vec<Job> {
    let local = art_datetime(now);
    let today = local.date_naive();
    let t = local.time();
    let trading = is_trading_day(today, &cfg.holidays);
    let mut jobs = Vec::new();

    if !paused
        && trading
        && t >= cfg.monthly_time
        && t < cfg.market_close
        && state.last_monthly.as_deref() != Some(month_key(today).as_str())
        && state
            .last_monthly_attempt
            .is_none_or(|a| now - a >= Duration::minutes(MONTHLY_RETRY_MINUTES))
    {
        jobs.push(Job::MonthlyRebalance);
    }

    if trading
        && t >= cfg.market_open
        && t < cfg.market_close
        && state
            .last_risk_check
            .is_none_or(|a| now - a >= Duration::minutes(i64::from(cfg.risk_check_every_minutes)))
    {
        jobs.push(Job::RiskCheck);
    }

    if trading && t >= cfg.daily_close_time && state.last_daily_close != Some(today) {
        jobs.push(Job::DailyClose);
    }

    if today.weekday() == cfg.weekly_report_weekday
        && t >= cfg.weekly_report_time
        && state.last_weekly != Some(today)
    {
        jobs.push(Job::WeeklyReport);
    }
    jobs
}

/// Records that `job` was just attempted (risk check and monthly only).
pub fn mark_attempt(state: &mut ScheduleState, job: Job, now: DateTime<Utc>) {
    match job {
        Job::RiskCheck => state.last_risk_check = Some(now),
        Job::MonthlyRebalance => state.last_monthly_attempt = Some(now),
        Job::DailyClose | Job::WeeklyReport => {}
    }
}

/// Records that `job` is finished for its period (monthly, daily close, weekly report).
pub fn mark_done(state: &mut ScheduleState, job: Job, now: DateTime<Utc>) {
    let today = art_datetime(now).date_naive();
    match job {
        Job::MonthlyRebalance => state.last_monthly = Some(month_key(today)),
        Job::DailyClose => state.last_daily_close = Some(today),
        Job::WeeklyReport => state.last_weekly = Some(today),
        Job::RiskCheck => {}
    }
}
