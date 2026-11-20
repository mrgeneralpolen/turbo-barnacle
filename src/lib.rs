//! Telegram-бот расписания колледжа (ТюмКЭУП, VK rasp_chpou).
//!
//! Порт Python-проекта helloban3 на Rust. Модули:
//! - `config` — переменные окружения (токены только из env, никогда в коде)
//! - `tyumen` — время Тюмени (UTC+5), даты ДД.ММ.ГГГГ, дни недели
//! - `parser` — чистые функции парсинга расписания (точная копия логики Python)
//! - `docx` — парсинг docx-таблиц (замены, звонки)
//! - `vk` — клиент VK API wall.get с экспоненциальным backoff
//! - `db` — rusqlite, схема ТОЧНО как в Python database.py
//! - `service` — ScheduleService: кэши, format_day_data, звонки, смена, файлы
//! - `bot` — хендлеры Telegram (teloxide), рассылки, watcher, cleaner
//! - `api` — Mini App HTTP API (те же пути и JSON, что в miniapp_api.py)
//! - `backup` — порт backup.py: ручной бэкап settings.db (SQLite backup API)
//! - `raw_api` — прямые вызовы Bot API (sendRichMessage, ephemeral,
//!   setWebhook), которых нет в teloxide-core 0.10.1
//! - `teachers` — данные преподавателей и маппинг групп display ↔ ключ
//!   (серверные JSON-ассеты, без капа «3 на предмет»)
//! - `reminders` — планировщик напоминаний о парах

pub mod api;
pub mod backup;
pub mod bot;
pub mod config;
pub mod db;
pub mod db_seed;
pub mod docx;
pub mod error;
pub mod parser;
pub mod raw_api;
pub mod reminders;
pub mod service;
pub mod teachers;
pub mod tyumen;
pub mod vk;

pub use error::AppError;
pub type Result<T> = std::result::Result<T, AppError>;
