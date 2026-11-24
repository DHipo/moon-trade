use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use moon_core::asset::Currency;
use moon_core::money::{dec_from_f64, dec_to_f64};
use moon_core::time::{ART, Clock, art_date};
use moon_core::types::{ExecMode, OrderStatus, Term};
use moon_iol::{Country, IolClient, IolError, IolOrderRequest, OperationsQuery};
use moon_store::FlowKind;
use rust_decimal::Decimal;

use crate::{
    AccountState, Broker, BrokerError, ExternalFlow, Listing, OrderProgress, OrderTicket,
    PlaceResult, parse_currency,
};

/// Adapter over the real IOL API. Every order placed here is REAL.
pub struct IolBroker {
    client: Arc<IolClient>,
    term: Term,
    clock: Arc<dyn Clock>,
}

impl IolBroker {
    pub fn new(client: Arc<IolClient>, term: Term, clock: Arc<dyn Clock>) -> Self {
        Self {
            client,
            term,
            clock,
        }
    }
}

fn err(e: IolError) -> BrokerError {
    BrokerError::Broker(e.to_string())
}

fn dec(x: f64) -> Result<Decimal, BrokerError> {
    dec_from_f64(x)
        .ok_or_else(|| BrokerError::Broker(format!("dato numérico inválido de IOL: {x}")))
}

/// Parses an IOL timestamp. RFC 3339 values keep their offset; date-only and naive values
/// are interpreted as ART local time.
fn parse_at(raw: Option<&str>, fallback: DateTime<Utc>) -> DateTime<Utc> {
    let Some(s) = raw else { return fallback };
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return dt.with_timezone(&Utc);
    }
    let naive = ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"]
        .iter()
        .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok())
        .or_else(|| {
            s.get(..10)
                .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
                .and_then(|d| d.and_hms_opt(0, 0, 0))
        });
    naive
        .and_then(|n| ART.from_local_datetime(&n).earliest())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(fallback)
}

#[async_trait::async_trait]
impl Broker for IolBroker {
    fn mode(&self) -> ExecMode {
        ExecMode::Live
    }

    async fn account(&self) -> Result<AccountState, BrokerError> {
        let balances = self.client.balances(Country::Ar).await.map_err(err)?;
        let unavailable =
            || BrokerError::Broker("saldo en pesos no disponible en la respuesta de IOL".into());
        let ars = balances.ars.as_ref().ok_or_else(unavailable)?;
        let avail = ars.available.get(self.term);
        let committed = ars.committed.get(self.term);
        let totals = ars.totals.get(self.term);
        let total = totals.or_else(|| avail.map(|a| a + committed.unwrap_or(0.0)));
        let available = avail.or_else(|| totals.map(|t| (t - committed.unwrap_or(0.0)).max(0.0)));
        let (Some(total), Some(available)) = (total, available) else {
            return Err(unavailable());
        };
        let cash_ars = dec(available)?;
        let cash_total_ars = dec(total)?;
        let pos = self.client.positions(Country::Ar).await.map_err(err)?;
        let mut positions: BTreeMap<String, Decimal> = BTreeMap::new();
        let mut sellable: BTreeMap<String, Decimal> = BTreeMap::new();
        for p in &pos.positions {
            let sym = p.instrument.symbol.clone();
            *positions.entry(sym.clone()).or_default() += dec(p.quantity)?;
            let avail = p
                .availability
                .as_ref()
                .and_then(|a| a.quantity_available)
                .unwrap_or(p.quantity);
            *sellable.entry(sym).or_default() += dec(avail)?;
        }
        Ok(AccountState {
            cash_ars,
            cash_total_ars,
            positions,
            sellable,
            as_of: self.clock.now(),
        })
    }

    async fn place(&self, ticket: &OrderTicket) -> Result<PlaceResult, BrokerError> {
        let req = IolOrderRequest {
            symbol: ticket.symbol.clone(),
            market_id: ticket.market,
            side: ticket.side,
            order_type: ticket.order_type,
            due_date: ticket.due_date,
            limit_price: ticket.limit_price.map(dec_to_f64),
            quantity: dec_to_f64(ticket.qty),
            settlement_term: Some(ticket.settlement_term),
        };
        let ack = match self.client.place_order(&req, ticket.idempotency_key).await {
            Ok(ack) => ack,
            // A definite 4xx means IOL refused the order: it was not created.
            Err(IolError::Api {
                status, message, ..
            }) if (400..500).contains(&status) && !matches!(status, 408 | 409 | 429) => {
                return Ok(PlaceResult {
                    broker_order_id: String::new(),
                    accepted: false,
                    messages: vec![message],
                    fill: None,
                });
            }
            // Timeouts, 5xx, 408/409/429 and unparsable replies: the outcome is unknown.
            Err(e) => return Err(err(e)),
        };
        Ok(PlaceResult {
            broker_order_id: ack.order_id,
            accepted: ack.successful_purchase,
            messages: ack.messages.into_iter().map(|m| m.message).collect(),
            fill: None,
        })
    }

    async fn refresh(
        &self,
        broker_order_id: &str,
        symbol: &str,
        placed_on: NaiveDate,
    ) -> Result<OrderProgress, BrokerError> {
        let page = self
            .client
            .operations(&OperationsQuery {
                start_date: placed_on,
                end_date: art_date(self.clock.now()),
                country: Some("AR".into()),
                symbol: Some(symbol.to_string()).filter(|s| !s.is_empty()),
                state: None,
            })
            .await
            .map_err(err)?;
        let truncated = page
            .total_count
            .is_some_and(|total| total > page.items.len() as u64);
        if truncated {
            tracing::warn!(
                order_id = broker_order_id,
                returned = page.items.len(),
                total_count = page.total_count,
                "IOL devolvió menos operaciones que su total; la lista puede estar truncada"
            );
        }
        let found = page.items.iter().find(|o| o.order_id == broker_order_id);
        let listing = match (found.is_some(), truncated) {
            (true, _) => Listing::Listed,
            (false, false) => Listing::NotListed,
            (false, true) => Listing::NotListedTruncated,
        };
        let (mut qty, price) = found
            .and_then(|o| o.executed.as_ref())
            .map(|e| (e.qty.unwrap_or(0.0), e.price.unwrap_or(0.0)))
            .unwrap_or((0.0, 0.0));
        let ordered = found.and_then(|o| o.order.as_ref()).and_then(|o| o.qty);
        if let Some(max) = ordered
            && qty > max
        {
            tracing::warn!(
                order_id = broker_order_id,
                executed = qty,
                ordered = max,
                "IOL informó más cantidad ejecutada que ordenada; se limita a lo ordenado"
            );
            qty = max;
        }
        let completed_without_fill =
            found.is_some_and(|o| o.status.eq_ignore_ascii_case("completed") && qty <= 0.0);
        if completed_without_fill {
            tracing::warn!(
                order_id = broker_order_id,
                "IOL marcó la orden como completada sin datos de ejecución; se mantiene pendiente"
            );
        }
        let status = match found.map(|o| o.status.to_lowercase()).as_deref() {
            Some("completed") if completed_without_fill => OrderStatus::Sent,
            Some("completed") => OrderStatus::Filled,
            Some("pending") if qty > 0.0 => OrderStatus::PartiallyFilled,
            Some("cancelled") => OrderStatus::Cancelled,
            Some("rejected") => OrderStatus::Rejected,
            _ => OrderStatus::Sent,
        };
        Ok(OrderProgress {
            status,
            filled_qty: dec(qty)?,
            avg_price: (price > 0.0).then(|| dec(price)).transpose()?,
            fees: None,
            listing,
        })
    }

    async fn cancel(&self, broker_order_id: &str) -> Result<(), BrokerError> {
        let reply = self
            .client
            .cancel_order(broker_order_id)
            .await
            .map_err(err)?;
        let confirmed = reply.cancelled
            || reply.status.as_deref().is_some_and(|s| {
                s.eq_ignore_ascii_case("cancelled") || s.eq_ignore_ascii_case("canceled")
            });
        if confirmed {
            Ok(())
        } else {
            Err(BrokerError::Broker(format!(
                "IOL no confirmó la cancelación de la orden {broker_order_id}"
            )))
        }
    }

    async fn external_flows(
        &self,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Result<Vec<ExternalFlow>, BrokerError> {
        let acts = self.client.activities(from, to, None).await.map_err(err)?;
        let now = self.clock.now();
        let mut out = Vec::new();
        for a in acts {
            if a.status.as_deref().map(str::to_lowercase).as_deref() != Some("completed") {
                continue;
            }
            if a.currency.as_deref().and_then(parse_currency) != Some(Currency::Ars) {
                continue;
            }
            let t = a.activity_type.as_deref().unwrap_or("").to_lowercase();
            let kind = if t.contains("deposit") || t.contains("depósito") {
                FlowKind::Deposit
            } else if t.contains("extraction") || t.contains("extracción") {
                FlowKind::Withdrawal
            } else {
                continue;
            };
            let Some(net) = a.net_amount else { continue };
            out.push(ExternalFlow {
                external_id: a.id,
                kind,
                amount_ars: dec(net.abs())?,
                at: parse_at(a.trade_date.as_deref(), now),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn dec_rejects_non_finite() {
        for x in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let e = dec(x).unwrap_err();
            assert!(e.to_string().contains("dato numérico inválido de IOL"));
        }
        assert_eq!(dec(1.5).unwrap(), rust_decimal::Decimal::new(15, 1));
    }
}
