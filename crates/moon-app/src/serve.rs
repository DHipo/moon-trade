//! The long-running service: scheduler loop plus the Telegram command/approval loop.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moon_core::time::art_date;
use moon_telegram::{IncomingKind, TelegramClient};

use crate::config::TradingMode;
use crate::engine::{Engine, STATE_SCHEDULE, STATE_TELEGRAM_OFFSET};
use crate::scheduler::{Job, ScheduleState, due_jobs, mark_attempt, mark_done};
use crate::telegram_bot::{BotAction, handle_incoming};

const TICK: Duration = Duration::from_secs(30);
const POLL_TIMEOUT_SECS: u32 = 25;
const POLL_RETRY: Duration = Duration::from_secs(10);
/// Identical error messages are notified at most once per this period.
const ERROR_REPEAT_HOURS: i64 = 1;

fn mode_label(mode: TradingMode) -> &'static str {
    match mode {
        TradingMode::Paper => "paper",
        TradingMode::LiveConfirm => "live_confirm",
        TradingMode::LiveAuto => "live_auto",
    }
}

fn job_label(job: Job) -> &'static str {
    match job {
        Job::MonthlyRebalance => "rebalanceo mensual",
        Job::RiskCheck => "control de riesgo",
        Job::DailyClose => "cierre diario",
        Job::WeeklyReport => "reporte semanal",
    }
}

/// Warnings to show right after the service starts.
pub fn startup_warnings(engine: &Engine) -> Vec<String> {
    let mut out = Vec::new();
    if engine.config().schedule.holidays.is_empty() {
        out.push(
            "⚠️ schedule.holidays está vacío: el bot no conoce los feriados de BYMA y puede intentar operar en días sin mercado. Cargalos en moon.toml."
                .to_string(),
        );
    }
    out
}

pub async fn serve(engine: Arc<Engine>, telegram: Option<TelegramClient>) -> anyhow::Result<()> {
    let telegram_task = telegram.map(|client| {
        let engine = engine.clone();
        tokio::spawn(async move { telegram_loop(engine, client).await })
    });
    run_service(
        engine,
        telegram_task,
        TICK,
        Duration::ZERO,
        tokio::signal::ctrl_c(),
    )
    .await
}

/// The scheduler loop. `telegram_task` is watched: if it ends or panics the service reports it
/// (best effort) and returns an error, so the OS service manager restarts the process.
/// `first_tick_delay` is how long to wait before the first scheduler tick.
pub async fn run_service<S>(
    engine: Arc<Engine>,
    mut telegram_task: Option<tokio::task::JoinHandle<()>>,
    tick: Duration,
    first_tick_delay: Duration,
    shutdown: S,
) -> anyhow::Result<()>
where
    S: std::future::Future,
{
    let notifier = engine.deps().notifier.clone();
    notifier
        .notify(&format!(
            "🟢 moon-trade iniciado en modo {}",
            mode_label(engine.config().mode)
        ))
        .await;
    for warning in startup_warnings(&engine) {
        notifier.notify(&warning).await;
    }

    let mut recent_errors: BTreeMap<String, DateTime<Utc>> = BTreeMap::new();
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + first_tick_delay, tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio::pin!(shutdown);
    let outcome = loop {
        tokio::select! {
            biased;
            ended = async {
                match telegram_task.as_mut() {
                    Some(task) => task.await,
                    None => std::future::pending().await,
                }
            } => {
                let detail = match ended {
                    Ok(()) => "terminó inesperadamente".to_string(),
                    Err(e) if e.is_panic() => "falló (panic)".to_string(),
                    Err(e) => format!("se canceló ({e})"),
                };
                notifier
                    .notify(&format!(
                        "🔴 El servicio de Telegram {detail}: moon-trade se detiene para que el sistema lo reinicie."
                    ))
                    .await;
                telegram_task = None;
                break Err(anyhow::anyhow!("la tarea de Telegram {detail}"));
            }
            _ = &mut shutdown => break Ok(()),
            _ = ticker.tick() => run_tick(&engine, &mut recent_errors).await,
        }
    };
    if let Some(task) = telegram_task {
        task.abort();
    }
    if outcome.is_ok() {
        notifier.notify("🔴 moon-trade detenido").await;
    }
    outcome
}

fn load_state(engine: &Engine) -> ScheduleState {
    match engine.db(|s| s.get_state::<ScheduleState>(&engine.state_key(STATE_SCHEDULE))) {
        Ok(state) => state.unwrap_or_default(),
        Err(e) => {
            tracing::error!("no se pudo leer el estado del planificador: {e}");
            ScheduleState::default()
        }
    }
}

fn save_state(engine: &Engine, state: &ScheduleState) {
    if let Err(e) = engine.db(|s| s.set_state(&engine.state_key(STATE_SCHEDULE), state)) {
        tracing::error!("no se pudo guardar el estado del planificador: {e}");
    }
}

async fn run_tick(engine: &Engine, recent_errors: &mut BTreeMap<String, DateTime<Utc>>) {
    let now = Utc::now();
    let paused = match engine.is_paused() {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("no se pudo leer la pausa: {e}");
            false
        }
    };
    let mut state = load_state(engine);
    for job in due_jobs(now, &engine.config().schedule, &state, paused) {
        let at = Utc::now();
        mark_attempt(&mut state, job, at);
        // Persist the attempt first so a crash never re-runs the job immediately.
        save_state(engine, &state);
        let result = run_job(engine, job).await;
        if let Err(e) = &result {
            notify_error(engine, recent_errors, job_label(job), &format!("{e:#}")).await;
        }
        // Monthly is done only when complete (see `MonthlyOutcome::is_complete`); daily close and
        // weekly report even on error. A risk check never "finishes".
        let finished = match &result {
            Ok(done) => *done,
            Err(_) => matches!(job, Job::DailyClose | Job::WeeklyReport),
        };
        if finished {
            mark_done(&mut state, job, at);
        }
        save_state(engine, &state);
    }
}

/// Runs one job; `Ok(true)` means it is finished for its period.
async fn run_job(engine: &Engine, job: Job) -> anyhow::Result<bool> {
    let today = art_date(Utc::now());
    match job {
        Job::MonthlyRebalance => engine
            .run_monthly(today)
            .await
            .map(|outcome| outcome.is_complete()),
        Job::RiskCheck => {
            let result = engine.run_risk_check().await.map(|_| true);
            // Deposit detection is a separate best-effort step: it never blocks the risk check.
            if let Err(e) = engine.check_deposits().await {
                tracing::warn!("no se pudieron revisar los depósitos: {e:#}");
            }
            result
        }
        Job::DailyClose => engine.run_daily_close(today).await.map(|()| true),
        Job::WeeklyReport => {
            let text = engine.weekly_report().await?;
            engine.deps().notifier.notify(&text).await;
            Ok(true)
        }
    }
}

async fn notify_error(
    engine: &Engine,
    recent: &mut BTreeMap<String, DateTime<Utc>>,
    task: &str,
    detail: &str,
) {
    let text = format!("⚠️ Error en {task}: {detail}");
    tracing::error!("{text}");
    let now = Utc::now();
    recent.retain(|_, at| now - *at < chrono::Duration::hours(ERROR_REPEAT_HOURS));
    if recent.contains_key(&text) {
        return;
    }
    recent.insert(text.clone(), now);
    engine.deps().notifier.notify(&text).await;
}

async fn telegram_loop(engine: Arc<Engine>, client: TelegramClient) {
    loop {
        let offset = engine
            .db(|s| s.get_state::<i64>(STATE_TELEGRAM_OFFSET))
            .ok()
            .flatten()
            .unwrap_or(0);
        match client.poll(offset, POLL_TIMEOUT_SECS).await {
            Ok(updates) => {
                for update in updates {
                    // Persist before handling: a processed update is never handled twice.
                    let next = update.update_id.saturating_add(1);
                    if let Err(e) = engine.db(|s| s.set_state(STATE_TELEGRAM_OFFSET, &next)) {
                        tracing::error!("no se pudo guardar el offset de Telegram: {e}");
                    }
                    if matches!(update.kind, IncomingKind::Callback { .. }) {
                        // An approval can wait for the trading lock for minutes: handle it in its
                        // own task so commands (/estado, /pausa) keep being answered.
                        let (engine, client) = (engine.clone(), client.clone());
                        tokio::spawn(async move {
                            let action = handle_incoming(&engine, &update).await;
                            apply_action(&client, action).await;
                        });
                    } else {
                        let action = handle_incoming(&engine, &update).await;
                        apply_action(&client, action).await;
                    }
                }
            }
            Err(e) => {
                // A malformed update can make every batch fail: do not spin.
                tracing::error!("error al consultar Telegram: {e}");
                tokio::time::sleep(POLL_RETRY).await;
            }
        }
    }
}

async fn apply_action(client: &TelegramClient, action: BotAction) {
    match action {
        BotAction::Reply(text) => {
            if let Err(e) = client.send_text(&text).await {
                tracing::error!("no se pudo responder por Telegram: {e}");
            }
        }
        BotAction::CallbackDone {
            callback_id,
            message_id,
            text,
        } => {
            if let Err(e) = client.answer_callback(&callback_id, &text).await {
                tracing::error!("no se pudo responder el botón de Telegram: {e}");
            }
            if let Some(id) = message_id
                && let Err(e) = client.edit_text(id, &text).await
            {
                tracing::error!("no se pudo editar el mensaje de Telegram: {e}");
            }
        }
        BotAction::Ignore => {}
    }
}
