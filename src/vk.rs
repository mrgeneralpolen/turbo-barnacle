//! Клиент VK API: GET wall.get с токеном из VK_TOKEN.
//!
//! Эталон: services.py update_check (~1237): URL
//! `https://api.vk.com/method/wall.get?domain={VK_DOMAIN}&access_token={token}&v=5.199&count={count}`.
//! При HTTP 429 или VK error 6 — экспоненциальный backoff:
//! задержка удваивается (с потолком как в Python) и ставится флаг
//! rate_limited; при успешном ответе флаг и задержка сбрасываются.
//!
//! Формула задержки из Python (update_check):
//! `min(30, 15 * (2 ** (consecutive_rate_limits - 1)))` секунд,
//! где consecutive_rate_limits — счётчик подряд идущих лимитов.
//! Здесь backoff НЕ спит сам: wall_get только фиксирует состояние
//! и возвращает ошибку (как Python, который возвращает set() и
//! оставляет планирование паузы вызывающему коду).

use crate::{AppError, Result};
use std::sync::RwLock;

#[derive(Debug, Clone, Default)]
pub struct VkAttachment {
    /// "photo" | "doc" | ...
    pub kind: String,
    /// URL фото / документа (для скачивания).
    pub url: Option<String>,
    /// Название документа (для docx).
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct VkPost {
    pub id: i64,
    pub date: i64,
    pub text: String,
    pub attachments: Vec<VkAttachment>,
}

/// Задержка backoff (секунды) после `attempts` подряд идущих лимитов.
/// Точная копия формулы из update_check: min(30, 15 * 2^(n-1)).
///
/// Чистая функция — для юнит-тестов без сети.
pub fn backoff_delay_for(attempts: u32) -> f64 {
    if attempts == 0 {
        return 0.0;
    }
    let delay = 15.0 * 2f64.powi(attempts.saturating_sub(1) as i32);
    delay.min(30.0)
}

pub struct VkClient {
    token: String,
    domain: String,
    client: reqwest::Client,
    rate_limited: RwLock<bool>,
    backoff_delay: RwLock<f64>,
    /// Счётчик подряд идущих 429 / error 6 (как consecutive_rate_limits).
    consecutive_rate_limits: RwLock<u32>,
}

impl VkClient {
    pub fn new(token: String, domain: String) -> Self {
        VkClient {
            token,
            domain,
            client: reqwest::Client::new(),
            rate_limited: RwLock::new(false),
            backoff_delay: RwLock::new(0.0),
            consecutive_rate_limits: RwLock::new(0),
        }
    }

    /// Зафиксировать очередной лимит (HTTP 429 или VK error 6):
    /// счётчик +1, флаг rate_limited, задержка по формуле.
    /// Публичный метод — его же вызывает wall_get; тесты проверяют
    /// состояние без сети.
    pub fn note_rate_limit(&self) {
        let mut n = self.consecutive_rate_limits.write().unwrap_or_else(|e| e.into_inner());
        *n += 1;
        let delay = backoff_delay_for(*n);
        if let Ok(mut d) = self.backoff_delay.write() {
            *d = delay;
        }
        if let Ok(mut f) = self.rate_limited.write() {
            *f = true;
        }
    }

    /// Сброс состояния после успешного ответа (как в update_check).
    pub fn reset_backoff(&self) {
        if let Ok(mut n) = self.consecutive_rate_limits.write() {
            *n = 0;
        }
        if let Ok(mut d) = self.backoff_delay.write() {
            *d = 0.0;
        }
        if let Ok(mut f) = self.rate_limited.write() {
            *f = false;
        }
    }

    /// wall.get, count постов. При 429 / error 6 — backoff, вернуть ошибку.
    /// При успехе — сбросить флаг и задержку.
    ///
    /// Порядок постов — как отдаёт VK (сначала новые), без разворота:
    /// разворот в хронологический порядок делает вызывающий код
    /// (в Python это `list(reversed(...))` в update_check перед
    /// process_vk_posts).
    pub async fn wall_get(&self, count: usize) -> Result<Vec<VkPost>> {
        let resp = self
            .client
            .get("https://api.vk.com/method/wall.get")
            .query(&[
                ("domain", self.domain.as_str()),
                ("access_token", self.token.as_str()),
                ("v", "5.199"),
                ("count", &count.to_string()),
            ])
            .send()
            .await
            .map_err(|e| AppError::vk(redact_vk_token(&e.to_string(), &self.token)))?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            self.note_rate_limit();
            return Err(AppError::vk(format!(
                "VK HTTP 429 Too Many Requests (backoff {}s)",
                self.backoff_delay()
            )));
        }
        let data: serde_json::Value = resp.json().await?;
        if let Some(err) = data.get("error") {
            let code = err.get("error_code").and_then(|c| c.as_i64());
            if code == Some(6) {
                self.note_rate_limit();
                return Err(AppError::vk(format!(
                    "VK API error 6 (too many requests, backoff {}s)",
                    self.backoff_delay()
                )));
            }
            return Err(AppError::vk(format!("VK API error: {err}")));
        }
        self.reset_backoff();
        let mut posts = Vec::new();
        if let Some(items) = data
            .get("response")
            .and_then(|r| r.get("items"))
            .and_then(|i| i.as_array())
        {
            for item in items {
                posts.push(parse_vk_post(item));
            }
        }
        Ok(posts)
    }

    pub fn is_rate_limited(&self) -> bool {
        *self.rate_limited.read().unwrap_or_else(|e| e.into_inner())
    }

    pub fn backoff_delay(&self) -> f64 {
        *self.backoff_delay.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Скачивание байтов (фото звонков, docx) с VK CDN.
    /// Аналог download_photo_bytes (services.py:537).
    /// Невалидный URL или не-200 — ошибка (Python в этом случае
    /// возвращает None; здесь Result, вызывающий код решает сам).
    pub async fn download_bytes(&self, url: &str) -> Result<Vec<u8>> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(AppError::vk(format!("bad download url: {url}")));
        }
        let resp = self.client.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(AppError::vk(format!("download failed: HTTP {status} for {url}")));
        }
        Ok(resp.bytes().await?.to_vec())
    }
}

/// Вырезает VK-токен из текста ошибки reqwest.
///
/// reqwest включает в `Display` полный URL, а wall.get несёт `access_token`
/// в query — без этого токен попадал бы в логи. Та же утечка, что у BOT_TOKEN
/// в raw_api (найдена живым тестом 20.11.2026).
fn redact_vk_token(msg: &str, token: &str) -> String {
    msg.replace(&format!("access_token={token}"), "access_token=<redacted>")
}

/// Разбор одного поста из ответа wall.get в VkPost.
/// Вложения: photo — URL самого большого размера из `sizes`
/// (VK отдаёт sizes по возрастанию; берём max по площади, при равных —
/// последний), doc — `url` + `title`. Остальные типы — только `kind`.
fn parse_vk_post(item: &serde_json::Value) -> VkPost {
    let id = item
        .get("id")
        .and_then(|v| v.as_i64())
        .or_else(|| {
            item.get("id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<i64>().ok())
        })
        .unwrap_or(0);
    let date = item.get("date").and_then(|v| v.as_i64()).unwrap_or(0);
    let text = item
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut attachments = Vec::new();
    if let Some(atts) = item.get("attachments").and_then(|a| a.as_array()) {
        for att in atts {
            let kind = att
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            match kind.as_str() {
                "photo" => {
                    let url = att
                        .get("photo")
                        .and_then(|p| p.get("sizes"))
                        .and_then(|s| s.as_array())
                        .and_then(|sizes| {
                            sizes
                                .iter()
                                .filter_map(|s| {
                                    let w = s.get("width").and_then(|v| v.as_i64()).unwrap_or(0);
                                    let h = s.get("height").and_then(|v| v.as_i64()).unwrap_or(0);
                                    let u = s.get("url").and_then(|v| v.as_str())?;
                                    Some((w.saturating_mul(h), u.to_string()))
                                })
                                .max_by_key(|(area, _)| *area)
                                .map(|(_, u)| u)
                        });
                    attachments.push(VkAttachment {
                        kind,
                        url,
                        title: None,
                    });
                }
                "doc" => {
                    let doc = att.get("doc");
                    attachments.push(VkAttachment {
                        kind,
                        url: doc
                            .and_then(|d| d.get("url"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                        title: doc
                            .and_then(|d| d.get("title"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string()),
                    });
                }
                _ => {
                    attachments.push(VkAttachment {
                        kind,
                        url: None,
                        title: None,
                    });
                }
            }
        }
    }
    VkPost {
        id,
        date,
        text,
        attachments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_vk_token_hides_token() {
        let msg = "error sending request for url (https://api.vk.com/method/wall.get?domain=rasp_chpou&access_token=vk1.a.SECRET123&v=5.199&count=10): operation timed out";
        let red = redact_vk_token(msg, "vk1.a.SECRET123");
        assert!(!red.contains("SECRET123"));
        assert!(red.contains("access_token=<redacted>"));
        assert!(red.contains("operation timed out"));
    }

    #[test]
    fn backoff_delay_formula_matches_python() {
        // min(30, 15 * 2^(n-1)): 15, 30, 30, 30, ...
        assert_eq!(backoff_delay_for(0), 0.0);
        assert_eq!(backoff_delay_for(1), 15.0);
        assert_eq!(backoff_delay_for(2), 30.0);
        assert_eq!(backoff_delay_for(3), 30.0);
        assert_eq!(backoff_delay_for(10), 30.0);
    }

    #[test]
    fn rate_limit_state_without_network() {
        let client = VkClient::new("test-token".to_string(), "rasp_chpou".to_string());
        assert!(!client.is_rate_limited());
        assert_eq!(client.backoff_delay(), 0.0);
        client.note_rate_limit();
        assert!(client.is_rate_limited());
        assert_eq!(client.backoff_delay(), 15.0);
        client.note_rate_limit();
        assert_eq!(client.backoff_delay(), 30.0);
        client.note_rate_limit();
        assert_eq!(client.backoff_delay(), 30.0);
        client.reset_backoff();
        assert!(!client.is_rate_limited());
        assert_eq!(client.backoff_delay(), 0.0);
        // после сброса счётчик начинается заново с 15
        client.note_rate_limit();
        assert_eq!(client.backoff_delay(), 15.0);
    }

    #[test]
    fn parse_vk_post_photo_doc() {
        let item: serde_json::Value = serde_json::json!({
            "id": 123,
            "date": 1793123456,
            "text": "Замена 20.11.2026",
            "attachments": [
                {"type": "photo", "photo": {"sizes": [
                    {"width": 100, "height": 100, "url": "https://cdn/small.jpg"},
                    {"width": 800, "height": 600, "url": "https://cdn/big.jpg"}
                ]}},
                {"type": "doc", "doc": {"title": "20.11.2026.docx", "url": "https://cdn/20.docx", "ext": "docx"}},
                {"type": "video", "video": {}}
            ]
        });
        let post = parse_vk_post(&item);
        assert_eq!(post.id, 123);
        assert_eq!(post.text, "Замена 20.11.2026");
        assert_eq!(post.attachments.len(), 3);
        assert_eq!(post.attachments[0].kind, "photo");
        assert_eq!(post.attachments[0].url.as_deref(), Some("https://cdn/big.jpg"));
        assert_eq!(post.attachments[1].kind, "doc");
        assert_eq!(post.attachments[1].url.as_deref(), Some("https://cdn/20.docx"));
        assert_eq!(post.attachments[1].title.as_deref(), Some("20.11.2026.docx"));
        assert_eq!(post.attachments[2].kind, "video");
        assert_eq!(post.attachments[2].url, None);
    }

    #[test]
    fn parse_vk_post_no_attachments() {
        let item: serde_json::Value =
            serde_json::json!({"id": 1, "date": 1, "text": "hello"});
        let post = parse_vk_post(&item);
        assert!(post.attachments.is_empty());
        assert_eq!(post.text, "hello");
    }
}
