use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use moon_core::asset::{Currency, Market};
use moon_core::money::dec_from_f64;
use moon_core::time::{ART, art_date};
use moon_core::types::Term;
use moon_iol::IolClient;
use moon_yahoo::YahooClient;
use rust_decimal::Decimal;

use crate::BrokerError;

#[derive(Debug, Clone, PartialEq)]
pub struct Quote {
    pub symbol: String,
    pub market: Market,
    pub price: Decimal,
    pub currency: Currency,
    pub at: DateTime<Utc>,
}

/// A quote older than this is not trusted for aggressive limit prices.
pub const FRESH_QUOTE_MAX_AGE_MINUTES: i64 = 30;

impl Quote {
    /// True when the quote is at most 30 minutes old and from the same ART day as `now`.
    /// A quote stamped in the future (clock skew) counts as fresh when it is from today.
    pub fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        now - self.at <= Duration::minutes(FRESH_QUOTE_MAX_AGE_MINUTES)
            && art_date(self.at) == art_date(now)
    }
}

/// Parses the `last_updated` stamp of an IOL quote: RFC 3339, or a naive date-time or a bare
/// date interpreted in ART. Returns `None` when it cannot be read.
pub fn parse_quote_time(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(raw) {
        return Some(t.with_timezone(&Utc));
    }
    let naive = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(raw, f).ok())
        .or_else(|| {
            NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        })?;
    ART.from_local_datetime(&naive)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
}

/// Timestamp that is never fresh: used when the quote time is unknown.
fn never_fresh() -> DateTime<Utc> {
    DateTime::<Utc>::UNIX_EPOCH
}

#[async_trait::async_trait]
pub trait QuoteSource: Send + Sync {
    async fn quote(&self, symbol: &str, market: Market) -> Result<Quote, BrokerError>;
}

/// Parses a currency label as reported by IOL or Yahoo (case-insensitive).
pub fn parse_currency(raw: &str) -> Option<Currency> {
    let up = raw.to_uppercase();
    if up.contains("ARS") || up.contains("PESO") {
        Some(Currency::Ars)
    } else if up.contains("USD") || up.contains("DOLAR") || up.contains("DÓLAR") {
        Some(Currency::Usd)
    } else {
        None
    }
}

pub struct IolQuotes {
    client: Arc<IolClient>,
    term: Term,
}

impl IolQuotes {
    pub fn new(client: Arc<IolClient>, term: Term) -> Self {
        Self { client, term }
    }
}

#[async_trait::async_trait]
impl QuoteSource for IolQuotes {
    async fn quote(&self, symbol: &str, market: Market) -> Result<Quote, BrokerError> {
        let q = self
            .client
            .quote(symbol, market, self.term)
            .await
            .map_err(|e| BrokerError::Quote(format!("{symbol}: {e}")))?;
        let p = &q.prices;
        let last = p.current.filter(|v| *v > 0.0);
        let raw = last
            .or_else(|| {
                [p.close, p.previous_close]
                    .into_iter()
                    .flatten()
                    .find(|v| *v > 0.0)
            })
            .ok_or_else(|| BrokerError::Quote(format!("{symbol}: sin precio")))?;
        // Only a last-trade price carries the quote time; a close is never fresh.
        let at = last
            .and(q.last_updated.as_deref())
            .and_then(parse_quote_time)
            .unwrap_or_else(never_fresh);
        let price = dec_from_f64(raw)
            .ok_or_else(|| BrokerError::Quote(format!("{symbol}: precio inválido")))?;
        let raw_ccy = q.currency.clone().unwrap_or_default();
        let currency = parse_currency(&raw_ccy).ok_or_else(|| BrokerError::CurrencyMismatch {
            symbol: symbol.to_string(),
            expected: Currency::Ars,
            got: raw_ccy,
        })?;
        Ok(Quote {
            symbol: symbol.to_string(),
            market,
            price,
            currency,
            at,
        })
    }
}

pub struct YahooQuotes {
    client: YahooClient,
}

impl YahooQuotes {
    pub fn new(client: YahooClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl QuoteSource for YahooQuotes {
    async fn quote(&self, symbol: &str, market: Market) -> Result<Quote, BrokerError> {
        let ysym = match market {
            Market::Bcba => format!("{symbol}.BA"),
            Market::Nyse | Market::Nasdaq => symbol.to_string(),
        };
        let q = self
            .client
            .latest_quote(&ysym)
            .await
            .map_err(|e| BrokerError::Quote(format!("{symbol}: {e}")))?;
        let price = dec_from_f64(q.price)
            .filter(|p| *p > Decimal::ZERO)
            .ok_or_else(|| BrokerError::Quote(format!("{symbol}: precio inválido")))?;
        let currency =
            parse_currency(&q.currency).ok_or_else(|| BrokerError::CurrencyMismatch {
                symbol: symbol.to_string(),
                expected: Currency::Ars,
                got: q.currency.clone(),
            })?;
        Ok(Quote {
            symbol: symbol.to_string(),
            market,
            price,
            currency,
            at: q.at,
        })
    }
}

/// In-memory quote source for tests.
#[derive(Default)]
pub struct StaticQuotes {
    quotes: Mutex<BTreeMap<(String, Market), Quote>>,
    /// Timestamp given to quotes set later; `None` means the real current time.
    stamp: Mutex<Option<DateTime<Utc>>>,
}

impl StaticQuotes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Quotes set from now on are stamped with `at` (to match a fixed test clock).
    pub fn with_stamp(at: DateTime<Utc>) -> Self {
        Self {
            quotes: Mutex::new(BTreeMap::new()),
            stamp: Mutex::new(Some(at)),
        }
    }

    /// Re-stamps an existing quote (e.g. to make it stale).
    pub fn set_at(&self, symbol: &str, market: Market, at: DateTime<Utc>) {
        if let Ok(mut m) = self.quotes.lock()
            && let Some(q) = m.get_mut(&(symbol.to_string(), market))
        {
            q.at = at;
        }
    }

    pub fn set(&self, symbol: &str, market: Market, price: Decimal, currency: Currency) {
        let at = self
            .stamp
            .lock()
            .ok()
            .and_then(|s| *s)
            .unwrap_or_else(Utc::now);
        let q = Quote {
            symbol: symbol.to_string(),
            market,
            price,
            currency,
            at,
        };
        if let Ok(mut m) = self.quotes.lock() {
            m.insert((symbol.to_string(), market), q);
        }
    }

    pub fn remove(&self, symbol: &str, market: Market) {
        if let Ok(mut m) = self.quotes.lock() {
            m.remove(&(symbol.to_string(), market));
        }
    }
}

#[async_trait::async_trait]
impl QuoteSource for StaticQuotes {
    async fn quote(&self, symbol: &str, market: Market) -> Result<Quote, BrokerError> {
        let m = self
            .quotes
            .lock()
            .map_err(|_| BrokerError::Quote("estado interno envenenado".into()))?;
        m.get(&(symbol.to_string(), market))
            .cloned()
            .ok_or_else(|| BrokerError::Quote(format!("{symbol}: sin cotización")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quote_at(at: DateTime<Utc>) -> Quote {
        Quote {
            symbol: "X".into(),
            market: Market::Bcba,
            price: Decimal::ONE,
            currency: Currency::Ars,
            at,
        }
    }

    #[test]
    fn parses_rfc3339_naive_and_date_only() {
        let utc = |h, m| Utc.with_ymd_and_hms(2026, 11, 23, h, m, 0).single();
        assert_eq!(parse_quote_time("2026-11-23T15:00:00Z"), utc(15, 0));
        // Naive values are ART (UTC-3).
        assert_eq!(parse_quote_time("2026-11-23T12:00:00"), utc(15, 0));
        assert_eq!(
            parse_quote_time("2026-11-23T12:00:00.500").map(|t| t.timestamp_subsec_millis()),
            Some(500)
        );
        assert_eq!(parse_quote_time("2026-11-23"), utc(3, 0));
        assert_eq!(parse_quote_time("ayer"), None);
        assert_eq!(parse_quote_time(""), None);
    }

    #[test]
    fn freshness_needs_thirty_minutes_and_the_same_art_day() {
        let now = Utc
            .with_ymd_and_hms(2026, 11, 23, 14, 30, 0)
            .single()
            .unwrap();
        assert!(quote_at(now).is_fresh(now));
        assert!(quote_at(now - Duration::minutes(30)).is_fresh(now));
        assert!(!quote_at(now - Duration::minutes(31)).is_fresh(now));
        assert!(!quote_at(never_fresh()).is_fresh(now));
        // 02:00 UTC is still the previous ART day.
        let early = Utc
            .with_ymd_and_hms(2026, 11, 23, 3, 10, 0)
            .single()
            .unwrap();
        assert!(!quote_at(early - Duration::minutes(20)).is_fresh(early));
    }
}
