//! The `Store`: connection handling and every query.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use moon_core::asset::AssetId;
use moon_core::bars::Bar;
use moon_core::types::{DataSource, ExecMode, OrderStatus};
use rusqlite::{Connection, OptionalExtension, Row, params};
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::StoreError;
use crate::records::*;
use crate::schema::MIGRATIONS;

/// SQLite-backed persistence. `Send`, so it can live behind a `Mutex`.
pub struct Store {
    conn: Connection,
}

// --- encoding helpers -------------------------------------------------------

/// Fixed-width RFC 3339 (UTC, nanoseconds) so string order equals time order.
fn enc_ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn enc_date(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

fn corrupt<E: std::fmt::Display>(what: &str, raw: &str, e: E) -> StoreError {
    StoreError::Corrupt(format!("{what} '{raw}': {e}"))
}

fn dec_ts(raw: &str) -> Result<DateTime<Utc>, StoreError> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| corrupt("timestamp", raw, e))
}

fn dec_date(raw: &str) -> Result<NaiveDate, StoreError> {
    NaiveDate::parse_from_str(raw, "%Y-%m-%d").map_err(|e| corrupt("fecha", raw, e))
}

fn dec_decimal(raw: &str) -> Result<Decimal, StoreError> {
    Decimal::from_str(raw).map_err(|e| corrupt("decimal", raw, e))
}

fn dec_uuid(raw: &str) -> Result<Uuid, StoreError> {
    Uuid::parse_str(raw).map_err(|e| corrupt("uuid", raw, e))
}

fn dec_enum<T: FromStr>(what: &str, raw: &str) -> Result<T, StoreError>
where
    T::Err: std::fmt::Display,
{
    raw.parse::<T>().map_err(|e| corrupt(what, raw, e))
}

fn dec_json<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, StoreError> {
    serde_json::from_str(raw).map_err(|e| corrupt("json", raw, e))
}

fn dec_proposal_kind(raw: &str) -> Result<ProposalKind, StoreError> {
    match raw {
        "monthly" => Ok(ProposalKind::Monthly),
        "deposit" => Ok(ProposalKind::Deposit),
        other => Err(corrupt("proposal kind", other, "valor desconocido")),
    }
}

fn dec_proposal_status(raw: &str) -> Result<ProposalStatus, StoreError> {
    match raw {
        "pending" => Ok(ProposalStatus::Pending),
        "approved" => Ok(ProposalStatus::Approved),
        "rejected" => Ok(ProposalStatus::Rejected),
        "expired" => Ok(ProposalStatus::Expired),
        "executed" => Ok(ProposalStatus::Executed),
        "failed" => Ok(ProposalStatus::Failed),
        other => Err(corrupt("proposal status", other, "valor desconocido")),
    }
}

fn dec_flow_kind(raw: &str) -> Result<FlowKind, StoreError> {
    match raw {
        "deposit" => Ok(FlowKind::Deposit),
        "withdrawal" => Ok(FlowKind::Withdrawal),
        other => Err(corrupt("flow kind", other, "valor desconocido")),
    }
}

fn opt_ts(raw: Option<String>) -> Result<Option<DateTime<Utc>>, StoreError> {
    raw.as_deref().map(dec_ts).transpose()
}

fn opt_decimal(raw: Option<String>) -> Result<Option<Decimal>, StoreError> {
    raw.as_deref().map(dec_decimal).transpose()
}

/// Runs a row-mapping query whose mapper can fail with `StoreError`.
fn query_all<T, P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    map: fn(&Row) -> Result<T, StoreError>,
) -> Result<Vec<T>, StoreError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params, |r| Ok(map(r)))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

fn query_opt<T, P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    map: fn(&Row) -> Result<T, StoreError>,
) -> Result<Option<T>, StoreError> {
    match conn.query_row(sql, params, |r| Ok(map(r))).optional()? {
        Some(res) => Ok(Some(res?)),
        None => Ok(None),
    }
}

// --- row mappers --------------------------------------------------------------

const ORDER_COLS: &str = "id, batch_id, created_at, updated_at, mode, reason, asset, symbol, \
    market, side, qty, limit_price, currency, status, broker_order_id, filled_qty, \
    avg_fill_price, fees, fees_estimated, error";

fn order_from_row(r: &Row) -> Result<OrderRecord, StoreError> {
    Ok(OrderRecord {
        id: dec_uuid(&r.get::<_, String>(0)?)?,
        batch_id: dec_uuid(&r.get::<_, String>(1)?)?,
        created_at: dec_ts(&r.get::<_, String>(2)?)?,
        updated_at: dec_ts(&r.get::<_, String>(3)?)?,
        mode: dec_enum("mode", &r.get::<_, String>(4)?)?,
        reason: dec_enum("reason", &r.get::<_, String>(5)?)?,
        asset: AssetId(r.get::<_, String>(6)?),
        symbol: r.get(7)?,
        market: dec_enum("market", &r.get::<_, String>(8)?)?,
        side: dec_enum("side", &r.get::<_, String>(9)?)?,
        qty: dec_decimal(&r.get::<_, String>(10)?)?,
        limit_price: opt_decimal(r.get(11)?)?,
        currency: dec_enum("currency", &r.get::<_, String>(12)?)?,
        status: dec_enum("status", &r.get::<_, String>(13)?)?,
        broker_order_id: r.get(14)?,
        filled_qty: dec_decimal(&r.get::<_, String>(15)?)?,
        avg_fill_price: opt_decimal(r.get(16)?)?,
        fees: dec_decimal(&r.get::<_, String>(17)?)?,
        fees_estimated: r.get::<_, i64>(18)? != 0,
        error: r.get(19)?,
    })
}

const SNAPSHOT_COLS: &str =
    "at, mode, cash_ars, equity_ars, ccl, equity_usd, nav, units, drawdown, positions";

fn snapshot_from_row(r: &Row) -> Result<Snapshot, StoreError> {
    Ok(Snapshot {
        at: dec_ts(&r.get::<_, String>(0)?)?,
        mode: dec_enum("mode", &r.get::<_, String>(1)?)?,
        cash_ars: dec_decimal(&r.get::<_, String>(2)?)?,
        equity_ars: dec_decimal(&r.get::<_, String>(3)?)?,
        ccl: dec_decimal(&r.get::<_, String>(4)?)?,
        equity_usd: dec_decimal(&r.get::<_, String>(5)?)?,
        nav: r.get(6)?,
        units: dec_decimal(&r.get::<_, String>(7)?)?,
        drawdown: r.get(8)?,
        positions: dec_json(&r.get::<_, String>(9)?)?,
    })
}

fn bar_from_row(r: &Row) -> Result<Bar, StoreError> {
    Ok(Bar {
        date: dec_date(&r.get::<_, String>(0)?)?,
        open: r.get(1)?,
        high: r.get(2)?,
        low: r.get(3)?,
        close: r.get(4)?,
        adj_close: r.get(5)?,
        volume: r.get(6)?,
    })
}

fn fx_from_row(r: &Row) -> Result<FxRecord, StoreError> {
    Ok(FxRecord {
        at: dec_ts(&r.get::<_, String>(0)?)?,
        rate: dec_decimal(&r.get::<_, String>(1)?)?,
        method: r.get(2)?,
        components: dec_json(&r.get::<_, String>(3)?)?,
    })
}

const PROPOSAL_COLS: &str =
    "id, created_at, expires_at, kind, status, decided_at, payload, telegram_message_id";

fn proposal_from_row(r: &Row) -> Result<Proposal, StoreError> {
    Ok(Proposal {
        id: dec_uuid(&r.get::<_, String>(0)?)?,
        created_at: dec_ts(&r.get::<_, String>(1)?)?,
        expires_at: dec_ts(&r.get::<_, String>(2)?)?,
        kind: dec_proposal_kind(&r.get::<_, String>(3)?)?,
        status: dec_proposal_status(&r.get::<_, String>(4)?)?,
        decided_at: opt_ts(r.get(5)?)?,
        payload: dec_json(&r.get::<_, String>(6)?)?,
        telegram_message_id: r.get(7)?,
    })
}

fn flow_from_row(r: &Row) -> Result<FlowRecord, StoreError> {
    Ok(FlowRecord {
        at: dec_ts(&r.get::<_, String>(0)?)?,
        mode: dec_enum("mode", &r.get::<_, String>(1)?)?,
        kind: dec_flow_kind(&r.get::<_, String>(2)?)?,
        amount_ars: dec_decimal(&r.get::<_, String>(3)?)?,
        external_id: r.get(4)?,
        note: r.get(5)?,
    })
}

fn decision_from_row(r: &Row) -> Result<DecisionRecord, StoreError> {
    Ok(DecisionRecord {
        id: r.get(0)?,
        at: dec_ts(&r.get::<_, String>(1)?)?,
        kind: r.get(2)?,
        payload: dec_json(&r.get::<_, String>(3)?)?,
    })
}

fn backtest_from_row(r: &Row) -> Result<BacktestRun, StoreError> {
    Ok(BacktestRun {
        id: Some(r.get(0)?),
        created_at: dec_ts(&r.get::<_, String>(1)?)?,
        config_hash: r.get(2)?,
        config: dec_json(&r.get::<_, String>(3)?)?,
        start: dec_date(&r.get::<_, String>(4)?)?,
        end: dec_date(&r.get::<_, String>(5)?)?,
        include_holdout: r.get::<_, i64>(6)? != 0,
        metrics: dec_json(&r.get::<_, String>(7)?)?,
    })
}

// --- Store ----------------------------------------------------------------------

impl Store {
    /// Opens (creating if needed) the database file and runs migrations.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Store, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store, StoreError> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut store = Store { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<(), StoreError> {
        let current = self.schema_version()? as usize;
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current) {
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.execute_batch(&format!("PRAGMA user_version = {}", i + 1))?;
            tx.commit()?;
        }
        Ok(())
    }

    pub fn schema_version(&self) -> Result<u32, StoreError> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    // --- bars ---

    /// Inserts or replaces bars for `(symbol, source, date)` in one transaction.
    pub fn upsert_bars(
        &mut self,
        symbol: &str,
        source: DataSource,
        bars: &[Bar],
    ) -> Result<usize, StoreError> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR REPLACE INTO bars \
                 (symbol, source, date, open, high, low, close, adj_close, volume) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for b in bars {
                stmt.execute(params![
                    symbol,
                    source.as_str(),
                    enc_date(b.date),
                    b.open,
                    b.high,
                    b.low,
                    b.close,
                    b.adj_close,
                    b.volume
                ])?;
            }
        }
        tx.commit()?;
        Ok(bars.len())
    }

    pub fn load_bars(&self, symbol: &str, source: DataSource) -> Result<Vec<Bar>, StoreError> {
        query_all(
            &self.conn,
            "SELECT date, open, high, low, close, adj_close, volume FROM bars \
             WHERE symbol = ?1 AND source = ?2 ORDER BY date ASC",
            params![symbol, source.as_str()],
            bar_from_row,
        )
    }

    pub fn last_bar_date(
        &self,
        symbol: &str,
        source: DataSource,
    ) -> Result<Option<NaiveDate>, StoreError> {
        let raw: Option<String> = self.conn.query_row(
            "SELECT MAX(date) FROM bars WHERE symbol = ?1 AND source = ?2",
            params![symbol, source.as_str()],
            |r| r.get(0),
        )?;
        raw.as_deref().map(dec_date).transpose()
    }

    // --- fx ---

    pub fn insert_fx(&mut self, rec: &FxRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO fx_rates (at, rate, method, components) VALUES (?1, ?2, ?3, ?4)",
            params![
                enc_ts(rec.at),
                rec.rate.to_string(),
                rec.method,
                serde_json::to_string(&rec.components)?
            ],
        )?;
        Ok(())
    }

    pub fn latest_fx(&self) -> Result<Option<FxRecord>, StoreError> {
        query_opt(
            &self.conn,
            "SELECT at, rate, method, components FROM fx_rates ORDER BY at DESC, id DESC LIMIT 1",
            [],
            fx_from_row,
        )
    }

    // --- key-value state ---

    pub fn get_state<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, StoreError> {
        let raw: Option<String> = self
            .conn
            .query_row("SELECT value FROM state_kv WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .optional()?;
        match raw {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }

    pub fn set_state<T: serde::Serialize>(
        &mut self,
        key: &str,
        value: &T,
    ) -> Result<(), StoreError> {
        let raw = serde_json::to_string(value)?;
        self.conn.execute(
            "INSERT INTO state_kv (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, raw],
        )?;
        Ok(())
    }

    pub fn delete_state(&mut self, key: &str) -> Result<(), StoreError> {
        self.conn
            .execute("DELETE FROM state_kv WHERE key = ?1", [key])?;
        Ok(())
    }

    // --- snapshots ---

    pub fn insert_snapshot(&mut self, s: &Snapshot) -> Result<(), StoreError> {
        self.conn.execute(
            &format!("INSERT INTO snapshots ({SNAPSHOT_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"),
            params![
                enc_ts(s.at),
                s.mode.as_str(),
                s.cash_ars.to_string(),
                s.equity_ars.to_string(),
                s.ccl.to_string(),
                s.equity_usd.to_string(),
                s.nav,
                s.units.to_string(),
                s.drawdown,
                serde_json::to_string(&s.positions)?
            ],
        )?;
        Ok(())
    }

    pub fn latest_snapshot(&self, mode: ExecMode) -> Result<Option<Snapshot>, StoreError> {
        query_opt(
            &self.conn,
            &format!(
                "SELECT {SNAPSHOT_COLS} FROM snapshots WHERE mode = ?1 \
                 ORDER BY at DESC, id DESC LIMIT 1"
            ),
            [mode.as_str()],
            snapshot_from_row,
        )
    }

    /// Snapshots with `from <= at <= to`, ascending.
    pub fn snapshots_between(
        &self,
        mode: ExecMode,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<Snapshot>, StoreError> {
        query_all(
            &self.conn,
            &format!(
                "SELECT {SNAPSHOT_COLS} FROM snapshots \
                 WHERE mode = ?1 AND at >= ?2 AND at <= ?3 ORDER BY at ASC, id ASC"
            ),
            params![mode.as_str(), enc_ts(from), enc_ts(to)],
            snapshot_from_row,
        )
    }

    // --- orders ---

    /// Idempotent insert: returns `false` when the id already exists.
    pub fn insert_order(&mut self, o: &OrderRecord) -> Result<bool, StoreError> {
        let n = self.conn.execute(
            &format!(
                "INSERT OR IGNORE INTO orders ({ORDER_COLS}) VALUES \
                 (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)"
            ),
            params![
                o.id.to_string(),
                o.batch_id.to_string(),
                enc_ts(o.created_at),
                enc_ts(o.updated_at),
                o.mode.as_str(),
                o.reason.as_str(),
                o.asset.as_str(),
                o.symbol,
                o.market.as_str(),
                o.side.as_str(),
                o.qty.to_string(),
                o.limit_price.map(|d| d.to_string()),
                o.currency.as_str(),
                o.status.as_str(),
                o.broker_order_id,
                o.filled_qty.to_string(),
                o.avg_fill_price.map(|d| d.to_string()),
                o.fees.to_string(),
                o.fees_estimated as i64,
                o.error
            ],
        )?;
        Ok(n == 1)
    }

    /// Overwrites the mutable fields of an existing order.
    pub fn update_order(&mut self, o: &OrderRecord) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE orders SET updated_at = ?2, status = ?3, broker_order_id = ?4, \
             filled_qty = ?5, avg_fill_price = ?6, fees = ?7, fees_estimated = ?8, \
             error = ?9, qty = ?10, limit_price = ?11 WHERE id = ?1",
            params![
                o.id.to_string(),
                enc_ts(o.updated_at),
                o.status.as_str(),
                o.broker_order_id,
                o.filled_qty.to_string(),
                o.avg_fill_price.map(|d| d.to_string()),
                o.fees.to_string(),
                o.fees_estimated as i64,
                o.error,
                o.qty.to_string(),
                o.limit_price.map(|d| d.to_string()),
            ],
        )?;
        Ok(())
    }

    pub fn get_order(&self, id: Uuid) -> Result<Option<OrderRecord>, StoreError> {
        query_opt(
            &self.conn,
            &format!("SELECT {ORDER_COLS} FROM orders WHERE id = ?1"),
            [id.to_string()],
            order_from_row,
        )
    }

    /// Orders whose status is not terminal, oldest first.
    pub fn open_orders(&self, mode: ExecMode) -> Result<Vec<OrderRecord>, StoreError> {
        let all = query_all(
            &self.conn,
            &format!(
                "SELECT {ORDER_COLS} FROM orders WHERE mode = ?1 ORDER BY created_at ASC, rowid ASC"
            ),
            [mode.as_str()],
            order_from_row,
        )?;
        Ok(all
            .into_iter()
            .filter(|o| !OrderStatus::is_terminal(o.status))
            .collect())
    }

    /// Orders with `from <= created_at <= to`, ascending.
    pub fn orders_between(
        &self,
        mode: ExecMode,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<OrderRecord>, StoreError> {
        query_all(
            &self.conn,
            &format!(
                "SELECT {ORDER_COLS} FROM orders WHERE mode = ?1 AND created_at >= ?2 \
                 AND created_at <= ?3 ORDER BY created_at ASC, rowid ASC"
            ),
            params![mode.as_str(), enc_ts(from), enc_ts(to)],
            order_from_row,
        )
    }

    // --- paper account ---

    pub fn paper_account(&self) -> Result<PaperAccount, StoreError> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT cash_ars, positions FROM paper_account WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((cash, positions)) => Ok(PaperAccount {
                cash_ars: dec_decimal(&cash)?,
                positions: dec_json::<BTreeMap<String, Decimal>>(&positions)?,
            }),
            None => Ok(PaperAccount::default()),
        }
    }

    pub fn save_paper_account(&mut self, a: &PaperAccount) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO paper_account (id, cash_ars, positions) VALUES (1, ?1, ?2) \
             ON CONFLICT(id) DO UPDATE SET cash_ars = excluded.cash_ars, \
             positions = excluded.positions",
            params![a.cash_ars.to_string(), serde_json::to_string(&a.positions)?],
        )?;
        Ok(())
    }

    // --- proposals ---

    pub fn insert_proposal(&mut self, p: &Proposal) -> Result<(), StoreError> {
        self.conn.execute(
            &format!(
                "INSERT INTO proposals ({PROPOSAL_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            ),
            params![
                p.id.to_string(),
                enc_ts(p.created_at),
                enc_ts(p.expires_at),
                p.kind.as_str(),
                p.status.as_str(),
                p.decided_at.map(enc_ts),
                serde_json::to_string(&p.payload)?,
                p.telegram_message_id
            ],
        )?;
        Ok(())
    }

    pub fn get_proposal(&self, id: Uuid) -> Result<Option<Proposal>, StoreError> {
        query_opt(
            &self.conn,
            &format!("SELECT {PROPOSAL_COLS} FROM proposals WHERE id = ?1"),
            [id.to_string()],
            proposal_from_row,
        )
    }

    pub fn set_proposal_status(
        &mut self,
        id: Uuid,
        status: ProposalStatus,
        at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE proposals SET status = ?2, decided_at = ?3 WHERE id = ?1",
            params![id.to_string(), status.as_str(), enc_ts(at)],
        )?;
        Ok(())
    }

    pub fn set_proposal_message(&mut self, id: Uuid, message_id: i64) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE proposals SET telegram_message_id = ?2 WHERE id = ?1",
            params![id.to_string(), message_id],
        )?;
        Ok(())
    }

    pub fn pending_proposals(&self) -> Result<Vec<Proposal>, StoreError> {
        query_all(
            &self.conn,
            &format!(
                "SELECT {PROPOSAL_COLS} FROM proposals WHERE status = 'pending' \
                 ORDER BY created_at ASC, rowid ASC"
            ),
            [],
            proposal_from_row,
        )
    }

    // --- flows ---

    /// Returns `false` when `external_id` was already stored.
    pub fn insert_flow(&mut self, f: &FlowRecord) -> Result<bool, StoreError> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO flows (at, mode, kind, amount_ars, external_id, note) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                enc_ts(f.at),
                f.mode.as_str(),
                f.kind.as_str(),
                f.amount_ars.to_string(),
                f.external_id,
                f.note
            ],
        )?;
        Ok(n == 1)
    }

    pub fn flows_between(
        &self,
        mode: ExecMode,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<FlowRecord>, StoreError> {
        query_all(
            &self.conn,
            "SELECT at, mode, kind, amount_ars, external_id, note FROM flows \
             WHERE mode = ?1 AND at >= ?2 AND at <= ?3 ORDER BY at ASC, id ASC",
            params![mode.as_str(), enc_ts(from), enc_ts(to)],
            flow_from_row,
        )
    }

    // --- decision log ---

    pub fn log_decision(
        &mut self,
        at: DateTime<Utc>,
        kind: &str,
        payload: &serde_json::Value,
    ) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO decisions (at, kind, payload) VALUES (?1, ?2, ?3)",
            params![enc_ts(at), kind, serde_json::to_string(payload)?],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Newest first.
    pub fn recent_decisions(&self, limit: usize) -> Result<Vec<DecisionRecord>, StoreError> {
        query_all(
            &self.conn,
            "SELECT id, at, kind, payload FROM decisions ORDER BY id DESC LIMIT ?1",
            [i64::try_from(limit).unwrap_or(i64::MAX)],
            decision_from_row,
        )
    }

    // --- backtest registry ---

    pub fn register_backtest(&mut self, run: &BacktestRun) -> Result<i64, StoreError> {
        self.conn.execute(
            "INSERT INTO backtest_runs \
             (created_at, config_hash, config, start, end, include_holdout, metrics) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                enc_ts(run.created_at),
                run.config_hash,
                serde_json::to_string(&run.config)?,
                enc_date(run.start),
                enc_date(run.end),
                run.include_holdout as i64,
                serde_json::to_string(&run.metrics)?
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Newest first.
    pub fn backtest_runs(&self) -> Result<Vec<BacktestRun>, StoreError> {
        query_all(
            &self.conn,
            "SELECT id, created_at, config_hash, config, start, end, include_holdout, metrics \
             FROM backtest_runs ORDER BY created_at DESC, id DESC",
            [],
            backtest_from_row,
        )
    }

    pub fn count_distinct_backtest_configs(&self) -> Result<u64, StoreError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(DISTINCT config_hash) FROM backtest_runs",
            [],
            |r| r.get(0),
        )?;
        u64::try_from(n).map_err(|e| corrupt("conteo", &n.to_string(), e))
    }
}
