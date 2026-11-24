//! The long-running service: scheduler loop plus the Telegram command/approval loop.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moon_core::time::art_date;
use moon_telegram::TelegramClient;

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

pub async fn serve(engine: Arc<Engine>, telegram: Option<TelegramClient>) -> anyhow::Result<()> {
    let notifier = engine.deps().notifier.clone();
    notifier
        .notify(&format!(
            "🟢 moon-trade iniciado en modo {}",
            mode_label(engine.config().mode)
        ))
        .await;

    let telegram_task = telegram.map(|client| {
        let engine = engine.clone();
        tokio::spawn(async move { telegram_loop(engine, client).await })
    });

    let mut recent_errors: BTreeMap<String, DateTime<Utc>> = BTreeMap::new();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = ticker.tick() => run_tick(&engine, &mut recent_errors).await,
        }
    }
    if let Some(task) = telegram_task {
        task.abort();
    }
    notifier.notify("🔴 moon-trade detenido").await;
    Ok(())
}

fn load_state(engine: &Engine) -> ScheduleState {
    match engine.db(|s| s.get_state::<ScheduleState>(STATE_SCHEDULE)) {
        Ok(state) => state.unwrap_or_default(),
        Err(e) => {
            tracing::error!("no se pudo leer el estado del planificador: {e}");
            ScheduleState::default()
        }
    }
}

fn save_state(engine: &Engine, state: &ScheduleState) {
    if let Err(e) = engine.db(|s| s.set_state(STATE_SCHEDULE, state)) {
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
        // Monthly is done on any Ok outcome; daily close and weekly report even on error.
        let finished = result.is_ok() || matches!(job, Job::DailyClose | Job::WeeklyReport);
        if finished {
            mark_done(&mut state, job, at);
        }
        save_state(engine, &state);
    }
}

async fn run_job(engine: &Engine, job: Job) -> anyhow::Result<()> {
    let today = art_date(Utc::now());
    match job {
        Job::MonthlyRebalance => engine.run_monthly(today).await.map(|_| ()),
        Job::RiskCheck => engine.run_risk_check().await.map(|_| ()),
        Job::DailyClose => engine.run_daily_close(today).await,
        Job::WeeklyReport => {
            let text = engine.weekly_report().await?;
            engine.deps().notifier.notify(&text).await;
            Ok(())
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
                    let action = handle_incoming(&engine, &update).await;
                    // Persist before replying: a processed update is never handled twice.
                    let next = update.update_id.saturating_add(1);
                    if let Err(e) = engine.db(|s| s.set_state(STATE_TELEGRAM_OFFSET, &next)) {
                        tracing::error!("no se pudo guardar el offset de Telegram: {e}");
                    }
                    apply_action(&client, action).await;
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
