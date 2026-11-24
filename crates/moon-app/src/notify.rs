//! Notification channels: Telegram, plain logs and an in-memory recorder for tests.

use std::sync::Mutex;

use moon_telegram::TelegramClient;
use uuid::Uuid;

#[async_trait::async_trait]
pub trait Notifier: Send + Sync {
    /// Sends a text message. Never fails: errors are logged.
    async fn notify(&self, text: &str);

    /// Sends a proposal with approve/reject buttons; returns the message id when there is one.
    async fn request_approval(&self, proposal_id: Uuid, text: &str) -> anyhow::Result<Option<i64>>;
}

pub struct TelegramNotifier {
    client: TelegramClient,
}

impl TelegramNotifier {
    pub fn new(client: TelegramClient) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl Notifier for TelegramNotifier {
    async fn notify(&self, text: &str) {
        if let Err(e) = self.client.send_text(text).await {
            tracing::error!("no se pudo enviar el aviso por Telegram: {e}");
        }
    }

    async fn request_approval(&self, proposal_id: Uuid, text: &str) -> anyhow::Result<Option<i64>> {
        let buttons = vec![
            ("✅ Aprobar".to_string(), format!("approve:{proposal_id}")),
            ("❌ Rechazar".to_string(), format!("reject:{proposal_id}")),
        ];
        let id = self.client.send_with_buttons(text, &buttons).await?;
        Ok(Some(id))
    }
}

/// Logs notifications through `tracing`; used when Telegram is disabled.
pub struct LogNotifier;

#[async_trait::async_trait]
impl Notifier for LogNotifier {
    async fn notify(&self, text: &str) {
        tracing::info!(target: "notify", "{text}");
    }

    async fn request_approval(&self, proposal_id: Uuid, text: &str) -> anyhow::Result<Option<i64>> {
        tracing::info!(target: "notify", "propuesta {proposal_id} pendiente de aprobación: {text}");
        Ok(None)
    }
}

/// Keeps every message in memory (tests).
#[derive(Default)]
pub struct RecordingNotifier {
    messages: Mutex<Vec<String>>,
    approvals: Mutex<Vec<Uuid>>,
    approval_texts: Mutex<Vec<String>>,
}

impl RecordingNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn messages(&self) -> Vec<String> {
        self.messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Texts of the approval requests, in order.
    pub fn approval_texts(&self) -> Vec<String> {
        self.approval_texts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn approvals(&self) -> Vec<Uuid> {
        self.approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

#[async_trait::async_trait]
impl Notifier for RecordingNotifier {
    async fn notify(&self, text: &str) {
        self.messages
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(text.to_string());
    }

    async fn request_approval(&self, proposal_id: Uuid, text: &str) -> anyhow::Result<Option<i64>> {
        self.approval_texts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(text.to_string());
        let mut a = self.approvals.lock().unwrap_or_else(|e| e.into_inner());
        a.push(proposal_id);
        Ok(Some(i64::try_from(a.len()).unwrap_or(i64::MAX)))
    }
}
