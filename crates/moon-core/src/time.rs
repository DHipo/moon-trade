use std::sync::Mutex;

use chrono::{DateTime, NaiveDate, Utc};

/// Time zone used for scheduling and for dates derived from IOL timestamps.
pub const ART: chrono_tz::Tz = chrono_tz::America::Argentina::Buenos_Aires;

/// Time zone of the US markets (signal data is dated in it).
pub const NEW_YORK: chrono_tz::Tz = chrono_tz::America::New_York;

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Clock whose time is set manually (tests, backtests).
pub struct FixedClock {
    t: Mutex<DateTime<Utc>>,
}

impl FixedClock {
    pub fn new(t: DateTime<Utc>) -> Self {
        Self { t: Mutex::new(t) }
    }

    pub fn set(&self, t: DateTime<Utc>) {
        *self.t.lock().unwrap_or_else(|e| e.into_inner()) = t;
    }

    pub fn advance(&self, d: chrono::Duration) {
        let mut g = self.t.lock().unwrap_or_else(|e| e.into_inner());
        *g += d;
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        *self.t.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub fn art_datetime(t: DateTime<Utc>) -> DateTime<chrono_tz::Tz> {
    t.with_timezone(&ART)
}

pub fn art_date(t: DateTime<Utc>) -> NaiveDate {
    art_datetime(t).date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn art_date_uses_buenos_aires() {
        // 02:00 UTC is 23:00 of the previous day in ART (UTC-3).
        let t = Utc.with_ymd_and_hms(2024, 3, 2, 2, 0, 0).unwrap();
        assert_eq!(art_date(t), NaiveDate::from_ymd_opt(2024, 3, 1).unwrap());
        assert_eq!(art_datetime(t).format("%H:%M").to_string(), "23:00");
    }

    #[test]
    fn fixed_clock_set_and_advance() {
        let t = Utc.with_ymd_and_hms(2024, 3, 2, 2, 0, 0).unwrap();
        let c = FixedClock::new(t);
        assert_eq!(c.now(), t);
        c.advance(chrono::Duration::hours(1));
        assert_eq!(c.now(), t + chrono::Duration::hours(1));
        c.set(t);
        assert_eq!(c.now(), t);
        let _ = SystemClock.now();
    }
}
