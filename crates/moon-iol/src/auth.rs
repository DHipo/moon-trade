use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;

/// Cached token pair. Deliberately has no `Debug`.
pub(crate) struct TokenState {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Deserialize)]
pub(crate) struct TokenData {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_in: Option<f64>,
    #[serde(default)]
    pub expires_at: Option<String>,
}

impl TokenData {
    pub(crate) fn into_state(self, now: DateTime<Utc>) -> TokenState {
        let from_at = self
            .expires_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc));
        let expires_at = from_at.unwrap_or_else(|| {
            let secs = self.expires_in.filter(|s| s.is_finite() && *s > 0.0);
            now + Duration::seconds(secs.map_or(900, |s| s as i64))
        });
        TokenState {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at,
        }
    }
}

/// A token is reused only while more than this many seconds remain before expiry.
pub(crate) const EXPIRY_MARGIN_SECS: i64 = 60;

impl TokenState {
    pub(crate) fn is_fresh(&self, now: DateTime<Utc>) -> bool {
        self.expires_at - now > Duration::seconds(EXPIRY_MARGIN_SECS)
    }
}
