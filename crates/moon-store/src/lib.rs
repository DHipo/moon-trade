//! SQLite persistence for moon-trade.

mod queries;
pub mod records;
mod schema;

pub use queries::Store;
pub use records::*;

/// Errors raised by the persistence layer.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("dato corrupto en la base: {0}")]
    Corrupt(String),
}
