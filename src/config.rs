//! Конфигурация из переменных окружения.
//!
//! Токены читаются ТОЛЬКО из env (или .env-файла в корне запуска —
//! как в Python: .env подхватывается, если переменная ещё не задана).
//! Токены никогда не хардкодятся и не пишутся в файлы.
//!
//! Переменные (как в Python):
//! - BOT_TOKEN (обязательно) — токен Telegram-бота
//! - VK_TOKEN (нужен для VK; без него update_check только логирует ошибку,
//!   как в Python) — токен VK API
//! - VK_DOMAIN (default "rasp_chpou")
//! - WEBHOOK_SECRET (default "") — секрет вебхука Telegram
//! - RAILWAY_PUBLIC_DOMAIN (default None) — хост для вебхука
//! - PORT (default 8080)
//! - DB_PATH (default "settings.db")

use crate::{AppError, Result};
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub bot_token: String,
    pub vk_token: String,
    pub vk_domain: String,
    pub webhook_secret: String,
    pub railway_domain: Option<String>,
    pub port: u16,
    pub db_path: String,
}

impl Config {
    /// Подхватывает .env (если есть) и читает переменные окружения.
    pub fn from_env() -> Result<Self> {
        load_dotenv();
        let bot_token = env::var("BOT_TOKEN").unwrap_or_default();
        if bot_token.is_empty() {
            return Err(AppError::config("BOT_TOKEN не задан в переменных окружения"));
        }
        let vk_token = env::var("VK_TOKEN").unwrap_or_default();
        let vk_domain = env::var("VK_DOMAIN").unwrap_or_else(|_| "rasp_chpou".to_string());
        let webhook_secret = env::var("WEBHOOK_SECRET").unwrap_or_default();
        let railway_domain = env::var("RAILWAY_PUBLIC_DOMAIN").ok().filter(|v| !v.is_empty());
        let port: u16 = env::var("PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8080);
        let db_path = env::var("DB_PATH").unwrap_or_else(|_| "settings.db".to_string());
        Ok(Config {
            bot_token,
            vk_token,
            vk_domain,
            webhook_secret,
            railway_domain,
            port,
            db_path,
        })
    }

    pub fn webhook_url(&self) -> Option<String> {
        self.railway_domain
            .as_ref()
            .map(|h| format!("https://{h}/webhook"))
    }

    pub fn webapp_url(&self) -> Option<String> {
        self.railway_domain
            .as_ref()
            .map(|h| format!("https://{h}/app/index.html"))
    }
}

/// Минимальный загрузчик .env: KEY=VALUE построчно, не перезаписывает
/// уже заданные переменные. Как в Python (main.py/services.py).
pub fn load_dotenv() {
    let path = std::path::Path::new(".env");
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || !line.contains('=') {
            continue;
        }
        let (k, v) = line.split_once('=').unwrap();
        let k = k.trim();
        let v = v.trim().trim_matches('"').trim_matches('\'');
        if !k.is_empty() && env::var(k).is_err() {
            env::set_var(k, v);
        }
    }
}
