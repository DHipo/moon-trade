pub mod backtest;
pub mod check;
pub mod data;
pub mod init;
pub mod paper;
pub mod pause;
pub mod plan;
pub mod status;

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail};
use moon_broker::{Broker, IolBroker, IolQuotes, PaperBroker, QuoteSource, YahooQuotes};
use moon_core::time::{Clock, SystemClock};
use moon_iol::{IolClient, IolCredentials};
use moon_store::{PaperAccount, Store};
use moon_telegram::TelegramClient;
use moon_yahoo::YahooClient;

use crate::config::{AppConfig, QuoteSourceKind, Secrets, TradingMode, validate_live};
use crate::engine::{Engine, EngineDeps, STATE_PAPER_INIT};
use crate::notify::{LogNotifier, Notifier, TelegramNotifier};

/// Telegram notifier when enabled and configured, otherwise a log-only one.
pub fn build_notifier(
    cfg: &AppConfig,
    secrets: &Secrets,
) -> (Arc<dyn Notifier>, Option<TelegramClient>) {
    if cfg.telegram.enabled {
        if let (Some(token), Some(chat_id)) = (&secrets.telegram_token, secrets.telegram_chat_id) {
            let client = TelegramClient::new(token.clone(), chat_id);
            return (
                Arc::new(TelegramNotifier::new(client.clone())),
                Some(client),
            );
        }
        tracing::warn!(
            "Telegram está habilitado pero faltan MOON_TELEGRAM_TOKEN o MOON_TELEGRAM_CHAT_ID: los avisos solo se registran en el log"
        );
    }
    (Arc::new(LogNotifier), None)
}

/// Seeds the paper account with the configured initial cash the first time.
fn seed_paper_account(store: &Mutex<Store>, cfg: &AppConfig) -> anyhow::Result<()> {
    let mut guard = store
        .lock()
        .map_err(|_| anyhow!("la base de datos quedó en un estado inconsistente"))?;
    if guard.get_state::<bool>(STATE_PAPER_INIT)?.unwrap_or(false) {
        return Ok(());
    }
    guard.save_paper_account(&PaperAccount {
        cash_ars: cfg.paper.initial_cash_ars,
        positions: Default::default(),
    })?;
    guard.set_state(STATE_PAPER_INIT, &true)?;
    Ok(())
}

/// Real wiring: store, quote source, broker (paper or IOL), notifier and system clock.
pub fn build_engine(
    cfg: AppConfig,
    secrets: &Secrets,
) -> anyhow::Result<(Engine, Option<TelegramClient>)> {
    validate_live(&cfg, secrets)?;
    let store = Arc::new(Mutex::new(Store::open(&cfg.database_path)?));
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);

    let iol_client = match (&secrets.iol_username, &secrets.iol_password) {
        (Some(username), Some(password)) => Some(Arc::new(IolClient::new(
            cfg.iol.base_url.clone(),
            IolCredentials {
                username: username.clone(),
                password: password.clone(),
            },
            clock.clone(),
        ))),
        _ => None,
    };

    let quotes: Arc<dyn QuoteSource> = match (cfg.execution.quote_source, &iol_client) {
        (QuoteSourceKind::Iol, Some(client)) => Arc::new(IolQuotes::new(
            client.clone(),
            cfg.execution.settlement_term,
        )),
        (QuoteSourceKind::Iol, None) => {
            tracing::warn!("sin credenciales de IOL: se usan cotizaciones de Yahoo");
            Arc::new(YahooQuotes::new(YahooClient::new()))
        }
        (QuoteSourceKind::Yahoo, _) => Arc::new(YahooQuotes::new(YahooClient::new())),
    };

    let broker: Arc<dyn Broker> = match cfg.mode {
        TradingMode::Paper => {
            seed_paper_account(&store, &cfg)?;
            Arc::new(PaperBroker::new(
                store.clone(),
                quotes.clone(),
                cfg.fees.clone(),
                cfg.execution.slippage_rate,
                clock.clone(),
            ))
        }
        TradingMode::LiveConfirm | TradingMode::LiveAuto => match &iol_client {
            Some(client) => Arc::new(IolBroker::new(
                client.clone(),
                cfg.execution.settlement_term,
                clock.clone(),
            )),
            None => bail!("El modo live requiere credenciales de IOL."),
        },
    };

    let (notifier, telegram) = build_notifier(&cfg, secrets);
    let engine = Engine::new(
        cfg,
        EngineDeps {
            store,
            broker,
            quotes,
            notifier,
            clock,
        },
    )?
    .with_data_sync(YahooClient::new());
    Ok((engine, telegram))
}
