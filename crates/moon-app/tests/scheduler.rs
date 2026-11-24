use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use moon_app::config::ScheduleConfig;
use moon_app::scheduler::{
    Job, ScheduleState, due_jobs, is_trading_day, mark_attempt, mark_done, previous_trading_day,
};

fn cfg(holidays: Vec<NaiveDate>) -> ScheduleConfig {
    let t = |h, m| NaiveTime::from_hms_opt(h, m, 0).expect("time");
    ScheduleConfig {
        market_open: t(11, 0),
        market_close: t(17, 0),
        monthly_time: t(11, 30),
        daily_close_time: t(17, 30),
        risk_check_every_minutes: 15,
        weekly_report_weekday: Weekday::Fri,
        weekly_report_time: t(18, 0),
        holidays,
    }
}

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).expect("date")
}

/// ART local time to UTC (ART is UTC-3 with no DST).
fn at(date: NaiveDate, h: u32, m: u32) -> DateTime<Utc> {
    let local = date.and_hms_opt(h, m, 0).expect("time");
    Utc.from_utc_datetime(&local) + Duration::hours(3)
}

#[test]
fn risk_check_every_15_minutes_in_market_hours() {
    let c = cfg(vec![]);
    let day = d(2026, 11, 24); // Tuesday
    let mut st = ScheduleState::default();
    assert!(!due_jobs(at(day, 10, 59), &c, &st, false).contains(&Job::RiskCheck));
    let t0 = at(day, 11, 0);
    assert!(due_jobs(t0, &c, &st, false).contains(&Job::RiskCheck));
    mark_attempt(&mut st, Job::RiskCheck, t0);
    assert!(!due_jobs(t0 + Duration::minutes(14), &c, &st, false).contains(&Job::RiskCheck));
    assert!(due_jobs(t0 + Duration::minutes(15), &c, &st, false).contains(&Job::RiskCheck));
    // Not at or after the close, and runs while paused.
    let fresh = ScheduleState::default();
    assert!(!due_jobs(at(day, 17, 0), &c, &fresh, false).contains(&Job::RiskCheck));
    assert!(due_jobs(at(day, 16, 59), &c, &fresh, true).contains(&Job::RiskCheck));
}

#[test]
fn holiday_and_weekend_have_no_jobs() {
    let holiday = d(2026, 12, 8); // Tuesday
    let c = cfg(vec![holiday]);
    let st = ScheduleState::default();
    assert!(due_jobs(at(holiday, 12, 0), &c, &st, false).is_empty());
    assert!(due_jobs(at(holiday, 17, 45), &c, &st, false).is_empty());
    let saturday = d(2026, 11, 28);
    assert!(due_jobs(at(saturday, 12, 0), &c, &st, false).is_empty());
    assert!(!is_trading_day(saturday, &[]));
    assert!(!is_trading_day(holiday, &[holiday]));
    assert!(is_trading_day(d(2026, 12, 9), &[holiday]));
}

#[test]
fn monthly_runs_once_per_month() {
    let c = cfg(vec![]);
    let first = d(2026, 12, 1); // Tuesday
    let mut st = ScheduleState::default();
    assert!(!due_jobs(at(first, 11, 29), &c, &st, false).contains(&Job::MonthlyRebalance));
    let t = at(first, 11, 30);
    assert_eq!(due_jobs(t, &c, &st, false)[0], Job::MonthlyRebalance);
    // Paused: never.
    assert!(!due_jobs(t, &c, &st, true).contains(&Job::MonthlyRebalance));
    // Failed attempt: wait 30 minutes.
    mark_attempt(&mut st, Job::MonthlyRebalance, t);
    assert!(!due_jobs(t + Duration::minutes(29), &c, &st, false).contains(&Job::MonthlyRebalance));
    assert!(due_jobs(t + Duration::minutes(30), &c, &st, false).contains(&Job::MonthlyRebalance));
    // Done: not again this month, even on a later day.
    mark_done(&mut st, Job::MonthlyRebalance, t);
    assert!(!due_jobs(t + Duration::hours(1), &c, &st, false).contains(&Job::MonthlyRebalance));
    assert!(!due_jobs(at(d(2026, 12, 3), 12, 0), &c, &st, false).contains(&Job::MonthlyRebalance));
    // Next month it is due again.
    assert!(due_jobs(at(d(2027, 1, 4), 11, 30), &c, &st, false).contains(&Job::MonthlyRebalance));
    // Catch-up on the 3rd trading day when missed.
    let missed = ScheduleState::default();
    assert!(
        due_jobs(at(d(2026, 12, 3), 12, 0), &c, &missed, false).contains(&Job::MonthlyRebalance)
    );
    // But not after the close.
    assert!(
        !due_jobs(at(d(2026, 12, 3), 17, 0), &c, &missed, false).contains(&Job::MonthlyRebalance)
    );
}

#[test]
fn daily_close_once_after_close_time() {
    let c = cfg(vec![]);
    let day = d(2026, 11, 24);
    let mut st = ScheduleState::default();
    assert!(!due_jobs(at(day, 17, 29), &c, &st, false).contains(&Job::DailyClose));
    let t = at(day, 17, 30);
    assert!(due_jobs(t, &c, &st, false).contains(&Job::DailyClose));
    mark_done(&mut st, Job::DailyClose, t);
    assert!(!due_jobs(at(day, 18, 0), &c, &st, false).contains(&Job::DailyClose));
    assert!(due_jobs(at(d(2026, 11, 25), 17, 30), &c, &st, false).contains(&Job::DailyClose));
}

#[test]
fn weekly_report_on_configured_day() {
    let c = cfg(vec![]);
    let friday = d(2026, 11, 27);
    let mut st = ScheduleState::default();
    assert!(!due_jobs(at(friday, 17, 59), &c, &st, false).contains(&Job::WeeklyReport));
    assert!(!due_jobs(at(d(2026, 11, 26), 18, 30), &c, &st, false).contains(&Job::WeeklyReport));
    let t = at(friday, 18, 0);
    assert!(due_jobs(t, &c, &st, false).contains(&Job::WeeklyReport));
    mark_done(&mut st, Job::WeeklyReport, t);
    assert!(!due_jobs(at(friday, 19, 0), &c, &st, false).contains(&Job::WeeklyReport));
}

#[test]
fn previous_trading_day_skips_weekend_and_holidays() {
    // Monday 2026-12-07 with Friday 2026-12-04 a holiday.
    assert_eq!(
        previous_trading_day(d(2026, 12, 7), &[d(2026, 12, 4)]),
        d(2026, 12, 3)
    );
    assert_eq!(previous_trading_day(d(2026, 12, 7), &[]), d(2026, 12, 4));
}
