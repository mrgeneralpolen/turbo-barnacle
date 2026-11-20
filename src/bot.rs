//! Telegram-бот (teloxide): хендлеры, рассылки, фоновые задачи, сервер.
//!
//! Эталон: main.py (1598 строк) + пропатченный main.py из schedule-miniapp
//! (menu button + кнопка «📱 Открыть расписание» в /start + reminder loop).
//!
//! Что портировано точно (тексты, эмодзи, кнопки, callback_data):
//! - Команды /start, /set, /help; кнопки «Расписание» / «На завтра» / «Звонки»
//!   (+ варианты с эмодзи); дата `RE_DATE_QUERY` `^(\d{1,2})\.(\d{1,2})(?:\.(\d{2,4}))?$`
//! - Callback'и: `set_grp:`, `bells_photo:`, `ephemeral_rasp:`,
//!   `ephemeral_bells:`, `widget_tab:`, `zvonki_date:`
//! - Клавиатуры: main_kb, spec_26_21_kb, zvonki_tomorrow_kb,
//!   build_day_update_inline_kb; тексты /start, /help, check_user_group, /set
//! - broadcast_day_updates: дедупликация событий («schedule» приоритетнее
//!   «change» для той же (группа, дата)), фильтр по наличию пар и по дате
//!   (>= сегодня), рассылка по чатам группы
//! - vk_watcher: старт через 5 с; 5 с в пик (7–9, 18–21 по Тюмени), иначе
//!   10 с; работа только 6–21ч; при vk_rate_limited — max(15, min(30, backoff))
//! - cleaner: старт через 10 с, раз в час: старые пины, clean_expired_files(14),
//!   clean_old_archive(45), load_ram
//! - setup_bot_commands: /start, /set, /help + menu button WebApp «Расписание»
//! - serve: один axum-сервер: GET / — health
//!   `{"status":"ok","service":"schedule_bot_next_gen"}`; POST /webhook;
//!   /api/* (api::router); /app/* — статика webapp/
//!
//! Расхождения с Python (вынужденные, см. также финальный отчёт):
//! 1. Rich Messages (aiogram 3.31: `answer_rich` / `send_rich_message`,
//!    `<table>`, `<tg-time>`, `<details>`) в teloxide 0.13 НЕТ — это
//!    aiogram-only. Отправка идёт прямым запросом к Bot API через
//!    `crate::raw_api::RawApi` (sendRichMessage, Bot API 10.1) с тем же
//!    fallback, что в Python (обычный HTML + fallback-клавиатура).
//!    Рендер rich_html — `render_rich_table_html` и др. в service.rs.
//! 2. Эфемерные сообщения (aiogram `EphemeralMessageParameters`, Bot API 10.3)
//!    в teloxide-core 0.10.1 НЕ поддерживаются — отправляются тоже через
//!    RawApi (sendMessage/sendPhoto с `ephemeral_message_parameters`);
//!    при ошибке — alert ≤195 символов, как fallback-ветка Python.
//! 3. `CopyTextButton` (build_copy_kb) в teloxide-core 0.10.1 отсутствует;
//!    в Python эта клавиатура нигде не вызывается, поведение не меняется.
//! 4. Файловый file_id фото звонков: `get_bells_photo_input` отдаёт
//!    `BellsPhotoInput::FileId` (если захвачен) или `Bytes`; после первой
//!    отправки фото file_id захватывается из `Message::photo()` и
//!    кэшируется (`cache_bells_photo_file_id`). В рассылке —
//!    последовательный probe (как в Python): первый чат с коротким
//!    текстом (≤1024) отправляет фото байтами, остальные — по file_id.
//!    `safe_send_day_update` возвращает захваченный file_id (Option).
//! 5. `DEAD_CHAT_ERRORS` в service.rs содержит только 4 подстроки против 15
//!    в services.py:141 — здесь используется полный список из Python
//!    (`DEAD_CHAT_ERRORS_FULL`).
//! 6. Семафоры `user_limit`/`broadcast_limit` из Python не переносятся:
//!    teloxide Dispatcher по умолчанию распределяет апдейты по чатам
//!    (один чат — последовательная обработка).
//! 7. Кэш build_resp/render в Python — отдельные lru/alru-кэши. В Rust
//!    аналога нет: после обновлений VK вызывается `service.load_ram()`
//!    (перечитывает архив из БД, стирая устаревшие записи). Очищать
//!    `archive_cache` напрямую НЕЛЬЗЯ — там же лежат свежие данные,
//!    только что записанные update_check.
//!
//! Webhook (teloxide 0.13, фича webhooks-axum, axum 0.7):
//! `webhooks::axum_no_setup(options)` — Router с POST /webhook без вызова
//! set_webhook (он монтируется в общий axum-сервер на путь из
//! `Options::path`, по умолчанию — путь из url, здесь «/webhook»).
//! set_webhook НЕ фатален: вызывается отдельно через RawApi в фоновой
//! задаче `run_webhook_setup_loop` с backoff (5/15/30/60/120/300 с) —
//! при недоступном Telegram/невалидном токене процесс продолжает работу,
//! HTTP-сервер и Mini App API поднимаются, webhook ставится позже.
//! При валидном токене поведение не меняется (первая попытка — сразу).
//! Проверка секрета — внутри teloxide: заголовок
//! `X-Telegram-Bot-Api-Secret-Token` сравнивается с `Options::secret_token`
//! (401 при несовпадении). При остановке listener'а webhook удаляется
//! (delete_webhook), как делал stop_future у axum_to_router.
//! В режиме polling — `delete_webhook` + `Dispatcher::dispatch()`.

use crate::api::{self, ApiState};
use crate::config::Config;
use crate::db::Db;
use crate::parser::{extract_pair_numbers, normalize_group, DayData};
use crate::raw_api::{RawApi, RawApiError};
use crate::reminders::{ReminderRunner, ReminderSender};
use crate::service::{BellsPhotoInput, ScheduleService, DEAD_CHAT_ERRORS as SERVICE_DEAD_CHAT_ERRORS};
use crate::tyumen::{fmt_date, tyumen_now, weekday_name};
use crate::{AppError, Result};
use axum::extract::Request;
use axum::http::HeaderValue;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use axum::{Json, Router};
use std::net::SocketAddr;
use chrono::{Datelike, NaiveDate, Timelike};
use regex::Regex;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use teloxide::dispatching::UpdateHandler;
use teloxide::prelude::*;
use teloxide::types::{
    BotCommand, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, KeyboardButton,
    KeyboardMarkup as ReplyKeyboardMarkup, LinkPreviewOptions, MenuButton, MessageId, ParseMode,
    ReplyMarkup, ThreadId, WebAppInfo,
};
use teloxide::update_listeners::webhooks;
use tower_http::services::ServeDir;

// ============================== КОНСТАНТЫ ==============================

/// Тексты кнопок «Расписание» (main.py: RASP_BTN_TEXTS).
pub const RASP_BTN_TEXTS: &[&str] = &["Расписание", "📅 Моё расписание"];
/// Тексты кнопок «На завтра» (main.py: TOMORROW_BTN_TEXTS).
pub const TOMORROW_BTN_TEXTS: &[&str] = &["На завтра", "❓ Что на завтра?"];
/// Тексты кнопок «Звонки» (main.py: ZVONKI_BTN_TEXTS).
pub const ZVONKI_BTN_TEXTS: &[&str] = &["Звонки", "🔔 Звонки"];

/// Полный список подстрок мёртвого чата (services.py:141). В service.rs
/// константа DEAD_CHAT_ERRORS неполная (4 из 15) — здесь полный список.
pub const DEAD_CHAT_ERRORS_FULL: &[&str] = &[
    "topic_closed",
    "topic_deleted",
    "chat not found",
    "chat_write_forbidden",
    "bot was blocked",
    "deactivated",
    "kicked",
    "not a member",
    "chat was deleted",
    "user is deactivated",
    " have no rights to send a message",
    "group chat was upgraded to a supergroup chat",
    "upgraded to a supergroup",
    "migrated to a supergroup",
    "peer_id_invalid",
];

fn date_query_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(\d{1,2})\.(\d{1,2})(?:\.(\d{2,4}))?$").unwrap())
}

fn valid_group_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // main.py: RE_VALID_GROUP = re.compile(r"([2-9]\d)\s*[-. \s−]\s*(\d{1,2})")
    RE.get_or_init(|| Regex::new(r"([2-9]\d)\s*[-. \s−]\s*(\d{1,2})").unwrap())
}

fn cancel_words_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // services.py get_group_day_schedule: \b(?:нет|отмен\w*|снят\w*)\b, IGNORECASE
    RE.get_or_init(|| Regex::new(r"(?i)(?:нет|отмен\w*|снят\w*)").unwrap())
}

/// Ошибка Telegram означает мёртвый чат (проверка по полному списку Python).
pub fn is_dead_chat_error(err_text: &str) -> bool {
    let lower = err_text.to_lowercase();
    DEAD_CHAT_ERRORS_FULL.iter().any(|m| lower.contains(m))
        || SERVICE_DEAD_CHAT_ERRORS.iter().any(|m| lower.contains(m))
}

// ============================== HTML-УТИЛИТЫ ==============================

/// Экранирование как html.escape в Python.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

/// Убрать HTML-теги (для текста alert'а callback'а, как в Python).
pub fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Чистка строки изменения как в build_resp: убрать <b> и ⚠️/ℹ️ в начале.
pub fn clean_change_line(s: &str) -> String {
    let mut t = s.replace("<b>", "").replace("</b>", "");
    loop {
        let trimmed = t.trim_start();
        if let Some(rest) = trimmed.strip_prefix('⚠') {
            // ⚠️ — это U+26A0 + variation selector U+FE0F
            let rest = rest.strip_prefix('\u{fe0f}').unwrap_or(rest);
            t = rest.trim_start().to_string();
        } else if let Some(rest) = trimmed.strip_prefix('ℹ') {
            let rest = rest.strip_prefix('\u{fe0f}').unwrap_or(rest);
            t = rest.trim_start().to_string();
        } else {
            break;
        }
    }
    t.trim().to_string()
}

// ============================== ЧИСТЫЕ ПАРСЕРЫ ==============================

/// Команда бота, распарсенная из текста сообщения (аналог aiogram Command).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedCommand {
    Start,
    Help,
    /// /set с аргументом (None — без аргумента).
    Set(Option<String>),
}

/// Разбор "/start", "/set 24-27", "/set@botname ИП-24-27" и т.п.
/// Возвращает None, если текст — не команда start/help/set.
pub fn parse_command(text: &str) -> Option<ParsedCommand> {
    let rest = text.strip_prefix('/')?;
    let (head, args) = match rest.split_once(|c: char| c.is_whitespace()) {
        Some((h, a)) => (h, a.trim()),
        None => (rest, ""),
    };
    let name = head.split('@').next().unwrap_or("");
    match name {
        "start" => Some(ParsedCommand::Start),
        "help" => Some(ParsedCommand::Help),
        "set" => Some(ParsedCommand::Set(if args.is_empty() {
            None
        } else {
            Some(args.to_string())
        })),
        _ => None,
    }
}

/// Части даты из RE_DATE_QUERY: (день, месяц, год). Год: без года — текущий
/// год Тюмени, 2 цифры — 20xx. Возвращает None, если текст не подходит под
/// шаблон (даже если дата календарно неверна — это проверяет вызывающий).
pub fn parse_date_parts(text: &str) -> Option<(u32, u32, i32)> {
    let caps = date_query_re().captures(text.trim())?;
    let d: u32 = caps[1].parse().ok()?;
    let m: u32 = caps[2].parse().ok()?;
    let y: i32 = match caps.get(3) {
        None => tyumen_now().year(),
        Some(y) => {
            let s = y.as_str();
            let v: i32 = s.parse().ok()?;
            if s.len() == 2 {
                2000 + v
            } else {
                v
            }
        }
    };
    Some((d, m, y))
}

/// Дата из RE_DATE_QUERY (None — не шаблон или несуществующая дата).
/// main.py: get_specific_date.
pub fn parse_date_query(text: &str) -> Option<NaiveDate> {
    let (d, m, y) = parse_date_parts(text)?;
    NaiveDate::from_ymd_opt(y, m, d)
}

/// Действие из callback_data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackAction {
    SetGroup(String),
    BellsPhoto(String),
    EphemeralRasp(String),
    EphemeralBells(String),
    WidgetTab(String),
    ZvonkiDate(String),
}

/// Разбор callback_data: set_grp:/bells_photo:/ephemeral_rasp:/
/// ephemeral_bells:/widget_tab:/zvonki_date:.
pub fn parse_callback_data(data: &str) -> Option<CallbackAction> {
    let (prefix, arg) = data.split_once(':')?;
    let arg = arg.trim().to_string();
    match prefix {
        "set_grp" => Some(CallbackAction::SetGroup(arg)),
        "bells_photo" => Some(CallbackAction::BellsPhoto(arg)),
        "ephemeral_rasp" => Some(CallbackAction::EphemeralRasp(arg)),
        "ephemeral_bells" => Some(CallbackAction::EphemeralBells(arg)),
        "widget_tab" => Some(CallbackAction::WidgetTab(arg)),
        "zvonki_date" => Some(CallbackAction::ZvonkiDate(arg)),
        _ => None,
    }
}

/// Проверка формата группы как в set_cmd: RE_VALID_GROUP.search(clean_group).
pub fn is_valid_group(clean_group: &str) -> bool {
    valid_group_re().is_match(clean_group)
}

// ============================== ТЕКСТЫ ==============================

/// build_help_messages (main.py): fallback HTML для /help.
pub fn build_help_text() -> &'static str {
    "<b>Настройка</b>\n\
     • <code>/set [группа]</code> — привязать группу к чату\n\
     • Примеры: <code>/set 24-27</code>, <code>/set ИП-24-27</code>, <code>/set ИП-24-27б</code>\n\n\
     <b>Меню</b>\n\
     • <b>Расписание</b> — расписание на ближайшие учебные дни\n\
     • <b>На завтра</b> — расписание на следующий учебный день\n\
     • <b>Звонки</b> — сетка звонков для вашего курса\n\n\
     <b>Поиск по дате</b>\n\
     Напишите дату в чат, чтобы открыть нужный день:\n\
     • <code>20.01</code> или <code>26.01.26</code>\n\n\
     <b>Автообновление</b>\n\
     Бот автоматически отслеживает расписание и замены в группе ВК.\n\
     <blockquote>В беседах выдайте боту права администратора для автозакрепа актуального расписания.</blockquote>"
}

/// build_start_messages (main.py): fallback HTML для /start.
pub fn build_start_text() -> &'static str {
    "<b>Расписание занятий</b>\n\n\
     Чтобы начать, укажите свою группу (полностью или только цифры):\n\
     • <code>/set 24-27</code>\n\
     • <code>/set ИП-24-27</code>\n\
     • <code>/set ИП-24-27б</code>\n\n\
     Список команд: /help\n\n\
     <a href=\"https://t.me/AtlantaVPN_bot?start=partner_bDkgJA\">Рабочий VPN</a>"
}

// ============================== КЛАВИАТУРЫ ==============================

/// main_kb (main.py): Расписание | На завтра / Звонки, resize_keyboard.
pub fn main_kb() -> ReplyKeyboardMarkup {
    let mut kb = ReplyKeyboardMarkup::new(vec![
        vec![
            KeyboardButton::new("Расписание"),
            KeyboardButton::new("На завтра"),
        ],
        vec![KeyboardButton::new("Звонки")],
    ]);
    kb.resize_keyboard = true;
    kb
}

/// Только для личных чатов: в группах Reply-клавиатура не показывается
/// (throttled_answer в Python сбрасывает её для chat_id < 0).
pub fn private_main_kb(chat_id: i64) -> Option<ReplyKeyboardMarkup> {
    if chat_id > 0 {
        Some(main_kb())
    } else {
        None
    }
}

/// spec_26_21_kb (main.py): выбор специальности для 26-21.
pub fn spec_26_21_kb() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[
        InlineKeyboardButton::callback("ЮР-26-21 (Юриспруденция)", "set_grp:ЮР-26-21"),
    ], [
        InlineKeyboardButton::callback("ТД-26-21 (Торговое дело)", "set_grp:ТД-26-21"),
    ]])
}

/// zvonki_tomorrow_kb (main.py).
pub fn zvonki_tomorrow_kb(tomorrow_date_str: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[
        InlineKeyboardButton::callback(
            format!("Звонки на завтра ({})", &tomorrow_date_str[..5.min(tomorrow_date_str.len())]),
            format!("zvonki_date:{tomorrow_date_str}"),
        ),
    ]])
}

/// Кнопка «📱 Открыть расписание» (WebApp) — /start и напоминания.
pub fn webapp_kb(webapp_url: &url::Url) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[
        InlineKeyboardButton::web_app(
            "📱 Открыть расписание",
            WebAppInfo { url: webapp_url.clone() },
        ),
    ]])
}

/// build_day_update_inline_kb (main.py): кнопка фото звонков + эфемерные
/// кнопки для групп. Эфемерные callback'и отвечают через RawApi
/// (sendMessage + ephemeral_message_parameters); alert ≤195 — только
/// fallback при ошибке отправки.
pub fn day_update_inline_kb(
    chat_id: i64,
    date_str: &str,
    has_bells_photo: bool,
) -> Option<InlineKeyboardMarkup> {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    if has_bells_photo {
        rows.push(vec![InlineKeyboardButton::callback(
            "Сокращенные звонки (фото)",
            format!("bells_photo:{date_str}"),
        )]);
    }
    if chat_id < 0 {
        rows.push(vec![
            InlineKeyboardButton::callback("Расписание", format!("ephemeral_rasp:{date_str}")),
            InlineKeyboardButton::callback("Звонки", format!("ephemeral_bells:{date_str}")),
        ]);
    }
    if rows.is_empty() {
        None
    } else {
        Some(InlineKeyboardMarkup::new(rows))
    }
}

// ============================== BUILD_RESP / ВИДЖЕТ ==============================

/// Fallback-текст дня из DayData — точная копия fallback-части build_resp
/// (main.py:197). `only_changes=true` — как format_day_data(only_changes=True):
/// основное расписание не показывается, только замены.
pub fn format_day_fallback(
    group_name: &str,
    date_str: &str,
    day: &DayData,
    only_changes: bool,
) -> String {
    if day.target_group.is_empty() && day.changes.is_empty() {
        return format!(
            "Данных для <b>{}</b> на <b>{}</b> нет.",
            escape_html(group_name),
            escape_html(date_str)
        );
    }
    let display = if day.target_group.is_empty() {
        group_name.to_string()
    } else {
        day.target_group.clone()
    };
    let mut content = String::new();
    if !only_changes && !day.pairs.is_empty() {
        let lines: Vec<String> = day
            .pairs
            .iter()
            .map(|(p, (subj, room))| {
                let room_part = if room.is_empty() || room == "—" {
                    String::new()
                } else {
                    format!(" — {}", escape_html(room))
                };
                format!("{p}. {}{}", escape_html(subj), room_part)
            })
            .collect();
        content.push_str("\n<b>Основное</b>\n");
        content.push_str(&lines.join("\n"));
        content.push('\n');
    }
    if !day.changes.is_empty() {
        let cleaned: Vec<String> = day
            .changes
            .iter()
            .map(|c| escape_html(&clean_change_line(c)))
            .collect();
        content.push_str("\n<b>Замены</b>\n");
        content.push_str(&cleaned.join("\n"));
        content.push('\n');
    }
    let header = format!("<b>{}</b> • {}\n", escape_html(&display), escape_html(date_str));
    if content.is_empty() {
        format!("{header}\n<i>Пар нет</i>")
    } else {
        format!("{header}{content}")
    }
}

/// build_resp (main.py:197): (fallback-текст, rich_html).
/// rich_html — None, если данных нет (нет target_group и нет замен),
/// как `if not target_g and not changes: return fallback, None` в Python.
pub fn build_resp(
    service: &ScheduleService,
    group_name: &str,
    date_str: &str,
    only_changes: bool,
) -> (String, Option<String>) {
    let day = service.format_day_data(group_name, date_str);
    if day.target_group.is_empty() && day.changes.is_empty() {
        return (
            format_day_fallback(group_name, date_str, &day, only_changes),
            None,
        );
    }
    let target = if day.target_group.is_empty() {
        None
    } else {
        Some(day.target_group.as_str())
    };
    let rich = service.render_rich_table_html(
        group_name,
        date_str,
        &day.pairs,
        &day.changes,
        target,
        false,
        false,
    );
    (
        format_day_fallback(group_name, date_str, &day, only_changes),
        Some(rich),
    )
}

/// Текст виджета расписания (fallback-часть build_interactive_schedule_widget):
/// дни текущей учебной недели Пн–Сб (до 6), каждый — через build_resp.
/// Возвращает текст для отправки (включая сообщение «расписания нет»).
pub fn build_widget_fallback(group_name: &str, days: &[(String, DayData)]) -> String {
    if days.is_empty() {
        return format!(
            "На ближайшие дни расписания для <b>{}</b> нет.",
            escape_html(group_name)
        );
    }
    let parts: Vec<String> = days
        .iter()
        .map(|(d, day)| format_day_fallback(group_name, d, day, false))
        .collect();
    parts.join("\n\n")
}

/// build_interactive_schedule_widget (main.py:284): (fallback, rich).
/// Виджет недели (до 6 дней Пн–Сб): вкладки — <details> с открытым
/// выбранным днём, сверху — блок текущей пары с <tg-time>.
/// active_date — выбранная вкладка (widget_tab) или None: первый день
/// >= сегодня, иначе последний доступный. Rich обрезается до 15000
/// символов при >32000 байт (как в Python).
pub async fn build_widget_rich(
    service: &ScheduleService,
    group_name: &str,
    active_date: Option<&str>,
) -> (String, Option<String>) {
    let cache = service.archive_cache.read().unwrap();
    let mut sorted_dates: Vec<String> = cache.keys().cloned().collect();
    sorted_dates.sort_by_key(|d| crate::tyumen::parse_date(d).unwrap_or_default());
    if sorted_dates.is_empty() {
        return ("Архив пуст. Данные ещё не загружены.".to_string(), None);
    }
    let now = tyumen_now();
    let today = now.date_naive();
    let week_start = today - chrono::Duration::days(today.weekday().num_days_from_monday() as i64);
    let mut target: Vec<String> = sorted_dates
        .iter()
        .filter(|d| {
            crate::tyumen::parse_date(d).map(|pd| pd >= week_start).unwrap_or(false)
        })
        .take(6)
        .cloned()
        .collect();
    if target.is_empty() {
        target = sorted_dates.iter().rev().take(6).cloned().collect();
        target.reverse();
    }
    drop(cache);
    let mut available: Vec<(String, DayData)> = Vec::new();
    let mut display_group = group_name.to_string();
    for d in &target {
        let day = service.format_day_data(group_name, d);
        if !day.target_group.is_empty() {
            display_group = day.target_group.clone();
        }
        if !day.pairs.is_empty() || !day.changes.is_empty() {
            available.push((d.clone(), day));
        }
    }
    if available.is_empty() {
        return (
            format!(
                "На ближайшие дни расписания для <b>{}</b> нет.",
                escape_html(group_name)
            ),
            None,
        );
    }
    let today_str = fmt_date(today);
    let selected = match active_date {
        Some(a) if available.iter().any(|(d, _)| d == a) => a.to_string(),
        _ => available
            .iter()
            .find(|(d, _)| d >= &today_str)
            .map(|(d, _)| d.clone())
            .unwrap_or_else(|| available.last().unwrap().0.clone()),
    };
    let mut filtered_days: Vec<(String, DayData)> = Vec::new();
    let mut rich_details: Vec<String> = Vec::new();
    for (d, day) in &available {
        let (_, rich_probe) = build_resp(service, group_name, d, false);
        if rich_probe.is_some() {
            filtered_days.push((d.clone(), day.clone()));
            let target = if day.target_group.is_empty() {
                None
            } else {
                Some(day.target_group.as_str())
            };
            rich_details.push(service.render_rich_table_html(
                group_name,
                d,
                &day.pairs,
                &day.changes,
                target,
                true,
                d == &selected,
            ));
        }
    }
    let fallback_text = if filtered_days.is_empty() {
        String::new()
    } else {
        build_widget_fallback(group_name, &filtered_days)
    };
    let progress_block = service.current_pair_progress_rich(&display_group, now);
    let mut blocks: Vec<String> = Vec::new();
    if !progress_block.is_empty() {
        blocks.push(progress_block);
    }
    blocks.extend(rich_details);
    let mut rich_text = blocks.join("\n\n");
    if rich_text.as_bytes().len() > 32000 {
        rich_text = rich_text.chars().take(15000).collect();
    }
    (
        fallback_text,
        if rich_text.is_empty() {
            None
        } else {
            Some(rich_text)
        },
    )
}

/// Текст виджета расписания (fallback-часть build_interactive_schedule_widget):
/// дни текущей учебной недели Пн–Сб (до 6), каждый — через build_resp.
/// Возвращает текст для отправки (включая сообщение «расписания нет»).
pub async fn build_widget_text(service: &ScheduleService, group_name: &str) -> String {
    build_widget_rich(service, group_name, None).await.0
}

/// _build_day_update_fallback (main.py): заголовок обновления дня.
pub fn day_update_fallback(
    group_name: &str,
    target_date: &str,
    fb_text: &str,
    is_text_only: bool,
) -> String {
    let header = if is_text_only {
        format!("<b>Изменения на {}</b>", escape_html(target_date))
    } else {
        format!("<b>Расписание на {}</b>", escape_html(target_date))
    };
    let mut lines = fb_text.splitn(2, '\n');
    let first = lines.next().unwrap_or("");
    let grp_line = first
        .split('•')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let grp_line = if grp_line.is_empty() {
        format!("<b>{}</b>", escape_html(group_name))
    } else {
        grp_line
    };
    let rest = lines.next().unwrap_or("");
    format!("{header}\n{grp_line}\n{rest}").trim().to_string()
}

// ============================== HAS_CLASSES / BROADCAST ==============================

/// has_classes_on_date(get_group_day_schedule(...)) — services.py:359,655.
/// Отменённые пары (изменение со словами нет/отмен*/снят* + номера пар)
/// исключаются; остальные изменения считаются парами.
pub fn has_classes(day: &DayData) -> bool {
    let mut cancelled: HashSet<i32> = HashSet::new();
    let mut extra_changes: Vec<&String> = Vec::new();
    for c in &day.changes {
        if cancel_words_re().is_match(c) {
            let nums = extract_pair_numbers(c);
            if !nums.is_empty() {
                cancelled.extend(nums);
            }
        } else {
            extra_changes.push(c);
        }
    }
    let is_real = |subj: &str| {
        !subj.is_empty()
            && !["практика", "выходной", "каникулы"]
                .iter()
                .any(|kw| subj.to_lowercase().contains(kw))
    };
    if day
        .pairs
        .iter()
        .any(|(n, (subj, _))| !cancelled.contains(n) && is_real(subj))
    {
        return true;
    }
    extra_changes.iter().any(|c| is_real(c))
}

/// Дедупликация событий vk_watcher (main.py:1439): для одной (группа, дата)
/// «schedule» приоритетнее «change». Детерминированно при любом порядке.
pub fn filter_events(
    events: &HashSet<(String, String, String)>,
) -> HashMap<(String, String), String> {
    let mut filtered: HashMap<(String, String), String> = HashMap::new();
    for (g, d, t) in events {
        if filtered.get(&(g.clone(), d.clone())).map(|s| s.as_str()) == Some("schedule") {
            continue;
        }
        filtered.insert((g.clone(), d.clone()), t.clone());
    }
    filtered
}

/// Дата «на завтра»: в субботу — через 2 дня (понедельник), иначе +1.
pub fn tomorrow_date(today: NaiveDate) -> NaiveDate {
    let days_ahead = if today.weekday() == chrono::Weekday::Sat { 2 } else { 1 };
    today + chrono::Duration::days(days_ahead)
}

/// Пик VK watcher (main.py): 7–9ч и 18–21ч по Тюмени.
pub fn is_peak_hour(hour: u32) -> bool {
    (7..9).contains(&hour) || (18..21).contains(&hour)
}

/// Рабочие часы watcher: 6–21ч.
pub fn in_work_hours(hour: u32) -> bool {
    (6..21).contains(&hour)
}

/// Задержка watcher: 5 с в пик, иначе 10 с; при vk_rate_limited —
/// max(15, min(30, backoff)) (main.py:1439).
pub fn watcher_sleep_secs(hour: u32, rate_limited: bool, backoff_secs: f64) -> f64 {
    if rate_limited {
        return backoff_secs.clamp(15.0, 30.0).max(15.0);
    }
    if is_peak_hour(hour) {
        5.0
    } else {
        10.0
    }
}

/// День недели по-русски для заголовков (tyumen::weekday_name).
pub fn weekday_ru(d: NaiveDate) -> &'static str {
    weekday_name(d)
}

// ============================== ОТПРАВКА (throttled_answer) ==============================

/// Отключение превью ссылок (как DefaultBotProperties link_preview_is_disabled).
fn no_preview() -> LinkPreviewOptions {
    LinkPreviewOptions {
        is_disabled: true,
        url: None,
        prefer_small_media: false,
        prefer_large_media: false,
        show_above_text: false,
    }
}

/// Проверка, что RequestError — RetryAfter, и сколько ждать.
fn retry_after_secs(err: &teloxide::RequestError) -> Option<Duration> {
    match err {
        teloxide::RequestError::RetryAfter(secs) => Some(Duration::from_secs(u64::from(secs.seconds()))),
        _ => None,
    }
}

/// Исход одной попытки отправки обычного сообщения.
enum SendOutcome {
    Sent(Message),
    RetryAfter(Duration),
    Failed,
}

/// Одна попытка send_message (без цикла повторов).
async fn send_message_once(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    text: &str,
    markup: Option<ReplyMarkup>,
    thread_id: Option<ThreadId>,
) -> SendOutcome {
    let mut req = <Bot as Requester>::send_message(bot, ChatId(chat_id), text)
        .parse_mode(ParseMode::Html)
        .link_preview_options(no_preview());
    if let Some(tid) = thread_id {
        req = req.message_thread_id(tid);
    }
    if let Some(m) = markup {
        req = req.reply_markup(m);
    }
    match req.await {
        Ok(msg) => SendOutcome::Sent(msg),
        Err(e) => {
            if let Some(delay) = retry_after_secs(&e) {
                return SendOutcome::RetryAfter(delay);
            }
            if matches!(e, teloxide::RequestError::MigrateToChatId(_))
                || is_dead_chat_error(&e.to_string())
            {
                let _ = db.delete_chat(chat_id).await;
            } else {
                tracing::error!("throttled_send to {chat_id} failed: {e}");
            }
            SendOutcome::Failed
        }
    }
}

/// throttled_answer (main.py): до 3 попыток, RetryAfter — пауза и повтор,
/// мёртвый чат (DEAD_CHAT_ERRORS / MigrateToChatId) — удалить чат из БД.
/// Возвращает отправленное сообщение или None.
pub async fn throttled_send(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    text: &str,
    markup: Option<ReplyMarkup>,
    thread_id: Option<ThreadId>,
) -> Option<Message> {
    for _ in 0..3 {
        match send_message_once(bot, db, chat_id, text, markup.clone(), thread_id).await {
            SendOutcome::Sent(msg) => return Some(msg),
            SendOutcome::RetryAfter(delay) => tokio::time::sleep(delay).await,
            SendOutcome::Failed => return None,
        }
    }
    None
}

/// throttled_answer с Rich Messages (main.py:124): сначала sendRichMessage
/// через RawApi (teloxide его не знает), при неудаче — обычный HTML-текст.
/// markup — клавиатура для rich (как reply_markup в kwargs Python);
/// fallback_markup — только для fallback, когда markup нет.
/// До 3 попыток; RetryAfter — пауза и повтор; мёртвый чат — удалить из БД.
pub async fn throttled_send_rich(
    bot: &Bot,
    raw: &RawApi,
    db: &Db,
    chat_id: i64,
    text: &str,
    rich_html: Option<&str>,
    markup: Option<ReplyMarkup>,
    fallback_markup: Option<ReplyMarkup>,
    thread_id: Option<ThreadId>,
) -> Option<Message> {
    let rich_html = rich_html.filter(|r| !r.is_empty());
    let thread_id_i64 = thread_id.map(|t| t.0.0 as i64);
    for _ in 0..3 {
        if let Some(rich) = rich_html {
            let markup_json = markup
                .as_ref()
                .and_then(|m| serde_json::to_value(m).ok());
            match raw
                .send_rich_message(chat_id, rich, thread_id_i64, markup_json)
                .await
            {
                Ok(msg) => return Some(msg),
                Err(RawApiError::RetryAfter(secs)) => {
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        "Rich message send failed, falling back to HTML text: {e}"
                    );
                }
            }
        }
        let fb_markup = markup.clone().or_else(|| fallback_markup.clone());
        match send_message_once(bot, db, chat_id, text, fb_markup, thread_id).await {
            SendOutcome::Sent(msg) => return Some(msg),
            SendOutcome::RetryAfter(delay) => tokio::time::sleep(delay).await,
            SendOutcome::Failed => return None,
        }
    }
    None
}

/// Отправка ответа на Message: топик (message_thread_id) и сброс
/// Reply-клавиатуры в группах — как throttled_answer.
pub async fn answer_msg(
    bot: &Bot,
    db: &Db,
    msg: &Message,
    text: &str,
    markup: Option<ReplyMarkup>,
) -> Option<Message> {
    let chat_id = msg.chat.id.0;
    let markup = match &markup {
        Some(ReplyMarkup::Keyboard(_)) if chat_id < 0 => None,
        _ => markup,
    };
    throttled_send(bot, db, chat_id, text, markup, msg.thread_id).await
}

/// answer_msg с Rich Messages: как answer_msg, но сначала пробует
/// sendRichMessage. Reply-клавиатура в группах сбрасывается и у rich,
/// и у fallback (как в throttled_answer Python).
pub async fn answer_msg_rich(
    bot: &Bot,
    raw: &RawApi,
    db: &Db,
    msg: &Message,
    text: &str,
    rich_html: Option<&str>,
    markup: Option<ReplyMarkup>,
    fallback_markup: Option<ReplyMarkup>,
) -> Option<Message> {
    let chat_id = msg.chat.id.0;
    let strip_keyboard = |m: Option<ReplyMarkup>| match &m {
        Some(ReplyMarkup::Keyboard(_)) if chat_id < 0 => None,
        _ => m,
    };
    throttled_send_rich(
        bot,
        raw,
        db,
        chat_id,
        text,
        rich_html,
        strip_keyboard(markup),
        strip_keyboard(fallback_markup),
        msg.thread_id,
    )
    .await
}

/// Отправка фото с retry (для safe_send_day_update и звонков).
/// FileId — отправка по готовому id (без перезаливки), Bytes — загрузка.
async fn send_photo_retry(
    bot: &Bot,
    db: &Db,
    chat_id: i64,
    input: BellsPhotoInput,
    caption: &str,
    markup: Option<ReplyMarkup>,
) -> Option<Message> {
    use teloxide::types::InputFile;
    for _ in 0..3 {
        let file = match &input {
            BellsPhotoInput::FileId(fid) => InputFile::file_id(fid.clone()),
            BellsPhotoInput::Bytes(b) => {
                InputFile::memory(b.clone()).file_name("bells.jpg")
            }
        };
        let mut req = bot
            .send_photo(ChatId(chat_id), file)
            .caption(caption)
            .parse_mode(ParseMode::Html);
        if let Some(m) = markup.clone() {
            req = req.reply_markup(m);
        }
        match req.await {
            Ok(msg) => return Some(msg),
            Err(e) => {
                if let Some(delay) = retry_after_secs(&e) {
                    tokio::time::sleep(delay).await;
                    continue;
                }
                if matches!(e, teloxide::RequestError::MigrateToChatId(_))
                    || is_dead_chat_error(&e.to_string())
                {
                    let _ = db.delete_chat(chat_id).await;
                } else {
                    tracing::warn!("send_photo to {chat_id} failed: {e}");
                }
                return None;
            }
        }
    }
    None
}

/// file_id отправленного фото (самый большой размер — последний),
/// или None. Как `sent.photo[-1].file_id` в Python.
fn sent_photo_file_id(msg: &Message) -> Option<String> {
    msg.photo()
        .and_then(|p| p.last())
        .map(|s| s.file.id.clone())
}

/// Захватить file_id отправленного фото звонков в кэш (память + БД),
/// как cache_bells_photo_file_id в Python. Ошибки кэша — только в лог.
async fn capture_bells_photo_file_id(
    service: &ScheduleService,
    date_str: &str,
    sent: &Message,
) -> Option<String> {
    let fid = sent_photo_file_id(sent)?;
    if let Err(e) = service.cache_bells_photo_file_id(date_str, &fid).await {
        tracing::warn!("cache_bells_photo_file_id({date_str}) failed: {e}");
    }
    Some(fid)
}

/// Ответить на callback_query (без alert — обычный ответ).
async fn answer_callback(bot: &Bot, q: &CallbackQuery, text: Option<&str>, alert: bool) {
    let mut req = bot.answer_callback_query(q.id.clone());
    if let Some(t) = text {
        req = req.text(t);
    }
    if alert {
        req = req.show_alert(true);
    }
    if let Err(e) = req.await {
        tracing::warn!("answer_callback_query failed: {e}");
    }
}

/// Alert с обрезанным текстом (fallback-ветка ephemeral-хендлеров Python:
/// `re.sub(r"<[^>]+>", "", fb_text)[:195]`, show_alert=True).
fn alert_text(fb_text: &str) -> String {
    strip_html_tags(fb_text).chars().take(195).collect()
}

fn callback_message(q: &CallbackQuery) -> Option<&Message> {
    q.message.as_ref().and_then(|m| m.regular_message())
}

/// check_user_group (main.py): группа из БД или подсказка + spec-клавиатура
/// для неоднозначной «26-21».
pub async fn check_user_group(bot: &Bot, db: &Db, msg: &Message) -> Option<String> {
    let chat_id = msg.chat.id.0;
    let group = match db.get_group(chat_id).await {
        Ok(g) => g,
        Err(e) => {
            tracing::error!("get_group({chat_id}) failed: {e}");
            None
        }
    };
    let Some(group) = group else {
        answer_msg(
            bot,
            db,
            msg,
            "Сначала укажите вашу группу.\nПример: <code>/set 24-27</code> или <code>/set БУХ-26-02</code>",
            private_main_kb(chat_id).map(ReplyMarkup::Keyboard),
        )
        .await;
        return None;
    };
    if group == "26-21" {
        answer_msg(
            bot,
            db,
            msg,
            "<b>В колледже две группы 26-21. Выберите специальность:</b>\n\n• <code>/set ЮР-26-21</code> — Юриспруденция\n• <code>/set ТД-26-21</code> — Торговое дело",
            Some(ReplyMarkup::InlineKeyboard(spec_26_21_kb())),
        )
        .await;
        return None;
    }
    Some(group)
}

// ============================== ReminderSender ==============================

/// Реализация ReminderSender для teloxide::Bot.
/// HTML-разметка, кнопка WebApp «📱 Открыть расписание» если есть URL;
/// ошибки — AppError::Telegram (текст важен для определения мёртвого чата).
impl ReminderSender for teloxide::Bot {
    async fn send_message(
        &self,
        chat_id: i64,
        text: String,
        webapp_url: Option<String>,
    ) -> Result<()> {
        // Requester::send_message — явная квалификация: у Bot есть и
        // ReminderSender::send_message с тем же именем.
        let mut req = <teloxide::Bot as Requester>::send_message(self, ChatId(chat_id), text)
            .parse_mode(ParseMode::Html)
            .link_preview_options(no_preview());
        if let Some(url) = webapp_url {
            if let Ok(url) = url::Url::parse(&url) {
                req = req.reply_markup(ReplyMarkup::InlineKeyboard(webapp_kb(&url)));
            }
        }
        req.await.map_err(|e| AppError::Telegram(e.to_string()))?;
        Ok(())
    }
}

// ============================== ХЕНДЛЕРЫ СООБЩЕНИЙ ==============================

type HandlerResult = std::result::Result<(), teloxide::RequestError>;

async fn start_handler(
    bot: Bot,
    msg: Message,
    _service: Arc<ScheduleService>,
    db: Arc<Db>,
    webapp_url: Option<String>,
) -> HandlerResult {
    let chat_id = msg.chat.id.0;
    answer_msg(
        &bot,
        &db,
        &msg,
        build_start_text(),
        private_main_kb(chat_id).map(ReplyMarkup::Keyboard),
    )
    .await;
    // Пропатченный main.py: после /start — сообщение с кнопкой Mini App.
    if let Some(url) = webapp_url {
        if let Ok(url) = url::Url::parse(&url) {
            throttled_send(
                &bot,
                &db,
                chat_id,
                "Расписание удобнее смотреть в приложении — с текущей парой, таймером и звонками:",
                Some(ReplyMarkup::InlineKeyboard(webapp_kb(&url))),
                msg.thread_id,
            )
            .await;
        }
    }
    Ok(())
}

async fn help_handler(
    bot: Bot,
    msg: Message,
    _service: Arc<ScheduleService>,
    db: Arc<Db>,
    _webapp_url: Option<String>,
) -> HandlerResult {
    let chat_id = msg.chat.id.0;
    answer_msg(
        &bot,
        &db,
        &msg,
        build_help_text(),
        private_main_kb(chat_id).map(ReplyMarkup::Keyboard),
    )
    .await;
    Ok(())
}

async fn set_handler(
    bot: Bot,
    msg: Message,
    service: Arc<ScheduleService>,
    db: Arc<Db>,
    _webapp_url: Option<String>,
    raw_api: Arc<RawApi>,
) -> HandlerResult {
    let chat_id = msg.chat.id.0;
    let args = match parse_command(msg.text().unwrap_or_default()) {
        Some(ParsedCommand::Set(a)) => a,
        _ => None,
    };
    let Some(raw) = args else {
        answer_msg(
            &bot,
            &db,
            &msg,
            "<b>Укажите группу.</b>\nПример: <code>/set 24-27</code>, <code>/set ИП-24-27</code> или <code>/set ИП-24-27б</code>",
            private_main_kb(chat_id).map(ReplyMarkup::Keyboard),
        )
        .await;
        return Ok(());
    };
    let clean_group = normalize_group(&raw, "");
    if clean_group == "26-21" {
        answer_msg(
            &bot,
            &db,
            &msg,
            "<b>В колледже две группы 26-21. Выберите специальность:</b>\n\n• <code>/set ЮР-26-21</code> — Юриспруденция\n• <code>/set ТД-26-21</code> — Торговое дело",
            Some(ReplyMarkup::InlineKeyboard(spec_26_21_kb())),
        )
        .await;
        return Ok(());
    }
    if is_valid_group(&clean_group) {
        if let Err(e) = db.set_group(chat_id, &clean_group).await {
            tracing::error!("set_group({chat_id}) failed: {e}");
        }
        answer_msg(
            &bot,
            &db,
            &msg,
            &format!("Группа <b>{}</b> привязана.", escape_html(&clean_group)),
            private_main_kb(chat_id).map(ReplyMarkup::Keyboard),
        )
        .await;
        let (widget, widget_rich) = build_widget_rich(&service, &clean_group, None).await;
        answer_msg_rich(
            &bot,
            &raw_api,
            &db,
            &msg,
            &widget,
            widget_rich.as_deref(),
            private_main_kb(chat_id).map(ReplyMarkup::Keyboard),
            None,
        )
        .await;
    } else {
        answer_msg(
            &bot,
            &db,
            &msg,
            "<b>Некорректный формат группы.</b>\nПримеры: <code>24-27</code>, <code>26-02</code>, <code>ЮР-26-21</code>, <code>ТД-26-21</code>.",
            None,
        )
        .await;
    }
    Ok(())
}

/// Кнопка «Расписание»: виджет недели (Rich, fallback — текст).
async fn rasp_handler(
    bot: Bot,
    msg: Message,
    service: Arc<ScheduleService>,
    db: Arc<Db>,
    _webapp_url: Option<String>,
    raw: Arc<RawApi>,
) -> HandlerResult {
    let Some(group) = check_user_group(&bot, &db, &msg).await else {
        return Ok(());
    };
    let (widget, widget_rich) = build_widget_rich(&service, &group, None).await;
    answer_msg_rich(
        &bot,
        &raw,
        &db,
        &msg,
        &widget,
        widget_rich.as_deref(),
        private_main_kb(msg.chat.id.0).map(ReplyMarkup::Keyboard),
        None,
    )
    .await;
    Ok(())
}

/// Кнопка «На завтра»: в субботу — понедельник, иначе +1 день.
async fn tomorrow_handler(
    bot: Bot,
    msg: Message,
    service: Arc<ScheduleService>,
    db: Arc<Db>,
    _webapp_url: Option<String>,
    raw: Arc<RawApi>,
) -> HandlerResult {
    let Some(group) = check_user_group(&bot, &db, &msg).await else {
        return Ok(());
    };
    let t_date = tomorrow_date(tyumen_now().date_naive());
    let (fb, rich) = build_resp(&service, &group, &fmt_date(t_date), false);
    answer_msg_rich(
        &bot,
        &raw,
        &db,
        &msg,
        &fb,
        rich.as_deref(),
        private_main_kb(msg.chat.id.0).map(ReplyMarkup::Keyboard),
        None,
    )
    .await;
    Ok(())
}

/// Кнопка «Звонки»: get_zvonki_resp_rich + кнопка «Звонки на завтра»,
/// если can_open_tomorrow_zvonki. В воскресенье base_date — понедельник
/// (внутри get_zvonki_resp_rich при target=None). Клавиатура — как в
/// Python: reply_markup — main_kb, fallback — завтра-кнопка.
async fn zvonki_handler(
    bot: Bot,
    msg: Message,
    service: Arc<ScheduleService>,
    db: Arc<Db>,
    _webapp_url: Option<String>,
    raw: Arc<RawApi>,
) -> HandlerResult {
    let Some(group) = check_user_group(&bot, &db, &msg).await else {
        return Ok(());
    };
    let now = tyumen_now();
    let today = now.date_naive();
    let tomorrow = today + chrono::Duration::days(1);
    let show_tomorrow_btn = service.can_open_tomorrow_zvonki(&fmt_date(tomorrow));
    let (fb, rich) = service.get_zvonki_resp_rich(&group, None, show_tomorrow_btn, now);
    let chat_id = msg.chat.id.0;
    let markup = private_main_kb(chat_id).map(ReplyMarkup::Keyboard);
    let fallback_markup = if show_tomorrow_btn {
        Some(ReplyMarkup::InlineKeyboard(zvonki_tomorrow_kb(&fmt_date(tomorrow))))
    } else {
        None
    };
    answer_msg_rich(
        &bot,
        &raw,
        &db,
        &msg,
        &fb,
        rich.as_deref(),
        markup,
        fallback_markup,
    )
    .await;
    Ok(())
}

/// Дата RE_DATE_QUERY: расписание на конкретный день.
async fn date_query_handler(
    bot: Bot,
    msg: Message,
    service: Arc<ScheduleService>,
    db: Arc<Db>,
    _webapp_url: Option<String>,
    raw: Arc<RawApi>,
) -> HandlerResult {
    let Some(group) = check_user_group(&bot, &db, &msg).await else {
        return Ok(());
    };
    let text = msg.text().unwrap_or_default();
    let Some((d, m, y)) = parse_date_parts(text) else {
        return Ok(());
    };
    let Some(target) = NaiveDate::from_ymd_opt(y, m, d) else {
        answer_msg(&bot, &db, &msg, "Указана несуществующая календарная дата.", None).await;
        return Ok(());
    };
    let (fb, rich) = build_resp(&service, &group, &fmt_date(target), false);
    answer_msg_rich(
        &bot,
        &raw,
        &db,
        &msg,
        &fb,
        rich.as_deref(),
        private_main_kb(msg.chat.id.0).map(ReplyMarkup::Keyboard),
        None,
    )
    .await;
    Ok(())
}

/// F.pinned_message: удаление сервисного сообщения о закрепе.
async fn delete_pin_service_handler(bot: Bot, msg: Message) -> HandlerResult {
    if let Err(e) = bot.delete_message(msg.chat.id, msg.id).await {
        tracing::debug!("delete pinned service message failed: {e}");
    }
    Ok(())
}

/// F.migrate_to_chat_id: группа upgraded в супергруппу — удалить старый ID.
async fn migrate_handler(bot: Bot, msg: Message, db: Arc<Db>) -> HandlerResult {
    let _ = bot;
    if let Some(_new_id) = msg.migrate_to_chat_id() {
        let old_id = msg.chat.id.0;
        if old_id < 0 {
            if let Err(e) = db.delete_chat(old_id).await {
                tracing::error!("delete_chat({old_id}) after migrate failed: {e}");
            } else {
                tracing::info!("🧹 Старый чат группы {old_id} удалён из БД после апгрейда в супергруппу");
            }
        }
    }
    Ok(())
}

// ============================== ХЕНДЛЕРЫ CALLBACK'ОВ ==============================

/// on_select_specialty_26_21: set_grp:ЮР-26-21 / ТД-26-21.
async fn set_grp_callback(
    bot: Bot,
    q: CallbackQuery,
    service: Arc<ScheduleService>,
    db: Arc<Db>,
    _webapp_url: Option<String>,
    raw: Arc<RawApi>,
) -> HandlerResult {
    let Some(msg) = callback_message(&q) else {
        return Ok(());
    };
    let chat_id = msg.chat.id.0;
    let selected = match parse_callback_data(q.data.as_deref().unwrap_or_default()) {
        Some(CallbackAction::SetGroup(g)) => g,
        _ => return Ok(()),
    };
    let clean_group = normalize_group(&selected, "");
    if clean_group != "ЮР-26-21" && clean_group != "ТД-26-21" {
        answer_callback(&bot, &q, Some("Некорректная группа"), true).await;
        return Ok(());
    }
    if let Err(e) = db.set_group(chat_id, &clean_group).await {
        tracing::error!("set_group({chat_id}) failed: {e}");
    }
    answer_callback(&bot, &q, Some(&format!("Группа {clean_group}")), false).await;
    if let Err(e) = bot.delete_message(msg.chat.id, msg.id).await {
        tracing::debug!("delete set_grp message failed: {e}");
    }
    answer_msg(
        &bot,
        &db,
        msg,
        &format!("Группа <b>{}</b> привязана.", escape_html(&clean_group)),
        private_main_kb(chat_id).map(ReplyMarkup::Keyboard),
    )
    .await;
    let (widget, widget_rich) = build_widget_rich(&service, &clean_group, None).await;
    answer_msg_rich(
        &bot,
        &raw,
        &db,
        msg,
        &widget,
        widget_rich.as_deref(),
        private_main_kb(chat_id).map(ReplyMarkup::Keyboard),
        None,
    )
    .await;
    Ok(())
}

/// on_bells_photo_callback: bells_photo:ДД.ММ.ГГГГ.
/// В беседах — эфемерно (как в Python), с публичным fallback при ошибке,
/// не связанной с запретом фото; в ЛС — обычная отправка.
async fn bells_photo_callback(
    bot: Bot,
    q: CallbackQuery,
    service: Arc<ScheduleService>,
    db: Arc<Db>,
    _webapp_url: Option<String>,
    raw: Arc<RawApi>,
) -> HandlerResult {
    let Some(msg) = callback_message(&q) else {
        return Ok(());
    };
    let chat_id = msg.chat.id.0;
    let date_str = match parse_callback_data(q.data.as_deref().unwrap_or_default()) {
        Some(CallbackAction::BellsPhoto(d)) => d,
        _ => return Ok(()),
    };
    if service.get_bells_photo_for_date(&date_str).is_none() {
        answer_callback(
            &bot,
            &q,
            Some("На эту дату фото сокращенных звонков не найдено"),
            true,
        )
        .await;
        return Ok(());
    }
    let ok = send_bells_photo_ephemeral(
        &bot,
        &raw,
        &db,
        &service,
        &q,
        chat_id,
        &date_str,
        true,
    )
    .await;
    if ok {
        answer_callback(&bot, &q, None, false).await;
    } else {
        answer_callback(
            &bot,
            &q,
            Some("Не удалось загрузить фото звонков, попробуйте позже"),
            true,
        )
        .await;
    }
    Ok(())
}

/// Маркеры ошибки «в чате запрещены фото/медиа» — как
/// PHOTO_CHAT_RESTRICTION_ERRORS (services.py:159).
pub const PHOTO_RESTRICTION_MARKERS: &[&str] = &[
    "chat_send_media_forbidden",
    "chat_send_photos_forbidden",
    "not enough rights to send photos",
    "not enough rights to send media",
    "have no rights to send a photo",
    "have no rights to send media",
    "need administrator rights",
    "right to send media",
    "right to send photos",
];

/// is_photo_restriction_error (services.py:172).
pub fn is_photo_restriction_error(err_text: &str) -> bool {
    let lower = err_text.to_lowercase();
    PHOTO_RESTRICTION_MARKERS
        .iter()
        .any(|m| lower.contains(m))
}

/// Эфемерная отправка фото звонков — как _send_bells_photo_for_callback
/// (main.py:919): в беседах — эфемерно (file_id или байты), при успехе
/// file_id захватывается в кэш. allow_public_group_fallback — при
/// неудаче эфемерной отправки (не из-за запрета фото) отправить публично.
/// В ЛС — обычная отправка. Возвращает true при успехе.
async fn send_bells_photo_ephemeral(
    bot: &Bot,
    raw: &RawApi,
    db: &Db,
    service: &ScheduleService,
    q: &CallbackQuery,
    chat_id: i64,
    date_str: &str,
    allow_public_group_fallback: bool,
) -> bool {
    if callback_message(q).is_none() {
        return false;
    };
    let caption = format!("Сокращенные звонки на {}", escape_html(date_str));
    if chat_id < 0 {
        let input = match service.get_bells_photo_input(date_str).await {
            Ok(Some(i)) => i,
            _ => return false,
        };
        let user_id = q.from.id.0 as i64;
        let sent = match &input {
            BellsPhotoInput::FileId(fid) => {
                raw.send_ephemeral_photo_by_file_id(chat_id, fid, &caption, user_id, &q.id)
                    .await
            }
            BellsPhotoInput::Bytes(bytes) => {
                raw.send_ephemeral_photo_bytes(chat_id, bytes, &caption, user_id, &q.id)
                    .await
            }
        };
        match sent {
            Ok(sent_msg) => {
                capture_bells_photo_file_id(service, date_str, &sent_msg).await;
                return true;
            }
            Err(e) => {
                tracing::warn!("Ephemeral send_photo fallback in chat {chat_id}: {e}");
                if !allow_public_group_fallback
                    || is_photo_restriction_error(&e.to_string())
                {
                    return false;
                }
                // Иначе — публичная отправка ниже.
            }
        }
    }
    let input = match service.get_bells_photo_input(date_str).await {
        Ok(Some(i)) => i,
        _ => return false,
    };
    match send_photo_retry(bot, db, chat_id, input, &caption, None).await {
        Some(sent) => {
            capture_bells_photo_file_id(service, date_str, &sent).await;
            true
        }
        None => false,
    }
}

/// on_ephemeral_rasp_callback: ephemeral_rasp:ДД.ММ.ГГГГ.
/// Эфемерный ответ через RawApi (sendMessage + ephemeral_message_parameters);
/// при ошибке — alert ≤195 символов (fallback-ветка Python).
async fn ephemeral_rasp_callback(
    bot: Bot,
    q: CallbackQuery,
    service: Arc<ScheduleService>,
    db: Arc<Db>,
    _webapp_url: Option<String>,
    raw: Arc<RawApi>,
) -> HandlerResult {
    let Some(msg) = callback_message(&q) else {
        return Ok(());
    };
    let chat_id = msg.chat.id.0;
    let date_str = match parse_callback_data(q.data.as_deref().unwrap_or_default()) {
        Some(CallbackAction::EphemeralRasp(d)) => d,
        _ => return Ok(()),
    };
    let group = match db.get_group(chat_id).await {
        Ok(Some(g)) if g != "26-21" => g,
        _ => {
            answer_callback(&bot, &q, Some("Сначала привяжите группу через /set"), true).await;
            return Ok(());
        }
    };
    let (fb, _) = build_resp(&service, &group, &date_str, false);
    match raw
        .send_ephemeral_message(chat_id, &fb, q.from.id.0 as i64, &q.id)
        .await
    {
        Ok(_) => answer_callback(&bot, &q, None, false).await,
        Err(e) => {
            tracing::warn!("ephemeral rasp send failed: {e}");
            answer_callback(&bot, &q, Some(&alert_text(&fb)), true).await;
        }
    }
    Ok(())
}

/// on_ephemeral_bells_callback: ephemeral_bells:ДД.ММ.ГГГГ.
/// Сначала эфемерное фото звонков, иначе эфемерный текст; при ошибке —
/// alert ≤195 символов (как в Python).
async fn ephemeral_bells_callback(
    bot: Bot,
    q: CallbackQuery,
    service: Arc<ScheduleService>,
    db: Arc<Db>,
    _webapp_url: Option<String>,
    raw: Arc<RawApi>,
) -> HandlerResult {
    let Some(msg) = callback_message(&q) else {
        return Ok(());
    };
    let chat_id = msg.chat.id.0;
    let date_str = match parse_callback_data(q.data.as_deref().unwrap_or_default()) {
        Some(CallbackAction::EphemeralBells(d)) => d,
        _ => return Ok(()),
    };
    let group = match db.get_group(chat_id).await {
        Ok(Some(g)) if g != "26-21" => g,
        _ => {
            answer_callback(&bot, &q, Some("Сначала привяжите группу через /set"), true).await;
            return Ok(());
        }
    };
    if service.get_bells_photo_for_date(&date_str).is_some()
        && send_bells_photo_ephemeral(&bot, &raw, &db, &service, &q, chat_id, &date_str, false)
            .await
    {
        answer_callback(&bot, &q, None, false).await;
        return Ok(());
    }
    let target = crate::tyumen::parse_date(&date_str);
    let (fb, _) = match target {
        Some(d) => service.get_zvonki_resp_rich(&group, Some(d), false, tyumen_now()),
        None => service.get_zvonki_resp_rich(&group, None, false, tyumen_now()),
    };
    match raw
        .send_ephemeral_message(chat_id, &fb, q.from.id.0 as i64, &q.id)
        .await
    {
        Ok(_) => answer_callback(&bot, &q, None, false).await,
        Err(e) => {
            tracing::warn!("ephemeral bells send failed: {e}");
            answer_callback(&bot, &q, Some(&alert_text(&fb)), true).await;
        }
    }
    Ok(())
}

/// on_widget_tab_callback: widget_tab:ДД.ММ.ГГГГ.
/// Сначала edit_rich (виджет с выбранной вкладкой), при неудаче —
/// edit_text расписания дня (как в Python).
async fn widget_tab_callback(
    bot: Bot,
    q: CallbackQuery,
    service: Arc<ScheduleService>,
    db: Arc<Db>,
    _webapp_url: Option<String>,
    raw: Arc<RawApi>,
) -> HandlerResult {
    let Some(msg) = callback_message(&q) else {
        return Ok(());
    };
    let chat_id = msg.chat.id.0;
    let selected_date = match parse_callback_data(q.data.as_deref().unwrap_or_default()) {
        Some(CallbackAction::WidgetTab(d)) => d,
        _ => return Ok(()),
    };
    let group = match db.get_group(chat_id).await {
        Ok(Some(g)) if g != "26-21" => g,
        _ => {
            answer_callback(&bot, &q, Some("Сначала привяжите группу через /set"), true).await;
            return Ok(());
        }
    };
    answer_callback(&bot, &q, Some(&selected_date[..5.min(selected_date.len())]), false).await;
    let (fb_text, rich_html) = build_widget_rich(&service, &group, Some(&selected_date)).await;
    if let Some(rich) = &rich_html {
        match raw
            .edit_rich_message(chat_id, msg.id.0 as i64, rich, None)
            .await
        {
            Ok(_) => return Ok(()),
            Err(e) => tracing::debug!("edit_rich widget tab failed: {e}"),
        }
    }
    let (day_fb, day_rich) = build_resp(&service, &group, &selected_date, false);
    let text = if day_rich.is_some() {
        day_fb
    } else {
        fb_text
    };
    if let Err(e) = bot
        .edit_message_text(msg.chat.id, msg.id, text)
        .parse_mode(ParseMode::Html)
        .await
    {
        tracing::debug!("edit widget tab failed: {e}");
    }
    Ok(())
}

/// on_zvonki_date_callback: zvonki_date:ДД.ММ.ГГГГ.
/// В беседах — эфемерно (фото, иначе текст); если эфемерная отправка
/// не удалась — публичный fallback, как в Python. В ЛС — Rich + фото.
async fn zvonki_date_callback(
    bot: Bot,
    q: CallbackQuery,
    service: Arc<ScheduleService>,
    db: Arc<Db>,
    _webapp_url: Option<String>,
    raw: Arc<RawApi>,
) -> HandlerResult {
    let Some(msg) = callback_message(&q) else {
        return Ok(());
    };
    let chat_id = msg.chat.id.0;
    let date_str = match parse_callback_data(q.data.as_deref().unwrap_or_default()) {
        Some(CallbackAction::ZvonkiDate(d)) => d,
        _ => return Ok(()),
    };
    let Some(target_date) = crate::tyumen::parse_date(&date_str) else {
        answer_callback(&bot, &q, Some("Некорректная дата"), true).await;
        return Ok(());
    };
    let group = match db.get_group(chat_id).await {
        Ok(Some(g)) if g != "26-21" => g,
        _ => {
            answer_callback(&bot, &q, Some("Сначала привяжите группу через /set"), true).await;
            return Ok(());
        }
    };
    if !service.has_zvonki_for_date(&date_str) {
        answer_callback(
            &bot,
            &q,
            Some("На выбранную дату расписания звонков нет"),
            true,
        )
        .await;
        return Ok(());
    }
    let zv_photo = service.get_bells_photo_for_date(&date_str);
    // В групповых чатах — эфемерно: фото, иначе текст.
    if chat_id < 0 {
        if zv_photo.is_some()
            && send_bells_photo_ephemeral(
                &bot, &raw, &db, &service, &q, chat_id, &date_str, false,
            )
            .await
        {
            answer_callback(&bot, &q, None, false).await;
            return Ok(());
        }
        let (fb, _) =
            service.get_zvonki_resp_rich(&group, Some(target_date), false, tyumen_now());
        match raw
            .send_ephemeral_message(chat_id, &fb, q.from.id.0 as i64, &q.id)
            .await
        {
            Ok(_) => {
                answer_callback(&bot, &q, None, false).await;
                return Ok(());
            }
            Err(e) => {
                tracing::warn!("ephemeral zvonki send failed, public fallback: {e}");
                // Падаем ниже — публичная отправка, как в Python.
            }
        }
    }
    answer_callback(&bot, &q, Some(&format!("Звонки на {}", &date_str[..5.min(date_str.len())])), false).await;
    if let Err(e) = bot
        .edit_message_reply_markup(msg.chat.id, msg.id)
        .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
        .await
    {
        tracing::debug!("edit_reply_markup clear failed: {e}");
    }
    let (fb, rich) = service.get_zvonki_resp_rich(&group, Some(target_date), false, tyumen_now());
    throttled_send_rich(&bot, &raw, &db, chat_id, &fb, rich.as_deref(), None, None, msg.thread_id).await;
    if zv_photo.is_some() {
        let caption = format!("<b>Специальные звонки на {}</b>", escape_html(&date_str));
        if let Ok(Some(input)) = service.get_bells_photo_input(&date_str).await {
            if let Some(sent) = send_photo_retry(&bot, &db, chat_id, input, &caption, None).await {
                capture_bells_photo_file_id(&service, &date_str, &sent).await;
            }
        }
    }
    Ok(())
}

// ============================== РАССЫЛКА ДНЯ ==============================

/// safe_send_day_update (main.py): отправка обновления дня в чат с пином.
/// Возвращает захваченный file_id фото звонков (Some), если фото было
/// отправлено и file_id получен; иначе None (в т.ч. когда отправлен
/// только текст или отправка не удалась). Без rich — обновление не
/// отправляется вообще (как `if not fb_text or rich_html is None: return None`
/// в Python).
pub async fn safe_send_day_update(
    bot: &Bot,
    raw: &RawApi,
    service: &ScheduleService,
    db: &Db,
    chat_id: i64,
    group_name: &str,
    target_date: &str,
    update_type: &str,
    bells_photo_id: Option<&str>,
    prechecked_classes: bool,
) -> Option<String> {
    if !prechecked_classes {
        let day = service.format_day_data(group_name, target_date);
        if !has_classes(&day) {
            return None;
        }
    }
    let is_text_only = update_type == "change";
    let (fb_text, rich_html) = build_resp(service, group_name, target_date, is_text_only);
    let Some(rich_html) = rich_html else {
        return None;
    };
    let bells_photo = bells_photo_id
        .map(|s| s.to_string())
        .or_else(|| service.get_bells_photo_for_date(target_date));
    let fallback_full = day_update_fallback(group_name, target_date, &fb_text, is_text_only);

    // Rich-версия текста обновления: заголовок <h3> заменяется на
    // «Расписание на {дата} • » / «Изменения на {дата} • » (main.py:452).
    let rich_full = {
        let prefix_h3 = if is_text_only {
            format!("<h3>Изменения на {} • ", escape_html(target_date))
        } else {
            format!("<h3>Расписание на {} • ", escape_html(target_date))
        };
        if rich_html.starts_with("<h3>") {
            match rich_html.find("</h3>") {
                Some(end) => {
                    let h3_inner = &rich_html[4..end];
                    let grp_part = h3_inner.split('•').next().unwrap_or("").trim();
                    format!("{prefix_h3}{grp_part}</h3>{}", &rich_html[end + 5..])
                }
                None => format!(
                    "{prefix_h3}Группа {}</h3>\n{rich_html}",
                    escape_html(group_name)
                ),
            }
        } else {
            format!(
                "{prefix_h3}Группа {}</h3>\n{rich_html}",
                escape_html(group_name)
            )
        }
    };

    if update_type == "schedule" {
        if let Ok(Some(last_msg_id)) = db.get_last_pin_for_chat(chat_id).await {
            let _ = bot
                .unpin_chat_message(ChatId(chat_id))
                .message_id(MessageId(last_msg_id as i32))
                .await;
            tokio::time::sleep(Duration::from_millis(40)).await;
            let _ = bot.delete_message(ChatId(chat_id), MessageId(last_msg_id as i32)).await;
        }
    }

    let mut sent: Option<Message> = None;
    let mut captured_fid: Option<String> = None;
    // Совмещённая отправка «Расписание + Фото», если текст влезает в caption.
    if !is_text_only && fallback_full.chars().count() <= 1024 && bells_photo.is_some() {
        if let Ok(Some(input)) = service.get_bells_photo_input(target_date).await {
            let inline_kb = day_update_inline_kb(chat_id, target_date, false);
            let markup = inline_kb.map(ReplyMarkup::InlineKeyboard).or_else(|| {
                private_main_kb(chat_id).map(ReplyMarkup::Keyboard)
            });
            sent = send_photo_retry(bot, db, chat_id, input, &fallback_full, markup).await;
            if let Some(ref msg) = sent {
                captured_fid = capture_bells_photo_file_id(service, target_date, msg).await;
            }
        }
    }
    if sent.is_none() {
        let card_kb = day_update_inline_kb(chat_id, target_date, bells_photo.is_some());
        let markup = card_kb.map(ReplyMarkup::InlineKeyboard).or_else(|| {
            private_main_kb(chat_id).map(ReplyMarkup::Keyboard)
        });
        sent = throttled_send_rich(
            bot,
            raw,
            db,
            chat_id,
            &fallback_full,
            Some(&rich_full),
            markup,
            None,
            None,
        )
        .await;
    }
    let Some(sent_msg) = sent else {
        return None;
    };
    if let Err(e) = bot
        .pin_chat_message(ChatId(chat_id), sent_msg.id)
        .disable_notification(true)
        .await
    {
        tracing::debug!("pin_day_update({chat_id}) failed: {e}");
    } else if let Err(e) = db.add_pinned_msg(chat_id, sent_msg.id.0 as i64).await {
        tracing::debug!("add_pinned_msg({chat_id}) failed: {e}");
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
    captured_fid
}

/// broadcast_day_updates (main.py:650): очереди доставки по (дата, тип),
/// фильтр по наличию пар и дате >= сегодня. Возвращает JoinHandle'ы
/// запущенных задач рассылки (fire-and-forget, как asyncio.create_task).
pub async fn broadcast_day_updates(
    service: Arc<ScheduleService>,
    db: Arc<Db>,
    bot: Bot,
    raw_api: Arc<RawApi>,
    filtered_events: HashMap<(String, String), String>,
    all_chats: Vec<(i64, String)>,
    today_date: NaiveDate,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();
    if filtered_events.is_empty() || all_chats.is_empty() {
        return handles;
    }
    let mut chats_by_group: HashMap<String, Vec<(i64, String)>> = HashMap::new();
    for (c_id, g_name) in &all_chats {
        chats_by_group
            .entry(normalize_group(g_name, ""))
            .or_default()
            .push((*c_id, g_name.clone()));
    }
    let mut queues: HashMap<(String, String), Vec<(i64, String)>> = HashMap::new();
    for ((upd_grp, upd_date), upd_type) in &filtered_events {
        if let Some(d) = crate::tyumen::parse_date(upd_date) {
            if d < today_date {
                continue;
            }
        }
        let target_chats = chats_by_group
            .get(&normalize_group(upd_grp, ""))
            .cloned()
            .unwrap_or_default();
        if target_chats.is_empty() {
            continue;
        }
        let day = service.format_day_data(upd_grp, upd_date);
        if !has_classes(&day) {
            continue;
        }
        queues
            .entry((upd_date.clone(), upd_type.clone()))
            .or_default()
            .extend(target_chats);
    }
    for ((upd_date, upd_type), delivery_queue) in queues {
        if delivery_queue.is_empty() {
            continue;
        }
        let bells_photo = if upd_type == "schedule" {
            service.get_bells_photo_for_date(&upd_date)
        } else {
            None
        };
        // Проба для захвата file_id (main.py:650): для очередей типа
        // «schedule» с фото-URL — сначала скачать байты (без этого первый
        // send_photo упадёт), затем последовательно, начиная с ЛС,
        // отправлять только чатам, чей текст влезает в caption ≤1024
        // (только совмещённая отправка захватывает file_id), до первого
        // захваченного file_id (макс. 25 чатов). Пробные чаты уже получили
        // обновление и исключаются из параллельной рассылки
        // (probed_indices) — двойной отправки нет. Per-date lock из
        // Python здесь не нужен: broadcast вызывается только из
        // последовательного цикла vk_watcher, конкурентных рассылок
        // одной даты не бывает.
        let mut photo_ref = bells_photo.clone();
        let mut probed_indices: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        if upd_type == "schedule" {
            if let Some(photo) = &bells_photo {
                if (photo.starts_with("http://") || photo.starts_with("https://"))
                    && service.download_photo_bytes(photo).await.is_ok()
                {
                    let mut short_group: HashMap<String, bool> = HashMap::new();
                    for (_, g_name) in &delivery_queue {
                        if short_group.contains_key(g_name) {
                            continue;
                        }
                        let (fb_probe, rich_probe) =
                            build_resp(&service, g_name, &upd_date, false);
                        let ok = !fb_probe.is_empty()
                            && rich_probe.is_some()
                            && day_update_fallback(g_name, &upd_date, &fb_probe, false)
                                .chars()
                                .count()
                                <= 1024;
                        short_group.insert(g_name.clone(), ok);
                    }
                    let mut probe_order: Vec<usize> = delivery_queue
                        .iter()
                        .enumerate()
                        .filter(|(_, (c_id, g_name))| {
                            *c_id > 0 && short_group.get(g_name).copied().unwrap_or(false)
                        })
                        .map(|(i, _)| i)
                        .collect();
                    probe_order.extend(
                        delivery_queue
                            .iter()
                            .enumerate()
                            .filter(|(_, (c_id, g_name))| {
                                *c_id < 0 && short_group.get(g_name).copied().unwrap_or(false)
                            })
                            .map(|(i, _)| i),
                    );
                    for idx in probe_order.into_iter().take(25) {
                        let (c_id, g_name) = &delivery_queue[idx];
                        probed_indices.insert(idx);
                        let captured = safe_send_day_update(
                            &bot,
                            &raw_api,
                            &service,
                            &db,
                            *c_id,
                            g_name,
                            &upd_date,
                            &upd_type,
                            photo_ref.as_deref(),
                            true,
                        )
                        .await;
                        if let Some(fid) = captured {
                            photo_ref = Some(fid);
                            break;
                        }
                    }
                }
            }
        }
        for (idx, (c_id, g_name)) in delivery_queue.into_iter().enumerate() {
            if probed_indices.contains(&idx) {
                continue;
            }
            let svc = service.clone();
            let dbc = db.clone();
            let botc = bot.clone();
            let rawc = raw_api.clone();
            let date = upd_date.clone();
            let typ = upd_type.clone();
            let photo = photo_ref.clone();
            handles.push(tokio::spawn(async move {
                safe_send_day_update(
                    &botc,
                    &rawc,
                    &svc,
                    &dbc,
                    c_id,
                    &g_name,
                    &date,
                    &typ,
                    photo.as_deref(),
                    true,
                )
                .await;
            }));
        }
    }
    handles
}

// ============================== DISPATCHER ==============================

/// Схема хендлеров (дерево dptree) — отдельно от dispatcher(), чтобы её
/// можно было собрать и проверить в офлайн-тестах без БД и сервиса.
pub fn handler_schema() -> UpdateHandler<teloxide::RequestError> {
    let message_handler = Update::filter_message()
        .branch(
            dptree::filter(|msg: Message| {
                matches!(parse_command(msg.text().unwrap_or_default()), Some(ParsedCommand::Start))
            })
            .endpoint(start_handler),
        )
        .branch(
            dptree::filter(|msg: Message| {
                matches!(parse_command(msg.text().unwrap_or_default()), Some(ParsedCommand::Set(_)))
            })
            .endpoint(set_handler),
        )
        .branch(
            dptree::filter(|msg: Message| {
                matches!(parse_command(msg.text().unwrap_or_default()), Some(ParsedCommand::Help))
            })
            .endpoint(help_handler),
        )
        .branch(
            dptree::filter(|msg: Message| parse_date_query(msg.text().unwrap_or_default()).is_some())
            .endpoint(date_query_handler),
        )
        .branch(
            dptree::filter(|msg: Message| {
                msg.text().map(|t| RASP_BTN_TEXTS.contains(&t)).unwrap_or(false)
            })
            .endpoint(rasp_handler),
        )
        .branch(
            dptree::filter(|msg: Message| {
                msg.text().map(|t| TOMORROW_BTN_TEXTS.contains(&t)).unwrap_or(false)
            })
            .endpoint(tomorrow_handler),
        )
        .branch(
            dptree::filter(|msg: Message| {
                msg.text().map(|t| ZVONKI_BTN_TEXTS.contains(&t)).unwrap_or(false)
            })
            .endpoint(zvonki_handler),
        )
        .branch(
            dptree::filter(|msg: Message| msg.pinned_message().is_some())
                .endpoint(delete_pin_service_handler),
        )
        .branch(
            dptree::filter(|msg: Message| msg.migrate_to_chat_id().is_some())
                .endpoint(migrate_handler),
        );

    let callback_handler = Update::filter_callback_query()
        .branch(
            dptree::filter(|q: CallbackQuery| {
                q.data.as_deref().map(|d| d.starts_with("set_grp:")).unwrap_or(false)
            })
            .endpoint(set_grp_callback),
        )
        .branch(
            dptree::filter(|q: CallbackQuery| {
                q.data.as_deref().map(|d| d.starts_with("bells_photo:")).unwrap_or(false)
            })
            .endpoint(bells_photo_callback),
        )
        .branch(
            dptree::filter(|q: CallbackQuery| {
                q.data.as_deref().map(|d| d.starts_with("ephemeral_rasp:")).unwrap_or(false)
            })
            .endpoint(ephemeral_rasp_callback),
        )
        .branch(
            dptree::filter(|q: CallbackQuery| {
                q.data.as_deref().map(|d| d.starts_with("ephemeral_bells:")).unwrap_or(false)
            })
            .endpoint(ephemeral_bells_callback),
        )
        .branch(
            dptree::filter(|q: CallbackQuery| {
                q.data.as_deref().map(|d| d.starts_with("widget_tab:")).unwrap_or(false)
            })
            .endpoint(widget_tab_callback),
        )
        .branch(
            dptree::filter(|q: CallbackQuery| {
                q.data.as_deref().map(|d| d.starts_with("zvonki_date:")).unwrap_or(false)
            })
            .endpoint(zvonki_date_callback),
        );

    dptree::entry().branch(message_handler).branch(callback_handler)
}

/// Собирает teloxide Dispatcher со всеми хендлерами из main.py.
/// Возвращает dispatcher; запуск (webhook/polling) — в serve().
pub fn dispatcher(
    bot: teloxide::Bot,
    service: Arc<ScheduleService>,
    db: Arc<Db>,
    webapp_url: Option<String>,
    raw_api: Arc<RawApi>,
) -> teloxide::dispatching::Dispatcher<teloxide::Bot, teloxide::RequestError, teloxide::dispatching::DefaultKey> {
    teloxide::dispatching::Dispatcher::builder(bot, handler_schema())
        .dependencies(dptree::deps![service, db, webapp_url, raw_api])
        .default_handler(|upd| async move {
            tracing::debug!("Unhandled update: {:?}", upd);
        })
        // with_custom_text уже возвращает Arc<Self>.
        .error_handler(
            teloxide::error_handlers::LoggingErrorHandler::with_custom_text(
                "An error has occurred in the dispatcher",
            ),
        )
        .build()
}

// ============================== ФОНОВЫЕ ЗАДАЧИ ==============================

/// Фоновая задача VK watcher. Точная копия vk_watcher (main.py:1439).
pub async fn vk_watcher(
    service: Arc<ScheduleService>,
    db: Arc<Db>,
    bot: teloxide::Bot,
    raw_api: Arc<RawApi>,
) {
    tokio::time::sleep(Duration::from_secs(5)).await;
    tracing::info!("👀 VK Watcher запущен в фоне");
    loop {
        let mut sleep_delay;
        let now = tyumen_now();
        let hour = now.hour();
        sleep_delay = watcher_sleep_secs(hour, false, 0.0);
        if in_work_hours(hour) {
            match service.update_check(5).await {
                Ok(updated_events) => {
                    if service.vk.is_rate_limited() {
                        sleep_delay = watcher_sleep_secs(
                            hour,
                            true,
                            service.vk.backoff_delay(),
                        );
                    }
                    if !updated_events.is_empty() {
                        // Кэшей ответов как в Python здесь нет; свежие данные
                        // уже в archive_cache (их НЕЛЬЗЯ чистить — см. шапку).
                        // load_ram перечитывает архив из БД и убирает stale.
                        if let Err(e) = service.load_ram().await {
                            tracing::error!("load_ram after VK update failed: {e}");
                        } else {
                            tracing::info!("🧹 Кэш ответов очищен после обновления");
                        }
                        let filtered = filter_events(&updated_events);
                        match db.get_all_chats().await {
                            Ok(all_chats) => {
                                let today = tyumen_now().date_naive();
                                broadcast_day_updates(
                                    service.clone(),
                                    db.clone(),
                                    bot.clone(),
                                    raw_api.clone(),
                                    filtered,
                                    all_chats,
                                    today,
                                )
                                .await;
                            }
                            Err(e) => tracing::error!("get_all_chats failed: {e}"),
                        }
                    }
                }
                Err(e) => tracing::error!("VK Watcher Error: {e}"),
            }
        }
        tokio::time::sleep(Duration::from_secs_f64(sleep_delay)).await;
    }
}

/// Фоновая задача cleaner. Точная копия cleaner (main.py:1477).
pub async fn cleaner(service: Arc<ScheduleService>, db: Arc<Db>, bot: teloxide::Bot) {
    tokio::time::sleep(Duration::from_secs(10)).await;
    tracing::info!("🧹 Cleaner запущен");
    loop {
        match db.get_old_pins().await {
            Ok(old_pins) => {
                for (c_id, m_id) in old_pins {
                    match bot
                        .delete_message(ChatId(c_id), MessageId(m_id as i32))
                        .await
                    {
                        Ok(_) => {}
                        Err(e) => {
                            if matches!(e, teloxide::RequestError::MigrateToChatId(_))
                                || is_dead_chat_error(&e.to_string())
                            {
                                let _ = db.delete_chat(c_id).await;
                            }
                        }
                    }
                }
            }
            Err(e) => tracing::error!("get_old_pins failed: {e}"),
        }
        if let Err(e) = db.clean_expired_files(14).await {
            tracing::error!("clean_expired_files failed: {e}");
        }
        if let Err(e) = db.clean_old_archive(45).await {
            tracing::error!("clean_old_archive failed: {e}");
        }
        if let Err(e) = service.load_ram().await {
            tracing::error!("Cleaner load_ram failed: {e}");
        }
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

// ============================== КОМАНДЫ / MENU BUTTON ==============================

/// Установка команд бота + menu button. Как setup_bot_commands (main.py:1517
/// + пропатченный main.py: menu button WebApp «Расписание»).
pub async fn setup_bot_commands(bot: &teloxide::Bot, webapp_url: Option<&str>) -> Result<()> {
    let commands = vec![
        BotCommand::new("start", "Начало работы"),
        BotCommand::new("set", "Выбрать группу (например, /set 24-27)"),
        BotCommand::new("help", "Справка по командам"),
    ];
    bot.set_my_commands(commands)
        .await
        .map_err(|e| AppError::Telegram(e.to_string()))?;
    if let Some(url) = webapp_url {
        match url::Url::parse(url) {
            Ok(url) => {
                if let Err(e) = bot
                    .set_chat_menu_button()
                    .menu_button(MenuButton::WebApp {
                        text: "Расписание".to_string(),
                        web_app: WebAppInfo { url },
                    })
                    .await
                {
                    tracing::warn!("Не удалось установить кнопку Mini App: {e}");
                }
            }
            Err(e) => tracing::warn!("Не удалось установить кнопку Mini App: bad URL {url}: {e}"),
        }
    }
    Ok(())
}

// ============================== HEALTH / СЕРВЕР ==============================

/// Тело health-ответа: {"status":"ok","service":"schedule_bot_next_gen"}.
pub fn health_json() -> serde_json::Value {
    json!({"status": "ok", "service": "schedule_bot_next_gen"})
}

/// Cache-Control для статики /app/* (цель — открытие Mini App < 1 с):
/// версионируемые файлы (liquidGL.js и т.п.) — immutable на год,
/// index.html — no-cache, чтобы обновления Mini App доходили сразу.
/// Gzip здесь НЕТ: tower-http 0.5.2 compression-gzip требует
/// async-compression, которого нет в Cargo.lock и в локальном кэше —
/// офлайн включить нельзя (зафиксировано как пропущенное, без скачиваний).
/// Публичная: используется в `examples/verify_e2e.rs` вместо копии.
pub async fn static_cache_headers_mw(req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let is_static = path.starts_with("/app/");
    let mut resp = next.run(req).await;
    if is_static {
        let value = if path == "/app/" || path == "/app/index.html" {
            "no-cache"
        } else {
            "public, max-age=31536000, immutable"
        };
        resp.headers_mut()
            .insert("cache-control", HeaderValue::from_static(value));
    }
    resp
}

async fn health_handler() -> Json<serde_json::Value> {
    Json(health_json())
}

/// Роутер только с health-эндпоинтом (для тестов и переиспользования).
pub fn health_router() -> Router {
    Router::new().route("/", get(health_handler))
}

/// Сигнал завершения: Ctrl-C или SIGTERM (как on_shutdown в Python).
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::debug!("ctrl_c listener failed: {e}");
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::debug!("SIGTERM listener failed: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Точка входа сервера: webhook/polling, axum-сервер (health + /webhook +
/// /api/* + /app/*), фоновые задачи, graceful shutdown.
/// Ждёт доступности Telegram (getMe) перед стартом диспетчера.
///
/// teloxide внутри `dispatch()`/`dispatch_with_listener()` вызывает getMe один
/// раз и ПАНИКУЕТ при ошибке сети — при временном сбое Telegram (или прокси
/// на пути к api.telegram.org) это убивало бы приём сообщений, хотя HTTP-сервер
/// и VK-watcher продолжали работать. Поэтому сначала сами дожимаем getMe
/// с backoff (5/10/20/30/30… с, без ограничения попыток) и только потом
/// стартуем диспетчер. Вызывается в начале задачи диспетчера в обоих режимах.
/// Найдено живым тестом 20.11.2026: из песочницы до api.telegram.org доходят
/// не все запросы (curl: 2 из 3), а диспетчер падал на первом же таймауте.
async fn wait_telegram_ready(bot: &teloxide::Bot) {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match bot.get_me().await {
            Ok(_) => {
                tracing::info!("✅ Telegram доступен (getMe, попытка {attempt})");
                return;
            }
            Err(e) => {
                let delay = telegram_retry_delay(attempt);
                tracing::warn!(
                    "getMe (попытка {attempt}) не удался: {e} — повтор через {delay:?}"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Задержка перед N-й повторной попыткой getMe: 5, 10, 20, затем 30 с.
fn telegram_retry_delay(attempt: u32) -> std::time::Duration {
    let secs = match attempt {
        1 => 5,
        2 => 10,
        3 => 20,
        _ => 30,
    };
    std::time::Duration::from_secs(secs)
}

/// init_db и load_ram уже выполнены в main.rs до вызова serve().
/// Задержка перед очередной попыткой setWebhook: 5, 15, 30, 60, 120, 300 с
/// (дальше — 300 с на каждую попытку).
pub fn webhook_retry_delay(attempt: usize) -> Duration {
    const DELAYS_SECS: [u64; 6] = [5, 15, 30, 60, 120, 300];
    Duration::from_secs(DELAYS_SECS[attempt.min(DELAYS_SECS.len() - 1)])
}

/// Фоновая установка webhook (НЕ фатальная): повторяет setWebhook до
/// успеха с backoff 5/15/30/60/120/300 с. Параметры — как у teloxide
/// setup_webhook: url, drop_pending_updates=true, secret_token.
/// Вызывается из serve() после поднятия HTTP-сервера: при недоступном
/// Telegram или невалидном токене процесс НЕ умирает, Mini App API
/// продолжает отвечать, а webhook встанет, как только Telegram
/// станет доступен (или после перезапуска с валидным токеном).
async fn run_webhook_setup_loop(raw: RawApi, webhook_url: String, secret: String) {
    let mut attempt = 0usize;
    loop {
        match raw.set_webhook(&webhook_url, true, &secret).await {
            Ok(()) => {
                tracing::info!("✅ Webhook установлен: {webhook_url}");
                return;
            }
            Err(e) => {
                let delay = webhook_retry_delay(attempt);
                tracing::warn!(
                    "set_webhook failed (попытка {}): {e} — повтор через {delay:?}",
                    attempt + 1
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

pub async fn serve(config: Config, service: Arc<ScheduleService>, db: Arc<Db>) -> Result<()> {
    let bot = teloxide::Bot::new(config.bot_token.clone());
    // Прямые вызовы Bot API (Rich Messages, эфемерные, setWebhook) —
    // один экземпляр на весь процесс: dispatcher, vk_watcher, webhook-loop.
    let raw_api = Arc::new(RawApi::new(config.bot_token.clone()));
    let webapp_url = config.webapp_url();
    tracing::info!("🚀 Startup: установка команд бота...");
    if let Err(e) = setup_bot_commands(&bot, webapp_url.as_deref()).await {
        tracing::warn!("Не удалось установить команды: {e}");
    }

    // Фоновые задачи (как on_startup в Python + reminder_task из патча).
    let vk_handle = tokio::spawn(vk_watcher(
        service.clone(),
        db.clone(),
        bot.clone(),
        raw_api.clone(),
    ));
    let cleaner_handle = tokio::spawn(cleaner(service.clone(), db.clone(), bot.clone()));
    let reminders = Arc::new(ReminderRunner::new());
    let reminder_handle = {
        let runner = reminders.clone();
        let svc = service.clone();
        let dbc = db.clone();
        let sender = Arc::new(bot.clone());
        let url = webapp_url.clone();
        tokio::spawn(async move {
            runner.run_loop(sender, svc, dbc, url).await;
        })
    };

    // Dispatcher + транспорт обновлений. В webhook-режиме axum_no_setup
    // возвращает Router с POST /webhook — он монтируется в общий сервер ниже.
    let mut dp = dispatcher(
        bot.clone(),
        service.clone(),
        db.clone(),
        webapp_url.clone(),
        raw_api.clone(),
    );
    let (dp_handle, webhook_router): (tokio::task::JoinHandle<()>, Router) =
        if let Some(webhook_url) = config.webhook_url() {
        // Webhook: axum_no_setup НЕ вызывает set_webhook (в отличие от
        // axum_to_router) — сервер поднимается всегда, а setWebhook
        // выполняется в фоне с повторами (run_webhook_setup_loop).
        // Секрет берётся тот же, что уйдёт в setWebhook: teloxide
        // проверяет его в заголовке X-Telegram-Bot-Api-Secret-Token
        // входящих апдейтов.
        let addr: std::net::SocketAddr =
            ([0, 0, 0, 0], config.port).into();
        let url: url::Url = webhook_url
            .parse()
            .map_err(|e| AppError::config(format!("bad webhook URL {webhook_url}: {e}")))?;
        let mut options = webhooks::Options::new(addr, url).drop_pending_updates();
        if !config.webhook_secret.is_empty()
            && config
                .webhook_secret
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            options = options.secret_token(config.webhook_secret.clone());
        } else if !config.webhook_secret.is_empty() {
            tracing::warn!("WEBHOOK_SECRET содержит недопустимые символы — teloxide сгенерирует свой");
        }
        let secret = options.get_or_gen_secret_token().to_owned();
        let (listener, stop_flag, webhook_router) = webhooks::axum_no_setup(options);
        // setWebhook — в фоне, не фатально.
        tokio::spawn(run_webhook_setup_loop(
            RawApi::new(config.bot_token.clone()),
            webhook_url.clone(),
            secret,
        ));
        // При остановке listener'а — delete_webhook (как делал stop_future
        // у axum_to_router).
        let botc = bot.clone();
        tokio::spawn(async move {
            stop_flag.await;
            if let Err(e) = botc.delete_webhook().await {
                tracing::error!("Couldn't delete webhook: {e}");
            }
        });
        let bot_for_wait = bot.clone();
        (
            tokio::spawn(async move {
                wait_telegram_ready(&bot_for_wait).await;
                dp.dispatch_with_listener(
                    listener,
                    teloxide::error_handlers::LoggingErrorHandler::with_custom_text(
                        "An error from the update listener",
                    ),
                )
                .await;
            }),
            webhook_router,
        )
    } else {
        // Long polling: сначала сбросить webhook, как в on_startup.
        tracing::warn!("⚠️ RAILWAY_PUBLIC_DOMAIN не задан — запуск в режиме Long Polling");
        if let Err(e) = bot.delete_webhook().drop_pending_updates(true).await {
            tracing::warn!("delete_webhook failed: {e}");
        }
        let bot_for_wait = bot.clone();
        (
            tokio::spawn(async move {
                wait_telegram_ready(&bot_for_wait).await;
                dp.dispatch().await;
            }),
            Router::new(),
        )
    };

    let webapp_dir =
        std::env::var("WEBAPP_DIR").unwrap_or_else(|_| "webapp".to_string());
    let app = Router::new()
        .merge(health_router())
        .merge(webhook_router)
        .merge(api::router_with_middleware(ApiState {
            service: service.clone(),
            db: db.clone(),
            bot_token: config.bot_token.clone(),
        }))
        .nest_service("/app", ServeDir::new(webapp_dir))
        // Cache-Control только для /app/* (см. static_cache_headers_mw).
        .layer(middleware::from_fn(static_cache_headers_mw))
        // Security headers — на весь app, включая /app/* и /.
        // На /api/* они добавляются и здесь, и в router_with_middleware
        // (повторная вставка тех же значений безвредна).
        .layer(middleware::from_fn(api::security_headers_mw));

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", config.port))
        .await
        .map_err(|e| AppError::Io(e))?;
    tracing::info!("🌐 HTTP-сервер на 0.0.0.0:{}", config.port);
    // into_make_service_with_connect_info — для ConnectInfo в rate-limit
    // (IP клиента) в api::router_with_middleware.
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| AppError::Io(e))?;

    // Graceful shutdown (как on_shutdown в Python): отмена задач.
    tracing::info!("🛑 Shutdown: завершение фоновых задач...");
    dp_handle.abort();
    vk_handle.abort();
    cleaner_handle.abort();
    reminder_handle.abort();
    Ok(())
}

// ============================== ТЕСТЫ (офлайн) ==============================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use teloxide::types::InlineKeyboardButtonKind;

    fn day_data() -> DayData {
        DayData {
            target_group: "ЮР-26-21".to_string(),
            pairs: BTreeMap::from([
                (1, ("Химия".to_string(), "206".to_string())),
                (2, ("Физика".to_string(), "101".to_string())),
                (3, ("Математика".to_string(), "—".to_string())),
            ]),
            changes: vec!["<b>Замена</b>: 1 пара — Химия".to_string()],
        }
    }

    // ---------- telegram_retry_delay ----------

    #[test]
    fn telegram_retry_delay_backoff() {
        assert_eq!(telegram_retry_delay(1).as_secs(), 5);
        assert_eq!(telegram_retry_delay(2).as_secs(), 10);
        assert_eq!(telegram_retry_delay(3).as_secs(), 20);
        assert_eq!(telegram_retry_delay(4).as_secs(), 30);
        assert_eq!(telegram_retry_delay(99).as_secs(), 30);
    }

    // ---------- parse_command ----------

    #[test]
    fn parse_command_variants() {
        assert_eq!(parse_command("/start"), Some(ParsedCommand::Start));
        assert_eq!(parse_command("/start@rasp_bot"), Some(ParsedCommand::Start));
        assert_eq!(parse_command("/help"), Some(ParsedCommand::Help));
        assert_eq!(parse_command("/help@rasp_bot"), Some(ParsedCommand::Help));
        assert_eq!(parse_command("/set"), Some(ParsedCommand::Set(None)));
        assert_eq!(
            parse_command("/set 24-27"),
            Some(ParsedCommand::Set(Some("24-27".to_string())))
        );
        assert_eq!(
            parse_command("/set@rasp_bot ИП-24-27б"),
            Some(ParsedCommand::Set(Some("ИП-24-27б".to_string())))
        );
        assert_eq!(parse_command("/startfoo"), None);
        assert_eq!(parse_command("Расписание"), None);
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("/unknown"), None);
    }

    // ---------- RE_DATE_QUERY ----------

    #[test]
    fn parse_date_query_forms() {
        let year = tyumen_now().year();
        // Без года — текущий год Тюмени.
        assert_eq!(
            parse_date_query("20.11"),
            NaiveDate::from_ymd_opt(year, 11, 20)
        );
        // Год из двух цифр — 20xx.
        assert_eq!(
            parse_date_query("20.11.26"),
            NaiveDate::from_ymd_opt(2026, 11, 20)
        );
        // Полный год.
        assert_eq!(
            parse_date_query("20.11.2026"),
            NaiveDate::from_ymd_opt(2026, 11, 20)
        );
        // Однозначные день/месяц.
        assert_eq!(
            parse_date_query("5.1.26"),
            NaiveDate::from_ymd_opt(2026, 1, 5)
        );
        // Несуществующие даты.
        assert_eq!(parse_date_query("32.11"), None);
        assert_eq!(parse_date_query("20.13"), None);
        assert_eq!(parse_date_query("29.02.2026"), None); // 2026 — не високосный
        assert_eq!(
            parse_date_query("29.02.2024"),
            NaiveDate::from_ymd_opt(2024, 2, 29)
        );
        // Не шаблон.
        assert_eq!(parse_date_query("20-11"), None);
        assert_eq!(parse_date_query("20.11.20261"), None);
        assert_eq!(parse_date_query("завтра"), None);
    }

    #[test]
    fn parse_date_parts_keeps_invalid_date() {
        // Части разбираются даже для несуществующей даты — хендлер сам
        // отвечает «Указана несуществующая календарная дата.»
        assert!(parse_date_parts("32.01").is_some());
        assert_eq!(parse_date_query("32.01"), None);
    }

    // ---------- callback_data ----------

    #[test]
    fn parse_callback_data_all_prefixes() {
        assert_eq!(
            parse_callback_data("set_grp:ЮР-26-21"),
            Some(CallbackAction::SetGroup("ЮР-26-21".to_string()))
        );
        assert_eq!(
            parse_callback_data("bells_photo:20.11.2026"),
            Some(CallbackAction::BellsPhoto("20.11.2026".to_string()))
        );
        assert_eq!(
            parse_callback_data("ephemeral_rasp:20.11.2026"),
            Some(CallbackAction::EphemeralRasp("20.11.2026".to_string()))
        );
        assert_eq!(
            parse_callback_data("ephemeral_bells:20.11.2026"),
            Some(CallbackAction::EphemeralBells("20.11.2026".to_string()))
        );
        assert_eq!(
            parse_callback_data("widget_tab:20.11.2026"),
            Some(CallbackAction::WidgetTab("20.11.2026".to_string()))
        );
        assert_eq!(
            parse_callback_data("zvonki_date:20.11.2026"),
            Some(CallbackAction::ZvonkiDate("20.11.2026".to_string()))
        );
        assert_eq!(parse_callback_data("unknown:20.11.2026"), None);
        assert_eq!(parse_callback_data("set_grp:"), Some(CallbackAction::SetGroup(String::new())));
        assert_eq!(parse_callback_data("nocolon"), None);
    }

    // ---------- группы ----------

    #[test]
    fn valid_group_regex() {
        assert!(is_valid_group("24-27"));
        assert!(is_valid_group("ИП-24-27"));
        assert!(is_valid_group("ИП-24-27б"));
        assert!(is_valid_group("ЮР-26-21"));
        assert!(is_valid_group("26-21"));
        assert!(is_valid_group("БУХ-26-02"));
        assert!(!is_valid_group("hello"));
        assert!(!is_valid_group("1-2"));
        assert!(!is_valid_group(""));
    }

    // ---------- html ----------

    #[test]
    fn html_helpers() {
        assert_eq!(escape_html("<b>А&Б</b>"), "&lt;b&gt;А&amp;Б&lt;/b&gt;");
        assert_eq!(escape_html("\"x\""), "&quot;x&quot;");
        assert_eq!(strip_html_tags("<b>Привет</b> мир"), "Привет мир");
        assert_eq!(clean_change_line("⚠️ <b>Замена</b>"), "Замена");
        assert_eq!(clean_change_line("ℹ️ текст"), "текст");
        assert_eq!(clean_change_line("обычный текст"), "обычный текст");
        assert_eq!(alert_text("<b>длинный</b> текст"), "длинный текст");
        let long = "x".repeat(300);
        assert_eq!(alert_text(&long).chars().count(), 195);
    }

    // ---------- build_resp fallback ----------

    #[test]
    fn format_day_fallback_full() {
        let fb = format_day_fallback("ЮР-26-21", "20.11.2026", &day_data(), false);
        assert!(fb.starts_with("<b>ЮР-26-21</b> • 20.11.2026"));
        assert!(fb.contains("<b>Основное</b>"));
        assert!(fb.contains("1. Химия — 206"));
        assert!(fb.contains("2. Физика — 101"));
        assert!(fb.contains("3. Математика")); // кабинет «—» не показывается
        assert!(!fb.contains("Математика —"));
        assert!(fb.contains("<b>Замены</b>"));
        assert!(fb.contains("Замена: 1 пара — Химия")); // <b> убран
    }

    #[test]
    fn format_day_fallback_only_changes() {
        let fb = format_day_fallback("ЮР-26-21", "20.11.2026", &day_data(), true);
        assert!(!fb.contains("<b>Основное</b>"));
        assert!(fb.contains("<b>Замены</b>"));
    }

    #[test]
    fn format_day_fallback_no_data() {
        let empty = DayData::default();
        let fb = format_day_fallback("24-27", "20.11.2026", &empty, false);
        assert_eq!(fb, "Данных для <b>24-27</b> на <b>20.11.2026</b> нет.");
        let no_pairs = DayData {
            target_group: "24-27".to_string(),
            pairs: BTreeMap::new(),
            changes: vec![],
        };
        let fb = format_day_fallback("24-27", "20.11.2026", &no_pairs, false);
        assert!(fb.contains("<i>Пар нет</i>"));
    }

    #[test]
    fn build_widget_fallback_joins_days() {
        assert!(build_widget_fallback("24-27", &[]).contains("расписания для <b>24-27</b> нет"));
        let days = vec![
            ("20.11.2026".to_string(), day_data()),
            ("21.11.2026".to_string(), day_data()),
        ];
        let text = build_widget_fallback("ЮР-26-21", &days);
        assert_eq!(text.matches("20.11.2026").count(), 1);
        assert_eq!(text.matches("21.11.2026").count(), 1);
    }

    #[test]
    fn day_update_fallback_headers() {
        let fb = "<b>ЮР-26-21</b> • 20.11.2026\n\n<b>Основное</b>\n1. Химия — 206\n";
        let schedule = day_update_fallback("ЮР-26-21", "20.11.2026", fb, false);
        assert!(schedule.starts_with("<b>Расписание на 20.11.2026</b>"));
        assert!(schedule.contains("<b>ЮР-26-21</b>"));
        assert!(!schedule.contains("• 20.11.2026\n\n<b>Основное</b>\n1. Химия — 206\n\n<b>Основное</b>"));
        let change = day_update_fallback("ЮР-26-21", "20.11.2026", fb, true);
        assert!(change.starts_with("<b>Изменения на 20.11.2026</b>"));
    }

    // ---------- has_classes ----------

    #[test]
    fn has_classes_cases() {
        assert!(has_classes(&day_data()));
        let practice = DayData {
            target_group: "24-27".to_string(),
            pairs: BTreeMap::from([(1, ("Практика".to_string(), "—".to_string()))]),
            changes: vec![],
        };
        assert!(!has_classes(&practice));
        let weekend = DayData {
            target_group: "24-27".to_string(),
            pairs: BTreeMap::new(),
            changes: vec!["Выходной".to_string()],
        };
        assert!(!has_classes(&weekend));
        let extra = DayData {
            target_group: "24-27".to_string(),
            pairs: BTreeMap::new(),
            changes: vec!["Физкультура в Экопарке, сбор в 9.30".to_string()],
        };
        assert!(has_classes(&extra));
        assert!(!has_classes(&DayData::default()));
    }

    // ---------- broadcast: дедупликация событий ----------

    #[test]
    fn filter_events_schedule_priority() {
        // «schedule» приоритетнее «change» независимо от порядка в HashSet.
        let mut events = HashSet::new();
        events.insert(("ЮР-26-21".to_string(), "20.11.2026".to_string(), "change".to_string()));
        events.insert(("ЮР-26-21".to_string(), "20.11.2026".to_string(), "schedule".to_string()));
        events.insert(("ТД-26-21".to_string(), "20.11.2026".to_string(), "change".to_string()));
        let filtered = filter_events(&events);
        assert_eq!(filtered.len(), 2);
        assert_eq!(
            filtered.get(&("ЮР-26-21".to_string(), "20.11.2026".to_string())),
            Some(&"schedule".to_string())
        );
        assert_eq!(
            filtered.get(&("ТД-26-21".to_string(), "20.11.2026".to_string())),
            Some(&"change".to_string())
        );
        // Только change — остаётся change.
        let mut only_change = HashSet::new();
        only_change.insert(("24-27".to_string(), "21.11.2026".to_string(), "change".to_string()));
        let filtered = filter_events(&only_change);
        assert_eq!(
            filtered.get(&("24-27".to_string(), "21.11.2026".to_string())),
            Some(&"change".to_string())
        );
    }

    // ---------- watcher: время ----------

    #[test]
    fn watcher_timing() {
        assert!(is_peak_hour(7));
        assert!(is_peak_hour(8));
        assert!(!is_peak_hour(9));
        assert!(is_peak_hour(18));
        assert!(is_peak_hour(20));
        assert!(!is_peak_hour(21));
        assert!(!is_peak_hour(3));
        assert!(in_work_hours(6));
        assert!(in_work_hours(20));
        assert!(!in_work_hours(21));
        assert!(!in_work_hours(5));
        assert_eq!(watcher_sleep_secs(8, false, 0.0), 5.0);
        assert_eq!(watcher_sleep_secs(12, false, 0.0), 10.0);
        assert_eq!(watcher_sleep_secs(12, true, 5.0), 15.0); // max(15, min(30, 5))
        assert_eq!(watcher_sleep_secs(12, true, 40.0), 30.0); // max(15, min(30, 40))
        assert_eq!(watcher_sleep_secs(12, true, 20.0), 20.0);
    }

    #[test]
    fn tomorrow_date_skips_sunday() {
        // 20.11.2026 — пятница -> суббота; 21.11.2026 — суббота -> понедельник.
        let fri = NaiveDate::from_ymd_opt(2026, 11, 20).unwrap();
        let sat = NaiveDate::from_ymd_opt(2026, 11, 21).unwrap();
        assert_eq!(tomorrow_date(fri), NaiveDate::from_ymd_opt(2026, 11, 21).unwrap());
        assert_eq!(tomorrow_date(sat), NaiveDate::from_ymd_opt(2026, 11, 23).unwrap());
    }

    // ---------- клавиатуры ----------

    #[test]
    fn keyboards_shape() {
        let kb = main_kb();
        assert_eq!(kb.keyboard.len(), 2);
        assert_eq!(kb.keyboard[0].len(), 2);
        assert_eq!(kb.keyboard[1].len(), 1);
        assert_eq!(kb.keyboard[0][0].text, "Расписание");
        assert!(kb.resize_keyboard);

        let spec = spec_26_21_kb();
        assert_eq!(spec.inline_keyboard.len(), 2);
        assert_eq!(spec.inline_keyboard[0][0].text, "ЮР-26-21 (Юриспруденция)");
        assert!(matches!(
            spec.inline_keyboard[0][0].kind,
            InlineKeyboardButtonKind::CallbackData(ref d) if d == "set_grp:ЮР-26-21"
        ));
        assert!(matches!(
            spec.inline_keyboard[1][0].kind,
            InlineKeyboardButtonKind::CallbackData(ref d) if d == "set_grp:ТД-26-21"
        ));

        let zv = zvonki_tomorrow_kb("21.11.2026");
        assert_eq!(zv.inline_keyboard[0][0].text, "Звонки на завтра (21.11)");
        assert!(matches!(
            zv.inline_keyboard[0][0].kind,
            InlineKeyboardButtonKind::CallbackData(ref d) if d == "zvonki_date:21.11.2026"
        ));

        // ЛС без фото — кнопок нет; группа без фото — эфемерные кнопки.
        assert!(day_update_inline_kb(123, "20.11.2026", false).is_none());
        let kb = day_update_inline_kb(123, "20.11.2026", true).unwrap();
        assert_eq!(kb.inline_keyboard.len(), 1);
        let kb = day_update_inline_kb(-100, "20.11.2026", false).unwrap();
        assert_eq!(kb.inline_keyboard.len(), 1);
        assert_eq!(kb.inline_keyboard[0].len(), 2);
        let kb = day_update_inline_kb(-100, "20.11.2026", true).unwrap();
        assert_eq!(kb.inline_keyboard.len(), 2);

        assert!(private_main_kb(123).is_some());
        assert!(private_main_kb(-100).is_none());
    }

    // ---------- dead chat / health / schema ----------

    #[test]
    fn dead_chat_detection() {
        assert!(is_dead_chat_error("chat not found"));
        assert!(is_dead_chat_error("Bot was blocked by the user"));
        assert!(is_dead_chat_error("CHAT_WRITE_FORBIDDEN"));
        assert!(is_dead_chat_error("group chat was upgraded to a supergroup chat"));
        assert!(!is_dead_chat_error("Bad Request: message is too long"));
        assert!(!is_dead_chat_error(""));
    }

    #[test]
    fn health_json_shape() {
        assert_eq!(
            health_json(),
            serde_json::json!({"status": "ok", "service": "schedule_bot_next_gen"})
        );
        // Роутер с health-эндпоинтом собирается офлайн.
        let _router = health_router();
    }

    #[test]
    fn handler_schema_builds() {
        // Дерево хендлеров собирается офлайн: все ветки (команды, дата,
        // кнопки, callback'и, pinned, migrate) зарегистрированы — любая
        // ошибка типов/зависимостей здесь не скомпилируется.
        let _schema = handler_schema();
    }

    // ---------- webhook: не фатальный set_webhook ----------

    #[test]
    fn webhook_retry_delay_schedule() {
        // 5/15/30/60/120/300 с, дальше — 300 с на каждую попытку.
        let expected = [5, 15, 30, 60, 120, 300, 300, 300];
        for (attempt, secs) in expected.iter().enumerate() {
            assert_eq!(
                webhook_retry_delay(attempt),
                Duration::from_secs(*secs),
                "attempt {attempt}"
            );
        }
    }

    #[test]
    fn axum_no_setup_builds_without_network() {
        // axum_no_setup НЕ вызывает set_webhook и не ходит в сеть:
        // старт с недоступным/невалидным webhook не роняет сервер —
        // Router с POST /webhook собирается офлайн (URL — фиктивный).
        let url: url::Url = "https://example.invalid/webhook".parse().unwrap();
        let options = webhooks::Options::new(([0, 0, 0, 0], 8080).into(), url)
            .drop_pending_updates();
        let (_listener, _stop_flag, router) = webhooks::axum_no_setup(options);
        // Роутер содержит маршрут /webhook (иначе апдейты некуда принимать).
        let _ = router;
    }
}
