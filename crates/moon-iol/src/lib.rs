//! IOL (InvertirOnline) API client for moon-trade.
//!
//! IOL has no sandbox: every order call is real. Credentials and tokens are never logged.

mod auth;
mod client;
mod dto;
mod error;

pub use client::{Country, DEFAULT_BASE_URL, IolClient};
pub use dto::*;
pub use error::{IolCredentials, IolError};
