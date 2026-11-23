# moon-trade MVP Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `moon`, a personal Rust CLI + service that manages an IOL (InvertirOnline) portfolio with monthly tactical allocation, a deterministic risk layer, paper/live execution, SQLite persistence and Telegram notifications.

**Architecture:** Cargo workspace. `moon-core` is pure, synchronous domain logic (strategies, risk, fees, rebalancing, backtest, metrics). Thin async HTTP clients (`moon-yahoo`, `moon-iol`, `moon-telegram`), persistence (`moon-store`), execution adapters (`moon-broker`) and the binary (`moon-app`) wire it together. Signals and backtests use USD underlyings (Yahoo); execution uses CEDEARs in ARS on BCBA via IOL; ARS↔USD uses an implied CCL.

**Tech Stack:** Rust stable (edition 2024), tokio, reqwest (rustls), rusqlite (bundled), rust_decimal, chrono + chrono-tz, serde/toml, clap, tracing, thiserror/anyhow, wiremock, proptest.

**Spec:** `docs/superpowers/specs/2026-11-23-moon-trade-design.md` (read it first) and `docs/iol-api.md` (exact IOL field names).

## Global Constraints

- Workspace at repo root; crates in `crates/<name>`; members via `members = ["crates/*"]`; `resolver = "3"`; edition 2024.
- Only dependencies declared in root `[workspace.dependencies]` (Task 1). Each crate uses `dep.workspace = true`. Adding a new dependency requires adding it to the root list.
- Code, identifiers and code comments in English. Every user-facing string (CLI output, Telegram messages, errors shown to the user, README) in Spanish.
- Money, prices used for orders/ledger, quantities and fees: `rust_decimal::Decimal`. Market-data series, indicators, weights and NAV index: `f64`. Convert only through `moon_core::money` helpers.
- Money rounding: 2 decimals, `RoundingStrategy::MidpointAwayFromZero` (`money::round_money`). Execution prices: 4 decimals. Quantities: floored to lot size (`money::floor_to_lot`).
- Keyed collections in outputs and persisted data: `BTreeMap`/`BTreeSet` (deterministic order).
- No `unwrap()` outside tests (clippy `unwrap_used = "deny"`); `unsafe_code = "forbid"`. Prefer `?` and typed errors (`thiserror` in libraries, `anyhow` only in `moon-app`).
- Never log or `Debug`-print secrets (IOL password, tokens, Telegram bot token). Implement `Debug` manually with `"***"` where a struct holds one.
- No test touches the network: HTTP through `wiremock`, SQLite in memory or in `tempfile` dirs.
- Time zone for scheduling and for dates derived from IOL timestamps: `America/Argentina/Buenos_Aires` (`moon_core::time::ART`).
- Every task ends green on: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`. Then commit with the given message (end the message with the line `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`) and `git push`.

## Review Focus

1. **Missing or stale prices** (no quote for a held symbol, Yahoo gaps, holidays): valuation must fail and no order may be sent; the user gets a Telegram/CLI error. Pinned in Task 12 (`missing_quote_blocks_trading`).
2. **Unknown or partial order state** (IOL operations not yet listing the order, partial execution): never re-send the same order; buys are recomputed from real cash; leftovers are cancelled at close. Pinned in Task 12 (`sent_order_not_resent_and_cancelled_at_close`, `idempotent_batch_places_once`).
3. **Deposits and withdrawals** must not count as performance and must not trip the drawdown breaker. Pinned in Task 4 (`nav_unchanged_by_flows`) and Task 12 (`paper_deposit_keeps_nav`).
4. **Cash exhausted by fees, minimum commission or rounding**: cash may never go negative and sells never exceed holdings. Pinned in Task 4 (proptest `plan_invariants`).
5. **Service restarts, downtime and holidays**: the monthly rebalance runs exactly once per month (catch-up on a later trading day if missed) and never twice after a restart. Pinned in Task 13 (`monthly_runs_once_per_month`, `holiday_and_weekend_have_no_jobs`).

---

### Task 1: Workspace scaffold and tooling

**Files:**
- Create: `Cargo.toml`, `rust-toolchain.toml`, `rustfmt.toml`, `clippy.toml`, `.gitattributes`, `.env.example`
- Modify: `.gitignore` (create if missing), `README.md`
- Create: `.github/workflows/ci.yml`, `.github/dependabot.yml`, `scripts/check.sh`, `scripts/check.ps1`
- Create crates (each with `Cargo.toml` + `src/lib.rs` containing a crate doc comment): `crates/moon-core`, `crates/moon-store`, `crates/moon-yahoo`, `crates/moon-iol`, `crates/moon-telegram`, `crates/moon-broker`; and `crates/moon-app` with `src/main.rs` (binary name `moon`).

**Interfaces:**
- Produces: the workspace, crate names (`moon-core`, `moon-store`, `moon-yahoo`, `moon-iol`, `moon-telegram`, `moon-broker`, `moon-app`), the dependency list and lints every later task relies on.

- [ ] **Step 1: Root `Cargo.toml`**

```toml
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT"
rust-version = "1.85"
publish = false

[workspace.dependencies]
moon-core = { path = "crates/moon-core" }
moon-store = { path = "crates/moon-store" }
moon-yahoo = { path = "crates/moon-yahoo" }
moon-iol = { path = "crates/moon-iol" }
moon-telegram = { path = "crates/moon-telegram" }
moon-broker = { path = "crates/moon-broker" }
anyhow = "1"
async-trait = "0.1"
chrono = { version = "0.4.38", default-features = false, features = ["std", "clock", "serde"] }
chrono-tz = "0.10"
clap = { version = "4.5", features = ["derive"] }
dotenvy = "0.15"
proptest = "1.5"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls"] }
rusqlite = { version = "0.32", features = ["bundled"] }
rust_decimal = { version = "1.36", features = ["serde"] }
rust_decimal_macros = "1.36"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sha2 = "0.10"
tempfile = "3"
thiserror = "2"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "time", "sync", "signal"] }
toml = "0.8"
tracing = "0.1"
tracing-appender = "0.2"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
uuid = { version = "1", features = ["v4", "v5", "serde"] }
wiremock = "0.6"

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
unwrap_used = "deny"
dbg_macro = "deny"
todo = "deny"
```

Every crate `Cargo.toml` uses `version.workspace = true`, `edition.workspace = true`, `license.workspace = true`, `publish.workspace = true` and `[lints] workspace = true`. In this task crates have no dependencies except `moon-app`, which gets `clap` and prints `moon-trade <version>` for `moon --version` (clap `#[command(version)]`).

- [ ] **Step 2: Tooling files**

- `rust-toolchain.toml`: `channel = "stable"`, `components = ["rustfmt", "clippy"]`.
- `rustfmt.toml`: `edition = "2024"`, `max_width = 100`.
- `clippy.toml`: `allow-unwrap-in-tests = true`.
- `.gitattributes`: `* text=auto eol=lf` and `*.ps1 text eol=crlf`.
- `.gitignore`: `/target`, `.env`, `*.db`, `*.db-shm`, `*.db-wal`, `/logs`, `/data`, `moon.toml`.
- `.env.example` (empty values, Spanish comment header saying never to commit `.env`): `MOON_IOL_USERNAME=`, `MOON_IOL_PASSWORD=`, `MOON_TELEGRAM_TOKEN=`, `MOON_TELEGRAM_CHAT_ID=`, `RUST_LOG=info`.
- `scripts/check.sh` (bash, `set -euo pipefail`) and `scripts/check.ps1` (`$ErrorActionPreference = "Stop"`, check `$LASTEXITCODE` after each): run fmt check, clippy `-D warnings`, tests.
- `README.md`: keep the title; add a short Spanish paragraph (what it is, "en construcción", link to the design doc).

- [ ] **Step 3: CI**

`.github/workflows/ci.yml`, triggers: push to `main` and `feat/**`, and `pull_request`; `permissions: contents: read`; env `CARGO_TERM_COLOR: always`. Jobs:
- `lint` (ubuntu-latest): `actions/checkout@v4`, `dtolnay/rust-toolchain@stable` with `components: rustfmt, clippy`, `Swatinem/rust-cache@v2`, `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --locked -- -D warnings`.
- `test` (matrix `ubuntu-latest`, `windows-latest`): checkout, toolchain, cache, `cargo test --workspace --locked`.
- `audit` (ubuntu-latest): checkout, `taiki-e/install-action@v2` with `tool: cargo-audit`, `cargo audit`.

`.github/dependabot.yml`: `cargo` at `/` weekly and `github-actions` at `/` weekly.

- [ ] **Step 4: Verify**

Run: `cargo build --workspace && cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo run -p moon-app -- --version`
Expected: all succeed; last command prints `moon-trade 0.1.0` (or `moon 0.1.0`). Commit `Cargo.lock`.

- [ ] **Step 5: Commit and push**

`git add -A && git commit -m "chore: scaffold workspace, tooling and CI"` then `git push`.

---

### Task 2: moon-core foundations

**Files:**
- Modify: `crates/moon-core/Cargo.toml` (deps: chrono, chrono-tz, rust_decimal, serde, serde_json, thiserror; dev: rust_decimal_macros, toml, proptest; feature `test-util = []`)
- Create: `crates/moon-core/src/{lib.rs,types.rs,asset.rs,bars.rs,money.rs,time.rs,error.rs,weights.rs,testutil.rs}`

**Interfaces:**
- Produces (all `pub`, re-exported modules from `lib.rs`):

```rust
// error.rs
#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum CoreError {
    #[error("universo inválido: {0}")] InvalidUniverse(String),
    #[error("serie de precios inválida: {0}")] InvalidSeries(String),
    #[error("configuración inválida: {0}")] InvalidConfig(String),
    #[error("falta el precio de {0}")] MissingPrice(AssetId),
    #[error("datos insuficientes: {0}")] MissingData(String),
    #[error("no se pudo interpretar: {0}")] Parse(String),
}

// types.rs — every enum derives Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
// Serialize, Deserialize; has `fn as_str(&self) -> &'static str` returning the serde name and
// `impl std::str::FromStr<Err = CoreError>` parsing that same name.
#[serde(rename_all = "snake_case")] pub enum Side { Buy, Sell }
#[serde(rename_all = "snake_case")] pub enum ExecMode { Paper, Live }
#[serde(rename_all = "snake_case")] pub enum OrderReason { Monthly, Deposit, Emergency, Manual }
#[serde(rename_all = "snake_case")] pub enum OrderStatus { PendingSend, Sent, PartiallyFilled, Filled, Cancelled, Rejected, Failed }
impl OrderStatus { pub fn is_terminal(self) -> bool } // Filled | Cancelled | Rejected | Failed
#[serde(rename_all = "snake_case")] pub enum DataSource { Yahoo, Iol }
#[serde(rename_all = "UPPERCASE")] pub enum Term { T0, T1, T2 }
#[serde(rename_all = "snake_case")] pub enum OrderType { Limit, Market }

// asset.rs
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AssetId(pub String);
impl AssetId {
    pub const CASH: &'static str = "CASH";
    pub fn new(s: impl Into<String>) -> Self;
    pub fn cash() -> Self;
    pub fn is_cash(&self) -> bool;
    pub fn as_str(&self) -> &str;
} // + Display
#[serde(rename_all = "UPPERCASE")] pub enum Market { Bcba, Nyse, Nasdaq }  // + as_str ("BCBA"...), FromStr, Copy, Ord, Hash
#[serde(rename_all = "UPPERCASE")] pub enum Currency { Ars, Usd }           // + as_str, FromStr, Copy, Ord, Hash
#[serde(rename_all = "snake_case")] pub enum Role { Risky, Refuge, Canary, Benchmark }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecSpec { pub symbol: String, pub market: Market, pub currency: Currency,
                      #[serde(default = "default_lot")] pub lot_size: u32 }          // default 1
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetSpec { pub id: AssetId, pub role: Role,
                       #[serde(default)] pub signal_symbol: Option<String>,
                       #[serde(default = "default_ratio")] pub cedear_ratio: f64,    // default 1.0; CEDEARs per underlying share
                       #[serde(default)] pub exec: Option<ExecSpec> }
#[derive(Debug, Clone, PartialEq)]
pub struct Universe { /* private: Vec<AssetSpec> */ }
impl Universe {
    pub fn new(assets: Vec<AssetSpec>) -> Result<Self, CoreError>;
    pub fn assets(&self) -> &[AssetSpec];
    pub fn risky(&self) -> Vec<&AssetSpec>;
    pub fn refuge(&self) -> &AssetSpec;
    pub fn canaries(&self) -> Vec<&AssetSpec>;
    pub fn benchmark(&self) -> Option<&AssetSpec>;
    pub fn tradable(&self) -> Vec<&AssetSpec>;          // risky + refuge, excluding CASH
    pub fn get(&self, id: &AssetId) -> Option<&AssetSpec>;
    pub fn by_exec_symbol(&self, symbol: &str) -> Option<&AssetSpec>;
    pub fn signal_symbols(&self) -> Vec<String>;        // unique, sorted, excluding None
}

// bars.rs
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bar { pub date: NaiveDate, pub open: f64, pub high: f64, pub low: f64,
                 pub close: f64, pub adj_close: f64, pub volume: f64 }
impl Bar { pub fn adj_factor(&self) -> f64; pub fn adj_open(&self) -> f64; } // adj_close/close; open*factor
#[derive(Debug, Clone, PartialEq)]
pub struct PriceSeries { /* private: Vec<Bar> sorted ascending, unique dates */ }
impl PriceSeries {
    pub fn new(bars: Vec<Bar>) -> Result<Self, CoreError>;
    pub fn bars(&self) -> &[Bar];
    pub fn is_empty(&self) -> bool;
    pub fn first_date(&self) -> Option<NaiveDate>;
    pub fn last_date(&self) -> Option<NaiveDate>;
    pub fn up_to(&self, date: NaiveDate) -> &[Bar];                 // date <= `date`
    pub fn bar_on(&self, date: NaiveDate) -> Option<&Bar>;
    pub fn on_or_before(&self, date: NaiveDate) -> Option<&Bar>;
}
pub fn monthly_closes(bars: &[Bar]) -> Vec<(NaiveDate, f64)>;         // last bar of each calendar month: (date, adj_close)

// money.rs
pub fn dec_from_f64(x: f64) -> Option<Decimal>;    // None if not finite; parse of `format!("{x}")` (shortest round-trip)
pub fn dec_to_f64(d: Decimal) -> f64;
pub fn round_money(d: Decimal) -> Decimal;         // 2 dp MidpointAwayFromZero
pub fn round_price(d: Decimal) -> Decimal;         // 4 dp MidpointAwayFromZero
pub fn floor_to_lot(qty: Decimal, lot: u32) -> Decimal; // floor(qty/lot)*lot; lot 0 => 1; negative => 0
pub fn weight_to_dec(w: f64) -> Decimal;           // clamp to [0,1], 8 dp

// time.rs
pub const ART: chrono_tz::Tz = chrono_tz::America::Argentina::Buenos_Aires;
pub trait Clock: Send + Sync { fn now(&self) -> DateTime<Utc>; }
pub struct SystemClock;                              // impl Clock
pub struct FixedClock { /* Mutex<DateTime<Utc>> */ }
impl FixedClock { pub fn new(t: DateTime<Utc>) -> Self; pub fn set(&self, t: DateTime<Utc>); pub fn advance(&self, d: chrono::Duration); } // impl Clock
pub fn art_date(t: DateTime<Utc>) -> NaiveDate;
pub fn art_datetime(t: DateTime<Utc>) -> DateTime<chrono_tz::Tz>;

// weights.rs
pub const WEIGHT_EPS: f64 = 1e-9;
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TargetWeights(pub BTreeMap<AssetId, f64>);
impl TargetWeights {
    pub fn new() -> Self;
    pub fn all_in(id: AssetId) -> Self;
    pub fn add(&mut self, id: AssetId, w: f64);        // accumulates; ignores w <= 0
    pub fn get(&self, id: &AssetId) -> f64;            // 0.0 if absent
    pub fn total(&self) -> f64;
    pub fn iter(&self) -> impl Iterator<Item = (&AssetId, f64)>;
    pub fn is_valid(&self) -> bool;                    // all finite & >= 0, |total-1| <= 1e-6
}

// testutil.rs  (#[cfg(any(test, feature = "test-util"))] pub mod testutil)
pub fn d(y: i32, m: u32, day: u32) -> NaiveDate;
pub fn daily_series(start: NaiveDate, closes: &[f64]) -> PriceSeries;   // one bar per weekday from start; o=h=l=c=adj=close, volume 1000
pub fn monthly_series(start_year: i32, start_month: u32, closes: &[f64]) -> PriceSeries; // one bar on the last weekday of each month
pub fn spec(id: &str, role: Role) -> AssetSpec;     // signal_symbol = id, exec = Some(BCBA, ARS, lot 1) for risky/refuge
```

Universe validation rules (`Universe::new`): ids unique; exactly one `Refuge`; at least one `Risky`; `Risky` needs `signal_symbol` and `exec`; refuge with id `CASH` must have no `exec`; any other refuge needs `signal_symbol` and `exec`; `Canary`/`Benchmark` need `signal_symbol`; `cedear_ratio > 0` and finite; `lot_size >= 1`; no asset other than the refuge may use id `CASH`.

`PriceSeries::new`: sorts by date, keeps the last bar for duplicate dates, errors (`InvalidSeries`) on any non-finite or non-positive `close`/`adj_close`.

- [ ] **Step 1: Write failing tests** in each module's `#[cfg(test)] mod tests`:
  - `universe_rejects_duplicate_ids`, `universe_requires_single_refuge`, `universe_risky_needs_exec`, `universe_accepts_cash_refuge`, `universe_tradable_excludes_cash` (CASH refuge + 2 risky → `tradable().len() == 2`).
  - `asset_spec_parses_from_toml`: parse `id = "SPY"\nrole = "risky"\nsignal_symbol = "SPY"\ncedear_ratio = 20.0\nexec = { symbol = "SPY", market = "BCBA", currency = "ARS" }` → `exec.lot_size == 1`, `market == Market::Bcba`.
  - `series_sorts_and_dedups`, `series_rejects_non_positive`, `up_to_is_inclusive`.
  - `monthly_closes_takes_last_bar_of_month`: bars on 2024-01-30, 2024-01-31, 2024-02-28, 2024-02-29 → `[(2024-01-31, …), (2024-02-29, …)]`.
  - `adj_open_scales_by_factor`: close 100, adj_close 50, open 90 → `adj_open() == 45.0`.
  - `dec_from_f64_is_exact_for_short_decimals` (`dec_from_f64(0.1) == Some(dec!(0.1))`, NaN → None), `round_money_half_away` (`1.005 → 1.01`, `-1.005 → -1.01`), `floor_to_lot_rounds_down` (`17,5 → 15`; `4,5 → 0`; `-3,1 → 0`).
  - `weights_validity` (0.5+0.5 valid; 0.5+0.6 invalid; NaN invalid), `enum_round_trip` (every enum: `as_str` then `from_str` equals original; `serde_json::to_string(&Term::T1) == "\"T1\""`, `Side::Buy → "\"buy\""`).
- [ ] **Step 2: Run** `cargo test -p moon-core` → FAIL (items missing).
- [ ] **Step 3: Implement** the interfaces above.
- [ ] **Step 4: Run** `cargo test -p moon-core` → PASS; then the global checks.
- [ ] **Step 5: Commit and push** `feat(core): domain types, universe, price series, money and time helpers`.

---

### Task 3: Indicators, strategies and allocator

**Files:**
- Create: `crates/moon-core/src/{indicators.rs,allocator.rs,risk.rs}`, `crates/moon-core/src/strategy/{mod.rs,faber.rs,dual_momentum.rs,canary.rs}`
- Modify: `crates/moon-core/src/lib.rs`

**Interfaces:**
- Consumes: Task 2 (`AssetId`, `Universe`, `AssetSpec`, `Bar`, `PriceSeries`, `monthly_closes`, `TargetWeights`, `CoreError`, `testutil`).
- Produces:

```rust
// indicators.rs
pub fn sma(values: &[f64], n: usize) -> Option<f64>;                             // mean of last n; None if n == 0 || len < n
pub fn trailing_return(monthly: &[(NaiveDate, f64)], months: usize) -> Option<f64>; // last / value `months` earlier - 1; needs len > months
pub fn momentum_13612w(monthly: &[(NaiveDate, f64)]) -> Option<f64>;            // (12*r1 + 4*r3 + 2*r6 + r12) / 4; needs 13 points
pub fn annualized_volatility(bars: &[Bar], lookback: usize) -> Option<f64>;     // sample stdev of last `lookback` simple adj_close returns * sqrt(252); None if lookback < 2 or bars.len() < lookback + 1

// strategy/mod.rs
pub struct SignalContext<'a> { /* as_of, universe, series */ }
impl<'a> SignalContext<'a> {
    pub fn new(as_of: NaiveDate, universe: &'a Universe, series: &'a BTreeMap<AssetId, PriceSeries>) -> Self;
    pub fn as_of(&self) -> NaiveDate;
    pub fn universe(&self) -> &'a Universe;
    pub fn daily(&self, id: &AssetId) -> &'a [Bar];                 // series.up_to(as_of); empty if absent
    pub fn monthly(&self, id: &AssetId) -> Vec<(NaiveDate, f64)>;   // monthly_closes(daily(id))
    pub fn refuge_return(&self, months: usize) -> f64;             // CASH => 0.0; missing data => 0.0
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Sizing { Equal, InverseVol { lookback_days: usize } }
pub trait Strategy: Send + Sync {
    fn name(&self) -> &'static str;
    fn target(&self, ctx: &SignalContext<'_>) -> TargetWeights;      // risky + refuge, total 1
}
pub fn size_selection(ctx: &SignalContext<'_>, selected: &[AssetId], budget: f64, sizing: Sizing) -> TargetWeights;
// strategy/faber.rs
pub struct FaberSma { pub months: usize, pub sizing: Sizing }          // name "faber_sma"
// strategy/dual_momentum.rs
pub struct DualMomentum { pub lookback_months: usize, pub top_n: usize, pub sizing: Sizing } // name "dual_momentum"
// strategy/canary.rs
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanaryReport { pub bad: usize, pub total: usize, pub defensive_fraction: f64 }
pub struct CanaryOverlay { pub threshold: usize }
impl CanaryOverlay { pub fn apply(&self, ctx: &SignalContext<'_>, weights: TargetWeights) -> (TargetWeights, CanaryReport); }
pub fn combine(targets: &[TargetWeights]) -> TargetWeights;              // equal-weight average; empty input => empty

// risk.rs (this task creates it; Task 4 extends it)
#[derive(Debug, thiserror::Error, Clone, PartialEq)]
pub enum RiskViolation {
    #[error("activo fuera del universo: {0}")] UnknownAsset(AssetId),
    #[error("activo no operable: {0}")] NonTradableAsset(AssetId),
    #[error("pesos inválidos: {0}")] InvalidWeights(String),
    #[error("{asset} supera el tope: {weight:.4} > {cap:.4}")] WeightAboveCap { asset: AssetId, weight: f64, cap: f64 },
    #[error("orden de {asset} demasiado grande: {value} > {max}")] OrderTooLarge { asset: AssetId, value: Decimal, max: Decimal },
    #[error("demasiadas órdenes hoy: {count} > {max}")] TooManyOrders { count: u32, max: u32 },
    #[error("una salida de emergencia solo puede vender: {0}")] EmergencyBuy(AssetId),
}
pub fn apply_weight_cap(weights: &TargetWeights, refuge: &AssetId, max_weight: f64) -> TargetWeights;
pub fn check_targets(weights: &TargetWeights, universe: &Universe, max_weight: f64) -> Result<(), RiskViolation>;

// allocator.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizingKind { #[default] Equal, InverseVol }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StrategyConfig { FaberSma { months: usize }, DualMomentum { lookback_months: usize, top_n: usize } }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocationConfig {
    pub strategies: Vec<StrategyConfig>,
    #[serde(default)] pub sizing: SizingKind,
    #[serde(default = "default_vol_lookback")] pub vol_lookback_days: usize,   // 63
    #[serde(default)] pub canary_threshold: Option<usize>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyOutput { pub name: String, pub weights: TargetWeights }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AllocationDecision { pub as_of: NaiveDate, pub per_strategy: Vec<StrategyOutput>, pub combined: TargetWeights,
                                pub canary: Option<CanaryReport>, pub final_weights: TargetWeights }
impl AllocationDecision { pub fn risky_weight(&self, universe: &Universe) -> f64; }
pub struct Allocator { /* strategies, canary, max_weight */ }
impl Allocator {
    pub fn new(cfg: &AllocationConfig, max_weight: f64) -> Result<Self, CoreError>;
    pub fn warmup_months(&self) -> usize;   // max(faber months, dm lookback, 12 if canary) + 1
    pub fn allocate(&self, ctx: &SignalContext<'_>) -> AllocationDecision;
}
```

Strategy rules (not derivable from signatures):
- `size_selection`: `Equal` → each selected gets `budget / selected.len()`; `InverseVol` → weights ∝ `1/vol` (vol from `annualized_volatility(ctx.daily(id), lookback_days)`) scaled to `budget`; if any selected vol is `None` or 0, fall back to `Equal`. Empty selection → empty weights.
- `FaberSma`: N = number of risky assets. Asset passes if it has ≥ `months` monthly closes and its last monthly close is **strictly greater** than `sma(last months closes)`. Selected = passing assets; risky budget = |selected| / N via `size_selection`; refuge gets `1 - Σ risky`.
- `DualMomentum`: candidates = risky assets with `trailing_return(monthly, lookback)` = `Some`; sort by return desc, ties by `AssetId` asc; take the first `min(top_n, candidates)`; a pick passes if its return is **strictly greater** than `ctx.refuge_return(lookback)`; budget for passing = |passing| / `top_n`; refuge gets the rest.
- `CanaryOverlay`: for each canary, `momentum_13612w`; `None` → not counted; negative → bad. `f = min(bad, threshold) / threshold` (0 if total == 0). Every risky weight × (1 − f); refuge += removed amount.
- `Allocator::allocate`: run each strategy → `combine` → canary overlay if configured and universe has canaries → `apply_weight_cap(max_weight)` → `final_weights`. `Allocator::new` errors (`InvalidConfig`) when there are no strategies, any `months`/`lookback_months`/`top_n` is 0, `canary_threshold == Some(0)`, or `max_weight` not in `(0, 1]`.
- `apply_weight_cap`: every non-refuge weight above `max_weight` is cut to `max_weight` and the excess added to the refuge.
- `check_targets`: every id must be in the universe and be `Risky` or the refuge (canary/benchmark ids → `NonTradableAsset`); `is_valid()` must hold; every non-refuge weight ≤ `max_weight + 1e-9`.

- [ ] **Step 1: Write failing tests** (use `testutil`):
  - `sma_and_returns`: `sma(&[1.,2.,3.,4.],2) == Some(3.5)`; monthly closes `[100,110,121]` → `trailing_return(..,2) == Some(0.21)` (±1e-12); `trailing_return(..,3) == None`.
  - `momentum_13612w_known_value`: 13 monthly closes all growing 1% per month → value equals `(12*r1+4*r3+2*r6+r12)/4` computed in the test.
  - `volatility_of_constant_series_is_zero`, `volatility_none_without_history`.
  - `faber_above_sma_gets_share`: 2 risky (A rising 11 months, B falling 11 months), CASH refuge → `A == 0.5`, `B == 0.0`, `CASH == 0.5`.
  - `faber_insufficient_history_goes_to_refuge`: 5 months only → all CASH.
  - `dual_momentum_picks_top_n_and_filters_absolute`: 3 risky with 12m returns +30%, +10%, −5%, `top_n = 2`, CASH refuge → top two get 0.5 each; with `top_n = 3` the −5% share (1/3) goes to CASH.
  - `dual_momentum_compares_against_refuge_asset`: refuge asset with +20% 12m return; risky at +10% → all refuge.
  - `inverse_vol_favors_low_vol`: two selected assets, vol 10% vs 20% → weights 2/3 and 1/3 of budget (±1e-9).
  - `canary_half_defensive`: 2 canaries, 1 negative, threshold 2 → risky weights halved, report `{bad:1,total:2,defensive_fraction:0.5}`.
  - `no_look_ahead`: decision as of 2024-06-28 is identical after appending bars dated after it with prices ×10.
  - `cap_moves_excess_to_refuge`: `{A:0.5, CASH:0.5}`, cap 0.2 → `{A:0.2, CASH:0.8}`.
  - `check_targets_rejects_unknown_and_canary`, `allocator_output_is_valid` (end-to-end weights `is_valid()` and pass `check_targets`).
  - `allocation_config_parses_from_toml` (two strategies, `sizing = "inverse_vol"`, `canary_threshold = 2`).
- [ ] **Step 2: Run** `cargo test -p moon-core` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p moon-core` → PASS; global checks.
- [ ] **Step 5: Commit and push** `feat(core): indicators, Faber and dual momentum strategies, canary overlay, allocator`.

---

### Task 4: Fees, rebalancing, risk limits, drawdown breaker and NAV

**Files:**
- Create: `crates/moon-core/src/{fees.rs,rebalance.rs,nav.rs}`
- Modify: `crates/moon-core/src/risk.rs`, `crates/moon-core/src/lib.rs`

**Interfaces:**
- Consumes: Tasks 2–3.
- Produces:

```rust
// fees.rs
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeeModel { pub commission_rate: Decimal, pub market_fee_rate: Decimal, pub vat_rate: Decimal,
                      #[serde(default)] pub min_commission: Decimal }       // rates are fractions: 0.006 = 0.6 %
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeeBreakdown { pub commission: Decimal, pub vat: Decimal, pub market_fee: Decimal, pub total: Decimal }
impl FeeModel { pub fn zero() -> Self; pub fn breakdown(&self, trade_value: Decimal) -> FeeBreakdown; pub fn total(&self, trade_value: Decimal) -> Decimal; }

// rebalance.rs
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RebalanceParams { pub band: f64, pub min_trade_value: Decimal, pub cash_buffer: f64,
                             pub slippage_rate: Decimal, pub force_full: bool }
pub struct RebalanceInput<'a> {
    pub cash: Decimal,
    pub holdings: &'a BTreeMap<AssetId, Decimal>,
    pub prices: &'a BTreeMap<AssetId, Decimal>,
    pub lot_sizes: &'a BTreeMap<AssetId, u32>,       // missing => 1
    pub targets: &'a TargetWeights,                  // may contain CASH, never traded
    pub fees: &'a FeeModel,
    pub params: &'a RebalanceParams,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedOrder { pub asset: AssetId, pub side: Side, pub qty: Decimal, pub ref_price: Decimal,
                          pub exec_price: Decimal, pub est_value: Decimal, pub est_fees: Decimal }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason { WithinBand, BelowMinTrade, InsufficientCash, LotRounding }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkippedTrade { pub asset: AssetId, pub side: Side, pub reason: SkipReason }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RebalancePlan { pub equity: Decimal, pub orders: Vec<PlannedOrder>, pub skipped: Vec<SkippedTrade>, pub est_cash_after: Decimal }
impl RebalancePlan { pub fn sells(&self) -> Vec<&PlannedOrder>; pub fn buys(&self) -> Vec<&PlannedOrder>; pub fn is_empty(&self) -> bool; }
pub fn plan_rebalance(input: &RebalanceInput<'_>) -> Result<RebalancePlan, CoreError>;

// risk.rs (additions)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskLimits { pub max_weight: f64, pub max_drawdown: f64, pub rebalance_band: f64,
                        pub min_trade_usd: Decimal, pub max_order_usd: Decimal, pub max_orders_per_day: u32,
                        pub cash_buffer: f64, #[serde(default)] pub max_equity_usd: Option<Decimal> }
impl RiskLimits { pub fn validate(&self) -> Result<(), CoreError>; }
// ranges: 0 < max_weight <= 1; 0 < max_drawdown < 1; 0 <= rebalance_band < 0.5; 0 <= cash_buffer <= 0.2;
// min_trade_usd >= 0; max_order_usd > 0; max_orders_per_day >= 1; max_equity_usd > 0 if set
pub fn check_orders(orders: &[PlannedOrder], max_order_value: Decimal, max_orders_per_day: u32,
                    already_today: u32, emergency: bool) -> Result<(), RiskViolation>;
// emergency => only verifies every order is a Sell (EmergencyBuy otherwise); no size/count limits.
// normal => each est_value <= max_order_value; already_today + orders.len() <= max_orders_per_day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DrawdownState { pub peak_nav: f64, pub tripped: bool, pub tripped_on: Option<NaiveDate> }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawdownSignal { Normal, Tripped, StillTripped }
impl DrawdownState {
    pub fn observe(&mut self, nav: f64, today: NaiveDate, max_drawdown: f64) -> DrawdownSignal;
    pub fn drawdown(&self, nav: f64) -> f64;     // 0 if peak <= 0, else max(0, 1 - nav/peak)
    pub fn reset(&mut self, nav: f64);           // monthly decision: peak = nav, tripped = false, tripped_on = None
}

// nav.rs
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct NavTracker { pub units: Decimal }
impl NavTracker {
    pub fn start(initial_equity: Decimal) -> Self;                 // units = max(initial_equity, 0)  (NAV starts at 1.0)
    pub fn nav(&self, equity: Decimal) -> Option<f64>;             // equity / units; None if units <= 0
    pub fn apply_flow(&mut self, flow: Decimal, equity_before_flow: Decimal);
}
```

Exact rules:
- `FeeModel::breakdown(v)`: if `v <= 0` all zero. `commission = round_money(max(v*commission_rate, min_commission))`; `vat = round_money(commission*vat_rate)`; `market_fee = round_money(v*market_fee_rate)`; `total = commission + vat + market_fee`.
- `DrawdownState::observe`: if `tripped` → `StillTripped` (peak unchanged). Else `peak = max(peak, nav)`; if `drawdown(nav) >= max_drawdown` → set `tripped`, `tripped_on = today`, return `Tripped`; else `Normal`.
- `NavTracker::apply_flow(flow, eb)`: if `units <= 0` or `eb <= 0` → `units = max(eb + flow, 0)`; else `units = max(units + flow * units / eb, 0)`.
- `plan_rebalance` algorithm (`U` = assets with holding > 0 or target > 0, excluding CASH):
  1. Error `MissingPrice` if any asset in `U` lacks a price or has price ≤ 0; `InvalidConfig` if `cash < 0`, `band ∉ [0,0.5)`, `cash_buffer ∉ [0,0.2]`.
  2. `equity = cash + Σ qty·price`; if `equity <= 0` return an empty plan with `est_cash_after = cash`.
  3. `investable = equity · (1 − cash_buffer)`; per asset `cur = qty·price`, `tgt = investable·w`, `dev = cur/equity − w`.
  4. `deploy = cash/equity > cash_buffer + band`.
  5. Trade the asset if `force_full` or (`w == 0` and qty > 0) or (qty == 0 and `w > 0`) or `|dev| > band` or (`deploy` and `tgt > cur`); otherwise, if `tgt != cur`, skip with `WithinBand`.
  6. Sells (`tgt < cur`): `exec = round_price(price·(1 − slippage))`; `qty = held` if `w == 0`, else `min(held, floor_to_lot((cur − tgt)/price, lot))`; 0 → `LotRounding`; `value = round_money(qty·exec)`; `value < min_trade_value` and `w != 0` → `BelowMinTrade`; `fees = total(value)`; proceeds `+= value − fees`.
  7. `budget = cash + proceeds`.
  8. Buys (`tgt > cur`): `exec = round_price(price·(1 + slippage))`; `qty = floor_to_lot((tgt − cur)/exec, lot)`; 0 → `LotRounding`; `value = round_money(qty·exec)`; `< min_trade_value` → `BelowMinTrade`; `cost = value + total(value)`.
  9. If `Σcost > budget`: multiply every buy qty by `budget/Σcost`, re-floor to lots, recompute; drop buys that became 0 or below minimum (`InsufficientCash`); then, while `Σcost > budget`, drop the buy with the smallest value (`InsufficientCash`).
  10. `est_cash_after = budget − Σcost` (never negative; return `InvalidConfig` if it would be).
  11. `orders` = sells sorted by asset id, then buys sorted by asset id. `est_value`/`est_fees` are the values above.

- [ ] **Step 1: Write failing tests:**
  - `fees_breakdown_example`: rates 0.006 / 0.0008 / 0.21, min 0, value 10000 → commission 60.00, vat 12.60, market 8.00, total 80.60.
  - `fees_min_commission`: min 100, value 1000 → 100, 21.00, 0.80, total 121.80; value 0 → total 0.
  - `initial_deployment_exact`: cash 10000, no holdings, prices SPY 100 / QQQ 50, targets 0.5/0.5, buffer 0.01, band 0.05, zero fees and slippage → buys SPY 49 and QQQ 99, `est_cash_after == 150`.
  - `within_band_no_orders`: holdings already at target ±2 % → no orders, skipped `WithinBand`.
  - `full_exit_when_weight_zero`: holding 10 units, weight 0 → sell exactly 10 even if below min trade.
  - `sells_listed_before_buys`, `missing_price_is_error`, `lot_size_respected` (lot 5 → qty multiple of 5).
  - `deposit_is_deployed`: holdings at target plus extra cash = 20 % of equity, band 0.05 → buys only.
  - `insufficient_cash_scales_buys`: min commission large enough to break naive buys → `est_cash_after >= 0`, some skipped `InsufficientCash`.
  - `emergency_force_full_sells_all_risky`: targets all refuge asset, `force_full` → all risky sold, refuge bought.
  - proptest `plan_invariants`: cash 0..1e6, 1–4 assets, prices 1..1000, holdings 0..500, random weights normalized (sometimes CASH), lot ∈ {1,5,10}, band 0..0.1, buffer 0..0.05, slippage 0..0.01, fee rates 0..0.01, min commission 0..50 → `est_cash_after >= 0`; `Σbuy(est_value+est_fees) <= cash + Σsell(est_value−est_fees)`; each sell qty ≤ holding; every qty > 0 and a multiple of its lot; no order for an asset with no holding and zero target.
  - `check_orders_limits` (too large, too many, emergency buy rejected, emergency large sell accepted), `risk_limits_validate_ranges`.
  - `drawdown_trips_once`: peak 1.0, nav 0.84 with max 0.15 → `Tripped`; again → `StillTripped`; after `reset(0.84)` → `Normal`.
  - `nav_unchanged_by_flows`: start 1000; equity grows to 1100 (nav 1.1); deposit 550 at equity 1100 → nav at 1650 is 1.1; withdrawal 1650 → units 0, nav None.
- [ ] **Step 2: Run** `cargo test -p moon-core` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p moon-core` → PASS; global checks.
- [ ] **Step 5: Commit and push** `feat(core): fee model, rebalancer, risk limits, drawdown breaker and NAV tracker`.

---

### Task 5: Backtest engine and metrics

**Files:**
- Create: `crates/moon-core/src/{metrics.rs,backtest.rs}`
- Modify: `crates/moon-core/src/lib.rs`

**Interfaces:**
- Consumes: Tasks 2–4.
- Produces:

```rust
// metrics.rs
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Metrics { pub start: NaiveDate, pub end: NaiveDate, pub years: f64, pub total_return: f64, pub cagr: f64,
                     pub volatility: f64, pub sharpe: f64, pub max_drawdown: f64, pub calmar: f64,
                     pub best_month: f64, pub worst_month: f64, pub positive_months: f64, pub turnover: f64,
                     pub total_fees: Decimal, pub trades: usize, pub time_in_refuge: f64,
                     pub final_equity: Decimal, pub net_contributions: Decimal }
pub fn nav_metrics(navs: &[(NaiveDate, f64)], risk_free_rate: f64) -> Metrics;   // trading fields zero
pub fn compute_metrics(points: &[EquityPoint], trades: &[Trade], risk_free_rate: f64) -> Metrics;

// backtest.rs
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestConfig { pub start: NaiveDate, #[serde(default)] pub end: Option<NaiveDate>,
                            pub initial_capital: Decimal, #[serde(default)] pub monthly_contribution: Decimal,
                            pub slippage_rate: Decimal, pub fees: FeeModel,
                            #[serde(default)] pub holdout_start: Option<NaiveDate>, #[serde(default)] pub risk_free_rate: f64 }
pub struct BacktestInput<'a> { pub config: &'a BacktestConfig, pub risk: &'a RiskLimits, pub universe: &'a Universe,
                               pub allocator: &'a Allocator, pub series: &'a BTreeMap<AssetId, PriceSeries>,
                               pub include_holdout: bool }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EquityPoint { pub date: NaiveDate, pub equity: Decimal, pub cash: Decimal, pub flow: Decimal,
                         pub nav: f64, pub drawdown: f64, pub refuge_weight: f64 }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade { pub date: NaiveDate, pub asset: AssetId, pub side: Side, pub qty: Decimal, pub price: Decimal,
                   pub value: Decimal, pub fees: Decimal, pub reason: OrderReason }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestResult { pub start: NaiveDate, pub end: NaiveDate, pub equity_curve: Vec<EquityPoint>,
                            pub trades: Vec<Trade>, pub decisions: Vec<AllocationDecision>,
                            pub emergency_exits: Vec<NaiveDate>, pub metrics: Metrics, pub benchmark: Option<Metrics> }
pub fn run_backtest(input: &BacktestInput<'_>) -> Result<BacktestResult, CoreError>;
```

Metric formulas: daily returns `r_t = nav_t/nav_{t−1} − 1`; `years = days(start,end)/365.25`; `total_return = nav_end/nav_start − 1`; `cagr = (nav_end/nav_start)^(1/years) − 1` (0 if `years <= 0`); `volatility = stdev_sample(r)·√252`; `sharpe = (mean(r) − rf_d)/stdev(r)·√252` with `rf_d = (1+rf)^(1/252) − 1`, 0 if stdev = 0; `max_drawdown = max_t(1 − nav_t / max_{s≤t} nav_s)`; `calmar = cagr/max_drawdown` (0 if mdd = 0); monthly returns from the last nav of each month relative to the previous month's last nav (first month relative to the first point); `positive_months` = share > 0; `turnover = (Σ|trade value| / 2) / mean(equity) / years`; `total_fees = Σ fees`; `time_in_refuge = mean(refuge_weight)`; `final_equity` = last equity; `net_contributions = initial equity + Σ flow`. Fewer than 2 points → zeros (never panic).

Backtest loop:
- Unit price of a tradable asset on day `d`: `dec_from_f64(adj_open or adj_close / cedear_ratio)` rounded to 6 dp; if the asset has no bar on `d`, use the last earlier bar's `adj_close`. CASH price is 1 and is never traded (cash is the refuge).
- Lots from `exec.lot_size`. `RebalanceParams { band: risk.rebalance_band, min_trade_value: risk.min_trade_usd, cash_buffer: risk.cash_buffer, slippage_rate: config.slippage_rate, force_full }`.
- `eff_start`: the first date in the calendar `>= config.start` such that every risky, refuge (non-CASH) and canary series has at least `allocator.warmup_months()` monthly closes dated before it; error `MissingData` if none. `eff_end = min(config.end or last calendar date, holdout_start − 1 day unless include_holdout)`; error `MissingData` if `eff_end <= eff_start`.
- Calendar = sorted union of bar dates of all tradable non-CASH series within `[eff_start, eff_end]`.
- State: `cash = initial_capital`, holdings empty, `NavTracker::start(initial_capital)`, `DrawdownState::default()`.
- For each date: (a) if it is the first calendar date of a month (including `eff_start`): if not the first date, add `monthly_contribution` (`apply_flow(flow, equity at previous close)`, `cash += flow`, record `flow`); `as_of` = previous calendar date (for `eff_start`: the day before); `decision = allocator.allocate(SignalContext::new(as_of, …))`; `drawdown.reset(nav at previous close or 1.0)`; rebalance at the open toward `decision.final_weights` with `force_full = false`, `OrderReason::Monthly`; clear any pending emergency. (b) else if an emergency is pending: rebalance at the open to `TargetWeights::all_in(refuge id)` with `force_full = true`, `OrderReason::Emergency`; record the date in `emergency_exits`. (c) Fill each planned order at its `exec_price` with `est_fees`; update cash and holdings; record `Trade`s. (d) At the close: value holdings at adjusted close, compute nav, call `drawdown.observe`; `Tripped` → emergency pending for the next date. Push the `EquityPoint` (refuge_weight = refuge value / equity; CASH refuge → cash / equity).
- Benchmark (if the universe has one and its series exists): navs = benchmark `adj_close` on calendar dates (forward-filled) divided by the first one → `nav_metrics`.

- [ ] **Step 1: Write failing tests** (synthetic data via `testutil`, `FeeModel::zero()` unless stated):
  - `metrics_known_drawdown`: navs `[1.0, 1.2, 0.9, 1.1]` → `max_drawdown == 0.25`; constant navs → `sharpe == 0`, `volatility == 0`.
  - `metrics_cagr_one_year`: navs from 1.0 to 1.1 over 365.25 days → `cagr ≈ 0.10`.
  - `buy_and_hold_like_when_always_risky`: one risky always rising, CASH refuge, Faber 10m, cap 1.0, band 0 → after the first month almost all equity is in the risky asset; final equity within 1 % of buy and hold.
  - `decisions_never_use_future_data`: every `decision.as_of` < date of its first trade.
  - `fees_reduce_equity`: same run with fees 0.6 % ends with lower equity and `total_fees > 0`.
  - `contributions_do_not_change_nav`: flat prices, monthly contribution 100 → nav stays 1.0 (±1e-9) while equity grows.
  - `crash_triggers_emergency_exit`: risky series drops 30 % mid-month with `max_drawdown 0.15` → `emergency_exits` not empty; the next trades are `Emergency` sells; the following month re-evaluates normally.
  - `holdout_excluded_by_default`: `holdout_start` set → `result.end < holdout_start`; with `include_holdout` the end passes it.
  - `insufficient_history_errors`: only 3 months of data → `Err(MissingData)`.
- [ ] **Step 2: Run** → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; global checks.
- [ ] **Step 5: Commit and push** `feat(core): backtest engine and performance metrics`.

---

### Task 6: moon-store (SQLite)

**Files:**
- Modify: `crates/moon-store/Cargo.toml` (deps: moon-core, rusqlite, serde, serde_json, chrono, rust_decimal, uuid, thiserror; dev: tempfile, rust_decimal_macros, moon-core with feature `test-util`)
- Create: `crates/moon-store/src/{lib.rs,schema.rs,records.rs,queries.rs}`

**Interfaces:**
- Consumes: Task 2 enums (`Side`, `ExecMode`, `OrderReason`, `OrderStatus`, `DataSource`), `AssetId`, `Market`, `Currency`, `Bar`.
- Produces:

```rust
#[derive(Debug, thiserror::Error)]
pub enum StoreError { #[error("sqlite: {0}")] Sqlite(#[from] rusqlite::Error),
                      #[error("json: {0}")] Json(#[from] serde_json::Error),
                      #[error("dato corrupto en la base: {0}")] Corrupt(String) }
pub struct Store { /* rusqlite::Connection */ }

// records.rs — all derive Debug, Clone, PartialEq, Serialize, Deserialize
pub struct OrderRecord { pub id: Uuid, pub batch_id: Uuid, pub created_at: DateTime<Utc>, pub updated_at: DateTime<Utc>,
    pub mode: ExecMode, pub reason: OrderReason, pub asset: AssetId, pub symbol: String, pub market: Market,
    pub side: Side, pub qty: Decimal, pub limit_price: Option<Decimal>, pub currency: Currency, pub status: OrderStatus,
    pub broker_order_id: Option<String>, pub filled_qty: Decimal, pub avg_fill_price: Option<Decimal>,
    pub fees: Decimal, pub fees_estimated: bool, pub error: Option<String> }
pub struct Snapshot { pub at: DateTime<Utc>, pub mode: ExecMode, pub cash_ars: Decimal, pub equity_ars: Decimal,
    pub ccl: Decimal, pub equity_usd: Decimal, pub nav: f64, pub units: Decimal, pub drawdown: f64,
    pub positions: BTreeMap<String, Decimal> }
pub struct FxRecord { pub at: DateTime<Utc>, pub rate: Decimal, pub method: String, pub components: serde_json::Value }
#[serde(rename_all = "snake_case")] pub enum ProposalKind { Monthly, Deposit }
#[serde(rename_all = "snake_case")] pub enum ProposalStatus { Pending, Approved, Rejected, Expired, Executed, Failed }
pub struct Proposal { pub id: Uuid, pub created_at: DateTime<Utc>, pub expires_at: DateTime<Utc>, pub kind: ProposalKind,
    pub status: ProposalStatus, pub decided_at: Option<DateTime<Utc>>, pub payload: serde_json::Value,
    pub telegram_message_id: Option<i64> }
#[derive(Default)] pub struct PaperAccount { pub cash_ars: Decimal, pub positions: BTreeMap<String, Decimal> }
#[serde(rename_all = "snake_case")] pub enum FlowKind { Deposit, Withdrawal }
pub struct FlowRecord { pub at: DateTime<Utc>, pub mode: ExecMode, pub kind: FlowKind, pub amount_ars: Decimal,
    pub external_id: Option<String>, pub note: String }
pub struct DecisionRecord { pub id: i64, pub at: DateTime<Utc>, pub kind: String, pub payload: serde_json::Value }
pub struct BacktestRun { pub id: Option<i64>, pub created_at: DateTime<Utc>, pub config_hash: String,
    pub config: serde_json::Value, pub start: NaiveDate, pub end: NaiveDate, pub include_holdout: bool,
    pub metrics: serde_json::Value }

impl Store {
    pub fn open(path: &Path) -> Result<Store, StoreError>;      // creates file, PRAGMA journal_mode=WAL, foreign_keys=ON, busy_timeout=5000, migrate
    pub fn open_in_memory() -> Result<Store, StoreError>;
    pub fn schema_version(&self) -> Result<u32, StoreError>;
    pub fn upsert_bars(&mut self, symbol: &str, source: DataSource, bars: &[Bar]) -> Result<usize, StoreError>; // one transaction; replaces same (symbol, source, date)
    pub fn load_bars(&self, symbol: &str, source: DataSource) -> Result<Vec<Bar>, StoreError>;                 // ascending
    pub fn last_bar_date(&self, symbol: &str, source: DataSource) -> Result<Option<NaiveDate>, StoreError>;
    pub fn insert_fx(&mut self, rec: &FxRecord) -> Result<(), StoreError>;
    pub fn latest_fx(&self) -> Result<Option<FxRecord>, StoreError>;
    pub fn get_state<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>, StoreError>;
    pub fn set_state<T: serde::Serialize>(&mut self, key: &str, value: &T) -> Result<(), StoreError>;
    pub fn delete_state(&mut self, key: &str) -> Result<(), StoreError>;
    pub fn insert_snapshot(&mut self, s: &Snapshot) -> Result<(), StoreError>;
    pub fn latest_snapshot(&self, mode: ExecMode) -> Result<Option<Snapshot>, StoreError>;
    pub fn snapshots_between(&self, mode: ExecMode, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<Snapshot>, StoreError>;
    pub fn insert_order(&mut self, o: &OrderRecord) -> Result<bool, StoreError>;   // INSERT OR IGNORE; false if id existed
    pub fn update_order(&mut self, o: &OrderRecord) -> Result<(), StoreError>;
    pub fn get_order(&self, id: Uuid) -> Result<Option<OrderRecord>, StoreError>;
    pub fn open_orders(&self, mode: ExecMode) -> Result<Vec<OrderRecord>, StoreError>;  // status not terminal
    pub fn orders_between(&self, mode: ExecMode, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<OrderRecord>, StoreError>; // by created_at
    pub fn paper_account(&self) -> Result<PaperAccount, StoreError>;                  // default if none saved
    pub fn save_paper_account(&mut self, a: &PaperAccount) -> Result<(), StoreError>;
    pub fn insert_proposal(&mut self, p: &Proposal) -> Result<(), StoreError>;
    pub fn get_proposal(&self, id: Uuid) -> Result<Option<Proposal>, StoreError>;
    pub fn set_proposal_status(&mut self, id: Uuid, status: ProposalStatus, at: DateTime<Utc>) -> Result<(), StoreError>;
    pub fn set_proposal_message(&mut self, id: Uuid, message_id: i64) -> Result<(), StoreError>;
    pub fn pending_proposals(&self) -> Result<Vec<Proposal>, StoreError>;
    pub fn insert_flow(&mut self, f: &FlowRecord) -> Result<bool, StoreError>;      // false if external_id already stored
    pub fn flows_between(&self, mode: ExecMode, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Vec<FlowRecord>, StoreError>;
    pub fn log_decision(&mut self, at: DateTime<Utc>, kind: &str, payload: &serde_json::Value) -> Result<i64, StoreError>;
    pub fn recent_decisions(&self, limit: usize) -> Result<Vec<DecisionRecord>, StoreError>;  // newest first
    pub fn register_backtest(&mut self, run: &BacktestRun) -> Result<i64, StoreError>;
    pub fn backtest_runs(&self) -> Result<Vec<BacktestRun>, StoreError>;                    // newest first
    pub fn count_distinct_backtest_configs(&self) -> Result<u64, StoreError>;
}
```

Storage rules: migrations in `schema.rs` as `const MIGRATIONS: &[&str]`, applied in order inside a transaction while `PRAGMA user_version` < index+1. Version 1 creates tables `bars(symbol, source, date, open, high, low, close, adj_close, volume, PRIMARY KEY(symbol, source, date))`, `fx_rates`, `state_kv(key PRIMARY KEY, value TEXT JSON)`, `snapshots`, `orders(id PRIMARY KEY, …)` + index on `(mode, status)`, `paper_account(id INTEGER PRIMARY KEY CHECK(id = 1), cash_ars TEXT, positions TEXT JSON)`, `proposals`, `flows(… external_id TEXT UNIQUE)`, `decisions`, `backtest_runs`. Decimals are TEXT (`to_string` / `Decimal::from_str`), timestamps RFC 3339 TEXT, dates `YYYY-MM-DD` TEXT, enums via `as_str`/`FromStr`, UUIDs as hyphenated TEXT. A value that fails to parse → `StoreError::Corrupt`.

- [ ] **Step 1: Write failing tests** (`tests` module or `crates/moon-store/tests/store.rs`):
  - `migrations_are_idempotent`: open the same tempfile twice → `schema_version() == 1` both times.
  - `bars_upsert_replaces_same_date`, `bars_load_sorted`, `last_bar_date`.
  - `decimal_round_trip_exact`: snapshot with `cash_ars = dec!(0.1) + dec!(0.2)` reads back exactly `dec!(0.3)`; `qty = dec!(123456789.123456789)` exact.
  - `order_insert_is_idempotent` (second insert returns false), `open_orders_excludes_terminal`, `update_order_persists_fill`.
  - `proposal_lifecycle` (pending → approved → executed; `pending_proposals` empty after).
  - `flows_dedupe_by_external_id`, `state_generic_round_trip` (store a `BTreeMap<String, i64>` and a `bool`).
  - `paper_account_default_then_saved`, `backtest_registry_counts_distinct_hashes` (3 runs, 2 hashes → 2).
- [ ] **Step 2: Run** `cargo test -p moon-store` → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS (first build compiles bundled SQLite; may take a while); global checks.
- [ ] **Step 5: Commit and push** `feat(store): SQLite persistence with migrations`.

---

### Task 7: moon-yahoo (historical data client)

**Files:**
- Modify: `crates/moon-yahoo/Cargo.toml` (deps: moon-core, reqwest, serde, serde_json, chrono, thiserror, tokio, tracing; dev: wiremock, tokio)
- Create: `crates/moon-yahoo/src/lib.rs`, `crates/moon-yahoo/tests/fixtures/chart_spy.json`, `crates/moon-yahoo/tests/fixtures/chart_error.json`, `crates/moon-yahoo/tests/yahoo.rs`

**Interfaces:**
- Consumes: `moon_core::bars::Bar`.
- Produces:

```rust
#[derive(Debug, thiserror::Error)]
pub enum YahooError { #[error("http: {0}")] Http(#[from] reqwest::Error), #[error("estado HTTP {0}")] Status(u16),
                      #[error("Yahoo respondió error {code}: {description}")] Api { code: String, description: String },
                      #[error("respuesta inválida: {0}")] Parse(String), #[error("sin datos para {0}")] NoData(String) }
#[derive(Debug, Clone, PartialEq)]
pub struct YahooQuote { pub symbol: String, pub price: f64, pub currency: String, pub at: DateTime<Utc> }
#[derive(Debug, Clone)]
pub struct YahooClient { /* reqwest::Client, base_url, retry_delay, max_attempts = 3 */ }
impl YahooClient {
    pub fn new() -> Self;                                         // base "https://query1.finance.yahoo.com", retry delay 1s
    pub fn with_base_url(base_url: impl Into<String>) -> Self;
    pub fn with_retry_delay(self, delay: std::time::Duration) -> Self;
    pub async fn daily_bars(&self, symbol: &str, from: NaiveDate, to: NaiveDate) -> Result<Vec<Bar>, YahooError>;
    pub async fn latest_quote(&self, symbol: &str) -> Result<YahooQuote, YahooError>;
}
```

Request contract: `GET {base}/v8/finance/chart/{symbol}` (symbol added with `Url::path_segments_mut().push`, so it is percent-encoded); `daily_bars` query: `period1` = `from` 00:00 UTC unix seconds, `period2` = (`to` + 1 day) 00:00 UTC, `interval=1d`, `events=div,splits`, `includeAdjustedClose=true`; `latest_quote` query: `range=5d`, `interval=1d`, and reads `meta.regularMarketPrice`, `meta.currency`, `meta.regularMarketTime`. Header `User-Agent: Mozilla/5.0 (Windows NT 10.0; Win64; x64) moon-trade`. Parse `chart.result[0]`: `timestamp[]`, `indicators.quote[0].{open,high,low,close,volume}[]`, `indicators.adjclose[0].adjclose[]` (missing → use close); date = `timestamp + meta.gmtoffset` seconds as a UTC date; skip indices where any of open/high/low/close is null; `chart.error` non-null → `Api`; empty result → `NoData`. Retry up to 3 attempts total on 429, 5xx and connection/timeout errors, sleeping `retry_delay` between attempts; 404 → `Status(404)` with no retry.

- [ ] **Step 1: Write failing tests** with wiremock (`with_retry_delay(Duration::ZERO)`):
  - `parses_bars_and_skips_nulls`: fixture with 5 timestamps (one null close), gmtoffset −14400 → 4 bars, dates correct, `adj_close` from adjclose.
  - `uses_close_when_adjclose_missing`.
  - `sends_expected_query`: matcher on path `/v8/finance/chart/SPY` and query params `interval=1d`, `period1`, `period2`.
  - `encodes_special_symbols`: symbol `^GSPC` → request path `/v8/finance/chart/%5EGSPC`.
  - `api_error_maps_to_api_variant` (fixture `chart_error.json` with `{"chart":{"result":null,"error":{"code":"Not Found","description":"No data found"}}}`).
  - `retries_on_500_then_succeeds` (first response 500, second 200 → Ok), `no_retry_on_404`.
  - `latest_quote_reads_meta`.
- [ ] **Step 2: Run** `cargo test -p moon-yahoo` → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; global checks.
- [ ] **Step 5: Commit and push** `feat(yahoo): daily bars and quote client`.

---

### Task 8: moon-iol (IOL API client)

**Files:**
- Modify: `crates/moon-iol/Cargo.toml` (deps: moon-core, reqwest, serde, serde_json, chrono, chrono-tz, thiserror, tokio, tracing, uuid; dev: wiremock, tokio)
- Create: `crates/moon-iol/src/{lib.rs,client.rs,auth.rs,dto.rs,error.rs}`, `crates/moon-iol/tests/iol.rs`

**Interfaces:**
- Consumes: `Market`, `Side`, `OrderType`, `Term`, `Bar`, `Clock`, `ART` from moon-core. Field names: `docs/iol-api.md`.
- Produces:

```rust
#[derive(Debug, thiserror::Error)]
pub enum IolError { #[error("http: {0}")] Http(#[from] reqwest::Error),
                    #[error("IOL respondió {status}: {message}")] Api { status: u16, code: Option<String>, message: String },
                    #[error("autenticación IOL: {0}")] Auth(String),
                    #[error("respuesta inválida de IOL: {0}")] Parse(String) }
#[derive(Clone)] pub struct IolCredentials { pub username: String, pub password: String }   // manual Debug: password "***"
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "UPPERCASE")] pub enum Country { Ar, Us }
pub struct IolClient { /* http, base_url, creds, clock: Arc<dyn Clock>, token: tokio::sync::Mutex<Option<TokenState>> */ } // manual Debug without secrets
impl IolClient {
    pub fn new(base_url: impl Into<String>, creds: IolCredentials, clock: Arc<dyn Clock>) -> Self;  // default base "https://api.iolinversiones.com"
    pub async fn bars(&self, symbol: &str, market: Market, from: NaiveDate, to: NaiveDate) -> Result<Vec<IolBar>, IolError>;
    pub async fn quote(&self, symbol: &str, market: Market, term: Term) -> Result<IolQuote, IolError>;
    pub async fn balances(&self, country: Country) -> Result<IolBalances, IolError>;
    pub async fn positions(&self, country: Country) -> Result<IolPositions, IolError>;
    pub async fn place_order(&self, req: &IolOrderRequest, idempotency_key: Uuid) -> Result<IolOrderAck, IolError>;
    pub async fn estimate_order(&self, req: &IolOrderRequest) -> Result<IolEstimate, IolError>;
    pub async fn operations(&self, q: &OperationsQuery) -> Result<Vec<IolOperation>, IolError>;
    pub async fn activities(&self, start: NaiveDate, end: NaiveDate, movement_type: Option<&str>) -> Result<Vec<IolActivity>, IolError>;
    pub async fn cancel_order(&self, order_id: &str) -> Result<IolCancel, IolError>;
}
// dto.rs — Deserialize DTOs are tolerant: every non-essential field is Option or #[serde(default)]; unknown fields ignored.
pub struct IolBar { pub open: f64, pub high: f64, pub low: f64, pub close: f64, #[serde(default)] pub volume: f64, pub timestamp: i64 }
impl IolBar { pub fn to_core_bar(&self) -> Bar; }      // date = timestamp in ART; adj_close = close
pub struct IolPrices { pub current: Option<f64>, pub open: Option<f64>, pub close: Option<f64>, pub high: Option<f64>, pub low: Option<f64>, pub previous_close: Option<f64> }
pub struct IolQuote { pub symbol: String, pub market: Option<String>, pub settlement_term: Option<String>, pub currency: Option<String>,
                      pub prices: IolPrices, pub lot_size: Option<f64>, pub last_updated: Option<String> }
pub struct TermAmounts { pub t0: Option<f64>, pub t1: Option<f64>, pub t2: Option<f64>, pub t3: Option<f64> }
impl TermAmounts { pub fn get(&self, term: Term) -> Option<f64>; }
pub struct IolCurrencyBalance { pub currency: Option<String>, #[serde(default)] pub totals: TermAmounts, #[serde(default)] pub available: TermAmounts,
                                #[serde(default)] pub committed: TermAmounts, pub available_for_withdrawal: Option<f64> }
pub struct IolBalances { pub ars: Option<IolCurrencyBalance>, pub usd: Option<IolCurrencyBalance> }
pub struct IolInstrument { pub symbol: String, #[serde(rename = "type")] pub instrument_type: Option<String>, pub currency: Option<String>,
                           pub exchange: Option<String>, pub lot_size: Option<f64> }
pub struct IolAvailability { pub quantity_available: Option<f64>, pub quantity_held: Option<f64>, pub quantity_pending: Option<f64> }
pub struct IolMarketValue { pub current_price: Option<f64> }
pub struct IolPosition { pub quantity: f64, pub instrument: IolInstrument, pub availability: Option<IolAvailability>, pub market_value: Option<IolMarketValue> }
pub struct IolPositions { #[serde(default)] pub positions: Vec<IolPosition>, pub total_market_value: Option<f64> }
#[derive(Serialize)]
pub struct IolOrderRequest { pub symbol: String, pub market_id: Market, pub side: Side, pub order_type: OrderType,
                             pub due_date: NaiveDate,                       // serialized "YYYY-MM-DD"
                             #[serde(skip_serializing_if = "Option::is_none")] pub limit_price: Option<f64>,
                             pub quantity: f64,
                             #[serde(skip_serializing_if = "Option::is_none")] pub settlement_term: Option<Term> }
pub struct IolMessage { pub code: Option<String>, pub message: String }
pub struct IolOrderAck { pub order_id: String, pub successful_purchase: bool, #[serde(default)] pub messages: Vec<IolMessage> }
pub struct IolMoney { pub currency: Option<String>, pub value: f64 }
pub struct IolRegulatoryFees { pub market_fee: Option<IolMoney>, pub vat: Option<IolMoney> }
pub struct IolEstimate { pub amount: Option<IolMoney>, pub commission: Option<IolMoney>, pub regulatory_fees: Option<IolRegulatoryFees> }
#[derive(Debug, Clone, Default)]
pub struct OperationsQuery { pub start_date: NaiveDate, pub end_date: NaiveDate, pub country: Option<String>, pub symbol: Option<String>, pub state: Option<String> }
pub struct IolOpAmounts { pub qty: Option<f64>, pub price: Option<f64>, pub amount: Option<f64> }
pub struct IolOperation { pub order_id: String, pub status: String, pub symbol: Option<String>, pub order_date: Option<String>,
                          pub currency: Option<String>, pub order: Option<IolOpAmounts>, pub executed: Option<IolOpAmounts> }
pub struct IolFees { pub commission: Option<f64>, pub vat: Option<f64>, pub other: Option<f64> }
pub struct IolActivity { pub id: String, pub activity_type: Option<String>, pub trade_date: Option<String>, pub net_amount: Option<f64>,
                         pub currency: Option<String>, pub status: Option<String>, pub fees: Option<IolFees> }
pub struct IolCancel { pub order_id: String, pub status: Option<String>, #[serde(default)] pub cancelled: bool }
```

Behavior: every response is the envelope `{meta, data, error}` (`docs/iol-api.md`); non-2xx or non-null `error` → `IolError::Api` with message from `error.message` (or the HTTP status text). Token: `POST /auth/token` JSON; cache `access_token`, `refresh_token`, `expires_at`; reuse while `expires_at − now > 60 s` (clock injected); otherwise refresh with the refresh token, and if the refresh fails, log in with the password. A 401 on any other call clears the token and retries that call once. `bars` sends `from=YYYY-MM-DD 00:00:00`, `to=YYYY-MM-DD 23:59:59`. `place_order` sends header `X-Idempotency-Key: <uuid>`. Never log request bodies of `/auth/token` nor the `Authorization` header.

- [ ] **Step 1: Write failing tests** (wiremock + `FixedClock`):
  - `login_once_and_reuse_token`: two `quote` calls → `/auth/token` hit exactly once (`.expect(1)`), both requests carry `Authorization: Bearer <token>`.
  - `refreshes_when_expired`: advance clock past `expires_at` → refresh request body `{"grant_type":"refresh_token","refresh_token":"r1"}`.
  - `relogin_when_refresh_fails`: refresh returns 401 → password login.
  - `retries_once_on_401`: quote returns 401 then 200 → Ok; login hit twice.
  - `api_error_envelope_maps`: 400 with `{"meta":{…},"data":null,"error":{"message":"Saldo insuficiente","code":"E1"}}` → `Api { status: 400, code: Some("E1"), .. }`.
  - `order_request_shape`: body equals `{"symbol":"SPY","market_id":"BCBA","side":"buy","order_type":"limit","due_date":"2026-11-24","limit_price":1234.5,"quantity":10.0,"settlement_term":"T1"}` (compare as `serde_json::Value`) and header `X-Idempotency-Key` present.
  - `bar_date_is_argentina_local`: timestamp `1732417200` (2024-11-24 03:00 UTC) → date 2024-11-24; timestamp `1732402800` (2024-11-23 23:00 UTC) → 2024-11-23 (20:00 ART).
  - `positions_tolerate_missing_fields`, `balances_term_lookup` (`available.t1`), `operations_parse_status_and_executed`.
  - `credentials_debug_hides_password` (`format!("{:?}", creds)` does not contain the password).
- [ ] **Step 2: Run** `cargo test -p moon-iol` → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; global checks.
- [ ] **Step 5: Commit and push** `feat(iol): IOL API client with token management`.

---

### Task 9: moon-telegram (Bot API client)

**Files:**
- Modify: `crates/moon-telegram/Cargo.toml` (deps: reqwest, serde, serde_json, thiserror, tokio, tracing; dev: wiremock, tokio)
- Create: `crates/moon-telegram/src/lib.rs`, `crates/moon-telegram/tests/telegram.rs`

**Interfaces:**
- Produces:

```rust
#[derive(Debug, thiserror::Error)]
pub enum TelegramError { #[error("http: {0}")] Http(#[from] reqwest::Error),
                         #[error("Telegram respondió error {code}: {description}")] Api { code: i64, description: String },
                         #[error("respuesta inválida de Telegram: {0}")] Parse(String) }
#[derive(Clone)] pub struct TelegramClient { /* http, base_url, token, chat_id */ }   // manual Debug hides token
impl TelegramClient {
    pub fn new(token: impl Into<String>, chat_id: i64) -> Self;            // base "https://api.telegram.org"
    pub fn with_base_url(self, base_url: impl Into<String>) -> Self;
    pub fn chat_id(&self) -> i64;
    pub async fn send_text(&self, text: &str) -> Result<i64, TelegramError>;                                  // returns message_id
    pub async fn send_with_buttons(&self, text: &str, buttons: &[(String, String)]) -> Result<i64, TelegramError>; // (label, callback_data), one row
    pub async fn edit_text(&self, message_id: i64, text: &str) -> Result<(), TelegramError>;                  // also removes the keyboard
    pub async fn answer_callback(&self, callback_id: &str, text: &str) -> Result<(), TelegramError>;
    pub async fn poll(&self, offset: i64, timeout_secs: u32) -> Result<Vec<Incoming>, TelegramError>;
}
#[derive(Debug, Clone, PartialEq)] pub struct Incoming { pub update_id: i64, pub kind: IncomingKind }
#[derive(Debug, Clone, PartialEq)]
pub enum IncomingKind { Command { name: String, args: String },              // "/estado@bot arg" → name "estado", args "arg"
                        Callback { id: String, data: String, message_id: Option<i64> },
                        Other }
pub const MAX_TEXT: usize = 4096;
pub fn truncate_text(text: &str) -> String;                                  // > 4096 chars → first 4095 chars + "…" (char-safe)
```

Contract: methods POST JSON to `{base}/bot{token}/{method}` (`sendMessage`, `editMessageText`, `answerCallbackQuery`, `getUpdates`); plain text (no `parse_mode`); `send_with_buttons` sends `reply_markup: {"inline_keyboard": [[{"text": label, "callback_data": data}, …]]}`; `editMessageText` sends `reply_markup: {"inline_keyboard": []}`; responses `{"ok": true, "result": …}`; `ok:false` → `Api { code: error_code, description }`. `poll` calls `getUpdates` with `offset`, `timeout`, `allowed_updates: ["message","callback_query"]`; any update whose chat id ≠ configured `chat_id` becomes `Other` (still returned so the caller advances the offset). Never log the token or URLs containing it.

- [ ] **Step 1: Write failing tests:** `send_text_body_shape`, `send_with_buttons_has_inline_keyboard`, `poll_parses_command_and_callback`, `poll_ignores_other_chats`, `command_strips_bot_suffix_and_args`, `api_error_maps`, `truncate_long_text` (5000 'á' → 4096 chars ending with "…"), `debug_hides_token`.
- [ ] **Step 2: Run** `cargo test -p moon-telegram` → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; global checks.
- [ ] **Step 5: Commit and push** `feat(telegram): Bot API client`.

---

### Task 10: moon-broker (paper and IOL execution, quotes, implied CCL)

**Files:**
- Modify: `crates/moon-broker/Cargo.toml` (deps: moon-core, moon-store, moon-iol, moon-yahoo, async-trait, tokio, rust_decimal, chrono, uuid, serde, serde_json, thiserror, tracing; dev: wiremock, tokio, rust_decimal_macros, moon-core with `test-util`)
- Create: `crates/moon-broker/src/{lib.rs,quotes.rs,paper.rs,iol_broker.rs,fx.rs}`, `crates/moon-broker/tests/broker.rs`

**Interfaces:**
- Consumes: Store API (Task 6), `IolClient` (Task 8), `YahooClient` (Task 7), core types and `FeeModel`.
- Produces:

```rust
#[derive(Debug, thiserror::Error)]
pub enum BrokerError { #[error("cotización: {0}")] Quote(String), #[error("broker: {0}")] Broker(String),
    #[error("moneda inesperada para {symbol}: se esperaba {expected:?}, llegó {got}")] CurrencyMismatch { symbol: String, expected: Currency, got: String },
    #[error("no encontrado: {0}")] NotFound(String), #[error("base de datos: {0}")] Store(#[from] moon_store::StoreError) }
#[derive(Debug, Clone, PartialEq)]
pub struct AccountState { pub cash_ars: Decimal, pub positions: BTreeMap<String, Decimal>, pub sellable: BTreeMap<String, Decimal>, pub as_of: DateTime<Utc> }
#[derive(Debug, Clone, PartialEq)]
pub struct OrderTicket { pub idempotency_key: Uuid, pub symbol: String, pub market: Market, pub side: Side, pub qty: Decimal,
                         pub order_type: OrderType, pub limit_price: Option<Decimal>, pub due_date: NaiveDate, pub settlement_term: Term }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderProgress { pub status: OrderStatus, pub filled_qty: Decimal, pub avg_price: Option<Decimal>, pub fees: Option<Decimal> }
#[derive(Debug, Clone, PartialEq)]
pub struct PlaceResult { pub broker_order_id: String, pub accepted: bool, pub messages: Vec<String>, pub fill: Option<OrderProgress> }
#[derive(Debug, Clone, PartialEq)]
pub struct ExternalFlow { pub external_id: String, pub kind: moon_store::FlowKind, pub amount_ars: Decimal, pub at: DateTime<Utc> }
#[async_trait::async_trait]
pub trait Broker: Send + Sync {
    fn mode(&self) -> ExecMode;
    async fn account(&self) -> Result<AccountState, BrokerError>;
    async fn place(&self, ticket: &OrderTicket) -> Result<PlaceResult, BrokerError>;
    async fn refresh(&self, broker_order_id: &str, placed_on: NaiveDate) -> Result<OrderProgress, BrokerError>;
    async fn cancel(&self, broker_order_id: &str) -> Result<(), BrokerError>;
    async fn external_flows(&self, from: NaiveDate, to: NaiveDate) -> Result<Vec<ExternalFlow>, BrokerError>;
}
#[derive(Debug, Clone, PartialEq)]
pub struct Quote { pub symbol: String, pub market: Market, pub price: Decimal, pub currency: Currency, pub at: DateTime<Utc> }
#[async_trait::async_trait]
pub trait QuoteSource: Send + Sync { async fn quote(&self, symbol: &str, market: Market) -> Result<Quote, BrokerError>; }
pub struct IolQuotes { /* Arc<IolClient>, Term */ }  impl IolQuotes { pub fn new(client: Arc<IolClient>, term: Term) -> Self; }
pub struct YahooQuotes { /* YahooClient */ }       impl YahooQuotes { pub fn new(client: YahooClient) -> Self; }
pub struct StaticQuotes { /* Mutex<BTreeMap<(String, Market), Quote>> */ }
impl StaticQuotes { pub fn new() -> Self; pub fn set(&self, symbol: &str, market: Market, price: Decimal, currency: Currency); pub fn remove(&self, symbol: &str, market: Market); }
pub fn parse_currency(raw: &str) -> Option<Currency>;   // case-insensitive: contains "ARS" or "PESO" → Ars; "USD" or "DOLAR"/"DÓLAR" → Usd
pub struct PaperBroker { /* Arc<Mutex<Store>>, Arc<dyn QuoteSource>, FeeModel, slippage_rate, Arc<dyn Clock> */ }
impl PaperBroker { pub fn new(store: Arc<std::sync::Mutex<Store>>, quotes: Arc<dyn QuoteSource>, fees: FeeModel,
                              slippage_rate: Decimal, clock: Arc<dyn Clock>) -> Self; }
pub struct IolBroker { /* Arc<IolClient>, Term, Arc<dyn Clock> */ }
impl IolBroker { pub fn new(client: Arc<IolClient>, term: Term, clock: Arc<dyn Clock>) -> Self; }
// fx.rs
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FxPair { pub local_symbol: String, pub adr_symbol: String, pub adr_market: Market, pub ratio: Decimal }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FxComponent { pub pair: String, pub rate: Decimal }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FxQuote { pub rate: Decimal, pub components: Vec<FxComponent>, pub at: DateTime<Utc> }
pub async fn implied_ccl(quotes: &dyn QuoteSource, pairs: &[FxPair], min_pairs: usize, now: DateTime<Utc>) -> Result<FxQuote, BrokerError>;
```

Behavior:
- `IolQuotes::quote`: price = `prices.current`, else `close`, else `previous_close` (first that is `Some` and > 0), else `Quote` error; currency via `parse_currency(quote.currency)`, unknown → `CurrencyMismatch`. `YahooQuotes::quote`: BCBA → `"{symbol}.BA"`, NYSE/NASDAQ → `symbol`; currency from `YahooQuote.currency`.
- `PaperBroker`: `mode() == Paper`. `account()` reads `store.paper_account()` (sellable = positions). `place()`: if state key `paper_order:<idempotency_key>` exists, return the stored result without applying again. Otherwise quote the symbol (BCBA must be ARS, else `CurrencyMismatch`); `fill = round_price(quote·(1 ± slippage))`; if `limit_price` is set and the fill is not marketable (buy fill > limit, sell fill < limit) → `accepted: false`, message `"precio límite no alcanzado (paper)"`; `value = round_money(qty·fill)`, `fees = fees.total(value)`; buy needs `value + fees <= cash` else `accepted: false` with `"efectivo insuficiente (paper)"`; sell needs `qty <= position` else `accepted: false` with `"tenencia insuficiente (paper)"`; on success update cash/positions (remove zero positions) and save in one store lock; `fill = Some(OrderProgress{Filled, qty, Some(fill), Some(fees)})`; `broker_order_id = "paper-<idempotency_key>"`; persist the `PlaceResult` fields under `paper_order:<key>`. `refresh()` returns the stored progress or `NotFound`. `cancel()` → Ok. `external_flows()` → empty.
- `IolBroker`: `mode() == Live`. `account()`: cash = `balances(AR).ars.available.get(term)`, falling back to `totals.get(term)`, else 0 (log a warning); positions = Σ `quantity` per `instrument.symbol`; sellable = `availability.quantity_available` or `quantity`. `place()` maps the ticket to `IolOrderRequest` (Decimals to f64), `accepted = ack.successful_purchase`, `fill = None`. `refresh()` calls `operations(start = placed_on, end = today ART, country "AR")`, finds `order_id == broker_order_id`: `"completed"` → `Filled` with `filled_qty = executed.qty`; `"pending"` → `PartiallyFilled` if executed qty > 0 else `Sent`; `"cancelled"` → `Cancelled`; `"rejected"` → `Rejected`; other or not found → `Sent`; `avg_price` = executed price if > 0; `fees = None`. `cancel()` → `cancel_order`. `external_flows()` → `activities(from, to, None)`, keep `status == "completed"` with `activity_type` containing `"deposit"`/`"depósito"` (Deposit) or `"extraction"`/`"extracción"` (Withdrawal) in ARS; amount = |net_amount|.
- `implied_ccl`: per pair, local quote `(local_symbol, BCBA)` must be ARS and ADR quote `(adr_symbol, adr_market)` USD, else skip the pair (log); `rate = local · ratio / adr`; fewer than `min_pairs` → `Quote` error; result = median (mean of the two middle values when even) rounded to 2 dp.

- [ ] **Step 1: Write failing tests:**
  - `paper_buy_applies_slippage_and_fees`: cash 1,000,000, quote 10,000, slippage 0.005, fees 0.006/0.0008/0.21 → fill 10,050.0000, value for qty 10 = 100,500.00, fees 603.00+126.63+80.40 = 810.03, cash after 898,689.97.
  - `paper_rejects_insufficient_cash_and_keeps_state`, `paper_rejects_selling_more_than_held`, `paper_limit_not_marketable`.
  - `paper_place_is_idempotent`: same key twice → cash debited once, same `broker_order_id`.
  - `paper_rejects_usd_quote_for_bcba` → `CurrencyMismatch`.
  - `iol_broker_account_maps_balances_and_positions` (wiremock), `iol_refresh_status_mapping` (one case per status), `iol_external_flows_filters_deposits`.
  - `implied_ccl_median_skips_bad_pairs`: 3 pairs, one ADR quoted in ARS → 2 used, median of e.g. 1180 and 1200 = 1190.00; `min_pairs = 3` → error.
  - `parse_currency_variants`.
- [ ] **Step 2: Run** `cargo test -p moon-broker` → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; global checks.
- [ ] **Step 5: Commit and push** `feat(broker): paper and IOL brokers, quote sources and implied CCL`.

---

### Task 11: moon-app configuration, CLI, data sync and backtest commands

**Files:**
- Modify: `crates/moon-app/Cargo.toml` (deps: moon-core, moon-store, moon-yahoo, moon-iol, moon-telegram, moon-broker, anyhow, clap, dotenvy, serde, serde_json, toml, sha2, tokio, tracing, tracing-subscriber, tracing-appender, chrono, rust_decimal, uuid, async-trait; dev: tempfile, rust_decimal_macros, moon-core with `test-util`)
- Create: `crates/moon-app/src/{lib.rs,cli.rs,config.rs,report.rs,logging.rs}`, `crates/moon-app/src/commands/{mod.rs,init.rs,data.rs,backtest.rs}`, `moon.example.toml` (repo root)
- Modify: `crates/moon-app/src/main.rs` (thin: parse CLI, init logging, call `moon_app::run`)

**Interfaces:**
- Consumes: Tasks 2–10.
- Produces:

```rust
// config.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum TradingMode { Paper, LiveConfirm, LiveAuto }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)] #[serde(rename_all = "snake_case")]
pub enum QuoteSourceKind { Iol, Yahoo }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionConfig { pub slippage_rate: Decimal, pub limit_tolerance: Decimal, pub price_tick: Decimal, pub settlement_term: Term,
    pub order_type: OrderType, pub quote_source: QuoteSourceKind, pub approval_timeout_minutes: u32,
    pub fill_timeout_minutes: u32, pub poll_interval_seconds: u32 }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FxConfig { pub pairs: Vec<FxPair>, pub min_pairs: usize }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleConfig { pub market_open: NaiveTime, pub market_close: NaiveTime, pub monthly_time: NaiveTime,
    pub daily_close_time: NaiveTime, pub risk_check_every_minutes: u32, pub weekly_report_weekday: Weekday,
    pub weekly_report_time: NaiveTime, #[serde(default)] pub holidays: Vec<NaiveDate> }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestSettings { #[serde(flatten)] pub config: BacktestConfig }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig { pub mode: TradingMode, pub database_path: PathBuf, pub log_dir: PathBuf,
    pub assets: Vec<AssetSpec>, pub allocation: AllocationConfig, pub risk: RiskLimits,
    pub fees: FeeModel,                       // live/paper fees, amounts in ARS
    pub execution: ExecutionConfig, pub fx: FxConfig, pub schedule: ScheduleConfig,
    pub backtest: BacktestSettings,           // backtest fees, amounts in USD
    pub paper: PaperConfig, pub live: LiveConfig, pub iol: IolConfig, pub telegram: TelegramConfig }
pub struct PaperConfig { pub initial_cash_ars: Decimal }
pub struct LiveConfig { pub enabled: bool }
pub struct IolConfig { pub base_url: String }
pub struct TelegramConfig { pub enabled: bool }
pub struct Secrets { pub iol_username: Option<String>, pub iol_password: Option<String>,
                     pub telegram_token: Option<String>, pub telegram_chat_id: Option<i64> }   // manual Debug hides values
impl AppConfig {
    pub fn load(path: &Path) -> anyhow::Result<AppConfig>;      // read + toml::from_str + validate
    pub fn validate(&self) -> anyhow::Result<()>;
    pub fn universe(&self) -> anyhow::Result<Universe>;
    pub fn allocator(&self) -> anyhow::Result<Allocator>;
    pub fn config_hash(&self) -> String;                         // sha256 hex of serde_json of {assets, allocation, risk, backtest}
    pub fn is_live(&self) -> bool;
}
pub fn load_secrets() -> Secrets;     // dotenvy::dotenv().ok(); env MOON_IOL_USERNAME, MOON_IOL_PASSWORD, MOON_TELEGRAM_TOKEN, MOON_TELEGRAM_CHAT_ID
pub fn validate_live(cfg: &AppConfig, secrets: &Secrets) -> anyhow::Result<()>;
// cli.rs (clap derive)
#[derive(Parser)] #[command(name = "moon", version, about = "moon-trade: administrador de cartera personal")]
pub struct Cli { #[arg(long, default_value = "moon.toml")] pub config: PathBuf, #[command(subcommand)] pub command: Command }
#[derive(Subcommand)]
pub enum Command { Init, Data { #[command(subcommand)] cmd: DataCmd },
    Backtest { #[arg(long)] include_holdout: bool, #[arg(long)] json: bool }, BacktestRuns,
    Plan, Status, Paper { #[command(subcommand)] cmd: PaperCmd }, Pause, Resume, Serve,
    Check { #[command(subcommand)] cmd: CheckCmd } }
#[derive(Subcommand)] pub enum DataCmd { Sync { #[arg(long)] from: Option<NaiveDate> } }
#[derive(Subcommand)] pub enum PaperCmd { Deposit { amount_ars: Decimal }, Reset }
#[derive(Subcommand)] pub enum CheckCmd { Iol, Telegram }
// lib.rs
pub async fn run(cli: Cli) -> anyhow::Result<()>;   // dispatch; Plan/Status/Paper/Pause/Resume/Serve/Check print "pendiente (tarea 12/13)" until implemented
// commands
pub async fn data_sync(cfg: &AppConfig, store: &mut Store, yahoo: &YahooClient, from: Option<NaiveDate>, today: NaiveDate) -> anyhow::Result<SyncReport>;
#[derive(Debug, Clone, PartialEq)] pub struct SyncReport { pub updated: Vec<(String, usize)>, pub failed: Vec<(String, String)> }
pub fn load_series(cfg: &AppConfig, store: &Store) -> anyhow::Result<BTreeMap<AssetId, PriceSeries>>;  // per asset id from its signal_symbol (source Yahoo)
pub fn run_backtest_cmd(cfg: &AppConfig, store: &mut Store, include_holdout: bool, now: DateTime<Utc>) -> anyhow::Result<BacktestOutcome>;
pub struct BacktestOutcome { pub result: BacktestResult, pub run_id: i64, pub distinct_configs: u64 }
// report.rs
pub fn format_metrics_table(strategy: &Metrics, benchmark: Option<&Metrics>) -> String;  // Spanish labels, percentages with 2 decimals
```

Validation (`AppConfig::validate`, Spanish error messages): universe builds; allocator builds; `risk.validate()`; `execution.slippage_rate`, `limit_tolerance` in `[0, 0.05]`; `price_tick > 0`; `fx.min_pairs >= 1` and `<= pairs.len()`; every FX ratio > 0; `market_open < monthly_time < market_close`; `risk_check_every_minutes >= 1`; backtest `initial_capital > 0`; if `mode != paper` and `!live.enabled` → error `"El modo live requiere [live] enabled = true (doble confirmación)."`. `validate_live` additionally requires IOL username and password when live. `data_sync` range: `from` arg, else last stored date + 1 day, else `backtest.start − (warmup_months·31 + 31) days`; to = `today`; symbols = `universe.signal_symbols()`; one failure does not stop the rest. `run_backtest_cmd` registers the run (`config_hash`, config JSON, metrics JSON) and returns the count of distinct configs. The CLI prints the metrics table, `"Configuraciones distintas probadas: N"`, and, with `--include-holdout`, the warning `"⚠️ Incluiste el período virgen: este resultado ya no sirve como validación final."`.

`moon.example.toml` (exact defaults; comments in Spanish; every instrument and number marked as an example to verify, not a recommendation):

```toml
# moon-trade: configuración de ejemplo. Copiala a moon.toml y revisá cada valor.
# Los instrumentos y números son EJEMPLOS para probar, no recomendaciones de inversión.
mode = "paper"              # paper | live_confirm | live_auto
database_path = "moon.db"
log_dir = "logs"

[live]
enabled = false             # doble confirmación: sin esto el bot nunca opera en real

[iol]
base_url = "https://api.iolinversiones.com"

[telegram]
enabled = false

[paper]
initial_cash_ars = 1000000

# --- Universo (verificá ratios de CEDEAR y tickers en IOL) ---
[[assets]]
id = "SPY"
role = "risky"
signal_symbol = "SPY"
cedear_ratio = 20.0
exec = { symbol = "SPY", market = "BCBA", currency = "ARS" }

[[assets]]
id = "QQQ"
role = "risky"
signal_symbol = "QQQ"
cedear_ratio = 20.0
exec = { symbol = "QQQ", market = "BCBA", currency = "ARS" }

[[assets]]
id = "IWM"
role = "risky"
signal_symbol = "IWM"
cedear_ratio = 10.0
exec = { symbol = "IWM", market = "BCBA", currency = "ARS" }

[[assets]]
id = "EEM"
role = "risky"
signal_symbol = "EEM"
cedear_ratio = 5.0
exec = { symbol = "EEM", market = "BCBA", currency = "ARS" }

[[assets]]
id = "GLD"
role = "risky"
signal_symbol = "GLD"
cedear_ratio = 50.0
exec = { symbol = "GLD", market = "BCBA", currency = "ARS" }

# Refugio: CASH = quedarse en pesos (ojo: pierde valor en dólares). Elegí tu refugio.
[[assets]]
id = "CASH"
role = "refuge"

[[assets]]
id = "VWO"
role = "canary"
signal_symbol = "VWO"

[[assets]]
id = "BND"
role = "canary"
signal_symbol = "BND"

[[assets]]
id = "BENCH_SPY"
role = "benchmark"
signal_symbol = "SPY"

[allocation]
sizing = "inverse_vol"       # equal | inverse_vol
vol_lookback_days = 63
canary_threshold = 2

[[allocation.strategies]]
kind = "faber_sma"
months = 10

[[allocation.strategies]]
kind = "dual_momentum"
lookback_months = 12
top_n = 3

[risk]
max_weight = 0.20
max_drawdown = 0.15
rebalance_band = 0.05
min_trade_usd = 20
max_order_usd = 5000
max_orders_per_day = 20
cash_buffer = 0.01
# max_equity_usd = 10000

[fees]                       # comisiones en vivo (montos en ARS). VERIFICAR con IOL.
commission_rate = 0.006
market_fee_rate = 0.0008
vat_rate = 0.21
min_commission = 0

[execution]
slippage_rate = 0.003
limit_tolerance = 0.005
price_tick = 0.01            # VERIFICAR el tick de precio de BYMA
settlement_term = "T1"
order_type = "limit"
quote_source = "iol"         # iol | yahoo
approval_timeout_minutes = 240
fill_timeout_minutes = 20
poll_interval_seconds = 30

[fx]                         # CCL implícito: local ARS / (ADR USD / ratio). VERIFICAR ratios.
min_pairs = 2
pairs = [
  { local_symbol = "GGAL", adr_symbol = "GGAL", adr_market = "NASDAQ", ratio = 10 },
  { local_symbol = "YPFD", adr_symbol = "YPF", adr_market = "NYSE", ratio = 1 },
  { local_symbol = "PAMP", adr_symbol = "PAM", adr_market = "NYSE", ratio = 25 },
  { local_symbol = "BMA", adr_symbol = "BMA", adr_market = "NYSE", ratio = 10 },
]

[schedule]                   # hora Argentina
market_open = "11:00:00"
market_close = "17:00:00"
monthly_time = "11:30:00"
daily_close_time = "17:30:00"
risk_check_every_minutes = 15
weekly_report_weekday = "Fri"
weekly_report_time = "18:00:00"
holidays = []                # feriados de BYMA, ej. ["2026-12-08"]

[backtest]
start = "2008-01-01"
initial_capital = 10000      # USD
monthly_contribution = 0
slippage_rate = 0.003
holdout_start = "2023-01-01" # período virgen: no mirarlo hasta la validación final
risk_free_rate = 0.0

[backtest.fees]              # comisiones del backtest (montos en USD)
commission_rate = 0.006
market_fee_rate = 0.0008
vat_rate = 0.21
min_commission = 0
```

- [ ] **Step 1: Write failing tests** (`crates/moon-app/tests/config.rs`, `tests/backtest_cmd.rs`):
  - `example_config_parses_and_validates`: `AppConfig::load("../../moon.example.toml")` → Ok, mode Paper, 5 risky, CASH refuge, and TOML floats become exact decimals: `cfg.fees.commission_rate == dec!(0.006)`, `cfg.execution.slippage_rate == dec!(0.003)` (if serde yields binary-float noise, deserialize these fields through `money::dec_from_f64`).
  - `live_requires_double_opt_in`: example with `mode = "live_auto"` → validate error containing `"doble confirmación"`; with `[live] enabled = true` but no secrets → `validate_live` error.
  - `config_hash_is_stable_and_sensitive`: same config → same hash; changing `risk.max_weight` → different hash.
  - `secrets_debug_hides_values`.
  - `backtest_cmd_runs_on_synthetic_data_and_registers`: in-memory store seeded with `testutil` daily bars (≥ 4 years) for every signal symbol of a small config (2 risky + CASH + 1 canary + benchmark) → Ok, `run_id > 0`, `distinct_configs == 1`; running twice keeps `distinct_configs == 1`.
  - `data_sync_reports_partial_failure`: wiremock Yahoo with one symbol 404 → `failed.len() == 1`, others stored.
  - `metrics_table_is_spanish`: output contains `"CAGR"`, `"Máxima caída"`, `"Sharpe"`.
- [ ] **Step 2: Run** `cargo test -p moon-app` → FAIL. **Step 3: Implement** (logging: `tracing_subscriber` with env filter `RUST_LOG` default `info`, stdout plus daily rolling file in `log_dir` via `tracing_appender`). **Step 4: Run** → PASS; global checks; manual smoke `cargo run -p moon-app -- --config moon.example.toml init` creates `moon.db` (delete it afterwards).
- [ ] **Step 5: Commit and push** `feat(app): configuration, CLI, data sync and backtest commands`.

---

### Task 12: Execution engine, notifications and operational commands

**Files:**
- Create: `crates/moon-app/src/notify.rs`, `crates/moon-app/src/engine/{mod.rs,valuation.rs,execution.rs,jobs.rs,reconcile.rs,text.rs}`, `crates/moon-app/src/commands/{plan.rs,status.rs,paper.rs,pause.rs}`, `crates/moon-app/tests/common/mod.rs`, `crates/moon-app/tests/engine.rs`
- Modify: `crates/moon-app/src/lib.rs`, `crates/moon-app/src/commands/mod.rs`

**Interfaces:**
- Consumes: everything above.
- Produces:

```rust
// notify.rs
#[async_trait::async_trait]
pub trait Notifier: Send + Sync {
    async fn notify(&self, text: &str);                                                    // never fails (logs errors)
    async fn request_approval(&self, proposal_id: Uuid, text: &str) -> anyhow::Result<Option<i64>>; // buttons "✅ Aprobar" approve:<id>, "❌ Rechazar" reject:<id>
}
pub struct TelegramNotifier { /* TelegramClient */ }  impl TelegramNotifier { pub fn new(client: TelegramClient) -> Self; }
pub struct LogNotifier;                                  // logs via tracing; request_approval → Ok(None)
pub struct RecordingNotifier { /* Mutex<Vec<String>>, Mutex<Vec<Uuid>> */ }
impl RecordingNotifier { pub fn new() -> Self; pub fn messages(&self) -> Vec<String>; pub fn approvals(&self) -> Vec<Uuid>; }
// engine/mod.rs
pub struct EngineDeps { pub store: Arc<std::sync::Mutex<Store>>, pub broker: Arc<dyn Broker>, pub quotes: Arc<dyn QuoteSource>,
                        pub notifier: Arc<dyn Notifier>, pub clock: Arc<dyn Clock> }
pub struct Engine { /* cfg, universe, allocator, deps, trading_lock: tokio::sync::Mutex<()> */ }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionValue { pub asset: AssetId, pub symbol: String, pub qty: Decimal, pub sellable: Decimal, pub price_ars: Decimal, pub value_ars: Decimal }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Valuation { pub at: DateTime<Utc>, pub cash_ars: Decimal, pub positions: Vec<PositionValue>,
                       pub untracked: Vec<(String, Decimal)>, pub equity_ars: Decimal, pub ccl: Decimal, pub equity_usd: Decimal }
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionReport { pub batch_id: Uuid, pub orders: Vec<OrderRecord>, pub errors: Vec<String> }
#[derive(Debug, Clone, PartialEq)]
pub enum MonthlyOutcome { Executed(ExecutionReport), ProposalSent(Uuid), NoChanges }
#[derive(Debug, Clone, PartialEq)]
pub struct RiskOutcome { pub nav: f64, pub drawdown: f64, pub tripped: bool, pub emergency: Option<ExecutionReport> }
impl Engine {
    pub fn new(cfg: AppConfig, deps: EngineDeps) -> anyhow::Result<Engine>;  // validates config; live mode requires cfg.live.enabled and broker.mode()==Live; paper requires broker.mode()==Paper
    pub fn config(&self) -> &AppConfig;
    pub fn exec_mode(&self) -> ExecMode;
    pub async fn valuation(&self) -> anyhow::Result<Valuation>;
    pub fn decide(&self, as_of: NaiveDate) -> anyhow::Result<AllocationDecision>;
    pub fn plan(&self, targets: &TargetWeights, v: &Valuation, force_full: bool) -> anyhow::Result<RebalancePlan>;
    pub async fn execute(&self, plan: &RebalancePlan, targets: &TargetWeights, reason: OrderReason, batch_id: Uuid, emergency: bool) -> anyhow::Result<ExecutionReport>;
    pub async fn run_monthly(&self, today: NaiveDate) -> anyhow::Result<MonthlyOutcome>;
    pub async fn handle_approval(&self, proposal_id: Uuid, approve: bool) -> anyhow::Result<String>;
    pub async fn run_risk_check(&self) -> anyhow::Result<RiskOutcome>;
    pub async fn run_daily_close(&self, today: NaiveDate) -> anyhow::Result<()>;
    pub async fn weekly_report(&self) -> anyhow::Result<String>;
    pub async fn status_text(&self) -> anyhow::Result<String>;
    pub fn set_paused(&self, paused: bool) -> anyhow::Result<()>;
    pub fn is_paused(&self) -> anyhow::Result<bool>;
    pub async fn paper_deposit(&self, amount_ars: Decimal) -> anyhow::Result<()>;
    pub async fn paper_reset(&self) -> anyhow::Result<()>;
}
// engine/reconcile.rs
#[derive(Debug, Clone, PartialEq)] pub struct Discrepancy { pub symbol: String, pub expected: Decimal, pub actual: Decimal }
pub fn reconcile_positions(previous: &BTreeMap<String, Decimal>, filled_today: &[OrderRecord], current: &BTreeMap<String, Decimal>) -> Vec<Discrepancy>;
pub fn order_id_for(batch_id: Uuid, asset: &AssetId, side: Side) -> Uuid;   // Uuid::new_v5(&batch_id, "<asset>:<side>")
// commands
pub fn build_engine(cfg: AppConfig, secrets: &Secrets) -> anyhow::Result<(Engine, Option<TelegramClient>)>; // real wiring: Store::open, quote source per config (IolQuotes needs credentials; else YahooQuotes), PaperBroker or IolBroker, TelegramNotifier if enabled + secrets else LogNotifier, SystemClock
```

State keys (constants in `engine/mod.rs`): `"paused"` (bool), `"nav"` (NavTracker), `"drawdown"` (DrawdownState), `"telegram_offset"` (i64), `"schedule"` (Task 13).

Engine rules:
- **Valuation:** `broker.account()`; for every tradable asset (risky + non-CASH refuge) quote `(exec.symbol, exec.market)`; an asset whose `exec.currency` is ARS must get an ARS quote, else `CurrencyMismatch` → error; any quote error → error (no partial valuation). FX = `implied_ccl` with `cfg.fx`; store an `FxRecord`. Positions of symbols not in the universe go to `untracked` and are excluded from equity. `equity_ars = cash + Σ value`; `equity_usd = round_money(equity_ars / ccl)`.
- **Decide:** series from the store (`load_series`) → `allocator.allocate(SignalContext::new(as_of, …))`.
- **Plan:** prices/holdings/lots per asset id; `min_trade_value = min_trade_usd · ccl`; if `max_equity_usd` is set and `equity_usd` exceeds it, scale every non-CASH target by `max_equity_usd / equity_usd` (the rest stays as cash, i.e. add it to CASH); `check_targets`; `plan_rebalance` with `cfg.fees`, `cfg.execution.slippage_rate`, `risk.rebalance_band`, `risk.cash_buffer`.
- **Execute:** `check_orders(plan.orders, max_order_usd · ccl, max_orders_per_day, orders already created today for this mode, emergency)`. For each sell then each buy: `id = order_id_for(batch_id, asset, side)`; `insert_order` (`PendingSend`); if it already existed, do not place again (reuse the stored record). Ticket: limit = buy `ceil_to_tick(ref·(1+limit_tolerance))`, sell `floor_to_tick(ref·(1−limit_tolerance))` (omit the limit for `order_type = market`); `due_date = today ART`. After `place`: not accepted → `Rejected` with messages joined in `error`; `fill` → copy status/qty/avg/fees; otherwise `Sent` with `broker_order_id`. A broker error marks the order `Failed`, stops the batch and is reported. When the progress has `fees: None`, estimate with `cfg.fees.total(filled_qty·avg_price)` and set `fees_estimated = true`. After the sells: while any sell is not terminal and `fill_timeout_minutes` has not elapsed, sleep `poll_interval_seconds` and `refresh`. Then recompute `valuation` + `plan(targets, …)` and execute **only its buys** (buys use real cash). Finish with one Spanish summary notification (orders, quantities, prices, fees, errors).
- **Monthly** (`run_monthly`): `as_of = previous_trading_day(today, holidays)` (helper in `engine/mod.rs`, Mon–Fri minus holidays); decision → `log_decision("monthly", …)`; valuation; reset `DrawdownState` to the current nav and persist; plan with `force_full = false`; empty → notify "sin cambios", `NoChanges`. Paper/LiveAuto → `execute(…, Monthly, new batch, false)` → `Executed`. LiveConfirm → store `Proposal { kind: Monthly, expires_at: now + approval_timeout_minutes (capped at today's market_close ART), payload: {"decision":…, "targets":…, "plan":…} }`, call `request_approval` with a Spanish summary, save the message id → `ProposalSent`.
- **Approval** (`handle_approval`): unknown → `"Propuesta inexistente."`; not `Pending` → `"Esta propuesta ya fue resuelta."`; expired → set `Expired`, `"La propuesta venció; se recalculará en el próximo rebalanceo."`; reject → `Rejected`; approve → `Approved`, fresh valuation, plan from the stored `targets`, execute (`Monthly`), then `Executed` or `Failed`; return a summary.
- **Risk check:** valuation; load/create `NavTracker` (start at current `equity_usd` if absent) and `DrawdownState`; `nav = tracker.nav(equity_usd)`; `observe(nav, today, max_drawdown)`; persist both. On `Tripped`: targets = all refuge, `plan(force_full = true)`, `execute(…, Emergency, new batch, true)` **regardless of pause and mode** (no proposal), notify `"🚨 Freno por caída: …"`.
- **Daily close:** (1) data sync via Yahoo (failures → warning notification, continue); (2) valuation; (3) `broker.external_flows(today, today)` → for each new flow (`insert_flow` true): convert to USD at the valuation CCL, `tracker.apply_flow(±usd, equity_usd − (±usd))`, persist, notify `"💰 Depósito detectado"` / `"💸 Extracción detectada"`; (4) reconcile against the latest snapshot + today's filled orders and notify discrepancies; (5) for each open order: `cancel` then `refresh` and update; (6) expire pending proposals past `expires_at`; (7) insert the snapshot (nav, units, drawdown, positions by symbol); (8) warn if the newest stored bar of any risky signal symbol is older than 5 calendar days.
- **Paper deposit/reset:** only in paper mode (else error `"Solo disponible en modo paper."`). Deposit: valuation before, add cash to the paper account, record `FlowRecord` (Deposit), `apply_flow` with the deposit in USD at the current CCL. Reset: paper account = `initial_cash_ars`, no positions; delete states `nav` and `drawdown`.
- **Text:** `status_text` and `weekly_report` in Spanish: mode, paused, equity USD/ARS, CCL, NAV and drawdown, positions with weights, last decision date, open orders/proposals; the weekly report adds the NAV change over the last 7 days and the week's trades and fees.
- CLI commands wired in `lib.rs::run`: `plan` (decision + plan, no execution), `status`, `paper deposit|reset`, `pause`/`resume` (also notify).

- [ ] **Step 1: Write failing tests** (`tests/common/mod.rs` provides: a config builder from `moon.example.toml` overridden to 2 risky + CASH, a seeded in-memory store with synthetic rising series via `testutil`, `StaticQuotes` with BCBA ARS quotes for the exec symbols and FX pairs (CCL 1200), `FixedClock` on a weekday 11:30 ART, `RecordingNotifier`, and `FakeBroker` (mode Live, records `place` calls, configurable `refresh` result and account)):
  - `paper_monthly_rebalance_buys_from_cash`: paper broker with 1,000,000 ARS → `Executed`, all orders `Filled`, paper positions not empty, cash ≥ 0, a decision logged.
  - `live_confirm_creates_proposal_then_executes_on_approval`: no `place` before approval; `approvals().len() == 1`; approve → places orders; approving again → `"ya fue resuelta"`.
  - `rejected_proposal_places_nothing`, `expired_proposal_is_not_executed`.
  - `emergency_exit_ignores_pause_and_confirmation`: live_confirm, paused, nav state making drawdown ≥ 15 % → sells placed immediately, `tripped == true`, message contains `"Freno por caída"`.
  - `missing_quote_blocks_trading`: remove one exec quote → `run_monthly` is `Err`, zero orders created.
  - `usd_quote_for_bcba_asset_is_error`.
  - `idempotent_batch_places_once`: `execute` twice with the same `batch_id` → each order placed once.
  - `sent_order_not_resent_and_cancelled_at_close`: FakeBroker `refresh` returns `Sent`, `fill_timeout_minutes = 0` → buys computed from the broker's real cash; `run_daily_close` calls `cancel` for the open order and records `Cancelled`.
  - `paper_deposit_keeps_nav`: nav before == nav after (±1e-9); deposit outside paper mode → error.
  - `daily_close_records_snapshot_and_flows`: FakeBroker reports one deposit flow → one `FlowRecord`, nav unchanged, snapshot stored; running twice does not duplicate the flow.
  - `engine_rejects_live_without_opt_in`: live mode with `live.enabled = false` → `Engine::new` error.
  - `reconcile_detects_mismatch` (unit test of `reconcile_positions`).
- [ ] **Step 2: Run** `cargo test -p moon-app` → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; global checks.
- [ ] **Step 5: Commit and push** `feat(app): execution engine, approvals, emergency exits and daily close`.

---

### Task 13: Scheduler, service, Telegram commands, checks and README

**Files:**
- Create: `crates/moon-app/src/{scheduler.rs,serve.rs,telegram_bot.rs}`, `crates/moon-app/src/commands/check.rs`, `crates/moon-app/tests/scheduler.rs`, `crates/moon-app/tests/telegram_bot.rs`
- Modify: `crates/moon-app/src/lib.rs`, `README.md`

**Interfaces:**
- Consumes: `Engine` (Task 12), `TelegramClient`/`Incoming` (Task 9), `ScheduleConfig` (Task 11).
- Produces:

```rust
// scheduler.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Job { MonthlyRebalance, RiskCheck, DailyClose, WeeklyReport }
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScheduleState { pub last_risk_check: Option<DateTime<Utc>>, pub last_monthly: Option<String>,      // "YYYY-MM"
                           pub last_monthly_attempt: Option<DateTime<Utc>>, pub last_daily_close: Option<NaiveDate>,
                           pub last_weekly: Option<NaiveDate> }
pub fn is_trading_day(d: NaiveDate, holidays: &[NaiveDate]) -> bool;
pub fn previous_trading_day(d: NaiveDate, holidays: &[NaiveDate]) -> NaiveDate;   // re-export/move of Task 12 helper
pub fn due_jobs(now: DateTime<Utc>, cfg: &ScheduleConfig, state: &ScheduleState, paused: bool) -> Vec<Job>;
pub fn mark_attempt(state: &mut ScheduleState, job: Job, now: DateTime<Utc>);
pub fn mark_done(state: &mut ScheduleState, job: Job, now: DateTime<Utc>);
// telegram_bot.rs
#[derive(Debug, Clone, PartialEq)] pub enum BotAction { Reply(String), CallbackDone { callback_id: String, message_id: Option<i64>, text: String }, Ignore }
pub async fn handle_incoming(engine: &Engine, incoming: &Incoming) -> BotAction;
// serve.rs
pub async fn serve(engine: Arc<Engine>, telegram: Option<TelegramClient>) -> anyhow::Result<()>;
// commands/check.rs
pub async fn check_iol(cfg: &AppConfig, secrets: &Secrets) -> anyhow::Result<String>;      // read-only: login, balances, positions, one quote
pub async fn check_telegram(secrets: &Secrets) -> anyhow::Result<String>;                  // sends "✅ moon-trade: prueba de Telegram"
```

Rules (all times in ART):
- `RiskCheck` due: trading day, `market_open <= t < market_close`, and `last_risk_check` is None or ≥ `risk_check_every_minutes` ago. Runs even when paused.
- `MonthlyRebalance` due: not paused, trading day, `monthly_time <= t < market_close`, `last_monthly != "YYYY-MM"` of today, and `last_monthly_attempt` is None or ≥ 30 min ago (catch-up on any later trading day of the month).
- `DailyClose` due: trading day, `t >= daily_close_time`, `last_daily_close != today`.
- `WeeklyReport` due: weekday == `weekly_report_weekday`, `t >= weekly_report_time`, `last_weekly != today`.
- Order: Monthly, RiskCheck, DailyClose, WeeklyReport. `mark_attempt` sets `last_risk_check` (RiskCheck) or `last_monthly_attempt` (Monthly); `mark_done` sets `last_monthly`, `last_daily_close`, `last_weekly`. Serve marks Monthly done on any `Ok` outcome; DailyClose and WeeklyReport are marked done even on error (after notifying).
- Telegram: commands `estado`/`status` → `status_text`; `pausa`/`pause` → pause (+ text that emergency exits stay active); `reanudar`/`resume` → resume; `start`/`ayuda`/`help` and unknown → help text listing the commands. Callback `approve:<uuid>` / `reject:<uuid>` → `handle_approval` → `CallbackDone`; malformed data → `CallbackDone` with `"Acción inválida."`. `Other` → `Ignore`.
- `serve`: notify `"🟢 moon-trade iniciado en modo <modo>"`; spawn the Telegram loop (if a client is given): offset from state `"telegram_offset"`, `poll(offset, 25)`, handle each update, persist `update_id + 1`, apply `BotAction` (`send_text`, or `answer_callback` + `edit_text`), on error sleep 10 s; main loop every 30 s: load `"schedule"` state, `due_jobs`, run each job, persist state; job errors notify `"⚠️ Error en <tarea>: <detalle>"` at most once per hour per identical message; `tokio::signal::ctrl_c()` → notify `"🔴 moon-trade detenido"` and return.
- README (Spanish, replaces the skeleton): qué es y qué no es (no es asesoramiento financiero); advertencias (IOL no tiene sandbox, toda orden live es real; el repo es público, nunca subir `.env` ni `moon.toml`); requisitos (Rust estable; en Windows, Build Tools de Visual Studio); instalación (`cargo build --release`); configuración (`moon.example.toml` → `moon.toml`, `.env.example` → `.env`); primeros pasos (`moon init`, `moon data sync`, `moon backtest`, `moon backtest-runs`, `moon plan`, `moon serve` en paper); Telegram (BotFather, obtener chat id, `moon check telegram`); IOL (habilitar API, `moon check iol`); modos y cómo pasar a live (escalonado: semanas en paper, luego `live_confirm` con monto chico vía `max_equity_usd`, luego `live_auto`; doble opt-in); correr como servicio (Windows: Programador de tareas "al iniciar sesión", reintentar si falla; Linux: unidad systemd de ejemplo); tabla de comandos; cómo funciona (régimen, estrategias, riesgo, freno, banda, costos, anti-overfitting) con links a las referencias; pendientes de verificar (lista del diseño §12); desarrollo (`scripts/check.ps1`, estructura de crates, CI).

- [ ] **Step 1: Write failing tests:**
  - `scheduler.rs`: `risk_check_every_15_minutes_in_market_hours`, `holiday_and_weekend_have_no_jobs`, `monthly_runs_once_per_month` (due at 11:30 on the first trading day; not due after `mark_done`; due again next month; catch-up on the 3rd trading day if missed; not due when paused; not due within 30 min of a failed attempt), `daily_close_once_after_close_time`, `weekly_report_on_configured_day`, `previous_trading_day_skips_weekend_and_holidays` (Monday 2026-12-07 with holiday Friday 2026-12-04 → Thursday 2026-12-03).
  - `telegram_bot.rs` (engine from `tests/common`): `estado_returns_status`, `pausa_and_reanudar_toggle_state`, `approve_callback_executes_proposal`, `malformed_callback_is_rejected`, `other_updates_are_ignored`.
- [ ] **Step 2: Run** `cargo test -p moon-app` → FAIL. **Step 3: Implement.** **Step 4: Run** → PASS; global checks; smoke: `cargo run -p moon-app -- --config moon.example.toml backtest-runs` prints that there are no runs (delete the generated `moon.db`).
- [ ] **Step 5: Commit and push** `feat(app): scheduler, service loop, Telegram commands, checks and README`.
