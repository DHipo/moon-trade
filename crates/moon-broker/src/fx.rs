use chrono::{DateTime, Utc};
use moon_core::asset::{Currency, Market};
use moon_core::money::round_money;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::{BrokerError, QuoteSource};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FxPair {
    pub local_symbol: String,
    pub adr_symbol: String,
    pub adr_market: Market,
    pub ratio: Decimal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FxComponent {
    pub pair: String,
    pub rate: Decimal,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FxQuote {
    pub rate: Decimal,
    pub components: Vec<FxComponent>,
    pub at: DateTime<Utc>,
}

/// Implied CCL (ARS per USD): median over `local * ratio / adr` of the usable pairs.
pub async fn implied_ccl(
    quotes: &dyn QuoteSource,
    pairs: &[FxPair],
    min_pairs: usize,
    now: DateTime<Utc>,
) -> Result<FxQuote, BrokerError> {
    let mut components = Vec::new();
    for p in pairs {
        let name = format!("{}/{}", p.local_symbol, p.adr_symbol);
        let local = match quotes.quote(&p.local_symbol, Market::Bcba).await {
            Ok(q) if q.currency == Currency::Ars && q.price > Decimal::ZERO => q,
            other => {
                tracing::warn!(pair = %name, "CCL: cotización local inválida, se omite ({other:?})");
                continue;
            }
        };
        let adr = match quotes.quote(&p.adr_symbol, p.adr_market).await {
            Ok(q) if q.currency == Currency::Usd && q.price > Decimal::ZERO => q,
            other => {
                tracing::warn!(pair = %name, "CCL: cotización ADR inválida, se omite ({other:?})");
                continue;
            }
        };
        components.push(FxComponent {
            pair: name,
            rate: local.price * p.ratio / adr.price,
        });
    }
    if components.is_empty() || components.len() < min_pairs {
        return Err(BrokerError::Quote(format!(
            "CCL implícito: {} pares válidos, se requieren {min_pairs}",
            components.len()
        )));
    }
    let mut rates: Vec<Decimal> = components.iter().map(|c| c.rate).collect();
    rates.sort();
    let n = rates.len();
    let median = if n % 2 == 1 {
        rates[n / 2]
    } else {
        (rates[n / 2 - 1] + rates[n / 2]) / Decimal::TWO
    };
    Ok(FxQuote {
        rate: round_money(median),
        components,
        at: now,
    })
}
