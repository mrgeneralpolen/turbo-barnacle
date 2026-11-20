//! Прямые вызовы Telegram Bot API через reqwest — методы, которых нет
//! в teloxide-core 0.10.1 (проверено grep по исходникам библиотеки):
//! - `sendRichMessage` (Bot API 10.1): `rich_message: {html, skip_entity_detection}`;
//!   `editMessageText` тоже принимает `rich_message`.
//! - `ephemeral_message_parameters` (Bot API 10.3) в `sendMessage`/`sendPhoto`:
//!   сообщение видит только нажавший кнопку (`receiver_user_id`, `callback_query_id`).
//! - `setWebhook` — для не фатальной установки вебхука (см. `bot::serve`):
//!   ошибка не роняет процесс, повторяется в фоне с backoff.
//!
//! Wire-формат сверен с aiogram 3.31.0 (`send_rich_message`,
//! `EphemeralMessageParameters`, `edit_rich`) и Bot API 10.1–10.4.
//! Токен — из `Config` (env), никогда не пишется в логи и файлы.
//! Ошибки reqwest проходят через `redact_bot_token`: reqwest включает
//! в текст ошибки полный URL, а URL Bot API содержит токен.

use crate::AppError;
use serde_json::{json, Value};
use std::time::Duration;
use teloxide::types::Message;

/// Ошибки прямого вызова Bot API.
#[derive(Debug)]
pub enum RawApiError {
    /// Flood control: повторить через N секунд.
    RetryAfter(u64),
    /// Telegram вернул ok=false: код + описание.
    Api { code: i64, description: String },
    /// Транспорт (нет сети, таймаут...) — текст уже без токена
    /// (см. `redact_bot_token`): reqwest встраивает URL в ошибку.
    Transport(String),
    /// Ответ не парсится как JSON Bot API / не парсится Message.
    BadResponse(String),
}

impl std::fmt::Display for RawApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RawApiError::RetryAfter(s) => write!(f, "Telegram flood control: retry after {s}s"),
            RawApiError::Api { code, description } => {
                write!(f, "Telegram API {code}: {description}")
            }
            RawApiError::Transport(e) => write!(f, "Telegram request failed: {e}"),
            RawApiError::BadResponse(e) => write!(f, "bad Telegram response: {e}"),
        }
    }
}

impl std::error::Error for RawApiError {}

impl From<RawApiError> for AppError {
    fn from(e: RawApiError) -> Self {
        match e {
            RawApiError::Transport(e) => AppError::Telegram(e),
            other => AppError::Telegram(other.to_string()),
        }
    }
}

impl RawApiError {
    /// Секунды flood-контроля, если ошибка — RetryAfter.
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            RawApiError::RetryAfter(s) => Some(*s),
            _ => None,
        }
    }
}

/// Прямой клиент Bot API (reqwest). Токен не логируется.
#[derive(Debug, Clone)]
pub struct RawApi {
    client: reqwest::Client,
    token: String,
}

/// Граница multipart/form-data для загрузки фото байтами.
/// Фиксированная — тело детерминированно, что нужно тестам.
pub const MULTIPART_BOUNDARY: &str = "----schedule-bot-boundary-7f3a9c";

/// Вырезает токен из текста ошибки reqwest.
///
/// reqwest включает в `Display` полный URL запроса, а URL Bot API содержит
/// токен (`https://api.telegram.org/bot<TOKEN>/<method>`) — без этого токен
/// попадал бы в логи (найдено живым тестом 20.11.2026: строка set_webhook
/// в логе содержала полный токен). Заменяет всё между `bot` и следующим
/// `/` на `<redacted>`; сообщения без URL проходят как есть.
fn redact_bot_token(msg: &str) -> String {
    const MARKER: &str = "api.telegram.org/bot";
    let Some(start) = msg.find(MARKER) else {
        return msg.to_string();
    };
    let token_start = start + MARKER.len();
    let Some(rel_end) = msg[token_start..].find('/') else {
        return msg.to_string();
    };
    let mut out = String::with_capacity(msg.len());
    out.push_str(&msg[..token_start]);
    out.push_str("<redacted>");
    out.push_str(&msg[token_start + rel_end..]);
    out
}

impl RawApi {
    pub fn new(token: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self::with_client(token, client)
    }

    pub fn with_client(token: String, client: reqwest::Client) -> Self {
        Self { client, token }
    }

    fn url(&self, method: &str) -> String {
        format!("https://api.telegram.org/bot{}/{}", self.token, method)
    }

    async fn post_json(&self, method: &str, body: Value) -> Result<Value, RawApiError> {
        let resp = self
            .client
            .post(self.url(method))
            .json(&body)
            .send()
            .await
            .map_err(|e| RawApiError::Transport(redact_bot_token(&e.to_string())))?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| RawApiError::Transport(redact_bot_token(&e.to_string())))?;
        parse_bot_api_response(&bytes)
    }

    async fn post_multipart(
        &self,
        method: &str,
        boundary: &str,
        body: Vec<u8>,
    ) -> Result<Value, RawApiError> {
        let resp = self
            .client
            .post(self.url(method))
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .map_err(|e| RawApiError::Transport(redact_bot_token(&e.to_string())))?;
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| RawApiError::Transport(redact_bot_token(&e.to_string())))?;
        parse_bot_api_response(&bytes)
    }

    /// sendRichMessage (Bot API 10.1). reply_markup — уже сериализованный
    /// JSON клавиатуры (InlineKeyboardMarkup / ReplyKeyboardMarkup).
    pub async fn send_rich_message(
        &self,
        chat_id: i64,
        rich_html: &str,
        thread_id: Option<i64>,
        reply_markup: Option<Value>,
    ) -> Result<Message, RawApiError> {
        let result = self
            .post_json(
                "sendRichMessage",
                send_rich_message_body(chat_id, rich_html, thread_id, reply_markup),
            )
            .await?;
        parse_message(result)
    }

    /// editMessageText с rich_message (как Message.edit_rich в aiogram).
    pub async fn edit_rich_message(
        &self,
        chat_id: i64,
        message_id: i64,
        rich_html: &str,
        reply_markup: Option<Value>,
    ) -> Result<Message, RawApiError> {
        let result = self
            .post_json(
                "editMessageText",
                edit_rich_message_body(chat_id, message_id, rich_html, reply_markup),
            )
            .await?;
        parse_message(result)
    }

    /// sendMessage с ephemeral_message_parameters (Bot API 10.3).
    /// Текст — HTML (parse_mode=HTML, как default у бота в Python).
    pub async fn send_ephemeral_message(
        &self,
        chat_id: i64,
        text: &str,
        receiver_user_id: i64,
        callback_query_id: &str,
    ) -> Result<Message, RawApiError> {
        let result = self
            .post_json(
                "sendMessage",
                send_ephemeral_message_body(chat_id, text, receiver_user_id, callback_query_id),
            )
            .await?;
        parse_message(result)
    }

    /// Эфемерное sendPhoto по готовому file_id (JSON, без загрузки).
    pub async fn send_ephemeral_photo_by_file_id(
        &self,
        chat_id: i64,
        file_id: &str,
        caption: &str,
        receiver_user_id: i64,
        callback_query_id: &str,
    ) -> Result<Message, RawApiError> {
        let result = self
            .post_json(
                "sendPhoto",
                send_ephemeral_photo_body(
                    chat_id,
                    file_id,
                    caption,
                    receiver_user_id,
                    callback_query_id,
                ),
            )
            .await?;
        parse_message(result)
    }

    /// Эфемерное sendPhoto байтами (multipart/form-data, как
    /// BufferedInputFile в aiogram).
    pub async fn send_ephemeral_photo_bytes(
        &self,
        chat_id: i64,
        bytes: &[u8],
        caption: &str,
        receiver_user_id: i64,
        callback_query_id: &str,
    ) -> Result<Message, RawApiError> {
        let fields = [
            ("chat_id", chat_id.to_string()),
            ("caption", caption.to_string()),
            ("parse_mode", "HTML".to_string()),
            (
                "ephemeral_message_parameters",
                ephemeral_params_body(receiver_user_id, callback_query_id).to_string(),
            ),
        ];
        let refs: Vec<(&str, &str)> = fields
            .iter()
            .map(|(k, v)| (*k, v.as_str()))
            .collect();
        let body = build_multipart_body(MULTIPART_BOUNDARY, &refs, "photo", "bells.jpg", bytes);
        let result = self
            .post_multipart("sendPhoto", MULTIPART_BOUNDARY, body)
            .await?;
        parse_message(result)
    }

    /// setWebhook (как setup_webhook в teloxide: url, drop_pending_updates,
    /// secret_token; max_connections/certificate не задаются — как в serve()).
    pub async fn set_webhook(
        &self,
        url: &str,
        drop_pending_updates: bool,
        secret_token: &str,
    ) -> Result<(), RawApiError> {
        self.post_json(
            "setWebhook",
            set_webhook_body(url, drop_pending_updates, secret_token),
        )
        .await?;
        Ok(())
    }
}

/// Тело sendRichMessage. skip_entity_detection=true — как в throttled_answer
/// Python (rich_html уже готовый, сущности в нём — разметка, не текст).
pub fn send_rich_message_body(
    chat_id: i64,
    rich_html: &str,
    thread_id: Option<i64>,
    reply_markup: Option<Value>,
) -> Value {
    let mut body = json!({
        "chat_id": chat_id,
        "rich_message": {"html": rich_html, "skip_entity_detection": true},
    });
    if let Some(t) = thread_id {
        body["message_thread_id"] = json!(t);
    }
    if let Some(m) = reply_markup {
        body["reply_markup"] = m;
    }
    body
}

/// Тело editMessageText с rich_message.
pub fn edit_rich_message_body(
    chat_id: i64,
    message_id: i64,
    rich_html: &str,
    reply_markup: Option<Value>,
) -> Value {
    let mut body = json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "rich_message": {"html": rich_html, "skip_entity_detection": true},
    });
    if let Some(m) = reply_markup {
        body["reply_markup"] = m;
    }
    body
}

/// ephemeral_message_parameters (Bot API 10.3).
pub fn ephemeral_params_body(receiver_user_id: i64, callback_query_id: &str) -> Value {
    json!({
        "receiver_user_id": receiver_user_id,
        "callback_query_id": callback_query_id,
    })
}

/// Тело sendMessage с эфемерными параметрами.
pub fn send_ephemeral_message_body(
    chat_id: i64,
    text: &str,
    receiver_user_id: i64,
    callback_query_id: &str,
) -> Value {
    json!({
        "chat_id": chat_id,
        "text": text,
        "parse_mode": "HTML",
        "ephemeral_message_parameters": ephemeral_params_body(receiver_user_id, callback_query_id),
    })
}

/// Тело sendPhoto по file_id с эфемерными параметрами.
pub fn send_ephemeral_photo_body(
    chat_id: i64,
    photo_file_id: &str,
    caption: &str,
    receiver_user_id: i64,
    callback_query_id: &str,
) -> Value {
    json!({
        "chat_id": chat_id,
        "photo": photo_file_id,
        "caption": caption,
        "parse_mode": "HTML",
        "ephemeral_message_parameters": ephemeral_params_body(receiver_user_id, callback_query_id),
    })
}

/// Тело setWebhook.
pub fn set_webhook_body(url: &str, drop_pending_updates: bool, secret_token: &str) -> Value {
    json!({
        "url": url,
        "drop_pending_updates": drop_pending_updates,
        "secret_token": secret_token,
    })
}

/// Разбор ответа Bot API: ok=true → result; ok=false → Api/RetryAfter.
/// HTTP-статус не проверяется: Telegram возвращает JSON с ошибкой и при 4xx.
pub fn parse_bot_api_response(body: &[u8]) -> Result<Value, RawApiError> {
    let v: Value =
        serde_json::from_slice(body).map_err(|e| RawApiError::BadResponse(e.to_string()))?;
    if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
        return Ok(v.get("result").cloned().unwrap_or(Value::Null));
    }
    let code = v
        .get("error_code")
        .and_then(|c| c.as_i64())
        .unwrap_or(-1);
    let description = v
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("unknown error")
        .to_string();
    if code == 429 {
        let retry_after = v
            .get("parameters")
            .and_then(|p| p.get("retry_after"))
            .and_then(|r| r.as_u64())
            .unwrap_or(0);
        return Err(RawApiError::RetryAfter(retry_after));
    }
    Err(RawApiError::Api { code, description })
}

/// result ответа Bot API → teloxide Message.
fn parse_message(v: Value) -> Result<Message, RawApiError> {
    serde_json::from_value(v).map_err(|e| RawApiError::BadResponse(format!("bad Message: {e}")))
}

/// multipart/form-data тело: текстовые поля + один файл.
/// Поле ephemeral_message_parameters передаётся JSON-строкой (как в aiogram).
pub fn build_multipart_body(
    boundary: &str,
    fields: &[(&str, &str)],
    file_field: &str,
    file_name: &str,
    file_bytes: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in fields {
        out.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    out.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"{file_name}\"\r\nContent-Type: image/jpeg\r\n\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(file_bytes);
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_bot_token_hides_token() {
        let msg = "error sending request for url (https://api.telegram.org/bot123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11/setWebhook): error trying to connect: operation timed out";
        let red = redact_bot_token(msg);
        assert!(!red.contains("123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11"));
        assert!(red.contains("api.telegram.org/bot<redacted>/setWebhook"));
        assert!(red.contains("operation timed out"));
        // Transport в Display тоже не должен нести токен.
        let err = RawApiError::Transport(red.clone());
        assert!(!err.to_string().contains("123456:ABC"));
    }

    #[test]
    fn redact_bot_token_passthrough() {
        assert_eq!(redact_bot_token("plain error"), "plain error");
        assert_eq!(
            redact_bot_token("https://api.telegram.org/bot"),
            "https://api.telegram.org/bot"
        );
    }

    #[test]
    fn rich_message_body_fields() {
        let kb = json!({"inline_keyboard": [[{"text": "x", "callback_data": "y"}]]});
        let b = send_rich_message_body(-100, "<h3>T</h3>", Some(7), Some(kb.clone()));
        assert_eq!(b["chat_id"], json!(-100));
        assert_eq!(b["rich_message"]["html"], json!("<h3>T</h3>"));
        assert_eq!(b["rich_message"]["skip_entity_detection"], json!(true));
        assert_eq!(b["message_thread_id"], json!(7));
        assert_eq!(b["reply_markup"], kb);
        // Без топика и клавиатуры ключей нет.
        let b2 = send_rich_message_body(5, "x", None, None);
        assert!(b2.get("message_thread_id").is_none());
        assert!(b2.get("reply_markup").is_none());
    }

    #[test]
    fn edit_rich_message_body_fields() {
        let b = edit_rich_message_body(-100, 42, "<h3>T</h3>", None);
        assert_eq!(b["chat_id"], json!(-100));
        assert_eq!(b["message_id"], json!(42));
        assert_eq!(b["rich_message"]["html"], json!("<h3>T</h3>"));
        assert!(b.get("reply_markup").is_none());
    }

    #[test]
    fn ephemeral_bodies() {
        let b = send_ephemeral_message_body(-100, "текст", 123, "cb-1");
        assert_eq!(b["chat_id"], json!(-100));
        assert_eq!(b["text"], json!("текст"));
        assert_eq!(b["parse_mode"], json!("HTML"));
        assert_eq!(
            b["ephemeral_message_parameters"],
            json!({"receiver_user_id": 123, "callback_query_id": "cb-1"})
        );
        let p = send_ephemeral_photo_body(-100, "FILEID", "подпись", 123, "cb-1");
        assert_eq!(p["photo"], json!("FILEID"));
        assert_eq!(p["caption"], json!("подпись"));
        assert_eq!(
            p["ephemeral_message_parameters"]["receiver_user_id"],
            json!(123)
        );
    }

    #[test]
    fn set_webhook_body_fields() {
        let b = set_webhook_body("https://example.com/webhook", true, "s3cr3t");
        assert_eq!(b["url"], json!("https://example.com/webhook"));
        assert_eq!(b["drop_pending_updates"], json!(true));
        assert_eq!(b["secret_token"], json!("s3cr3t"));
    }

    #[test]
    fn parse_response_ok_and_errors() {
        let ok = parse_bot_api_response(br#"{"ok":true,"result":{"message_id":1}}"#).unwrap();
        assert_eq!(ok["message_id"], json!(1));
        // ok=true без result — Null, не ошибка.
        assert!(parse_bot_api_response(br#"{"ok":true}"#).unwrap().is_null());
        let err = parse_bot_api_response(
            br#"{"ok":false,"error_code":400,"description":"Bad Request: chat not found"}"#,
        )
        .unwrap_err();
        match err {
            RawApiError::Api { code, description } => {
                assert_eq!(code, 400);
                assert!(description.contains("chat not found"));
            }
            other => panic!("unexpected {other:?}"),
        }
        let ra = parse_bot_api_response(
            br#"{"ok":false,"error_code":429,"description":"Too Many Requests","parameters":{"retry_after":30}}"#,
        )
        .unwrap_err();
        assert_eq!(ra.retry_after_secs(), Some(30));
        // 429 без retry_after — 0 (пауза не задана, решает вызывающий).
        let ra0 = parse_bot_api_response(
            br#"{"ok":false,"error_code":429,"description":"Too Many Requests"}"#,
        )
        .unwrap_err();
        assert_eq!(ra0.retry_after_secs(), Some(0));
        // Мусор — BadResponse.
        assert!(matches!(
            parse_bot_api_response(b"not json"),
            Err(RawApiError::BadResponse(_))
        ));
        // ok=false без описания — Api с заглушкой.
        let e2 = parse_bot_api_response(br#"{"ok":false,"error_code":403}"#).unwrap_err();
        assert!(matches!(e2, RawApiError::Api { code: 403, .. }));
    }

    #[test]
    fn multipart_body_shape() {
        let fields = [
            ("chat_id", "-100"),
            ("caption", "подпись"),
            ("ephemeral_message_parameters", r#"{"receiver_user_id":1}"#),
        ];
        let bytes = [0xFF, 0xD8, 0xFF, 0xE0, 0x11, 0x22];
        let body = build_multipart_body("B", &fields, "photo", "bells.jpg", &bytes);
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with("--B\r\n"));
        assert!(text.contains("name=\"chat_id\"\r\n\r\n-100\r\n"));
        assert!(text.contains("name=\"ephemeral_message_parameters\"\r\n\r\n{\"receiver_user_id\":1}\r\n"));
        assert!(text.contains("name=\"photo\"; filename=\"bells.jpg\"\r\nContent-Type: image/jpeg\r\n\r\n"));
        assert!(text.ends_with("--B--\r\n"));
        // Байты файла — внутри тела как есть.
        assert!(body.windows(6).any(|w| w == bytes));
    }

    #[test]
    fn api_url_uses_token() {
        let api = RawApi::new("123:ABC".to_string());
        assert_eq!(
            api.url("sendMessage"),
            "https://api.telegram.org/bot123:ABC/sendMessage"
        );
    }
}
