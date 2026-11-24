//! Schema migrations, applied in order while `PRAGMA user_version` is behind.

pub(crate) const MIGRATIONS: &[&str] = &[
    // Version 1.
    "
CREATE TABLE bars (
    symbol TEXT NOT NULL,
    source TEXT NOT NULL,
    date TEXT NOT NULL,
    open REAL NOT NULL,
    high REAL NOT NULL,
    low REAL NOT NULL,
    close REAL NOT NULL,
    adj_close REAL NOT NULL,
    volume REAL NOT NULL,
    PRIMARY KEY (symbol, source, date)
);
CREATE TABLE fx_rates (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    rate TEXT NOT NULL,
    method TEXT NOT NULL,
    components TEXT NOT NULL
);
CREATE INDEX idx_fx_rates_at ON fx_rates (at);
CREATE TABLE state_kv (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    mode TEXT NOT NULL,
    cash_ars TEXT NOT NULL,
    equity_ars TEXT NOT NULL,
    ccl TEXT NOT NULL,
    equity_usd TEXT NOT NULL,
    nav REAL NOT NULL,
    units TEXT NOT NULL,
    drawdown REAL NOT NULL,
    positions TEXT NOT NULL
);
CREATE INDEX idx_snapshots_mode_at ON snapshots (mode, at);
CREATE TABLE orders (
    id TEXT PRIMARY KEY,
    batch_id TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    mode TEXT NOT NULL,
    reason TEXT NOT NULL,
    asset TEXT NOT NULL,
    symbol TEXT NOT NULL,
    market TEXT NOT NULL,
    side TEXT NOT NULL,
    qty TEXT NOT NULL,
    limit_price TEXT,
    currency TEXT NOT NULL,
    status TEXT NOT NULL,
    broker_order_id TEXT,
    filled_qty TEXT NOT NULL,
    avg_fill_price TEXT,
    fees TEXT NOT NULL,
    fees_estimated INTEGER NOT NULL,
    error TEXT
);
CREATE INDEX idx_orders_mode_status ON orders (mode, status);
CREATE TABLE paper_account (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    cash_ars TEXT NOT NULL,
    positions TEXT NOT NULL
);
CREATE TABLE proposals (
    id TEXT PRIMARY KEY,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    decided_at TEXT,
    payload TEXT NOT NULL,
    telegram_message_id INTEGER
);
CREATE TABLE flows (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    mode TEXT NOT NULL,
    kind TEXT NOT NULL,
    amount_ars TEXT NOT NULL,
    external_id TEXT UNIQUE,
    note TEXT NOT NULL
);
CREATE INDEX idx_flows_mode_at ON flows (mode, at);
CREATE TABLE decisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    at TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL
);
CREATE TABLE backtest_runs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at TEXT NOT NULL,
    config_hash TEXT NOT NULL,
    config TEXT NOT NULL,
    start TEXT NOT NULL,
    end TEXT NOT NULL,
    include_holdout INTEGER NOT NULL,
    metrics TEXT NOT NULL
);
",
    // Version 2: cash including committed funds, for cash reconciliation.
    "ALTER TABLE snapshots ADD COLUMN cash_total_ars TEXT NULL;",
];
