# moon-trade live-readiness fixes (N1–N6, operations paging, fast refuge)

> For agentic workers: four independent workstreams (P1–P4) run in parallel, each in its own git worktree. Each owns the files listed for it; touch other files only where the item says so, and keep those hunks minimal.

**Goal:** close the residual findings of the fix-wave re-review (`.superpowers/sdd/2026-11-23-moon-trade-mvp/final-rereview.md`, Spanish) that block live trading, and make the emergency path reach the refuge as fast as safely possible.

**Spec:** `docs/superpowers/specs/2026-11-23-moon-trade-design.md` (binding), plus the earlier rulings in `.superpowers/sdd/2026-11-23-moon-trade-mvp/fix-wave-rulings.md`.

## Global constraints (unchanged)

Same as the MVP plan: Decimal for money, Spanish user-facing text, no `unwrap` outside tests, no `std::sync::Mutex` guard across `.await`, no network in tests, never re-send an unknown-outcome order, emergencies in every mode and when paused, `cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings` + `cargo test --workspace` green before each commit.

---

## P1 — Emergency path: repricing, fast refuge, refuge without quote (N1, M6, N3, "refugio lo más rápido posible")

Owns: `crates/moon-app/src/engine/jobs.rs` (functions `emergency_sweep`, `emergency_refuge_buy`, `refuge_buy_best_effort`, `cancel_open_orders`, and the emergency part of `run_risk_check`), `crates/moon-app/src/engine/execution.rs`, `crates/moon-broker/src/quotes.rs`, `crates/moon-app/src/config.rs` (execution section), `moon.example.toml` (`[execution]`), README emergency section, emergency tests in `crates/moon-app/tests/engine.rs`.

1. **Quote freshness.** `IolQuotes` sets `Quote.at` from IOL `last_updated` (accept RFC 3339; naive date-time or date-only interpreted in ART); if missing or unparseable, set `at` to the Unix epoch so it is never considered fresh. A quote is *fresh* when `now − at ≤ 30 min` and `art_date(at) == art_date(now)`.
2. **Repricing (N1, M6).** While the breaker is tripped, every risk check first refreshes every non-terminal emergency order. A still-unfilled emergency order older than `execution.emergency_reprice_minutes` (new, default 10, validated 1..=60) is cancelled, refreshed, and re-sent for the remaining sellable quantity as a **market** order. The first attempt uses a limit at `floor_to_tick(last·(1 − emergency_limit_discount))` only when the quote is fresh; otherwise it is a market order from the start. A definite rejection (`accepted: false`) of a limit order is retried as market on the next check. Unknown-outcome orders keep the existing rule (never re-sent, notify). Tests: stale quote → market on first attempt; unfilled limit older than the reprice window → cancelled and re-sent as market; rejected limit → market next check; working order younger than the window → left alone.
3. **Fast refuge.** In the same risk check that sends emergency sells, wait for them to fill up to `execution.emergency_fill_wait_seconds` (new, default 120, validated 0..=600), polling every `poll_interval_seconds`; as soon as no risky holding remains (or the wait ends), run the refuge buy in that same check with the real available cash. The emergency refuge buy is exempt from `max_orders_per_day` but still split into tranches by `max_order_usd` (existing tranche logic). If sells are still pending after the wait, the refuge buy happens on the next check as today. Tests: paper (immediate fills) → refuge bought in the same `run_risk_check` call; FakeBroker that fills on the second refresh → refuge bought within the wait.
4. **Refuge without quote (N3).** In the emergency refuge buy, a missing or zero refuge quote is an error: notify (once per hour) and retry on the next check; do not mark the trip as "refuge done". Test.
5. README: correct the emergency description (limit only with a fresh quote, repricing to market after the window, refuge bought in the same check when sells fill, refuge without quote retried and notified).

## P2 — Risk index anomalies and trip confirmation (N2)

Owns: `crates/moon-core/src/risk_index.rs`, `crates/moon-core/src/risk.rs` (`DrawdownState`, `RiskLimits`), the `observe_risk` function and the single `drawdown.observe(...)` call inside `run_risk_check` in `crates/moon-app/src/engine/jobs.rs`, `moon.example.toml` (`[risk]` only), new test file `crates/moon-app/tests/risk_anomalies.rs`.

1. **Corporate actions (ratio change / split).** In `RiskIndex::observe`, for each symbol present in both observations with `q_prev > 0` and `q_now > 0`: `kq = q_now/q_prev`, `kp = p_prev/p_now`. If `kq ≥ 1.5` and `|kq/kp − 1| ≤ 0.05` (or the inverse reverse split `kq ≤ 1/1.5` with the same tolerance) → treat as a corporate action: zero return for that symbol, record an anomaly.
2. **Pending ratio change.** If `q_now == q_prev` and `kp` is within 3 % of an integer `k ≥ 2` (or `1/kp` within 3 % of an integer ≥ 2) → suspected ratio change not yet reflected in quantities: zero return for that symbol, keep the PREVIOUS price as its base (do not rebase it), record an anomaly, and count consecutive suspicions per symbol (persisted in the index). After 4 consecutive suspicious observations the move is accepted as real: compute the return from the kept base and rebase.
3. **API.** `RiskIndex` keeps `observe(...) -> Result<f64, CoreError>` and gains `pub anomalies: Vec<IndexAnomaly>` (cleared at the start of each observe), `#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)] pub struct IndexAnomaly { pub symbol: String, pub kind: AnomalyKind, pub detail: String }`, `pub enum AnomalyKind { CorporateAction, SuspectedRatioChange, AcceptedAfterConfirmation }`. Tests for each case, including a real −50 % move that persists 4 observations being counted.
4. **Trip confirmation.** `RiskLimits.trip_confirmations: u32` (serde default 2, validated 1..=10). `DrawdownState` gains `breaches: u32` (serde default 0) and `pub fn observe_confirmed(&mut self, level: f64, today: NaiveDate, max_drawdown: f64, confirmations: u32) -> DrawdownSignal`: a breach increments `breaches`; it trips only when `breaches ≥ confirmations`; a non-breach resets `breaches` to 0. The existing `observe` stays (backtest keeps using it). Tests.
5. **Engine wiring (minimal hunks).** `observe_risk` notifies each anomaly in Spanish (once per symbol per day) and `run_risk_check` uses `observe_confirmed` with `risk.trip_confirmations`. Add `trip_confirmations = 2` with a Spanish comment to `[risk]` in `moon.example.toml`. Engine tests in the new `tests/risk_anomalies.rs`: a 2:1 ratio change (quantity ×2, price ÷2) never trips; a pending ratio change (price ÷2, quantity unchanged) does not trip during the confirmation window; a real breach confirmed on the second consecutive check trips.

## P3 — Reports, order ids, decisions per mode (N4, M11, N5, N6)

Owns: `crates/moon-app/src/engine/text.rs`, `crates/moon-app/src/commands/orders.rs`, `crates/moon-app/src/cli.rs` (orders subcommands), `crates/moon-store` (migration v3 + decisions API), the decision logging/loading call sites in `jobs.rs` (`run_monthly` `log_decision` call and `last_monthly_targets`), `crates/moon-app/tests/orders.rs`, report tests.

1. **N4/M11 weekly bridge.** Subtract only flows with `at > first_snapshot.at`; convert each flow to USD with the CCL of the first snapshot at or after the flow (fallback: the latest snapshot's CCL). Test: a deposit dated between the previous Friday 18:00 and Monday's close is not double counted (+100 real result shows +100).
2. **N5 decisions per mode.** Store migration v3: `ALTER TABLE decisions ADD COLUMN mode TEXT` (NULL for old rows). `log_decision(at, kind, mode, payload)` and `recent_decisions(limit, mode)` filter by mode (old NULL rows are ignored for live and treated as paper). Engine: monthly logs with its mode; `last_monthly_targets` reads only the current mode; deposit buys in live wait for the first live monthly decision. Tests (store + engine).
3. **N6 order ids.** Every order line in notifications/status shows a short id (first 8 hex chars of the UUID); `/estado` and `moon status` list open and unknown-outcome orders with short ids; new `moon orders list [--unknown]`; `moon orders resolve <id-or-prefix> ...` accepts a unique prefix of ≥ 6 chars (ambiguous or unknown → Spanish error listing candidates). Tests.

## P4 — IOL operations: narrow queries and truncation detection (M19)

Owns: `crates/moon-iol/src/{client.rs,dto.rs}`, `crates/moon-broker/src/iol_broker.rs` (`refresh`), `crates/moon-app/src/commands/check.rs` (operations section only), tests in moon-iol and moon-broker.

The IOL spec documents no pagination parameters, only `meta.total_count`.
1. `IolClient::operations` returns `OperationsPage { items: Vec<IolOperation>, total_count: Option<u64> }` (read from the envelope's `meta.total_count`).
2. `IolBroker::refresh(broker_order_id, placed_on)` queries with `symbol` (pass the ticket symbol: extend the `Broker::refresh` call path minimally, or store the symbol in a refresh hint map keyed by order id inside `IolBroker` when placing) and `start_date = placed_on`, `end_date = today ART`, `country = AR`. If `total_count > items.len()` → `tracing::warn!` with both numbers, and if the order is not among the items, return `Sent` (still unknown) — never assume cancelled.
3. `moon check iol` prints, for operations, returned vs `total_count`.
4. Tests: truncation warning path (wiremock `meta.total_count` larger than items), symbol filter sent in the query.

## Integration

The controller merges P1–P4 into `feat/mvp`, resolves conflicts (expected only in `jobs.rs`, `moon.example.toml`, README), runs the full checks, pushes, and dispatches one independent review of the combined diff.
