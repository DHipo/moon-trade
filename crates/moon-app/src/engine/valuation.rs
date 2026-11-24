//! Portfolio valuation and rebalance planning.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail};
use moon_broker::{BrokerError, implied_ccl};
use moon_core::asset::{AssetId, Currency};
use moon_core::money::{dec_to_f64, round_money};
use moon_core::rebalance::{RebalanceInput, RebalanceParams, RebalancePlan, plan_rebalance};
use moon_core::risk::check_targets;
use moon_core::weights::TargetWeights;
use moon_store::FxRecord;
use rust_decimal::Decimal;

use super::{Engine, PositionValue, Valuation};

impl Engine {
    /// Values the account at current quotes. A missing quote of a HELD asset (or of the CCL) is an
    /// error: there is no partial valuation of what is owned. Assets that are not held and have no
    /// quote are listed in `unpriced` and cannot be bought now.
    pub async fn valuation(&self) -> anyhow::Result<Valuation> {
        let account = self.deps.broker.account().await?;
        let now = self.now();

        let mut positions = Vec::new();
        let mut tracked: BTreeSet<String> = BTreeSet::new();
        let mut unpriced: Vec<AssetId> = Vec::new();
        for asset in self.universe.tradable() {
            let exec = asset
                .exec
                .as_ref()
                .ok_or_else(|| anyhow!("{} no tiene configuración de ejecución", asset.id))?;
            if exec.currency != Currency::Ars {
                bail!(
                    "{} se ejecuta en {}: solo se soportan activos que cotizan en ARS",
                    asset.id,
                    exec.currency.as_str()
                );
            }
            let qty = account
                .positions
                .get(&exec.symbol)
                .copied()
                .unwrap_or(Decimal::ZERO)
                .max(Decimal::ZERO);
            let sellable = account
                .sellable
                .get(&exec.symbol)
                .copied()
                .unwrap_or(Decimal::ZERO)
                .clamp(Decimal::ZERO, qty);
            let held = qty > Decimal::ZERO;
            // A missing quote only blocks the valuation when the asset is held: an asset that is
            // not held simply cannot be bought now.
            let quote = match self.deps.quotes.quote(&exec.symbol, exec.market).await {
                Ok(q) if q.price > Decimal::ZERO || held => q,
                Ok(_) => {
                    tracked.insert(exec.symbol.clone());
                    unpriced.push(asset.id.clone());
                    continue;
                }
                Err(e) if held => return Err(e.into()),
                Err(e) => {
                    tracing::warn!(
                        "sin cotización de {} (no está en cartera): {e}",
                        exec.symbol
                    );
                    tracked.insert(exec.symbol.clone());
                    unpriced.push(asset.id.clone());
                    continue;
                }
            };
            if quote.currency != Currency::Ars {
                return Err(BrokerError::CurrencyMismatch {
                    symbol: exec.symbol.clone(),
                    expected: Currency::Ars,
                    got: quote.currency.as_str().to_string(),
                }
                .into());
            }
            if quote.price <= Decimal::ZERO {
                bail!("cotización inválida para {}", exec.symbol);
            }
            tracked.insert(exec.symbol.clone());
            positions.push(PositionValue {
                asset: asset.id.clone(),
                symbol: exec.symbol.clone(),
                qty,
                sellable,
                price_ars: quote.price,
                value_ars: round_money(qty * quote.price),
            });
        }

        let fx = implied_ccl(
            self.deps.quotes.as_ref(),
            &self.cfg.fx.pairs,
            self.cfg.fx.min_pairs,
            now,
        )
        .await?;
        if fx.rate <= Decimal::ZERO {
            bail!("el CCL implícito no es positivo");
        }
        let record = FxRecord {
            at: fx.at,
            rate: fx.rate,
            method: "implied_ccl".to_string(),
            components: serde_json::to_value(&fx.components)?,
        };
        self.db(|s| s.insert_fx(&record))?;

        let untracked: Vec<(String, Decimal)> = account
            .positions
            .iter()
            .filter(|(sym, qty)| !tracked.contains(*sym) && **qty != Decimal::ZERO)
            .map(|(sym, qty)| (sym.clone(), *qty))
            .collect();

        let equity_ars =
            account.cash_total_ars + positions.iter().map(|p| p.value_ars).sum::<Decimal>();
        Ok(Valuation {
            at: now,
            cash_ars: account.cash_ars,
            cash_total_ars: account.cash_total_ars,
            positions,
            untracked,
            unpriced,
            equity_ars,
            ccl: fx.rate,
            equity_usd: round_money(equity_ars / fx.rate),
        })
    }

    /// Scales the non-CASH targets down so the managed equity never exceeds `max_equity_usd`;
    /// the remainder stays in CASH.
    fn cap_to_max_equity(&self, targets: &TargetWeights, v: &Valuation) -> TargetWeights {
        let Some(max) = self.cfg.risk.max_equity_usd else {
            return targets.clone();
        };
        if v.equity_usd <= max {
            return targets.clone();
        }
        let scale = dec_to_f64(max / v.equity_usd);
        let mut out = TargetWeights::new();
        let mut used = 0.0;
        for (id, w) in targets.iter() {
            if id.is_cash() {
                continue;
            }
            let scaled = w * scale;
            used += scaled;
            out.add(id.clone(), scaled);
        }
        out.add(AssetId::cash(), 1.0 - used);
        out
    }

    /// Moves the weight of assets that cannot be priced (not held, no quote) to CASH.
    fn without_unpriced(targets: &TargetWeights, v: &Valuation) -> TargetWeights {
        if v.unpriced.is_empty() {
            return targets.clone();
        }
        let mut out = TargetWeights::new();
        let mut moved = 0.0;
        for (id, w) in targets.iter() {
            if v.unpriced.contains(id) {
                moved += w;
            } else {
                out.add(id.clone(), w);
            }
        }
        if moved > 0.0 {
            out.add(AssetId::cash(), moved);
        }
        out
    }

    /// Tells the owner which targets were skipped because the asset has no quote.
    pub(crate) async fn notify_unpriced(&self, v: &Valuation) {
        if v.unpriced.is_empty() {
            return;
        }
        let names: Vec<String> = v.unpriced.iter().map(ToString::to_string).collect();
        self.deps
            .notifier
            .notify(&format!(
                "⚠️ Sin cotización de {}: no se puede comprar ahora y se omite (el resto del plan sigue).",
                names.join(", ")
            ))
            .await;
    }

    pub fn plan(
        &self,
        targets: &TargetWeights,
        v: &Valuation,
        force_full: bool,
    ) -> anyhow::Result<RebalancePlan> {
        check_targets(targets, &self.universe, self.cfg.risk.max_weight)?;
        let targets = self.cap_to_max_equity(targets, v);
        let targets = Self::without_unpriced(&targets, v);
        if !targets.is_valid() {
            bail!("los pesos objetivo no suman 1");
        }

        let mut holdings: BTreeMap<AssetId, Decimal> = BTreeMap::new();
        let mut prices: BTreeMap<AssetId, Decimal> = BTreeMap::new();
        let mut lots: BTreeMap<AssetId, u32> = BTreeMap::new();
        for p in &v.positions {
            holdings.insert(p.asset.clone(), p.qty);
            prices.insert(p.asset.clone(), p.price_ars);
            if let Some(exec) = self.universe.get(&p.asset).and_then(|a| a.exec.as_ref()) {
                lots.insert(p.asset.clone(), exec.lot_size);
            }
        }
        let params = RebalanceParams {
            band: self.cfg.risk.rebalance_band,
            min_trade_value: round_money(self.cfg.risk.min_trade_usd * v.ccl),
            cash_buffer: self.cfg.risk.cash_buffer,
            slippage_rate: self.cfg.execution.slippage_rate,
            force_full,
        };
        let input = RebalanceInput {
            cash: v.cash_ars,
            holdings: &holdings,
            prices: &prices,
            lot_sizes: &lots,
            targets: &targets,
            fees: &self.cfg.fees,
            params: &params,
        };
        Ok(plan_rebalance(&input)?)
    }
}
