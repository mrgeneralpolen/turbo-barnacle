//! Mini App HTTP API — те же пути и JSON, что в miniapp_api.py.
//!
//! Эндпоинты (все, кроме /api/ping, требуют X-Telegram-Init-Data):
//! - GET  /api/ping     — {"ok": true}, без initData (health-check)
//! - GET  /api/me       — пользователь + сохранённая группа (+ group_display)
//! - GET  /api/groups   — все группы из архива как {key, display}
//! - POST /api/group    — {"group": "..."}; 422 + choices для "26-21"
//! - GET  /api/day?date=ДД.ММ.ГГГГ
//! - GET  /api/week?date=ДД.ММ.ГГГГ — неделя Пн–Сб
//! - GET  /api/bells?date=ДД.ММ.ГГГГ — + структурированные "pairs"
//! - GET  /api/overview — сегодня + завтра + текущая пара
//! - GET  /api/reminders, POST /api/reminders — {"minutes": N}
//! - GET  /api/teachers?subject=&group= — преподаватели предмета
//! - GET  /api/search?q=&date= — поиск по архиву (предмет/преподаватель)
//! - GET  /api/stats — посещаемость группы (сервер, из архива + пропусков)
//! - GET  /api/bootstrap — me + группа + today/week/bells/reminders за 1 запрос
//! - GET/POST /api/notes — заметки к предметам (сервер)
//! - GET/POST/DELETE /api/absences — пропуски (сервер)
//!
//! Маппинг групп display ↔ ключ — только логика API (см. teachers.rs):
//! в БД ключ голый («24-27»), в Mini App — с префиксом («ИП-24-27»);
//! ТД/ЮР-26-21 — полные ключи с префиксом, display == key; голая «26-21»
//! отдельной группой не считается (в /api/groups её нет, расписание по ней —
//! 422). Существующие таблицы и значения групп в БД не меняются.
//!
//! Защита (router_with_middleware, подключается в bot.rs serve()):
//! rate-limit 300 запросов/мин/IP (in-memory fixed window, только std+tokio)
//! и security headers (nosniff, Referrer-Policy, SAMEORIGIN). CSP намеренно
//! НЕ ставится: в webapp/index.html инлайн-скрипты, CSP без 'unsafe-inline'
//! их сломает.
//!
//! Проверка initData — как в miniapp_api.py validate_init_data:
//! secret = HMAC_SHA256(key=b"WebAppData", msg=BOT_TOKEN),
//! hash = HMAC_SHA256(secret, data_check_string), data_check_string —
//! отсортированные "key=value" (кроме hash), соединённые "\n".
//! Свежесть — 24ч. Ошибки: 401 при неверной подписи/просрочке.
//! 409 — "Сначала укажите группу" (группа не задана или "26-21").
//! 400 — неверный date/JSON/minutes.
//!
//! Помощники (точные копии из miniapp_api.py): _clean_change, _bells_lines,
//! _parse_bells, _pair_times, _current_pair, _day_payload, _require_group.
//! _clean_change / _extract_pair_numbers / normalize_group / get_course_str
//! живут в parser.rs (та же логика, один источник правды) и здесь
//! переиспользуются, а не дублируются.

use crate::db::Db;
use crate::parser;
use crate::service::ScheduleService;
use crate::teachers;
use crate::tyumen;
use crate::Result;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::Router;
use std::path::Path;
use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveDate, Timelike};
use hmac::{Hmac, Mac};
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

/// Максимальный возраст initData, секунды (miniapp_api.py: MAX_INITDATA_AGE).
pub const MAX_INITDATA_AGE_SECS: i64 = 24 * 3600;

#[derive(Debug, Clone, Serialize)]
pub struct ApiTime {
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiPair {
    pub n: i32,
    pub subject: String,
    pub room: String,
    pub time: Option<ApiTime>,
    pub changed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct DayPayload {
    pub date: String,
    pub weekday: String,
    pub weekday_short: String,
    pub group: String,
    pub pairs: Vec<ApiPair>,
    pub changes: Vec<String>,
    pub has_data: bool,
    pub has_changes: bool,
    pub bells_photo_url: Option<String>,
    pub has_bells_photo: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BellsRow {
    pub label: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pair: Option<i32>,
    pub start: String,
    pub end: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BellsSection {
    pub title: String,
    pub rows: Vec<BellsRow>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CurrentPair {
    pub pair: i32,
    pub label: String,
    pub start: String,
    pub end: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends_in_sec: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub starts_in_sec: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct InitDataUser {
    pub id: i64,
    pub first_name: String,
    pub username: Option<String>,
}

#[derive(Clone)]
pub struct ApiState {
    pub service: Arc<ScheduleService>,
    pub db: Arc<Db>,
    pub bot_token: String,    /// Имя бота (без @) — для ссылки «Добавить на главный экран»:
    /// https://t.me/<username>/?startapp&addToHomeScreen. Пусто, если
    /// getMe при старте не удался, — тогда фронт откатывается на
    /// нативный метод tg.addToHomeScreen().
    pub bot_username: String,
}

// ================= validate_init_data =================

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Сравнение в постоянном времени (как hmac.compare_digest в Python).
fn const_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Проверка подписи initData. Чистая функция (тестируется офлайн).
/// Err(String) — текст ошибки как в Python ValueError.
pub fn validate_init_data(init_data: &str, bot_token: &str) -> std::result::Result<InitDataUser, String> {
    if init_data.is_empty() || bot_token.is_empty() {
        return Err("initData или BOT_TOKEN отсутствует".to_string());
    }
    // dict(parse_qsl(init_data, keep_blank_values=True)): percent-декодирование,
    // "+" -> пробел, дубликаты — побеждает последний.
    let mut pairs: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in url::form_urlencoded::parse(init_data.as_bytes()) {
        pairs.insert(k.into_owned(), v.into_owned());
    }
    let received_hash = match pairs.remove("hash") {
        Some(h) if !h.is_empty() => h,
        _ => return Err("в initData нет hash".to_string()),
    };
    let data_check_string = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut mac = Hmac::<Sha256>::new_from_slice(b"WebAppData")
        .map_err(|_| "ошибка HMAC".to_string())?;
    mac.update(bot_token.as_bytes());
    let secret_key = mac.finalize().into_bytes();
    let mut mac = Hmac::<Sha256>::new_from_slice(&secret_key)
        .map_err(|_| "ошибка HMAC".to_string())?;
    mac.update(data_check_string.as_bytes());
    let expected = hex_encode(&mac.finalize().into_bytes());
    if !const_time_eq(expected.as_bytes(), received_hash.to_lowercase().as_bytes()) {
        return Err("неверная подпись initData".to_string());
    }
    let auth_date: i64 = pairs
        .get("auth_date")
        .map(|s| s.as_str())
        .unwrap_or("0")
        .parse()
        .map_err(|_| "некорректный auth_date".to_string())?;
    let now_unix = chrono::Utc::now().timestamp();
    if now_unix - auth_date > MAX_INITDATA_AGE_SECS {
        return Err("initData просрочен".to_string());
    }
    let user_raw = pairs.get("user").map(|s| s.as_str()).unwrap_or("{}");
    let user: Value =
        serde_json::from_str(user_raw).map_err(|_| "некорректный user в initData".to_string())?;
    let Some(user_obj) = user.as_object() else {
        return Err("в initData нет пользователя".to_string());
    };
    let id = match user_obj.get("id") {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse::<i64>().ok(),
        _ => None,
    };
    let Some(id) = id.filter(|v| *v != 0) else {
        return Err("в initData нет пользователя".to_string());
    };
    Ok(InitDataUser {
        id,
        first_name: user_obj
            .get("first_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        username: user_obj
            .get("username")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

// ================= Помощники по расписанию =================

fn strip_b_tags(s: &str) -> String {
    s.replace("<b>", "").replace("</b>", "")
}

fn pair_time_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(\d+)\s*п:\s*(\d{1,2}):(\d{2})\s*[-–—]\s*(\d{1,2}):(\d{2})").unwrap()
    })
}

fn time_range_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\d{1,2}):(\d{2})\s*[-–—]\s*(\d{1,2}):(\d{2})").unwrap())
}

fn group_format_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"([2-9]\d)\s*[-. ]\s*(\d{1,2})").unwrap())
}

/// Строки сетки звонков на дату: спец. звонки из docx, иначе штатные.
/// Точная копия _bells_lines из miniapp_api.py.
pub fn bells_lines(service: &ScheduleService, group: &str, target: NaiveDate) -> Vec<String> {
    let d_str = tyumen::fmt_date(target);
    if let Some(parsed) = service.get_parsed_zvonki_for_date(&d_str) {
        let course = parser::get_course_str(group);
        if let Some(all) = parsed.get("all_courses") {
            return all.clone();
        }
        if course.contains("1 курс") {
            for key in ["1 курс", "1 курс_9", "1 курс_11"] {
                if let Some(lines) = parsed.get(key) {
                    if !lines.is_empty() {
                        return lines.clone();
                    }
                }
            }
            return Vec::new();
        }
        if let Some(lines) = parsed.get(&course) {
            return lines.clone();
        }
        return parsed.get("2 курс").cloned().unwrap_or_default();
    }
    if target.weekday() == chrono::Weekday::Sun {
        return Vec::new();
    }
    service
        .get_zvonki_text(group, target)
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// Строки звонков -> секции. Точная копия _parse_bells из miniapp_api.py.
pub fn parse_bells(lines: &[String]) -> Vec<BellsSection> {
    let mut sections: Vec<BellsSection> = Vec::new();
    let mut title = String::new();
    let mut rows: Vec<BellsRow> = Vec::new();
    for raw in lines {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.contains("1 смена")
            || line.contains("2 смена")
            || line.contains("База 9")
            || line.contains("База 11")
        {
            if !rows.is_empty() {
                sections.push(BellsSection {
                    title: std::mem::take(&mut title),
                    rows: std::mem::take(&mut rows),
                });
            }
            title = strip_b_tags(line);
            continue;
        }
        if let Some(caps) = pair_time_re().captures(line) {
            let n: i32 = caps[1].parse().unwrap_or(0);
            let sh: i32 = caps[2].parse().unwrap_or(0);
            let eh: i32 = caps[4].parse().unwrap_or(0);
            rows.push(BellsRow {
                label: format!("{n} пара"),
                kind: "pair".to_string(),
                pair: Some(n),
                start: format!("{sh:02}:{}", &caps[3]),
                end: format!("{eh:02}:{}", &caps[5]),
                note: None,
            });
            continue;
        }
        if let Some(caps) = time_range_re().captures(line) {
            let sh: i32 = caps[1].parse().unwrap_or(0);
            let eh: i32 = caps[3].parse().unwrap_or(0);
            let start = format!("{sh:02}:{}", &caps[2]);
            let end = format!("{eh:02}:{}", &caps[4]);
            let lower = line.to_lowercase();
            let (kind, label) = if lower.contains("бед") || line.contains("Обед") {
                ("lunch", "Обед".to_string())
            } else if line.contains("Разговоры") {
                ("event", "Разговоры о важном".to_string())
            } else if line.contains("Классный час") {
                ("event", "Классный час".to_string())
            } else {
                let lbl = strip_b_tags(line)
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                (
                    "event",
                    if lbl.is_empty() {
                        "Перерыв".to_string()
                    } else {
                        lbl
                    },
                )
            };
            rows.push(BellsRow {
                label,
                kind: kind.to_string(),
                pair: None,
                start,
                end,
                note: None,
            });
        }
    }
    if !rows.is_empty() {
        sections.push(BellsSection { title, rows });
    }
    sections
}

/// {номер пары: время} на дату. Точная копия _pair_times.
pub fn pair_times(service: &ScheduleService, group: &str, target: NaiveDate) -> BTreeMap<i32, ApiTime> {
    let mut out = BTreeMap::new();
    for sec in parse_bells(&bells_lines(service, group, target)) {
        for row in sec.rows {
            if row.kind == "pair" {
                if let Some(p) = row.pair {
                    out.insert(
                        p,
                        ApiTime {
                            start: row.start,
                            end: row.end,
                        },
                    );
                }
            }
        }
    }
    out
}

/// Текущая пара (или перерыв до следующей) на дату. Точная копия _current_pair.
/// now — в Тюмени; target != сегодня -> None.
pub fn current_pair(
    service: &ScheduleService,
    group: &str,
    target: NaiveDate,
    now: DateTime<FixedOffset>,
) -> Option<CurrentPair> {
    if target != now.date_naive() {
        return None;
    }
    let now_mins = now.hour() as i64 * 60 + now.minute() as i64;
    let flat: Vec<BellsRow> = parse_bells(&bells_lines(service, group, target))
        .into_iter()
        .flat_map(|s| s.rows)
        .collect();
    for row in &flat {
        if row.kind != "pair" {
            continue;
        }
        let (Some(start_m), Some(end_m)) = (
            tyumen::hhmm_to_mins(&row.start).map(i64::from),
            tyumen::hhmm_to_mins(&row.end).map(i64::from),
        ) else {
            continue;
        };
        if start_m <= now_mins && now_mins < end_m {
            let progress =
                ((now_mins - start_m) as f64 / (end_m - start_m).max(1) as f64 * 1000.0).round()
                    / 1000.0;
            return Some(CurrentPair {
                pair: row.pair.unwrap_or(0),
                label: row.label.clone(),
                start: row.start.clone(),
                end: row.end.clone(),
                status: "now".to_string(),
                ends_in_sec: Some((end_m - now_mins) * 60 - now.second() as i64),
                starts_in_sec: None,
                progress: Some(progress),
            });
        }
    }
    for row in &flat {
        if row.kind != "pair" {
            continue;
        }
        if let Some(start_m) = tyumen::hhmm_to_mins(&row.start).map(i64::from) {
            if start_m > now_mins {
                return Some(CurrentPair {
                    pair: row.pair.unwrap_or(0),
                    label: row.label.clone(),
                    start: row.start.clone(),
                    end: row.end.clone(),
                    status: "next".to_string(),
                    ends_in_sec: None,
                    starts_in_sec: Some((start_m - now_mins) * 60 - now.second() as i64),
                    progress: None,
                });
            }
        }
    }
    None
}

/// Полный payload дня. Точная копия _day_payload из miniapp_api.py.
pub fn day_payload(service: &ScheduleService, group: &str, target: NaiveDate) -> DayPayload {
    let d_str = tyumen::fmt_date(target);
    let day = service.format_day_data(group, &d_str);
    let clean_changes: Vec<String> = day
        .changes
        .iter()
        .map(|c| parser::clean_change(c))
        .filter(|c| !c.is_empty())
        .collect();
    let mut changed_pairs: HashSet<i32> = HashSet::new();
    for c in &day.changes {
        changed_pairs.extend(parser::extract_pair_numbers(c));
    }
    let times = pair_times(service, group, target);
    let pairs: Vec<ApiPair> = day
        .pairs
        .iter()
        .map(|(n, (subj, room))| ApiPair {
            n: *n,
            subject: subj.clone(),
            room: if room != "—" {
                room.clone()
            } else {
                String::new()
            },
            time: times.get(n).cloned(),
            changed: changed_pairs.contains(n),
        })
        .collect();
    let has_data = !pairs.is_empty() || !clean_changes.is_empty();
    let has_changes = !clean_changes.is_empty();
    let photo = service.get_bells_photo_for_date(&d_str);
    let has_bells_photo = photo.as_deref().map(|p| !p.is_empty()).unwrap_or(false);
    let bells_photo_url = photo.filter(|p| p.starts_with("http://") || p.starts_with("https://"));
    DayPayload {
        date: d_str,
        weekday: tyumen::weekday_name(target).to_string(),
        weekday_short: tyumen::weekday_short(target).to_string(),
        group: if day.target_group.is_empty() {
            group.to_string()
        } else {
            day.target_group
        },
        pairs,
        changes: clean_changes,
        has_data,
        has_changes,
        bells_photo_url,
        has_bells_photo,
    }
}

// ================= Payload-строители (переиспользуются хендлерами) =================

/// Тело /api/me. group_display — display-имя группы (см. teachers.rs).
fn me_payload(user: &InitDataUser, group: &Option<String>, bot_username: &str) -> Value {
    let group_set = group
        .as_deref()
        .map(|g| !g.is_empty() && g != "26-21")
        .unwrap_or(false);
    let needs_specialty = group.as_deref() == Some("26-21");
    json!({
        "user": {
            "id": user.id,
            "first_name": user.first_name,
            "username": user.username.clone().unwrap_or_default(),
        },
        "group": group,
        "group_display": group.as_ref().map(|g| teachers::group_display(g)),
        "group_set": group_set,
        "needs_specialty": needs_specialty,
        "now": tyumen::fmt_datetime(tyumen::tyumen_now()),
        "timezone": "Asia/Yekaterinburg (UTC+5)",
        "bot_username": bot_username,
    })
}

/// День + текущая пара (как отвечал /api/day).
fn day_payload_json(service: &ScheduleService, group: &str, target: NaiveDate) -> Value {
    let mut payload =
        serde_json::to_value(day_payload(service, group, target)).unwrap_or(Value::Null);
    let now = tyumen::tyumen_now();
    payload["current_pair"] =
        serde_json::to_value(current_pair(service, group, target, now)).unwrap_or(Value::Null);
    payload
}

/// Неделя Пн–Сб с якорем (как отвечал /api/week).
fn week_payload_json(service: &ScheduleService, group: &str, anchor: NaiveDate) -> Value {
    let monday = tyumen::week_monday(anchor);
    let now = tyumen::tyumen_now();
    let days: Vec<Value> = (0..6)
        .map(|i| {
            let d = monday + Duration::days(i);
            serde_json::to_value(day_payload(service, group, d)).unwrap_or(Value::Null)
        })
        .collect();
    json!({
        "week_start": tyumen::fmt_date(monday),
        "week_end": tyumen::fmt_date(monday + Duration::days(5)),
        "group": group,
        "today": tyumen::fmt_date(now.date_naive()),
        "current_pair": current_pair(service, group, now.date_naive(), now),
        "days": days,
    })
}

/// Дата для /api/bells без параметра: сегодня; в воскресенье — понедельник.
fn bells_target(query: &BTreeMap<String, String>) -> NaiveDate {
    if let Some(d) = query.get("date").and_then(|s| tyumen::parse_date(s)) {
        return d;
    }
    let mut today = tyumen::tyumen_today();
    if today.weekday() == chrono::Weekday::Sun {
        today += Duration::days(1);
    }
    today
}

/// Тело /api/bells: секции + структурированные "pairs" (номер, начало, конец)
/// из разбора текста звонков (та же сетка, что в sections).
fn bells_payload_json(service: &ScheduleService, group: &str, target: NaiveDate) -> Value {
    let now = tyumen::tyumen_now();
    let sections = parse_bells(&bells_lines(service, group, target));
    let pairs: Vec<Value> = pair_times(service, group, target)
        .iter()
        .map(|(n, t)| json!({"n": n, "start": t.start, "end": t.end}))
        .collect();
    let photo = service.get_bells_photo_for_date(&tyumen::fmt_date(target));
    let has_bells_photo = photo.as_deref().map(|p| !p.is_empty()).unwrap_or(false);
    let bells_photo_url = photo.filter(|p| p.starts_with("http://") || p.starts_with("https://"));
    json!({
        "date": tyumen::fmt_date(target),
        "weekday": tyumen::weekday_name(target),
        "group": group,
        "course": parser::get_course_str(group),
        "shift": service.get_group_shift(group, target),
        "sections": sections,
        "pairs": pairs,
        "current_pair": current_pair(service, group, target, now),
        "now": now.format("%H:%M").to_string(),
        "has_bells_photo": has_bells_photo,
        "bells_photo_url": bells_photo_url,
    })
}

/// Группа пользователя, пригодная для расписания: задана и не «26-21».
fn usable_group(group: &Option<String>) -> Option<String> {
    match group {
        Some(g) if !g.is_empty() && g != "26-21" => Some(g.clone()),
        _ => None,
    }
}

// ================= HTTP-слой =================

type ApiResponse = (StatusCode, Json<Value>);

fn ok(body: Value) -> ApiResponse {
    (StatusCode::OK, Json(body))
}

fn err(status: StatusCode, message: &str) -> ApiResponse {
    (status, Json(json!({"error": message})))
}

fn db_err(e: crate::AppError) -> ApiResponse {
    err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
}

/// Query-параметры URI как map (последний дубликат побеждает,
/// как dict(parse_qsl(...)) в Python).
fn query_map(uri: &Uri) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    if let Some(q) = uri.query() {
        for (k, v) in url::form_urlencoded::parse(q.as_bytes()) {
            map.insert(k.into_owned(), v.into_owned());
        }
    }
    map
}

/// Достаёт initData из заголовка/параметра и возвращает пользователя.
/// Точная копия _require_user: 401 при плохой подписи/просрочке.
fn require_user(
    bot_token: &str,
    headers: &HeaderMap,
    query: &BTreeMap<String, String>,
) -> std::result::Result<InitDataUser, ApiResponse> {
    let mut init_data = headers
        .get("X-Telegram-Init-Data")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if init_data.is_empty() {
        init_data = query.get("initData").cloned().unwrap_or_default();
    }
    if let Some(rest) = init_data.strip_prefix("tma ") {
        init_data = rest.to_string();
    }
    validate_init_data(&init_data, bot_token).map_err(|e| err(StatusCode::UNAUTHORIZED, &e))
}

/// Группа пользователя; None — не задана или двусмысленная "26-21".
/// Точная копия _require_group.
async fn require_group(db: &Db, user_id: i64) -> Result<Option<String>> {
    let group = db.get_group(user_id).await?;
    match group {
        Some(g) if !g.is_empty() && g != "26-21" => Ok(Some(g)),
        _ => Ok(None),
    }
}

/// str(body.get("group", "")) — как в Python (включая явный null -> "None").
fn body_group_str(body: &Value) -> String {
    match body.get("group") {
        None => String::new(),
        Some(Value::Null) => "None".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Some(v) => v.to_string(),
    }
}

/// int(body.get("minutes", 0)) — как в Python; None — не число.
fn body_minutes(body: &Value) -> Option<i64> {
    match body.get("minutes") {
        None => Some(0),
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(Value::Bool(b)) => Some(i64::from(*b)),
        Some(_) => None,
    }
}

/// GET /api/ping — проверка живости без initData ({"ok": true}).
/// В Python его нет (только строчка в комментарии); ping — явный
/// health-check вдобавок к корню `/` (200).
async fn api_ping() -> Json<Value> {
    Json(serde_json::json!({"ok": true}))
}

async fn api_me(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let group = match state.db.get_group(user.id).await {
        Ok(g) => g,
        Err(e) => return db_err(e),
    };
    ok(me_payload(&user, &group, &state.bot_username))
}

async fn api_groups(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResponse {
    let query = query_map(&uri);
    if let Err(r) = require_user(&state.bot_token, &headers, &query) {
        return r;
    }
    let mut groups = match state.db.get_archive_groups().await {
        Ok(g) => g,
        Err(e) => return db_err(e),
    };
    groups.sort_by(|a, b| {
        (parser::normalize_group(a, ""), a.clone())
            .cmp(&(parser::normalize_group(b, ""), b.clone()))
    });
    // Голая «26-21» (3 строки в архиве — остатки неоднозначного парсинга)
    // отдельной группой НЕ считается: в списке её нет, расписание по ней —
    // 422 с choices (см. POST /api/group). ТД/ЮР-26-21 — полные ключи,
    // display == key.
    let groups: Vec<Value> = groups
        .into_iter()
        .filter(|g| g != "26-21")
        .map(|key| json!({"key": key, "display": teachers::group_display(&key)}))
        .collect();
    ok(json!({ "groups": groups }))
}

async fn api_set_group(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return err(StatusCode::BAD_REQUEST, "Некорректный JSON"),
    };
    let raw = body_group_str(&parsed).trim().to_string();
    if raw.is_empty() {
        return err(StatusCode::BAD_REQUEST, "Укажите группу, например: 24-27");
    }
    let clean = parser::normalize_group(&raw, "");
    if clean == "26-21" {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": "ambiguous_group",
                "message": "В колледже две группы 26-21 — выберите специальность",
                "choices": ["ТД-26-21", "ЮР-26-21"],
            })),
        );
    }
    if !group_format_re().is_match(&clean) {
        return err(
            StatusCode::BAD_REQUEST,
            "Некорректный формат группы. Примеры: 24-27, ИП-24-27, ЮР-26-21",
        );
    }
    if let Err(e) = state.db.set_group(user.id, &clean).await {
        return db_err(e);
    }
    ok(json!({ "group": clean, "group_display": teachers::group_display(&clean), "ok": true }))
}

async fn api_day(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let group = match require_group(&state.db, user.id).await {
        Ok(Some(g)) => g,
        Ok(None) => return err(StatusCode::CONFLICT, "Сначала укажите группу"),
        Err(e) => return db_err(e),
    };
    let target = match query.get("date").and_then(|s| tyumen::parse_date(s)) {
        Some(d) => d,
        None => {
            return err(
                StatusCode::BAD_REQUEST,
                "Параметр date обязателен (ДД.ММ.ГГГГ)",
            )
        }
    };
    ok(day_payload_json(&state.service, &group, target))
}

async fn api_week(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let group = match require_group(&state.db, user.id).await {
        Ok(Some(g)) => g,
        Ok(None) => return err(StatusCode::CONFLICT, "Сначала укажите группу"),
        Err(e) => return db_err(e),
    };
    let anchor = query
        .get("date")
        .and_then(|s| tyumen::parse_date(s))
        .unwrap_or_else(tyumen::tyumen_today);
    ok(week_payload_json(&state.service, &group, anchor))
}

async fn api_bells(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let group = match require_group(&state.db, user.id).await {
        Ok(Some(g)) => g,
        Ok(None) => return err(StatusCode::CONFLICT, "Сначала укажите группу"),
        Err(e) => return db_err(e),
    };
    // В Python неверная/отсутствующая дата -> сегодня; в воскресенье — понедельник.
    let target = bells_target(&query);
    ok(bells_payload_json(&state.service, &group, target))
}

async fn api_overview(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let group = match require_group(&state.db, user.id).await {
        Ok(Some(g)) => g,
        Ok(None) => return err(StatusCode::CONFLICT, "Сначала укажите группу"),
        Err(e) => return db_err(e),
    };
    let now = tyumen::tyumen_now();
    let today = now.date_naive();
    // В субботу «завтра» = понедельник.
    let days_ahead = if today.weekday() == chrono::Weekday::Sat {
        2
    } else {
        1
    };
    let tomorrow = today + Duration::days(days_ahead);
    ok(json!({
        "group": group,
        "now": tyumen::fmt_datetime(now),
        "today": day_payload(&state.service, &group, today),
        "tomorrow": day_payload(&state.service, &group, tomorrow),
        "current_pair": current_pair(&state.service, &group, today, now),
        "bells_today": parse_bells(&bells_lines(&state.service, &group, today)),
    }))
}

async fn api_reminders_get(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match state.db.get_reminder_minutes(user.id).await {
        Ok(minutes) => ok(json!({ "minutes": minutes })),
        Err(e) => db_err(e),
    }
}

async fn api_reminders_set(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return err(StatusCode::BAD_REQUEST, "Некорректный JSON"),
    };
    let minutes = match body_minutes(&parsed) {
        Some(m) => m,
        None => return err(StatusCode::BAD_REQUEST, "minutes должен быть числом"),
    };
    if minutes != 0 && !(1..=120).contains(&minutes) {
        return err(StatusCode::BAD_REQUEST, "minutes: 0 (выкл.) или 1–120");
    }
    if let Err(e) = state.db.set_reminder_minutes(user.id, minutes).await {
        return db_err(e);
    }
    ok(json!({ "ok": true, "minutes": minutes }))
}

// ================= Новые эндпоинты (только данные с сервера) =================

/// GET /api/teachers?subject=&group= — преподаватели предмета по программе
/// группы (teacherFor из index.html, только на сервере).
/// group: параметр (ключ или display) или сохранённая группа; без группы —
/// общий список. 400 — пустой subject.
async fn api_teachers(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let subject = query
        .get("subject")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if subject.is_empty() {
        return err(StatusCode::BAD_REQUEST, "Укажите предмет (?subject=...)");
    }
    let group: Option<String> = match query.get("group") {
        Some(g) if !g.trim().is_empty() => Some(parser::normalize_group(g, "")),
        _ => match state.db.get_group(user.id).await {
            Ok(g) => usable_group(&g),
            Err(e) => return db_err(e),
        },
    };
    let found = teachers::teachers_for(&subject, group.as_deref());
    ok(json!({
        "subject": subject,
        "group": group,
        "spec": found.spec,
        "spec_name": found.spec_name,
        "teachers": found.teachers,
    }))
}

/// Один хит поиска: пара предмета в архиве.
#[derive(Debug, Clone)]
struct SearchHit {
    date: String,
    group: String,
    pair: i32,
    room: String,
}

/// Поиск по архиву (archive_cache): q — подстрока предмета
/// (case-insensitive) ИЛИ фамилии преподавателя; date — ДД.ММ.ГГГГ
/// (без неё — по всему архиву). Лимиты: ≤50 предметов, ≤200 хитов.
/// Даты — от новых к старым; голая «26-21» — не группа, пропускается.
fn search_archive(
    service: &ScheduleService,
    q: &str,
    date_filter: Option<NaiveDate>,
) -> Vec<(String, Vec<String>, Vec<SearchHit>)> {
    const MAX_SUBJECTS: usize = 50;
    const MAX_HITS: usize = 200;
    let q_norm = teachers::norm_subject(q);
    // Снимок (дата, группа) из кэша, даты — от новых к старым.
    let mut dates: Vec<String> = {
        let cache = service.archive_cache.read().unwrap();
        cache.keys().cloned().collect()
    };
    dates.sort_by_key(|d| tyumen::parse_date(d).unwrap_or_default());
    dates.reverse();
    // subject_norm -> (display, teachers, hits)
    let mut found: BTreeMap<String, (String, Vec<String>, Vec<SearchHit>)> = BTreeMap::new();
    let mut total_hits = 0usize;
    'dates: for date_str in &dates {
        if let Some(f) = date_filter {
            if tyumen::parse_date(date_str) != Some(f) {
                continue;
            }
        }
        let groups: Vec<String> = {
            let cache = service.archive_cache.read().unwrap();
            match cache.get(date_str) {
                Some(m) => m.keys().cloned().collect(),
                None => continue,
            }
        };
        let mut groups = groups;
        groups.sort();
        for group in groups {
            if group == "__ZVONKI__" || group == "26-21" {
                continue;
            }
            let day = service.format_day_data(&group, date_str);
            for (n, (subj, room)) in &day.pairs {
                // fix_room (как в REAL_DAYS фронтенда): кабинет из хвоста
                // предмета — в room, иначе поиск/статистика дробятся
                // по кабинетам («Математика каб. 212» vs «Математика каб. 301»).
                // day_payload намеренно остаётся 1:1 портом Python (без fix_room).
                let (clean_subj, clean_room) = parser::fix_room(subj, room);
                let subj_norm = teachers::norm_subject(&clean_subj);
                let subject_teachers = teachers::teachers_for_any(&clean_subj);
                let by_subject = subj_norm.contains(&q_norm);
                let by_teacher = subject_teachers
                    .iter()
                    .any(|t| t.to_lowercase().contains(&q_norm));
                if !by_subject && !by_teacher {
                    continue;
                }
                if total_hits >= MAX_HITS {
                    break 'dates;
                }
                // Новых предметов — не больше лимита; хиты к уже
                // найденным предметам добавляются дальше.
                if !found.contains_key(&subj_norm) && found.len() >= MAX_SUBJECTS {
                    continue;
                }
                let entry = found.entry(subj_norm).or_insert_with(|| {
                    (clean_subj.clone(), subject_teachers.clone(), Vec::new())
                });
                entry.2.push(SearchHit {
                    date: date_str.clone(),
                    group: group.clone(),
                    pair: *n,
                    room: if clean_room == "—" {
                        String::new()
                    } else {
                        clean_room.clone()
                    },
                });
                total_hits += 1;
            }
        }
    }
    // Лимит предметов: BTreeMap уже отсортирован по норме предмета.
    found
        .into_iter()
        .take(MAX_SUBJECTS)
        .map(|(_, (display, t, hits))| (display, t, hits))
        .collect()
}

async fn api_search(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let _ = user;
    let q = query
        .get("q")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if q.is_empty() {
        return err(StatusCode::BAD_REQUEST, "Укажите запрос (?q=...)");
    }
    let date_filter = match query.get("date") {
        None => None,
        Some(s) => match tyumen::parse_date(s) {
            Some(d) => Some(d),
            None => return err(StatusCode::BAD_REQUEST, "date — ДД.ММ.ГГГГ"),
        },
    };
    let date_str = date_filter.map(tyumen::fmt_date);
    let results: Vec<Value> = search_archive(&state.service, &q, date_filter)
        .into_iter()
        .map(|(subject, t, hits)| {
            json!({
                "subject": subject,
                "teachers": t,
                "hits": hits.iter().map(|h| json!({
                    "date": h.date,
                    "group": h.group,
                    "group_display": teachers::group_display(&h.group),
                    "pair": h.pair,
                    "room": h.room,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    ok(json!({ "q": q, "date": date_str, "results": results }))
}

/// GET /api/stats — посещаемость группы (сервер).
/// Покрывает computeStats/renderStatsSheet/computeAttendanceDetail
/// из index.html: pastTotal/attended — через past_pairs и absences,
/// по неделям — by_week, по предметам — by_subject.
/// - total_pairs: все пары группы в архиве; past_pairs: даты ≤ сегодня;
/// - changes_total: изменённых пар на прошедших датах;
/// - by_subject/by_week: только прошедшие даты (пары, изменения, пропуски);
/// - absences_total: все пропуски пользователя.
async fn api_stats(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let group = match require_group(&state.db, user.id).await {
        Ok(Some(g)) => g,
        Ok(None) => return err(StatusCode::CONFLICT, "Сначала укажите группу"),
        Err(e) => return db_err(e),
    };
    let today = tyumen::tyumen_today();
    let absences = match state.db.get_absences(user.id).await {
        Ok(a) => a,
        Err(e) => return db_err(e),
    };
    let mut total_pairs: i64 = 0;
    let mut past_pairs: i64 = 0;
    let mut changes_total: i64 = 0;
    // norm -> (display, pairs, changes, absences)
    let mut by_subject: BTreeMap<String, (String, i64, i64, i64)> = BTreeMap::new();
    // week_start -> (pairs, changes, absences)
    let mut by_week: BTreeMap<String, (i64, i64, i64)> = BTreeMap::new();
    let dates: Vec<String> = {
        let cache = state.service.archive_cache.read().unwrap();
        cache.keys().cloned().collect()
    };
    for date_str in &dates {
        let Some(d) = tyumen::parse_date(date_str) else {
            continue;
        };
        let day = state.service.format_day_data(&group, date_str);
        if day.pairs.is_empty() {
            continue;
        }
        let mut changed_pairs: HashSet<i32> = HashSet::new();
        for c in &day.changes {
            changed_pairs.extend(parser::extract_pair_numbers(c));
        }
        total_pairs += day.pairs.len() as i64;
        if d > today {
            continue;
        }
        let week_start = tyumen::fmt_date(tyumen::week_monday(d));
        let mut day_changed: i64 = 0;
        for (n, (subj, room)) in &day.pairs {
            past_pairs += 1;
            let is_changed = changed_pairs.contains(n);
            if is_changed {
                changes_total += 1;
                day_changed += 1;
            }
            // fix_room — как в поиске: предмет без хвоста-кабинета,
            // иначе by_subject дробится по кабинетам.
            let (clean_subj, _) = parser::fix_room(subj, room);
            let key = teachers::norm_subject(&clean_subj);
            let entry = by_subject
                .entry(key)
                .or_insert_with(|| (clean_subj.clone(), 0, 0, 0));
            entry.1 += 1;
            if is_changed {
                entry.2 += 1;
            }
        }
        let w = by_week.entry(week_start).or_insert((0, 0, 0));
        w.0 += day.pairs.len() as i64;
        w.1 += day_changed;
    }
    // Пропуски: только прошедшие даты; в неделю/предмет — только если
    // там были пары (как computeAttendanceDetail в index.html).
    let mut absences_total: i64 = 0;
    for (date_str, subject, _pair) in &absences {
        absences_total += 1;
        let Some(d) = tyumen::parse_date(date_str) else {
            continue;
        };
        if d > today {
            continue;
        }
        let week_start = tyumen::fmt_date(tyumen::week_monday(d));
        if let Some(w) = by_week.get_mut(&week_start) {
            w.2 += 1;
        }
        // fix_room — как у пар выше: пропуск, созданный из листа пары,
        // хранит предмет с хвостом кабинета («Обществознание каб. 403»);
        // без этого он не попадает в by_subject чистого предмета.
        let (clean_subj, _) = parser::fix_room(subject, "—");
        let key = teachers::norm_subject(&clean_subj);
        if let Some(entry) = by_subject.get_mut(&key) {
            entry.3 += 1;
        }
    }
    let by_subject_json: Vec<Value> = {
        let mut v: Vec<(String, i64, i64, i64, String)> = by_subject
            .into_iter()
            .map(|(_, (display, pairs, changes, abs))| (display, pairs, changes, abs, String::new()))
            .collect();
        // По убыванию пар, затем по названию.
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v.into_iter()
            .map(|(display, pairs, changes, abs, _)| {
                json!({"subject": display, "pairs": pairs, "changes": changes, "absences": abs})
            })
            .collect()
    };
    let by_week_json: Vec<Value> = {
        let mut v: Vec<(String, i64, i64, i64)> =
            by_week.into_iter().map(|(k, (p, c, a))| (k, p, c, a)).collect();
        // По дате, от новых к старым (строковая сортировка ДД.ММ.ГГГГ
        // хронологический порядок НЕ даёт).
        v.sort_by_key(|(w, _, _, _)| tyumen::parse_date(w).unwrap_or_default());
        v.reverse();
        v.into_iter()
            .map(|(week_start, pairs, changes, abs)| {
                json!({"week_start": week_start, "pairs": pairs, "changes": changes, "absences": abs})
            })
            .collect()
    };
    ok(json!({
        "group": group,
        "group_display": teachers::group_display(&group),
        "total_pairs": total_pairs,
        "past_pairs": past_pairs,
        "changes_total": changes_total,
        "by_subject": by_subject_json,
        "by_week": by_week_json,
        "absences_total": absences_total,
    }))
}

/// GET /api/bootstrap — один запрос при открытии: me + группа +
/// today/week/bells/reminders. Без группы — group null, today/week/bells
/// null (НЕ 409): фронт показывает выбор группы.
async fn api_bootstrap(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let group = match state.db.get_group(user.id).await {
        Ok(g) => g,
        Err(e) => return db_err(e),
    };
    let minutes = match state.db.get_reminder_minutes(user.id).await {
        Ok(m) => m,
        Err(e) => return db_err(e),
    };
    let group_display = group.as_ref().map(|g| teachers::group_display(g));
    let (today, week, bells) = match usable_group(&group) {
        Some(g) => {
            let now = tyumen::tyumen_now();
            let today_date = now.date_naive();
            (
                Some(day_payload_json(&state.service, &g, today_date)),
                Some(week_payload_json(&state.service, &g, today_date)),
                Some(bells_payload_json(
                    &state.service,
                    &g,
                    bells_target(&BTreeMap::new()),
                )),
            )
        }
        None => (None, None, None),
    };
    ok(json!({
        "me": me_payload(&user, &group, &state.bot_username),
        "group": group,
        "group_display": group_display,
        "today": today,
        "week": week,
        "bells": bells,
        "reminders": {"enabled": minutes > 0, "minutes": minutes},
    }))
}

/// GET /api/notes — заметки пользователя к предметам.
async fn api_notes_get(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match state.db.get_notes(user.id).await {
        Ok(notes) => ok(json!({
            "notes": notes.iter().map(|(subject, text, updated_at)| json!({
                "subject": subject, "text": text, "updated_at": updated_at,
            })).collect::<Vec<_>>(),
        })),
        Err(e) => db_err(e),
    }
}

/// POST /api/notes {"subject","text"} — upsert; пустой text — удалить.
async fn api_notes_post(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return err(StatusCode::BAD_REQUEST, "Некорректный JSON"),
    };
    let subject = match parsed.get("subject").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return err(StatusCode::BAD_REQUEST, "Укажите предмет (subject)"),
    };
    let text = match parsed.get("text").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        _ => return err(StatusCode::BAD_REQUEST, "Укажите текст (text)"),
    };
    let deleted = text.trim().is_empty();
    let res = if deleted {
        state.db.delete_note(user.id, &subject).await
    } else {
        state.db.upsert_note(user.id, &subject, text.trim()).await
    };
    match res {
        Ok(()) => ok(json!({ "ok": true, "subject": subject, "deleted": deleted })),
        Err(e) => db_err(e),
    }
}

/// GET /api/absences — пропуски пользователя.
async fn api_absences_get(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    match state.db.get_absences(user.id).await {
        Ok(abs) => ok(json!({
            "absences": abs.iter().map(|(date, subject, pair)| json!({
                "date": date, "subject": subject, "pair": pair,
            })).collect::<Vec<_>>(),
        })),
        Err(e) => db_err(e),
    }
}

/// int(body.get("pair")) — как body_minutes; None — не число.
fn body_pair(body: &Value) -> Option<i64> {
    match body.get("pair") {
        None => None,
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64)),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(_) => None,
    }
}

/// POST /api/absences {"date","subject","pair"} — отметить пропуск.
async fn api_absences_post(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return err(StatusCode::BAD_REQUEST, "Некорректный JSON"),
    };
    let date = match parsed.get("date").and_then(|v| v.as_str()) {
        Some(s) if tyumen::parse_date(s).is_some() => s.trim().to_string(),
        _ => return err(StatusCode::BAD_REQUEST, "date — ДД.ММ.ГГГГ"),
    };
    let subject = match parsed.get("subject").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return err(StatusCode::BAD_REQUEST, "Укажите предмет (subject)"),
    };
    let pair = match body_pair(&parsed) {
        Some(p) if (1..=12).contains(&p) => p,
        _ => return err(StatusCode::BAD_REQUEST, "pair — номер пары 1–12"),
    };
    match state
        .db
        .upsert_absence(user.id, &date, &subject, pair)
        .await
    {
        Ok(()) => ok(json!({ "ok": true, "date": date, "subject": subject, "pair": pair })),
        Err(e) => db_err(e),
    }
}

/// DELETE /api/absences {"date","subject"} — снять пропуск.
async fn api_absences_delete(
    State(state): State<ApiState>,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> ApiResponse {
    let query = query_map(&uri);
    let user = match require_user(&state.bot_token, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return err(StatusCode::BAD_REQUEST, "Некорректный JSON"),
    };
    let date = match parsed.get("date").and_then(|v| v.as_str()) {
        Some(s) if tyumen::parse_date(s).is_some() => s.trim().to_string(),
        _ => return err(StatusCode::BAD_REQUEST, "date — ДД.ММ.ГГГГ"),
    };
    let subject = match parsed.get("subject").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return err(StatusCode::BAD_REQUEST, "Укажите предмет (subject)"),
    };
    match state.db.delete_absence(user.id, &date, &subject).await {
        Ok(()) => ok(json!({ "ok": true, "date": date, "subject": subject })),
        Err(e) => db_err(e),
    }
}

// ================= Защита: rate-limit + security headers =================

/// Лимит /api/*: 300 запросов/мин/IP (in-memory fixed window).
/// Только std+tokio, без новых крейтов.
pub const API_RATE_LIMIT_PER_MINUTE: u32 = 300;
const RATE_LIMIT_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<HashMap<String, (Instant, u32)>>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// None — пропуск разрешён; Some(secs) — до конца окна (429).
    fn check(&self, key: &str) -> Option<u64> {
        let mut map = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        let entry = map.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= RATE_LIMIT_WINDOW {
            *entry = (now, 1);
            return None;
        }
        if entry.1 >= API_RATE_LIMIT_PER_MINUTE {
            let left = RATE_LIMIT_WINDOW
                .checked_sub(now.duration_since(entry.0))
                .map(|d| d.as_secs().max(1))
                .unwrap_or(1);
            return Some(left);
        }
        entry.1 += 1;
        None
    }
}

fn client_key(req: &Request) -> String {
    // axum::serve с into_make_service_with_connect_info даёт ConnectInfo;
    // за прокси (Railway) — X-Forwarded-For; иначе один общий бакет.
    if let Some(info) = req.extensions().get::<ConnectInfo<SocketAddr>>() {
        return info.0.ip().to_string();
    }
    if let Some(xff) = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = xff.split(',').next() {
            let first = first.trim();
            if !first.is_empty() {
                return first.to_string();
            }
        }
    }
    "unknown".to_string()
}

async fn rate_limit_mw(
    State(limiter): State<RateLimiter>,
    req: Request,
    next: Next,
) -> Response {
    let key = client_key(&req);
    if let Some(retry_after) = limiter.check(&key) {
        let (status, body) = err(
            StatusCode::TOO_MANY_REQUESTS,
            "Слишком много запросов, попробуйте позже",
        );
        let mut resp = (status, body).into_response();
        resp.headers_mut().insert(
            "retry-after",
            HeaderValue::from_str(&retry_after.to_string())
                .unwrap_or(HeaderValue::from_static("60")),
        );
        return resp;
    }
    next.run(req).await
}

/// Security headers для ответов (X-Content-Type-Options, Referrer-Policy,
/// X-Frame-Options). Публичная: используется в `bot::serve()` на весь app
/// (включая /app/* и /) и в `examples/verify_e2e.rs`.
pub async fn security_headers_mw(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        "referrer-policy",
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    h.insert("x-frame-options", HeaderValue::from_static("SAMEORIGIN"));
    // CSP намеренно НЕ ставится: в webapp/index.html инлайн-скрипты,
    // CSP без 'unsafe-inline' их сломает.
    resp
}

/// GET /app/app.html — само приложение, ТОЛЬКО с валидной initData
/// (query-параметр initData). Без подписи или с просроченной — 403.
/// Загрузчик /app/index.html запрашивает этот адрес; вне Telegram
/// приложение не открывается вообще: ни по прямой ссылке, ни из браузера.
/// Ответ — no-store/private: страница с кодом приложения не должна
/// осесть ни в одном кэше (см. static_cache_headers_mw: он /app/app.html
/// не трогает, заголовки здесь — единственные).
async fn api_app_html(
    State(state): State<ApiState>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let init_data = params.get("initData").map(String::as_str).unwrap_or("");
    if validate_init_data(init_data, &state.bot_token).is_err() {
        return (StatusCode::FORBIDDEN, "Forbidden").into_response();
    }
    let dir = std::env::var("WEBAPP_DIR").unwrap_or_else(|_| "webapp".to_string());
    let path = Path::new(&dir).join("app.html");
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store, private"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "Not Found").into_response(),
    }
}

/// Axum-роутер с /api/* (без /app/ — статика монтируется в main.rs).
/// Без middleware: rate-limit и security headers добавляет
/// router_with_middleware (подключается в bot.rs serve()).
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/app/app.html", get(api_app_html))
        .route("/api/ping", get(api_ping))
        .route("/api/me", get(api_me))
        .route("/api/groups", get(api_groups))
        .route("/api/group", post(api_set_group))
        .route("/api/day", get(api_day))
        .route("/api/week", get(api_week))
        .route("/api/bells", get(api_bells))
        .route("/api/overview", get(api_overview))
        .route(
            "/api/reminders",
            get(api_reminders_get).post(api_reminders_set),
        )
        .route("/api/teachers", get(api_teachers))
        .route("/api/search", get(api_search))
        .route("/api/stats", get(api_stats))
        .route("/api/bootstrap", get(api_bootstrap))
        .route("/api/notes", get(api_notes_get).post(api_notes_post))
        .route(
            "/api/absences",
            get(api_absences_get)
                .post(api_absences_post)
                .delete(api_absences_delete),
        )
        .with_state(state)
}

/// Тот же роутер + защита /api/*: rate-limit 300/мин/IP и security
/// headers. Используется в bot.rs serve(); в тестах — для проверки
/// middleware. Порядок слоёв: security headers — снаружи, поэтому они
/// ставятся и на 429 от rate-limit.
pub fn router_with_middleware(state: ApiState) -> Router {
    let limiter = RateLimiter::new();
    router(state)
        .layer(middleware::from_fn_with_state(limiter, rate_limit_mw))
        .layer(middleware::from_fn(security_headers_mw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    const TEST_TOKEN: &str = "123456:ABC-DEF1234ghIkl-zyx57W2v1u123ew11";

    /// initData с валидной подписью (как make_init_data в test_miniapp_api.py).
    fn make_init_data(user_id: i64, auth_date: i64) -> String {
        let user = serde_json::json!({"id": user_id, "first_name": "Тест", "username": "test"})
            .to_string();
        let mut pairs = BTreeMap::new();
        pairs.insert("auth_date".to_string(), auth_date.to_string());
        pairs.insert("user".to_string(), user);
        let check = pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut mac = Hmac::<Sha256>::new_from_slice(b"WebAppData").unwrap();
        mac.update(TEST_TOKEN.as_bytes());
        let secret = mac.finalize().into_bytes();
        let mut mac = Hmac::<Sha256>::new_from_slice(&secret).unwrap();
        mac.update(check.as_bytes());
        let hash = hex_encode(&mac.finalize().into_bytes());
        // parse_qsl-формат: user — URL-кодированный JSON.
        let user_enc = url::form_urlencoded::byte_serialize(user_json(user_id).as_bytes())
            .collect::<String>();
        format!("auth_date={auth_date}&user={user_enc}&hash={hash}")
    }

    fn user_json(user_id: i64) -> String {
        serde_json::json!({"id": user_id, "first_name": "Тест", "username": "test"}).to_string()
    }

    fn now_unix() -> i64 {
        chrono::Utc::now().timestamp()
    }

    #[test]
    fn validate_init_data_ok() {
        let init = make_init_data(111, now_unix());
        let user = validate_init_data(&init, TEST_TOKEN).unwrap();
        assert_eq!(user.id, 111);
        assert_eq!(user.first_name, "Тест");
        assert_eq!(user.username.as_deref(), Some("test"));
    }

    #[test]
    fn validate_init_data_tma_prefix_stripped_by_caller() {
        // Сам validate_init_data префикс не режет (это делает require_user);
        // здесь проверяем, что обрезанная строка валидна.
        let init = make_init_data(111, now_unix());
        let stripped = init.strip_prefix("tma ").unwrap_or(&init);
        assert!(validate_init_data(stripped, TEST_TOKEN).is_ok());
    }

    #[test]
    fn validate_init_data_bad_signature() {
        let init = make_init_data(111, now_unix()).replace("111", "112");
        let err = validate_init_data(&init, TEST_TOKEN).unwrap_err();
        assert_eq!(err, "неверная подпись initData");
    }

    #[test]
    fn validate_init_data_expired() {
        let init = make_init_data(111, now_unix() - 90_000);
        let err = validate_init_data(&init, TEST_TOKEN).unwrap_err();
        assert_eq!(err, "initData просрочен");
    }

    #[test]
    fn validate_init_data_no_user() {
        // initData без user: подпись считается по оставшимся парам.
        let auth_date = now_unix();
        let check = format!("auth_date={auth_date}");
        let mut mac = Hmac::<Sha256>::new_from_slice(b"WebAppData").unwrap();
        mac.update(TEST_TOKEN.as_bytes());
        let secret = mac.finalize().into_bytes();
        let mut mac = Hmac::<Sha256>::new_from_slice(&secret).unwrap();
        mac.update(check.as_bytes());
        let hash = hex_encode(&mac.finalize().into_bytes());
        let init = format!("auth_date={auth_date}&hash={hash}");
        let err = validate_init_data(&init, TEST_TOKEN).unwrap_err();
        assert_eq!(err, "в initData нет пользователя");
    }

    #[test]
    fn validate_init_data_no_hash() {
        let err = validate_init_data("auth_date=123&user={}", TEST_TOKEN).unwrap_err();
        assert_eq!(err, "в initData нет hash");
    }

    #[test]
    fn validate_init_data_no_token() {
        let init = make_init_data(111, now_unix());
        let err = validate_init_data(&init, "").unwrap_err();
        assert_eq!(err, "initData или BOT_TOKEN отсутствует");
    }

    #[test]
    fn parse_bells_sections() {
        // Тот же ZVONKI_TEXT, что в test_miniapp_api.py.
        let text = "<b>Четверг • 2 курс</b>\n\n<b>1 смена</b>\n\
             1п: 8:00 – 9:30\nОбед: 9:30 – 10:10\n2п: 10:10 – 11:40\n3п: 11:50 – 13:20\n\n\
             <b>2 смена</b>\n4п: 13:30 – 15:00\n5п: 15:30 – 17:00\n6п: 17:10 – 18:40";
        let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
        let sections = parse_bells(&lines);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].title, "1 смена");
        assert_eq!(sections[0].rows[0].label, "1 пара");
        assert_eq!(sections[0].rows[0].kind, "pair");
        assert_eq!(sections[0].rows[0].pair, Some(1));
        assert_eq!(sections[0].rows[0].start, "08:00");
        assert_eq!(sections[0].rows[0].end, "09:30");
        assert_eq!(sections[0].rows[1].kind, "lunch");
        assert_eq!(sections[0].rows[1].label, "Обед");
        assert_eq!(sections[1].title, "2 смена");
        assert_eq!(sections[1].rows[0].pair, Some(4));
        assert_eq!(sections[1].rows[0].start, "13:30");
    }

    #[test]
    fn parse_bells_events() {
        let lines = vec![
            "<b>1 смена</b>".to_string(),
            "Разговоры о важном: 13:30 – 14:00".to_string(),
            "Классный час: 13:30 – 14:00".to_string(),
            "Перерыв: 9:30 – 10:10".to_string(),
        ];
        let sections = parse_bells(&lines);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].rows[0].label, "Разговоры о важном");
        assert_eq!(sections[0].rows[0].kind, "event");
        assert_eq!(sections[0].rows[1].label, "Классный час");
        assert_eq!(sections[0].rows[2].label, "Перерыв");
    }

    /// Тестовый ApiState на временной копии фикстурной БД.
    async fn test_state() -> (tempfile::TempDir, ApiState) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("settings.db");
        std::fs::copy(
            format!(
                "{}/tests/fixtures/settings.db",
                env!("CARGO_MANIFEST_DIR")
            ),
            &db_path,
        )
        .unwrap();
        let db = Arc::new(Db::open(db_path.to_str().unwrap()).unwrap());
        db.init().await.unwrap();
        let service = Arc::new(ScheduleService::new(db.clone(), String::new(), String::new()));
        service.load_ram().await.unwrap();
        (
            dir,
            ApiState {
                service,
                db,
                bot_username: String::new(),
                bot_token: TEST_TOKEN.to_string(),
            },
        )
    }

    #[tokio::test]
    async fn day_payload_fixture_18_11() {
        let (_dir, state) = test_state().await;
        let target = tyumen::parse_date("18.11.2026").unwrap();
        let payload = day_payload(&state.service, "ЮР-26-21", target);
        assert_eq!(payload.date, "18.11.2026");
        assert_eq!(payload.weekday, "Среда");
        assert_eq!(payload.group, "ЮР-26-21");
        assert!(payload.has_data);
        assert_eq!(payload.pairs.len(), 3);
        assert!(payload.pairs[0].subject.contains("безопасности"), "{}", payload.pairs[0].subject);
        // services.py:2493 format_day_data НЕ применяет fix_room: кабинет
        // "каб. 60" остаётся в предмете, room "—" -> "" (как в живом Python).
        assert!(payload.pairs[1].subject.contains("География"), "{}", payload.pairs[1].subject);
        assert!(payload.pairs[1].room.is_empty(), "{}", payload.pairs[1].room);
        assert!(payload.pairs[2].subject.contains("Обществознание"), "{}", payload.pairs[2].subject);
        assert!(payload.pairs.iter().all(|p| !p.changed));
        assert!(payload.changes.is_empty());
        // Время пар — из штатной сетки среды, 1 курс.
        assert_eq!(
            payload.pairs[0].time.as_ref().map(|t| (t.start.as_str(), t.end.as_str())),
            Some(("08:00", "09:30"))
        );
        assert_eq!(
            payload.pairs[1].time.as_ref().map(|t| (t.start.as_str(), t.end.as_str())),
            Some(("10:10", "11:40"))
        );
    }

    /// Поднимает роутер на 127.0.0.1:0 и возвращает адрес.
    async fn spawn_test_server(state: ApiState) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router(state)).await.unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn http_group_ambiguous_and_ok() {
        let (_dir, state) = test_state().await;
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        let init = make_init_data(111, now_unix());

        // Двусмысленная 26-21 -> 422 + choices.
        let resp = client
            .post(format!("http://{addr}/api/group"))
            .header("X-Telegram-Init-Data", &init)
            .json(&serde_json::json!({"group": "26-21"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 422);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"], "ambiguous_group");
        assert_eq!(body["choices"], serde_json::json!(["ТД-26-21", "ЮР-26-21"]));

        // Обычная группа -> 200, группа сохранена.
        let resp = client
            .post(format!("http://{addr}/api/group"))
            .header("X-Telegram-Init-Data", &init)
            .json(&serde_json::json!({"group": "ЮР-26-21"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["group"], "ЮР-26-21");
        assert_eq!(body["ok"], true);

        // /api/me видит сохранённую группу.
        let resp = client
            .get(format!("http://{addr}/api/me"))
            .header("X-Telegram-Init-Data", &init)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["group"], "ЮР-26-21");
        assert_eq!(body["group_set"], true);
        assert_eq!(body["user"]["id"], 111);
    }

    #[tokio::test]
    async fn http_day_401_409_200() {
        let (_dir, state) = test_state().await;
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();

        // Без initData -> 401.
        let resp = client
            .get(format!("http://{addr}/api/day?date=18.11.2026"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);

        // initData есть, группы нет -> 409.
        let init = make_init_data(111, now_unix());
        let resp = client
            .get(format!("http://{addr}/api/day?date=18.11.2026"))
            .header("X-Telegram-Init-Data", &init)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 409);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["error"], "Сначала укажите группу");

        // Плохой date -> 400 (после привязки группы).
        client
            .post(format!("http://{addr}/api/group"))
            .header("X-Telegram-Init-Data", &init)
            .json(&serde_json::json!({"group": "ЮР-26-21"}))
            .send()
            .await
            .unwrap();
        let resp = client
            .get(format!("http://{addr}/api/day?date=не-дата"))
            .header("X-Telegram-Init-Data", &init)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        // Валидный день 18.11.2026 -> 3 пары.
        let resp = client
            .get(format!("http://{addr}/api/day?date=18.11.2026"))
            .header("X-Telegram-Init-Data", &init)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["pairs"].as_array().unwrap().len(), 3);
        assert!(body["pairs"][1]["subject"].as_str().unwrap().contains("География"));
        assert_eq!(body["has_data"], true);
    }

    #[tokio::test]
    async fn http_bells_and_reminders() {
        let (_dir, state) = test_state().await;
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        let init = make_init_data(111, now_unix());
        client
            .post(format!("http://{addr}/api/group"))
            .header("X-Telegram-Init-Data", &init)
            .json(&serde_json::json!({"group": "ЮР-26-21"}))
            .send()
            .await
            .unwrap();

        // /api/bells на 18.11.2026: секции, 1 пара в 08:00, курс и смена.
        let resp = client
            .get(format!("http://{addr}/api/bells?date=18.11.2026"))
            .header("X-Telegram-Init-Data", &init)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["course"], "1 курс");
        assert_eq!(body["shift"], 1);
        assert_eq!(body["sections"][0]["rows"][0]["start"], "08:00");
        assert_eq!(body["sections"][0]["rows"][0]["pair"], 1);

        // /api/reminders: GET 0, POST 15, GET 15, POST 500 -> 400, POST "abc" -> 400.
        let resp = client
            .get(format!("http://{addr}/api/reminders"))
            .header("X-Telegram-Init-Data", &init)
            .send()
            .await
            .unwrap();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["minutes"], 0);

        let resp = client
            .post(format!("http://{addr}/api/reminders"))
            .header("X-Telegram-Init-Data", &init)
            .json(&serde_json::json!({"minutes": 15}))
            .send()
            .await
            .unwrap();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["minutes"], 15);

        let resp = client
            .post(format!("http://{addr}/api/reminders"))
            .header("X-Telegram-Init-Data", &init)
            .json(&serde_json::json!({"minutes": 500}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);

        let resp = client
            .post(format!("http://{addr}/api/reminders"))
            .header("X-Telegram-Init-Data", &init)
            .json(&serde_json::json!({"minutes": "abc"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
    }

    /// Представители всех 18 специальностей колледжа из settings.db.
    /// Голые номера — однозначные по таблице образовательных программ
    /// (GROUP_SPEC, schedule-miniapp/work/spec_map.js): у каждого номера
    /// ровно одна специальность. (группа, специальность, пар 18.11,
    /// № первой пары, подстрока предмета первой пары).
    /// Предметы — как отдаёт day_payload: БЕЗ fix_room, кабинет остаётся
    /// внутри предмета ("Основы философии лаб. 203", room ""), как в Python.
    const ALL_SPECIALTY_GROUPS: &[(&str, &str, usize, i32, &str)] = &[
        ("23-12", "09.02.07", 3, 3, "Основы философии"),
        ("26-15", "09.02.11", 3, 1, "Физика"),
        ("26-12", "09.02.12", 3, 1, "Литература"),
        ("25-14", "10.02.05", 3, 1, "Основы алгоритмизации"),
        ("24-11", "19.02.13", 3, 1, "МДК.06.01"),
        ("24-09", "21.02.19", 4, 1, "Информационные технологии"),
        ("23-10", "38.02.01", 3, 4, "МДК.04.01"),
        ("24-04", "38.02.02", 3, 1, "Основы предпринимательской"),
        ("24-01", "38.02.03", 3, 4, "Иностранный язык"),
        ("25-26", "38.02.06", 3, 1, "Организация учета"),
        ("24-08", "38.02.07", 3, 1, "Рынок ценных бумаг"),
        ("24-10", "38.02.08", 3, 1, "МДК.03.01"),
        ("26-14", "39.02.01", 3, 1, "Физическая культура"),
        ("23-07", "40.02.02", 3, 2, "Криминология"),
        ("24-06", "40.02.04", 3, 4, "Жилищное право"),
        ("24-05", "43.02.15", 4, 1, "МДК.01.02"),
        ("24-03", "43.02.16", 3, 2, "Иностранный язык (второй)"),
        ("26-18", "46.02.02", 3, 1, "Основы безопасности"),
    ];

    #[tokio::test]
    async fn day_payload_all_specialties_18_11() {
        let (_dir, state) = test_state().await;
        let target = tyumen::parse_date("18.11.2026").unwrap();
        for (group, spec, n_pairs, first_n, first_subj) in ALL_SPECIALTY_GROUPS {
            let payload = day_payload(&state.service, group, target);
            assert_eq!(payload.date, "18.11.2026", "{group}");
            assert_eq!(payload.weekday, "Среда", "{group}");
            assert_eq!(payload.group, *group, "{spec}");
            assert!(payload.has_data, "{group} ({spec}): есть пары на 18.11");
            assert_eq!(payload.pairs.len(), *n_pairs, "{group} ({spec})");
            assert_eq!(payload.pairs[0].n, *first_n, "{group} ({spec})");
            assert!(
                payload.pairs[0].subject.contains(first_subj),
                "{group} ({spec}): {}",
                payload.pairs[0].subject
            );
            assert!(payload.changes.is_empty(), "{group} ({spec})");
            assert!(payload.pairs.iter().all(|p| !p.changed), "{group} ({spec})");
            // У каждой пары есть время из штатной сетки среды.
            for p in &payload.pairs {
                let t = p.time.as_ref().unwrap_or_else(|| panic!("{group} {}п без времени", p.n));
                assert!(!t.start.is_empty() && !t.end.is_empty(), "{group} {}п", p.n);
            }
            // 1-я пара у всех — 08:00–09:30 (одинаково для всех курсов).
            if let Some(p1) = payload.pairs.iter().find(|p| p.n == 1) {
                let t = p1.time.as_ref().unwrap();
                assert_eq!((t.start.as_str(), t.end.as_str()), ("08:00", "09:30"), "{group}");
            }
        }
        // 2-я смена (пары №>=5): время из секции «2 смена» среды.
        let p5 = day_payload(&state.service, "23-12", target)
            .pairs
            .into_iter()
            .find(|p| p.n == 5)
            .unwrap();
        assert_eq!(
            p5.time.as_ref().map(|t| (t.start.as_str(), t.end.as_str())),
            Some(("16:10", "17:40"))
        );
        let p6 = day_payload(&state.service, "24-06", target)
            .pairs
            .into_iter()
            .find(|p| p.n == 6)
            .unwrap();
        assert_eq!(
            p6.time.as_ref().map(|t| (t.start.as_str(), t.end.as_str())),
            Some(("17:50", "19:20"))
        );
        let p4 = day_payload(&state.service, "23-10", target)
            .pairs
            .into_iter()
            .find(|p| p.n == 4)
            .unwrap();
        assert_eq!(
            p4.time.as_ref().map(|t| (t.start.as_str(), t.end.as_str())),
            Some(("14:10", "15:40"))
        );
        // 2-я пара среды зависит от курса: 1 курс — 10:10–11:40,
        // 2 курс — 09:40–10:10, 3,4 курс — 09:40–11:10.
        for (group, want) in [
            ("26-15", ("10:10", "11:40")),
            ("25-14", ("09:40", "10:10")),
            ("24-11", ("09:40", "11:10")),
        ] {
            let p2 = day_payload(&state.service, group, target)
                .pairs
                .into_iter()
                .find(|p| p.n == 2)
                .unwrap();
            assert_eq!(
                p2.time.as_ref().map(|t| (t.start.as_str(), t.end.as_str())),
                Some(want),
                "{group} 2п"
            );
        }
    }

    #[tokio::test]
    async fn day_payload_changes_flags_18_11() {
        let (_dir, state) = test_state().await;
        let target = tyumen::parse_date("18.11.2026").unwrap();

        // 26-16 (ТЭС): замена только на 4п. — предмет пары НЕ подменяется,
        // только флаг changed (как в Python: changed_pairs из extract_pair_numbers).
        let payload = day_payload(&state.service, "26-16", target);
        assert!(payload.has_data);
        assert!(payload.has_changes);
        assert_eq!(payload.changes, vec!["ТЭС-26-16 4п. Математика каб. 216"]);
        assert_eq!(payload.pairs.len(), 3);
        for p in &payload.pairs {
            assert_eq!(p.changed, p.n == 4, "26-16 {}п", p.n);
        }
        assert!(payload.pairs.iter().find(|p| p.n == 4).unwrap().subject.contains("История"));

        // 26-23 (ПД): замена на 1п.
        let payload = day_payload(&state.service, "26-23", target);
        assert!(payload.has_changes);
        assert_eq!(
            payload.changes,
            vec!["ПД-26-23 1п. Информационные технологии в профессиональной деятельности каб. 407"]
        );
        for p in &payload.pairs {
            assert_eq!(p.changed, p.n == 1, "26-23 {}п", p.n);
        }
    }

    #[tokio::test]
    async fn day_payload_no_data_18_11() {
        let (_dir, state) = test_state().await;
        let target = tyumen::parse_date("18.11.2026").unwrap();
        // У 23-05 нет записей на 18.11 (в БД есть только другие даты).
        let payload = day_payload(&state.service, "23-05", target);
        assert!(!payload.has_data);
        assert!(!payload.has_changes);
        assert!(payload.pairs.is_empty());
        assert!(payload.changes.is_empty());
        assert_eq!(payload.group, "23-05");
    }

    /// POST /api/group для пользователя.
    async fn set_group_http(client: &reqwest::Client, addr: std::net::SocketAddr, user_id: i64, group: &str) -> reqwest::Response {
        let init = make_init_data(user_id, now_unix());
        client
            .post(format!("http://{addr}/api/group"))
            .header("X-Telegram-Init-Data", &init)
            .json(&serde_json::json!({"group": group}))
            .send()
            .await
            .unwrap()
    }

    async fn get_http(client: &reqwest::Client, addr: std::net::SocketAddr, user_id: i64, path: &str) -> (reqwest::StatusCode, Value) {
        let init = make_init_data(user_id, now_unix());
        let resp = client
            .get(format!("http://{addr}{path}"))
            .header("X-Telegram-Init-Data", &init)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        (status, body)
    }

    #[tokio::test]
    async fn http_day_week_bells_all_specialties() {
        let (_dir, state) = test_state().await;
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();

        // /api/day по представителям: голый номер, 2-я смена, замены, нет данных.
        for (user_id, group, n_pairs) in [
            (201, "23-12", 3),
            (202, "26-18", 3),
            (203, "26-16", 3),
            (204, "23-05", 0),
            (205, "25-14", 3),
            (206, "24-06", 3),
        ] {
            let resp = set_group_http(&client, addr, user_id, group).await;
            assert_eq!(resp.status(), 200, "set group {group}");
            let (status, body) = get_http(&client, addr, user_id, "/api/day?date=18.11.2026").await;
            assert_eq!(status, 200, "{group}");
            assert_eq!(body["group"], group);
            assert_eq!(body["date"], "18.11.2026");
            assert_eq!(body["pairs"].as_array().unwrap().len(), n_pairs, "{group}");
            assert_eq!(body["has_data"], n_pairs > 0, "{group}");
        }
        // 26-16: замены и флаг changed на 4-й паре.
        let (_, body) = get_http(&client, addr, 203, "/api/day?date=18.11.2026").await;
        assert_eq!(body["has_changes"], true);
        assert_eq!(body["changes"].as_array().unwrap().len(), 1);
        let pairs = body["pairs"].as_array().unwrap();
        assert_eq!(pairs.iter().find(|p| p["n"] == 4).unwrap()["changed"], true);
        assert!(pairs.iter().filter(|p| p["n"] != 4).all(|p| p["changed"] == false));
        // 23-05: пар нет.
        let (_, body) = get_http(&client, addr, 204, "/api/day?date=18.11.2026").await;
        assert_eq!(body["has_data"], false);

        // /api/week с якорем 18.11.2026 (среда): неделя Пн 16.11 — Сб 21.11.
        let (status, body) = get_http(&client, addr, 201, "/api/week?date=18.11.2026").await;
        assert_eq!(status, 200);
        assert_eq!(body["week_start"], "16.11.2026");
        assert_eq!(body["week_end"], "21.11.2026");
        assert_eq!(body["group"], "23-12");
        let days = body["days"].as_array().unwrap();
        assert_eq!(days.len(), 6);
        assert_eq!(days[2]["date"], "18.11.2026");
        assert_eq!(days[2]["pairs"].as_array().unwrap().len(), 3);
        assert_eq!(days[2]["pairs"][2]["n"], 5);
        // У 26-18 (1-я смена) в среду пары 1,2,3.
        let (status, body) = get_http(&client, addr, 202, "/api/week?date=18.11.2026").await;
        assert_eq!(status, 200);
        let days = body["days"].as_array().unwrap();
        let wed_pairs = days[2]["pairs"].as_array().unwrap();
        assert_eq!(
            wed_pairs.iter().map(|p| p["n"].as_i64().unwrap()).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        // /api/bells: курс и смена по группе.
        let (status, body) = get_http(&client, addr, 201, "/api/bells?date=18.11.2026").await;
        assert_eq!(status, 200);
        assert_eq!(body["course"], "3,4 курс");
        assert_eq!(body["shift"], 2);
        let sections = body["sections"].as_array().unwrap();
        let second = sections.iter().find(|s| s["title"] == "2 смена").expect("секция 2 смены");
        let row5 = second["rows"].as_array().unwrap().iter().find(|r| r["pair"] == 5).unwrap();
        assert_eq!(row5["start"], "16:10");
        assert_eq!(row5["end"], "17:40");

        let (status, body) = get_http(&client, addr, 202, "/api/bells?date=18.11.2026").await;
        assert_eq!(status, 200);
        assert_eq!(body["course"], "1 курс");
        assert_eq!(body["shift"], 1);
        assert_eq!(body["sections"][0]["title"], "1 смена");
        assert_eq!(body["sections"][0]["rows"][0]["pair"], 1);
        assert_eq!(body["sections"][0]["rows"][0]["start"], "08:00");

        // 2 курс: 2-я пара среды — 09:40–10:10 (разделённая пара с обедом).
        let (status, body) = get_http(&client, addr, 205, "/api/bells?date=18.11.2026").await;
        assert_eq!(status, 200);
        assert_eq!(body["course"], "2 курс");
        assert_eq!(body["shift"], 1);
        let row2 = body["sections"][0]["rows"].as_array().unwrap().iter().find(|r| r["pair"] == 2).unwrap();
        assert_eq!(row2["start"], "09:40");
        assert_eq!(row2["end"], "10:10");

        // /api/overview: структура (даты — сегодняшние, поэтому только форма).
        let (status, body) = get_http(&client, addr, 202, "/api/overview").await;
        assert_eq!(status, 200);
        assert_eq!(body["group"], "26-18");
        assert!(body["today"]["date"].is_string());
        assert!(body["tomorrow"]["date"].is_string());
        assert!(body["today"]["pairs"].is_array());
        assert!(body["tomorrow"]["pairs"].is_array());
        assert!(body["bells_today"].is_array());
    }

    #[tokio::test]
    async fn http_group_bare_and_invalid() {
        let (_dir, state) = test_state().await;
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();

        // Голые номера без префикса принимаются как есть.
        let resp = set_group_http(&client, addr, 301, "23-12").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.json::<Value>().await.unwrap()["group"], "23-12");
        let resp = set_group_http(&client, addr, 302, "24-27").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.json::<Value>().await.unwrap()["group"], "24-27");

        // Неоднозначная 26-21 -> 422 с choices (второй раз — другой пользователь).
        let resp = set_group_http(&client, addr, 303, "26-21").await;
        assert_eq!(resp.status(), 422);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["choices"], serde_json::json!(["ТД-26-21", "ЮР-26-21"]));

        // Мусор и пустая строка -> 400.
        let resp = set_group_http(&client, addr, 304, "abc").await;
        assert_eq!(resp.status(), 400);
        let resp = set_group_http(&client, addr, 305, "").await;
        assert_eq!(resp.status(), 400);
    }

    /// POST с initData, возвращает (статус, JSON).
    async fn post_json(
        client: &reqwest::Client,
        addr: std::net::SocketAddr,
        user_id: i64,
        path: &str,
        body: Value,
    ) -> (reqwest::StatusCode, Value) {
        let init = make_init_data(user_id, now_unix());
        let resp = client
            .post(format!("http://{addr}{path}"))
            .header("X-Telegram-Init-Data", &init)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        (status, body)
    }

    /// DELETE с initData и JSON-телом.
    async fn delete_json(
        client: &reqwest::Client,
        addr: std::net::SocketAddr,
        user_id: i64,
        path: &str,
        body: Value,
    ) -> (reqwest::StatusCode, Value) {
        let init = make_init_data(user_id, now_unix());
        let resp = client
            .delete(format!("http://{addr}{path}"))
            .header("X-Telegram-Init-Data", &init)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        (status, body)
    }

    /// Роутер с middleware (rate-limit + security headers) на 127.0.0.1:0.
    async fn spawn_test_server_mw(state: ApiState) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router_with_middleware(state))
                .await
                .unwrap();
        });
        addr
    }

    #[tokio::test]
    async fn http_teachers_matrix() {
        let (_dir, state) = test_state().await;
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();

        // ЮР-26-21 (сохранённая группа): История — без Барковой.
        set_group_http(&client, addr, 401, "ЮР-26-21").await;
        let (status, body) = get_http(&client, addr, 401, "/api/teachers?subject=%D0%98%D1%81%D1%82%D0%BE%D1%80%D0%B8%D1%8F").await;
        assert_eq!(status, 200);
        assert_eq!(body["spec"], "40.02.04");
        assert_eq!(body["spec_name"], "Юриспруденция");
        assert_eq!(body["group"], "ЮР-26-21");
        assert_eq!(
            body["teachers"],
            serde_json::json!(["Ильина Н.В.", "Канева Н.В.", "Титова Д.А."])
        );

        // Параметр group перебивает сохранённую: ТД-26-21 — История с Барковой и Титовой (4).
        let (status, body) = get_http(&client, addr, 401, "/api/teachers?subject=%D0%98%D1%81%D1%82%D0%BE%D1%80%D0%B8%D1%8F&group=%D0%A2%D0%94-26-21").await;
        assert_eq!(status, 200);
        assert_eq!(body["spec"], "38.02.08");
        assert_eq!(
            body["teachers"],
            serde_json::json!(["Баркова Я.Н.", "Ильина Н.В.", "Канева Н.В.", "Титова Д.А."])
        );

        // 24-27 (09.02.07): Математика — переопределение программы:
        // только трое из программы 09.02.07 (глобальный список — 6,
        // см. teachers::tests и случай без группы ниже).
        set_group_http(&client, addr, 402, "24-27").await;
        let (status, body) = get_http(&client, addr, 402, "/api/teachers?subject=%D0%9C%D0%B0%D1%82%D0%B5%D0%BC%D0%B0%D1%82%D0%B8%D0%BA%D0%B0").await;
        assert_eq!(status, 200);
        assert_eq!(body["spec"], "09.02.07");
        assert_eq!(
            body["teachers"],
            serde_json::json!(["Мирошникова В.В.", "Оболдина Е.А.", "Шапенова К.К."])
        );

        // Без группы: Математика — глобальный список шестерых (без капа).
        let (status, body) = get_http(&client, addr, 404, "/api/teachers?subject=%D0%9C%D0%B0%D1%82%D0%B5%D0%BC%D0%B0%D1%82%D0%B8%D0%BA%D0%B0").await;
        assert_eq!(status, 200);
        assert_eq!(body["spec"], Value::Null);
        assert_eq!(body["teachers"].as_array().unwrap().len(), 6);

        // 26-14 (39.02.01): Физическая культура — Мартьянов (переопределение).
        set_group_http(&client, addr, 403, "26-14").await;
        let (status, body) = get_http(&client, addr, 403, "/api/teachers?subject=%D1%84%D0%B8%D0%B7%D0%B8%D1%87%D0%B5%D1%81%D0%BA%D0%B0%D1%8F%20%D0%BA%D1%83%D0%BB%D1%8C%D1%82%D1%83%D1%80%D0%B0").await;
        assert_eq!(status, 200);
        assert_eq!(body["spec"], "39.02.01");
        assert_eq!(body["teachers"], serde_json::json!(["Мартьянов К.А."]));

        // Алиас «ОБЖ» у ЮР — Ковылина; без группы — все трое.
        let (status, body) = get_http(&client, addr, 401, "/api/teachers?subject=%D0%9E%D0%91%D0%96").await;
        assert_eq!(status, 200);
        assert_eq!(body["teachers"], serde_json::json!(["Ковылина А.А."]));
        let (status, body) = get_http(&client, addr, 404, "/api/teachers?subject=%D0%9E%D0%91%D0%96").await;
        assert_eq!(status, 200, "без сохранённой группы — тоже 200");
        assert_eq!(body["group"], Value::Null);
        assert_eq!(body["spec"], Value::Null);
        assert_eq!(
            body["teachers"],
            serde_json::json!(["Шамухаметов Р.Р.", "Южанин С.А.", "Ковылина А.А."])
        );

        // Алиас «Основы права» у ЮР — Божко+Степанова.
        let (status, body) = get_http(&client, addr, 401, "/api/teachers?subject=%D0%9E%D1%81%D0%BD%D0%BE%D0%B2%D1%8B%20%D0%BF%D1%80%D0%B0%D0%B2%D0%B0").await;
        assert_eq!(status, 200);
        assert_eq!(
            body["teachers"],
            serde_json::json!(["Божко О.А.", "Степанова В.С."])
        );

        // Неизвестный предмет — пустой список, spec всё равно определяется.
        let (status, body) = get_http(&client, addr, 401, "/api/teachers?subject=%D0%BD%D0%B5%D1%81%D1%83%D1%89%D0%B5%D1%81%D1%82%D0%B2%D1%83%D1%8E%D1%89%D0%B8%D0%B9").await;
        assert_eq!(status, 200);
        assert_eq!(body["teachers"], serde_json::json!([]));
        assert_eq!(body["spec"], "40.02.04");

        // Пустой subject -> 400.
        let (status, _) = get_http(&client, addr, 401, "/api/teachers?subject=%20%20").await;
        assert_eq!(status, 400);
        let (status, _) = get_http(&client, addr, 401, "/api/teachers").await;
        assert_eq!(status, 400);
    }

    #[tokio::test]
    async fn http_search_cases() {
        let (_dir, state) = test_state().await;
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        let init = make_init_data(501, now_unix());

        // По предмету на дату: Математика 18.11.2026.
        let (status, body) = get_http(&client, addr, 501, "/api/search?q=%D0%BC%D0%B0%D1%82%D0%B5%D0%BC%D0%B0%D1%82%D0%B8%D0%BA%D0%B0&date=18.11.2026").await;
        assert_eq!(status, 200);
        assert_eq!(body["q"], "математика");
        assert_eq!(body["date"], "18.11.2026");
        let results = body["results"].as_array().unwrap();
        assert!(!results.is_empty());
        let math = results
            .iter()
            .find(|r| r["subject"].as_str().unwrap().contains("Математика"))
            .expect("Математика в результатах");
        assert!(math["teachers"].as_array().unwrap().len() >= 6);
        let hits = math["hits"].as_array().unwrap();
        // 17 строк «Математика» 18.11 в архиве минус «Дискретная математика»
        // (отдельный предмет); fix_room переносит «каб. N» в room,
        // поэтому все 16 пар группируются под одной «Математикой».
        assert_eq!(hits.len(), 16, "пар с Математикой 18.11");
        assert!(hits.iter().all(|h| h["date"] == "18.11.2026"));
        assert!(hits.iter().all(|h| h["pair"].as_i64().unwrap() >= 1));
        assert!(hits.iter().all(|h| !h["group_display"].as_str().unwrap().is_empty()));
        assert!(hits.iter().all(|h| !h["room"].as_str().unwrap().is_empty()));

        // «Дискретная математика…» — отдельный предмет, 1 пара 24-14 2п,
        // кабинет «лаб. 225» вынесен из предмета в room.
        let discrete = results
            .iter()
            .find(|r| r["subject"].as_str().unwrap().contains("Дискретная"))
            .expect("Дискретная математика в результатах");
        let d_hits = discrete["hits"].as_array().unwrap();
        assert_eq!(d_hits.len(), 1);
        assert_eq!(d_hits[0]["group"], "24-14");
        assert_eq!(d_hits[0]["pair"], 2);
        assert_eq!(d_hits[0]["room"], "лаб. 225");

        // По преподавателю без даты: Оболдина — есть результаты, все с ней в teachers.
        let (status, body) = get_http(&client, addr, 501, "/api/search?q=%D0%BE%D0%B1%D0%BE%D0%BB%D0%B4%D0%B8%D0%BD%D0%B0").await;
        assert_eq!(status, 200);
        assert_eq!(body["date"], Value::Null);
        let results = body["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert!(results.len() <= 50);
        let total_hits: usize = results
            .iter()
            .map(|r| r["hits"].as_array().unwrap().len())
            .sum();
        assert!(total_hits <= 200 && total_hits > 0);
        assert!(results.iter().any(|r| r["teachers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t.as_str().unwrap().contains("Оболдина"))));

        // По преподавателю с датой: все хиты — на эту дату.
        let (status, body) = get_http(&client, addr, 501, "/api/search?q=%D0%BE%D0%B1%D0%BE%D0%BB%D0%B4%D0%B8%D0%BD%D0%B0&date=18.11.2026").await;
        assert_eq!(status, 200);
        for r in body["results"].as_array().unwrap() {
            for h in r["hits"].as_array().unwrap() {
                assert_eq!(h["date"], "18.11.2026");
            }
        }

        // Регистр не важен.
        let (status, body) = get_http(&client, addr, 501, "/api/search?q=%D0%9C%D0%90%D0%A2%D0%95%D0%9C%D0%90%D0%A2%D0%98%D0%9A%D0%90&date=18.11.2026").await;
        assert_eq!(status, 200);
        assert!(!body["results"].as_array().unwrap().is_empty());

        // Пустой запрос и плохая дата -> 400.
        let (status, _) = get_http(&client, addr, 501, "/api/search?q=%20%20").await;
        assert_eq!(status, 400);
        let (status, _) = get_http(&client, addr, 501, "/api/search?q=x").await;
        assert_eq!(status, 200, "нет совпадений — 200 с пустым списком");
        let (status, _) = get_http(&client, addr, 501, "/api/search?q=x&date=не-дата").await;
        assert_eq!(status, 400);

        // Без initData -> 401 (проверяется и в http_401_all_routes).
        let resp = client
            .get(format!("http://{addr}/api/search?q=x"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        let _ = init;
    }

    #[tokio::test]
    async fn http_stats_cases() {
        let (_dir, state) = test_state().await;
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();

        for (user_id, group, display, first_week) in [
            (601, "ЮР-26-21", "ЮР-26-21", "16.11.2026"),
            // 24-27: архив в фикстуре кончается 31.10.2026 —
            // последняя неделя Пн 26.10.2026 (не 16.11).
            (602, "24-27", "ИП-24-27", "26.10.2026"),
            (603, "26-14", "СР-26-14", "16.11.2026"),
        ] {
            set_group_http(&client, addr, user_id, group).await;
            let (status, body) = get_http(&client, addr, user_id, "/api/stats").await;
            assert_eq!(status, 200, "{group}");
            assert_eq!(body["group"], group);
            assert_eq!(body["group_display"], display);
            // Все даты фикстуры ≤ сегодня (20.11.2026) — total == past.
            assert_eq!(body["total_pairs"], body["past_pairs"], "{group}");
            assert!(body["past_pairs"].as_i64().unwrap() > 0, "{group}");
            assert!(body["changes_total"].as_i64().unwrap() >= 0);
            assert_eq!(body["absences_total"], 0);
            let by_subject = body["by_subject"].as_array().unwrap();
            let by_week = body["by_week"].as_array().unwrap();
            assert!(!by_subject.is_empty(), "{group}");
            assert!(!by_week.is_empty(), "{group}");
            // Инварианты: суммы по предметам/неделям == past_pairs.
            let sum_subj: i64 = by_subject.iter().map(|s| s["pairs"].as_i64().unwrap()).sum();
            let sum_week: i64 = by_week.iter().map(|w| w["pairs"].as_i64().unwrap()).sum();
            assert_eq!(sum_subj, body["past_pairs"].as_i64().unwrap(), "{group}");
            assert_eq!(sum_week, body["past_pairs"].as_i64().unwrap(), "{group}");
            // Недели — от новых к старым; первая — неделя последней даты
            // архива группы (у 24-27 архив кончается 31.10.2026).
            assert_eq!(by_week[0]["week_start"], first_week, "{group}");
            let week_dates: Vec<_> = by_week
                .iter()
                .map(|w| {
                    crate::tyumen::parse_date(w["week_start"].as_str().unwrap())
                        .expect("week_start — дата")
                })
                .collect();
            assert!(
                week_dates.windows(2).all(|w| w[0] > w[1]),
                "by_week убывает по дате: {group}"
            );
        }

        // Пропуск: ЮР 18.11, География 2п — виден в статистике, потом снимается.
        let (status, _) = post_json(
            &client,
            addr,
            601,
            "/api/absences",
            serde_json::json!({"date": "18.11.2026", "subject": "География", "pair": 2}),
        )
        .await;
        assert_eq!(status, 200);
        let (status, body) = get_http(&client, addr, 601, "/api/stats").await;
        assert_eq!(status, 200);
        assert_eq!(body["absences_total"], 1);
        let geo = body["by_subject"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["subject"].as_str().unwrap().contains("География"))
            .expect("География в by_subject");
        assert_eq!(geo["absences"], 1);
        let week = body["by_week"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["week_start"] == "16.11.2026")
            .expect("неделя 16.11 в by_week");
        assert_eq!(week["absences"], 1);
        let (status, _) = delete_json(
            &client,
            addr,
            601,
            "/api/absences",
            serde_json::json!({"date": "18.11.2026", "subject": "География"}),
        )
        .await;
        assert_eq!(status, 200);
        let (_, body) = get_http(&client, addr, 601, "/api/stats").await;
        assert_eq!(body["absences_total"], 0);

        // Без группы -> 409.
        let (status, _) = get_http(&client, addr, 604, "/api/stats").await;
        assert_eq!(status, 409);
    }

    #[tokio::test]
    async fn http_bootstrap_cases() {
        let (_dir, state) = test_state().await;
        let addr = spawn_test_server(state.clone()).await;
        let client = reqwest::Client::new();

        // Без группы: group null, today/week/bells null, НЕ 409.
        let (status, body) = get_http(&client, addr, 701, "/api/bootstrap").await;
        assert_eq!(status, 200);
        assert_eq!(body["group"], Value::Null);
        assert_eq!(body["group_display"], Value::Null);
        assert_eq!(body["today"], Value::Null);
        assert_eq!(body["week"], Value::Null);
        assert_eq!(body["bells"], Value::Null);
        assert_eq!(body["me"]["group"], Value::Null);
        assert_eq!(body["me"]["group_set"], false);
        assert_eq!(body["reminders"], serde_json::json!({"enabled": false, "minutes": 0}));

        // С группой: всё на месте, один запрос вместо четырёх.
        set_group_http(&client, addr, 702, "ЮР-26-21").await;
        // Напоминания: включить 15 мин — bootstrap их отдаёт.
        let (status, _) = post_json(
            &client,
            addr,
            702,
            "/api/reminders",
            serde_json::json!({"minutes": 15}),
        )
        .await;
        assert_eq!(status, 200);
        let (status, body) = get_http(&client, addr, 702, "/api/bootstrap").await;
        assert_eq!(status, 200);
        assert_eq!(body["group"], "ЮР-26-21");
        assert_eq!(body["group_display"], "ЮР-26-21");
        assert_eq!(body["me"]["group"], "ЮР-26-21");
        assert_eq!(body["me"]["group_set"], true);
        assert_eq!(body["reminders"], serde_json::json!({"enabled": true, "minutes": 15}));
        let today = tyumen::fmt_date(tyumen::tyumen_today());
        assert_eq!(body["today"]["date"], today);
        assert_eq!(body["today"]["group"], "ЮР-26-21");
        assert!(body["today"]["pairs"].is_array());
        assert_eq!(body["week"]["week_start"], "16.11.2026");
        assert_eq!(body["week"]["days"].as_array().unwrap().len(), 6);
        assert_eq!(body["bells"]["date"], today);
        assert!(body["bells"]["pairs"].is_array());
        assert!(!body["bells"]["pairs"].as_array().unwrap().is_empty());
        assert_eq!(body["bells"]["pairs"][0]["start"], "08:00");

        // Неоднозначная «26-21»: группа есть, но расписания нет (null, не 409).
        state.db.set_group(703, "26-21").await.unwrap();
        let (status, body) = get_http(&client, addr, 703, "/api/bootstrap").await;
        assert_eq!(status, 200);
        assert_eq!(body["group"], "26-21");
        assert_eq!(body["group_display"], "26-21");
        assert_eq!(body["today"], Value::Null);
        assert_eq!(body["week"], Value::Null);
        assert_eq!(body["bells"], Value::Null);
    }

    #[tokio::test]
    async fn http_notes_absences_crud() {
        let (_dir, state) = test_state().await;
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();

        // Заметки: пусто -> создать -> прочитать -> перезаписать ->
        // пустой текст удаляет -> изоляция по user_id.
        let (status, body) = get_http(&client, addr, 801, "/api/notes").await;
        assert_eq!(status, 200);
        assert_eq!(body["notes"], serde_json::json!([]));
        let (status, body) = post_json(
            &client,
            addr,
            801,
            "/api/notes",
            serde_json::json!({"subject": "Математика", "text": "повторить §3"}),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["ok"], true);
        assert_eq!(body["deleted"], false);
        let (status, body) = get_http(&client, addr, 801, "/api/notes").await;
        assert_eq!(status, 200);
        assert_eq!(body["notes"].as_array().unwrap().len(), 1);
        assert_eq!(body["notes"][0]["subject"], "Математика");
        assert_eq!(body["notes"][0]["text"], "повторить §3");
        assert!(!body["notes"][0]["updated_at"].as_str().unwrap().is_empty());
        let (status, _) = post_json(
            &client,
            addr,
            801,
            "/api/notes",
            serde_json::json!({"subject": "Математика", "text": "повторить §3–4"}),
        )
        .await;
        assert_eq!(status, 200);
        let (_, body) = get_http(&client, addr, 801, "/api/notes").await;
        assert_eq!(body["notes"].as_array().unwrap().len(), 1);
        assert_eq!(body["notes"][0]["text"], "повторить §3–4");
        // Другой пользователь не видит чужие заметки.
        let (_, body) = get_http(&client, addr, 802, "/api/notes").await;
        assert_eq!(body["notes"], serde_json::json!([]));
        // Пустой текст — удаление.
        let (status, body) = post_json(
            &client,
            addr,
            801,
            "/api/notes",
            serde_json::json!({"subject": "Математика", "text": "   "}),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["deleted"], true);
        let (_, body) = get_http(&client, addr, 801, "/api/notes").await;
        assert_eq!(body["notes"], serde_json::json!([]));
        // 400: нет subject / нет text / text не строка.
        let (status, _) = post_json(
            &client,
            addr,
            801,
            "/api/notes",
            serde_json::json!({"text": "x"}),
        )
        .await;
        assert_eq!(status, 400);
        let (status, _) = post_json(
            &client,
            addr,
            801,
            "/api/notes",
            serde_json::json!({"subject": "Математика"}),
        )
        .await;
        assert_eq!(status, 400);

        // Пропуски: создать -> upsert номера пары -> прочитать -> удалить.
        let (status, _) = get_http(&client, addr, 801, "/api/absences").await;
        assert_eq!(status, 200);
        let (status, body) = post_json(
            &client,
            addr,
            801,
            "/api/absences",
            serde_json::json!({"date": "18.11.2026", "subject": "Математика", "pair": 1}),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(body["ok"], true);
        let (status, _) = post_json(
            &client,
            addr,
            801,
            "/api/absences",
            serde_json::json!({"date": "18.11.2026", "subject": "Математика", "pair": 3}),
        )
        .await;
        assert_eq!(status, 200, "повторный upsert");
        let (_, body) = get_http(&client, addr, 801, "/api/absences").await;
        assert_eq!(body["absences"].as_array().unwrap().len(), 1);
        assert_eq!(body["absences"][0]["pair"], 3);
        // Изоляция по user_id.
        let (_, body) = get_http(&client, addr, 802, "/api/absences").await;
        assert_eq!(body["absences"], serde_json::json!([]));
        let (status, _) = delete_json(
            &client,
            addr,
            801,
            "/api/absences",
            serde_json::json!({"date": "18.11.2026", "subject": "Математика"}),
        )
        .await;
        assert_eq!(status, 200);
        let (_, body) = get_http(&client, addr, 801, "/api/absences").await;
        assert_eq!(body["absences"], serde_json::json!([]));
        // Повторное удаление — тоже 200.
        let (status, _) = delete_json(
            &client,
            addr,
            801,
            "/api/absences",
            serde_json::json!({"date": "18.11.2026", "subject": "Математика"}),
        )
        .await;
        assert_eq!(status, 200);
        // 400: плохая дата, нет предмета, пара вне 1–12.
        let (status, _) = post_json(
            &client,
            addr,
            801,
            "/api/absences",
            serde_json::json!({"date": "не-дата", "subject": "Математика", "pair": 1}),
        )
        .await;
        assert_eq!(status, 400);
        let (status, _) = post_json(
            &client,
            addr,
            801,
            "/api/absences",
            serde_json::json!({"date": "18.11.2026", "subject": "Математика", "pair": 0}),
        )
        .await;
        assert_eq!(status, 400);
        let (status, _) = post_json(
            &client,
            addr,
            801,
            "/api/absences",
            serde_json::json!({"date": "18.11.2026", "subject": "Математика", "pair": 13}),
        )
        .await;
        assert_eq!(status, 400);
    }

    #[tokio::test]
    async fn http_group_td_yur_roundtrip() {
        let (_dir, state) = test_state().await;
        let addr = spawn_test_server(state.clone()).await;
        let client = reqwest::Client::new();

        // ТД-26-21: выбор -> POST -> в БД ключ С префиксом -> me/day именно ТД.
        let resp = set_group_http(&client, addr, 901, "ТД-26-21").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(state.db.get_group(901).await.unwrap().as_deref(), Some("ТД-26-21"));
        let (_, body) = get_http(&client, addr, 901, "/api/me").await;
        assert_eq!(body["group"], "ТД-26-21");
        assert_eq!(body["group_display"], "ТД-26-21");
        let (_, body) = get_http(&client, addr, 901, "/api/day?date=18.11.2026").await;
        assert_eq!(body["group"], "ТД-26-21");
        assert_eq!(body["pairs"].as_array().unwrap().len(), 3);
        assert!(body["pairs"][0]["subject"].as_str().unwrap().contains("Стандартизация"));
        assert!(!body["pairs"][0]["subject"].as_str().unwrap().contains("безопасности"));

        // ЮР-26-21: то же, но именно ЮР (ОБЖ/География/Обществознание).
        let resp = set_group_http(&client, addr, 902, "ЮР-26-21").await;
        assert_eq!(resp.status(), 200);
        assert_eq!(state.db.get_group(902).await.unwrap().as_deref(), Some("ЮР-26-21"));
        let (_, body) = get_http(&client, addr, 902, "/api/me").await;
        assert_eq!(body["group"], "ЮР-26-21");
        assert_eq!(body["group_display"], "ЮР-26-21");
        let (_, body) = get_http(&client, addr, 902, "/api/day?date=18.11.2026").await;
        assert_eq!(body["group"], "ЮР-26-21");
        assert_eq!(body["pairs"].as_array().unwrap().len(), 3);
        assert!(body["pairs"][0]["subject"].as_str().unwrap().contains("безопасности"));
        assert!(!body["pairs"][0]["subject"].as_str().unwrap().contains("Стандартизация"));

        // /api/groups: обе — с display == key, голой «26-21» нет.
        let (_, body) = get_http(&client, addr, 901, "/api/groups").await;
        let groups = body["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 80, "81 группа архива минус голая 26-21");
        for g in ["ТД-26-21", "ЮР-26-21"] {
            let entry = groups.iter().find(|e| e["key"] == g).expect(g);
            assert_eq!(entry["display"], g);
        }
        assert!(groups.iter().all(|e| e["key"] != "26-21"));
        assert!(groups.iter().any(|e| e["key"] == "24-27" && e["display"] == "ИП-24-27"));
    }

    #[tokio::test]
    async fn http_group_mapping_all() {
        let (_dir, state) = test_state().await;
        let archive_groups = state.db.get_archive_groups().await.unwrap();
        assert_eq!(archive_groups.len(), 81);
        let addr = spawn_test_server(state.clone()).await;
        let client = reqwest::Client::new();

        // Все группы архива: POST display -> в БД голый ключ (кроме 26-21),
        // /api/me и /api/day — та же группа.
        for (i, key) in archive_groups.iter().enumerate() {
            let user_id = 1000 + i as i64;
            let display = crate::teachers::group_display(key);
            if key == "26-21" {
                let resp = set_group_http(&client, addr, user_id, &display).await;
                assert_eq!(resp.status(), 422, "голая 26-21 — не группа");
                continue;
            }
            let resp = set_group_http(&client, addr, user_id, &display).await;
            assert_eq!(resp.status(), 200, "POST display {display}");
            // Смена группы мгновенно: POST -> get_group сразу возвращает её.
            assert_eq!(
                state.db.get_group(user_id).await.unwrap().as_deref(),
                Some(key.as_str()),
                "POST {display} -> в БД {key}"
            );
            let (_, me) = get_http(&client, addr, user_id, "/api/me").await;
            assert_eq!(me["group"], *key);
            assert_eq!(me["group_display"], display);
            let (status, day) = get_http(&client, addr, user_id, "/api/day?date=18.11.2026").await;
            assert_eq!(status, 200, "{key}");
            assert_eq!(day["group"], *key);
        }

        // Все 168 групп программной таблицы: POST display -> ключ из
        // normalize_group; round-trip display == me.group_display — у 76:
        // 74 однозначных с префиксом + ТД/ЮР-26-21 (display == key).
        // Остальные — голые ключи без display (10 неоднозначных) и голые
        // записи таблицы (74): их display — префиксный вариант, а не они сами.
        let mut round_trips = 0usize;
        for (i, (display, _spec)) in crate::teachers::all_program_groups().iter().enumerate() {
            let user_id = 2000 + i as i64;
            let resp = set_group_http(&client, addr, user_id, display).await;
            assert_eq!(resp.status(), 200, "POST {display}");
            let want_key = crate::parser::normalize_group(display, "");
            assert_eq!(
                state.db.get_group(user_id).await.unwrap().as_deref(),
                Some(want_key.as_str()),
                "POST {display} -> {want_key}"
            );
            let (_, me) = get_http(&client, addr, user_id, "/api/me").await;
            assert_eq!(me["group"], want_key);
            if me["group_display"] == *display {
                round_trips += 1;
            }
        }
        assert_eq!(round_trips, 76);
    }

    #[tokio::test]
    async fn http_401_all_routes() {
        let (_dir, state) = test_state().await;
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        let gets = [
            "/api/me",
            "/api/groups",
            "/api/day?date=18.11.2026",
            "/api/week?date=18.11.2026",
            "/api/bells?date=18.11.2026",
            "/api/overview",
            "/api/reminders",
            "/api/teachers?subject=%D0%98%D1%81%D1%82%D0%BE%D1%80%D0%B8%D1%8F",
            "/api/search?q=x",
            "/api/stats",
            "/api/bootstrap",
            "/api/notes",
            "/api/absences",
        ];
        for path in gets {
            let resp = client.get(format!("http://{addr}{path}")).send().await.unwrap();
            assert_eq!(resp.status(), 401, "GET {path}");
        }
        let posts = [
            ("/api/group", serde_json::json!({"group": "24-27"})),
            ("/api/reminders", serde_json::json!({"minutes": 15})),
            ("/api/notes", serde_json::json!({"subject": "Математика", "text": "x"})),
            (
                "/api/absences",
                serde_json::json!({"date": "18.11.2026", "subject": "Математика", "pair": 1}),
            ),
        ];
        for (path, body) in posts {
            let resp = client
                .post(format!("http://{addr}{path}"))
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 401, "POST {path}");
        }
        let resp = client
            .delete(format!("http://{addr}/api/absences"))
            .json(&serde_json::json!({"date": "18.11.2026", "subject": "Математика"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "DELETE /api/absences");
    }

    /// /api/ping — 200 + {"ok": true} БЕЗ initData; остальные /api/*
    /// по-прежнему требуют авторизацию (см. http_401_all_routes выше:
    /// ping в тот список НЕ входит).
    #[tokio::test]
    async fn http_ping_no_auth() {
        let (_dir, state) = test_state().await;
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{addr}/api/ping"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.json::<serde_json::Value>().await.unwrap(),
            serde_json::json!({"ok": true})
        );
    }

    #[tokio::test]
    async fn http_bells_pairs() {
        let (_dir, state) = test_state().await;
        let addr = spawn_test_server(state).await;
        let client = reqwest::Client::new();
        set_group_http(&client, addr, 1101, "ЮР-26-21").await;
        let (status, body) = get_http(&client, addr, 1101, "/api/bells?date=18.11.2026").await;
        assert_eq!(status, 200);
        assert_eq!(body["course"], "1 курс");
        assert_eq!(body["shift"], 1);
        // Структурированные пары — та же сетка, что в sections:
        // pair_times собирает обе смены сетки среды (1 курс) — 6 пар.
        assert_eq!(
            body["pairs"],
            serde_json::json!([
                {"n": 1, "start": "08:00", "end": "09:30"},
                {"n": 2, "start": "10:10", "end": "11:40"},
                {"n": 3, "start": "11:50", "end": "13:20"},
                {"n": 4, "start": "14:10", "end": "15:40"},
                {"n": 5, "start": "16:10", "end": "17:40"},
                {"n": 6, "start": "17:50", "end": "19:20"},
            ])
        );
        assert!(body["sections"].is_array());
        // 2-я смена (23-12, среда): пары 4–6 из секции «2 смена».
        set_group_http(&client, addr, 1102, "23-12").await;
        let (status, body) = get_http(&client, addr, 1102, "/api/bells?date=18.11.2026").await;
        assert_eq!(status, 200);
        assert_eq!(body["shift"], 2);
        let pairs = body["pairs"].as_array().unwrap();
        assert_eq!(pairs.len(), 6);
        assert_eq!(pairs[3]["n"], 4);
        assert_eq!(pairs[3]["start"], "14:10");
        assert_eq!(pairs[5]["end"], "19:20");
    }

    #[tokio::test]
    async fn http_middleware_headers_and_rate_limit() {
        let (_dir, state) = test_state().await;
        let client = reqwest::Client::new();
        let init = make_init_data(1201, now_unix());

        // Security headers — на отдельном сервере со свежим лимитером.
        let addr = spawn_test_server_mw(state.clone()).await;
        let resp = client
            .get(format!("http://{addr}/api/me"))
            .header("X-Telegram-Init-Data", &init)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let headers = resp.headers();
        assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
        assert_eq!(
            headers.get("referrer-policy").unwrap(),
            "strict-origin-when-cross-origin"
        );
        assert_eq!(headers.get("x-frame-options").unwrap(), "SAMEORIGIN");
        assert!(headers.get("content-security-policy").is_none(), "CSP не ставится");

        // Rate-limit: 300 запросов/мин/IP — 300 проходят, 301-й -> 429.
        let addr = spawn_test_server_mw(state).await;
        for i in 0..300 {
            let resp = client
                .get(format!("http://{addr}/api/me"))
                .header("X-Telegram-Init-Data", &init)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "запрос {i}");
        }
        let resp = client
            .get(format!("http://{addr}/api/me"))
            .header("X-Telegram-Init-Data", &init)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 429);
        assert!(resp.headers().get("retry-after").is_some());
        let body: Value = resp.json().await.unwrap();
        assert!(body.get("error").is_some());
    }
}
