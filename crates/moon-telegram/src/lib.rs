//! Telegram Bot API client for moon-trade (notifications, approvals, commands).

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_BASE_URL: &str = "https://api.telegram.org";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_TIMEOUT_MARGIN_SECS: u64 = 10;

/// Maximum characters Telegram accepts in a text message.
pub const MAX_TEXT: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum TelegramError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Telegram respondió error {code}: {description}")]
    Api { code: i64, description: String },
    #[error("respuesta inválida de Telegram: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Incoming {
    pub update_id: i64,
    pub kind: IncomingKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IncomingKind {
    Command {
        name: String,
        args: String,
    },
    Callback {
        id: String,
        data: String,
        message_id: Option<i64>,
    },
    Other,
}

/// Truncates text above `MAX_TEXT` characters to the first `MAX_TEXT - 1` plus an ellipsis.
pub fn truncate_text(text: &str) -> String {
    if text.chars().count() <= MAX_TEXT {
        return text.to_string();
    }
    let mut out: String = text.chars().take(MAX_TEXT - 1).collect();
    out.push('…');
    out
}

#[derive(Clone)]
pub struct TelegramClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
    chat_id: i64,
}

impl fmt::Debug for TelegramClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TelegramClient")
            .field("base_url", &self.base_url)
            .field("token", &"***")
            .field("chat_id", &self.chat_id)
            .finish()
    }
}

#[derive(Deserialize)]
struct ApiResponse {
    ok: bool,
    result: Option<Value>,
    error_code: Option<i64>,
    description: Option<String>,
}

#[derive(Deserialize)]
struct Update {
    message: Option<Message>,
    callback_query: Option<CallbackQuery>,
}

#[derive(Deserialize)]
struct Message {
    message_id: i64,
    chat: Chat,
    text: Option<String>,
}

#[derive(Deserialize)]
struct User {
    id: i64,
}

#[derive(Deserialize)]
struct Chat {
    id: i64,
}

#[derive(Deserialize)]
struct CallbackQuery {
    id: String,
    from: Option<User>,
    data: Option<String>,
    message: Option<Message>,
}

/// Strips the request URL (it embeds the bot token) from a reqwest error.
fn http_err(err: reqwest::Error) -> TelegramError {
    TelegramError::Http(err.without_url())
}

impl TelegramClient {
    pub fn new(token: impl Into<String>, chat_id: i64) -> Self {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .unwrap_or_else(|e| {
                tracing::warn!(
                    "no se pudo construir el cliente HTTP de Telegram: {}",
                    e.without_url()
                );
                reqwest::Client::new()
            });
        Self {
            http,
            base_url: DEFAULT_BASE_URL.to_string(),
            token: token.into(),
            chat_id,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into().trim_end_matches('/').to_string();
        self
    }

    pub fn chat_id(&self) -> i64 {
        self.chat_id
    }

    fn url(&self, method: &str) -> String {
        format!("{}/bot{}/{}", self.base_url, self.token, method)
    }

    async fn call(
        &self,
        method: &str,
        body: &Value,
        timeout: Option<Duration>,
    ) -> Result<Value, TelegramError> {
        let mut req = self.http.post(self.url(method)).json(body);
        if let Some(t) = timeout {
            req = req.timeout(t);
        }
        let resp = req.send().await.map_err(http_err)?;
        let bytes = resp.bytes().await.map_err(http_err)?;
        let parsed: ApiResponse = serde_json::from_slice(&bytes)
            .map_err(|e| TelegramError::Parse(format!("{method}: {e}")))?;
        if !parsed.ok {
            return Err(TelegramError::Api {
                code: parsed.error_code.unwrap_or(0),
                description: parsed.description.unwrap_or_default(),
            });
        }
        parsed
            .result
            .ok_or_else(|| TelegramError::Parse(format!("{method}: falta result")))
    }

    async fn send_message(&self, body: Value) -> Result<i64, TelegramError> {
        let result = self.call("sendMessage", &body, None).await?;
        result
            .get("message_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| TelegramError::Parse("sendMessage: falta message_id".to_string()))
    }

    pub async fn send_text(&self, text: &str) -> Result<i64, TelegramError> {
        self.send_message(json!({"chat_id": self.chat_id, "text": truncate_text(text)}))
            .await
    }

    pub async fn send_with_buttons(
        &self,
        text: &str,
        buttons: &[(String, String)],
    ) -> Result<i64, TelegramError> {
        let row: Vec<Value> = buttons
            .iter()
            .map(|(label, data)| json!({"text": label, "callback_data": data}))
            .collect();
        self.send_message(json!({
            "chat_id": self.chat_id,
            "text": truncate_text(text),
            "reply_markup": {"inline_keyboard": [row]},
        }))
        .await
    }

    pub async fn edit_text(&self, message_id: i64, text: &str) -> Result<(), TelegramError> {
        self.call(
            "editMessageText",
            &json!({
                "chat_id": self.chat_id,
                "message_id": message_id,
                "text": truncate_text(text),
                "reply_markup": {"inline_keyboard": []},
            }),
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn answer_callback(
        &self,
        callback_id: &str,
        text: &str,
    ) -> Result<(), TelegramError> {
        self.call(
            "answerCallbackQuery",
            &json!({"callback_query_id": callback_id, "text": text}),
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn poll(
        &self,
        offset: i64,
        timeout_secs: u32,
    ) -> Result<Vec<Incoming>, TelegramError> {
        let body = json!({
            "offset": offset,
            "timeout": timeout_secs,
            "allowed_updates": ["message", "callback_query"],
        });
        let timeout = Duration::from_secs(u64::from(timeout_secs) + POLL_TIMEOUT_MARGIN_SECS);
        let result = self.call("getUpdates", &body, Some(timeout)).await?;
        let Value::Array(items) = result else {
            return Err(TelegramError::Parse(
                "getUpdates: se esperaba una lista".to_string(),
            ));
        };
        // Each update is parsed on its own: one malformed update must not block the others
        // nor stall the offset.
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            let Some(update_id) = item.get("update_id").and_then(Value::as_i64) else {
                tracing::warn!("update de Telegram sin update_id legible; se omite");
                continue;
            };
            let kind = match serde_json::from_value::<Update>(item) {
                Ok(u) => self.classify(u),
                Err(e) => {
                    tracing::warn!(update_id, "update de Telegram malformado: {e}");
                    IncomingKind::Other
                }
            };
            out.push(Incoming { update_id, kind });
        }
        Ok(out)
    }

    /// Anything not coming from the configured chat becomes `Other`.
    fn classify(&self, update: Update) -> IncomingKind {
        if let Some(cb) = update.callback_query {
            let chat = cb.message.as_ref().map(|m| m.chat.id);
            let from = cb.from.as_ref().map(|u| u.id);
            return match (chat, from, cb.data) {
                (Some(chat), Some(from), Some(data))
                    if chat == self.chat_id && from == self.chat_id =>
                {
                    IncomingKind::Callback {
                        id: cb.id,
                        data,
                        message_id: cb.message.map(|m| m.message_id),
                    }
                }
                _ => IncomingKind::Other,
            };
        }
        if let Some(msg) = update.message {
            if msg.chat.id != self.chat_id {
                return IncomingKind::Other;
            }
            if let Some(text) = msg.text {
                return parse_command(&text).unwrap_or(IncomingKind::Other);
            }
        }
        IncomingKind::Other
    }
}

fn parse_command(text: &str) -> Option<IncomingKind> {
    let rest = text.trim().strip_prefix('/')?;
    let (head, args) = match rest.split_once(char::is_whitespace) {
        Some((h, a)) => (h, a.trim()),
        None => (rest, ""),
    };
    let name = head.split('@').next().unwrap_or("");
    if name.is_empty() {
        return None;
    }
    Some(IncomingKind::Command {
        name: name.to_string(),
        args: args.to_string(),
    })
}
