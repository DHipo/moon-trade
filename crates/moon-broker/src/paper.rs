use std::sync::{Arc, Mutex};

use chrono::NaiveDate;
use moon_core::asset::{Currency, Market};
use moon_core::fees::FeeModel;
use moon_core::money::{round_money, round_price};
use moon_core::time::Clock;
use moon_core::types::{ExecMode, OrderStatus, Side};
use moon_store::Store;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::{
    AccountState, Broker, BrokerError, ExternalFlow, OrderProgress, OrderTicket, PlaceResult,
    QuoteSource,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaperOrderRecord {
    broker_order_id: String,
    accepted: bool,
    messages: Vec<String>,
    fill: Option<OrderProgress>,
}

impl PaperOrderRecord {
    fn into_result(self) -> PlaceResult {
        PlaceResult {
            broker_order_id: self.broker_order_id,
            accepted: self.accepted,
            messages: self.messages,
            fill: self.fill,
        }
    }
}

/// Simulated broker: fills against the latest quote with slippage, persisting to the store.
pub struct PaperBroker {
    store: Arc<Mutex<Store>>,
    quotes: Arc<dyn QuoteSource>,
    fees: FeeModel,
    slippage_rate: Decimal,
    clock: Arc<dyn Clock>,
}

impl PaperBroker {
    pub fn new(
        store: Arc<Mutex<Store>>,
        quotes: Arc<dyn QuoteSource>,
        fees: FeeModel,
        slippage_rate: Decimal,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            quotes,
            fees,
            slippage_rate,
            clock,
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Store>, BrokerError> {
        self.store
            .lock()
            .map_err(|_| BrokerError::Broker("store envenenado".into()))
    }

    fn key(ticket_key: impl std::fmt::Display) -> String {
        format!("paper_order:{ticket_key}")
    }
}

fn rejected(id: String, msg: &str) -> PaperOrderRecord {
    PaperOrderRecord {
        broker_order_id: id,
        accepted: false,
        messages: vec![msg.to_string()],
        fill: None,
    }
}

#[async_trait::async_trait]
impl Broker for PaperBroker {
    fn mode(&self) -> ExecMode {
        ExecMode::Paper
    }

    async fn account(&self) -> Result<AccountState, BrokerError> {
        let acct = self.lock()?.paper_account()?;
        Ok(AccountState {
            cash_ars: acct.cash_ars,
            cash_total_ars: acct.cash_ars,
            sellable: acct.positions.clone(),
            positions: acct.positions,
            as_of: self.clock.now(),
        })
    }

    async fn place(&self, ticket: &OrderTicket) -> Result<PlaceResult, BrokerError> {
        let state_key = Self::key(ticket.idempotency_key);
        if ticket.qty <= Decimal::ZERO {
            return Ok(rejected(
                format!("paper-{}", ticket.idempotency_key),
                "cantidad inválida (paper)",
            )
            .into_result());
        }
        if let Some(rec) = self.lock()?.get_state::<PaperOrderRecord>(&state_key)? {
            return Ok(rec.into_result());
        }
        let quote = self.quotes.quote(&ticket.symbol, ticket.market).await?;
        if ticket.market == Market::Bcba && quote.currency != Currency::Ars {
            return Err(BrokerError::CurrencyMismatch {
                symbol: ticket.symbol.clone(),
                expected: Currency::Ars,
                got: quote.currency.as_str().to_string(),
            });
        }
        let factor = match ticket.side {
            Side::Buy => Decimal::ONE + self.slippage_rate,
            Side::Sell => Decimal::ONE - self.slippage_rate,
        };
        let fill_price = round_price(quote.price * factor);
        let id = format!("paper-{}", ticket.idempotency_key);

        let marketable = match (ticket.limit_price, ticket.side) {
            (Some(l), Side::Buy) => fill_price <= l,
            (Some(l), Side::Sell) => fill_price >= l,
            (None, _) => true,
        };
        let value = round_money(ticket.qty * fill_price);
        let fees = self.fees.total(value);

        // Single lock: re-check idempotency, validate and apply atomically.
        let mut store = self.lock()?;
        if let Some(rec) = store.get_state::<PaperOrderRecord>(&state_key)? {
            return Ok(rec.into_result());
        }
        let mut acct = store.paper_account()?;
        let record = if !marketable {
            rejected(id, "precio límite no alcanzado (paper)")
        } else {
            let held = acct
                .positions
                .get(&ticket.symbol)
                .copied()
                .unwrap_or(Decimal::ZERO);
            match ticket.side {
                Side::Buy if value + fees > acct.cash_ars => {
                    rejected(id, "efectivo insuficiente (paper)")
                }
                Side::Sell if ticket.qty > held => rejected(id, "tenencia insuficiente (paper)"),
                side => {
                    let new_pos = match side {
                        Side::Buy => {
                            acct.cash_ars -= value + fees;
                            held + ticket.qty
                        }
                        Side::Sell => {
                            acct.cash_ars += value - fees;
                            held - ticket.qty
                        }
                    };
                    if new_pos.is_zero() {
                        acct.positions.remove(&ticket.symbol);
                    } else {
                        acct.positions.insert(ticket.symbol.clone(), new_pos);
                    }
                    let rec = PaperOrderRecord {
                        broker_order_id: id,
                        accepted: true,
                        messages: vec![],
                        fill: Some(OrderProgress {
                            status: OrderStatus::Filled,
                            filled_qty: ticket.qty,
                            avg_price: Some(fill_price),
                            fees: Some(fees),
                        }),
                    };
                    store.apply_paper_fill(&acct, &state_key, &rec)?;
                    return Ok(rec.into_result());
                }
            }
        };
        store.set_state(&state_key, &record)?;
        Ok(record.into_result())
    }

    async fn refresh(
        &self,
        broker_order_id: &str,
        _placed_on: NaiveDate,
    ) -> Result<OrderProgress, BrokerError> {
        let key = broker_order_id
            .strip_prefix("paper-")
            .ok_or_else(|| BrokerError::NotFound(broker_order_id.to_string()))?;
        let rec = self
            .lock()?
            .get_state::<PaperOrderRecord>(&Self::key(key))?
            .ok_or_else(|| BrokerError::NotFound(broker_order_id.to_string()))?;
        Ok(rec.fill.unwrap_or(OrderProgress {
            status: OrderStatus::Rejected,
            filled_qty: Decimal::ZERO,
            avg_price: None,
            fees: None,
        }))
    }

    async fn cancel(&self, _broker_order_id: &str) -> Result<(), BrokerError> {
        Ok(())
    }

    async fn external_flows(
        &self,
        _from: NaiveDate,
        _to: NaiveDate,
    ) -> Result<Vec<ExternalFlow>, BrokerError> {
        Ok(vec![])
    }
}
