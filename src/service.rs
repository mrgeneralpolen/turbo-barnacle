//! ScheduleService — состояние и логика поверх Db + VkClient + parser.
//!
//! Эталон: класс ScheduleService в services.py (375–3297). Методы ниже —
//! та же публичная поверхность, что используется в main.py, miniapp_api.py
//! и reminders.py. Детали реализации (кэши, подписи постов, разбор текста
//! постов, сохранение в архив) — читать services.py построчно:
//! - load_ram ~3257, update_check ~1237, process_vk_posts ~1291
//! - parse_text_complex ~941, save_fast ~2411, save_fast_batch ~2431
//! - get_group_day_schedule ~655, get_file_for_date ~683
//! - get_group_shift ~2749, get_group_bells_end_time ~2777
//! - get_parsed_zvonki_for_date ~2895, has_zvonki_for_date ~2908
//! - get_zvonki_text ~2945, get_tomorrow_first_bell_info ~2966
//! - get_zvonki_resp ~3052, can_open_tomorrow_zvonki ~2877
//! - get_bells_photo_for_date ~474, cache_bells_photo_file_id ~486
//! - download_photo_bytes ~537, resolve_photo_input ~610, get_bells_photo_input ~648
//! - safe_send_document ~1022, safe_send_photo ~1072, safe_send_rich_alert ~1170
//!   (реализованы в bot.rs — у service нет Bot-хендла, не дублируются)
//! - start_session ~705, close_session ~713 (в Rust reqwest-клиент живёт
//!   в VkClient; отдельных start/close не требуется)
//!
//! DEAD_CHAT_ERRORS — services.py:141–155, полный список из 15 подстрок.
//!
//! Расхождения с Python (вынужденные, см. также финальный отчёт):
//! - Rich HTML портирован 1-в-1 (render_rich_table_html,
//!   format_parsed_zvonki_rich, get_tomorrow_bell_button_html, make_tg_time,
//!   get_zvonki_resp_rich, current_pair_progress_rich); отправка — через
//!   RawApi в bot.rs (teloxide sendRichMessage не знает).
//!   get_zvonki_resp возвращает только fallback-текст (как fb_text в Python).
//! - Кэши — std::sync::RwLock (не tokio): синхронные методы читают их
//!   напрямую через read().unwrap() и никогда не возвращают пустое
//!   значение из-за занятого lock. Гарды никогда не удерживаются через
//!   .await (все записи — короткие in-memory операции в scoped-блоках),
//!   поэтому блокировка потока исполнителя — микросекунды.
//! - Сигнатуры контракта местами уже, чем в Python: can_open_tomorrow_zvonki
//!   принимает только дату (без группы) — порог окончания пар взят 13:20
//!   (конец 1-й смены); has_zvonki_for_date — только дату (без группы);
//!   get_group_bells_end_time возвращает Option<String> "H:MM" вместо
//!   кортежа (час, минута); get_tomorrow_first_bell_info — Option<String>
//!   вида "1п 8:00" вместо кортежа (метка, время).
//! - process_vk_posts не рассылает сам (нет Bot-хендла): docx звонков и
//!   фото сохраняются в архив/файлы, а рассылка идёт через события,
//!   которые возвращает метод (их ест broadcast_day_updates в bot.rs).
//!   Прямая отправка docx звонков / rich-алертов / фото звонков по чатам,
//!   которую Python делал внутри process_vk_posts, в service не делается.
//! - VkPost/VkAttachment в vk.rs не несут owner_id/id вложений и
//!   copy_history: подпись поста и file_unique_id строятся из
//!   (date, hash(text), kind:url/title) вместо (edited/date, hash(text),
//!   owner_id_id вложений); репосты (copy_history) не раскрываются.
//! - parse_docx_pure в docx.rs берёт дату только из шапки таблицы
//!   (без post_dates/default_date из Python): docx без распознаваемой
//!   даты в шапке пропускается.
//! - download_photo_bytes не кэширует байты и не хранит список
//!   неудачных URL (в Python — кэш на 20 записей и 60-секундный
//!   запрет повтора); каждый вызов качает заново, невалидный payload —
//!   ошибка. get_bells_photo_input возвращает file_id, если он уже
//!   захвачен, иначе скачанные байты (как resolve_photo_input в Python).
//!
//! Добавленные поля/методы (контракт не менялся):
//! - приватные поля vk_token (VkClient не отдаёт токен, а update_check
//!   должен вернуть пустое множество без токена), files_cache
//!   (для синхронных get_file_for_date/has_file_for_date/
//!   get_active_zvonki_files — в контракте такого поля нет),
//!   processed_posts/processed_files (in-memory дедупликация Python).
//! - pub fn save_file_to_cache(...) — как save_file_to_cache в Python
//!   (только память; запись в БД — отдельным вызовом Db).
//! - pub async fn cache_bells_photo_file_id(...) — как одноимённый
//!   метод Python: запоминает file_id фото звонков в кэшах и в БД.

use crate::db::{Db, ScheduleFile};
use crate::docx::{parse_docx_pure, parse_zvonki_docx};
use crate::parser::{
    cell_has_pair_marker, extract_all_dates_from_post, extract_pair_numbers,
    find_single_pair_marker, format_day_data as parser_format_day_data, get_course_str,
    normalize_date, normalize_group, translate_latin, DayData,
};
use crate::tyumen::{fmt_date, parse_date, tyumen_datetime, tyumen_now, WEEKDAYS};
use crate::vk::{VkClient, VkPost};
use crate::{AppError, Result};
use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveTime, Timelike, Weekday};
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, RwLock};

/// Подстроки ошибок Telegram, означающие мёртвый чат.
/// Полный список из services.py:141–155 (15 подстрок).
pub const DEAD_CHAT_ERRORS: &[&str] = &[
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

/// Подпись поста для дедупликации: (edited|date, hash(text), id вложений).
/// Как _compute_post_signature (services.py:404); в Rust вместо
/// owner_id_id вложений — kind:url/title (см. шапку файла).
type PostSignature = (i64, u64, Vec<String>);

#[derive(Default)]
struct ProcessedPosts {
    map: HashMap<i64, PostSignature>,
    order: VecDeque<i64>,
}

impl ProcessedPosts {
    fn get(&self, post_id: i64) -> Option<&PostSignature> {
        self.map.get(&post_id)
    }

    fn remember(&mut self, post_id: i64, sig: PostSignature) {
        self.map.remove(&post_id);
        self.order.retain(|id| *id != post_id);
        self.map.insert(post_id, sig);
        self.order.push_back(post_id);
        while self.map.len() > 200 {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            } else {
                break;
            }
        }
    }
}

/// Штатная сетка звонков (STATIC_ZVONKI, services.py:170–340), 1-в-1.
/// Ключ дня: "monday" (Пн и Вс — в воскресенье кнопка открывает
/// понедельник), "tuesday", "wednesday", "thursday_saturday".
fn static_zvonki_text(day_key: &str, course: &str) -> Option<&'static str> {
    Some(match (day_key, course) {
        ("monday", "1 курс") => {
            "<b>Понедельник • 1 курс</b>\n\n<b>1 смена</b>\n1п: 8:00 – 9:30\nОбед: 9:30 – 10:10\n2п: 10:10 – 11:40\n3п: 11:50 – 13:20\nРазговоры о важном: 13:30 – 14:00\n\n<b>2 смена</b>\n4п: 14:10 – 15:40\nОбед: 15:40 – 16:10\n5п: 16:10 – 17:40\n6п: 17:50 – 19:20"
        }
        ("monday", "2 курс") => {
            "<b>Понедельник • 2 курс</b>\n\n<b>1 смена</b>\n1п: 8:00 – 9:30\n2п: 9:40–10:10 / 10:40–11:40 (Обед 10:10 – 10:40)\n3п: 11:50 – 13:20\n\n<b>2 смена</b>\n4п: 14:10 – 15:40\nОбед: 15:40 – 16:10\n5п: 16:10 – 17:40\n6п: 17:50 – 19:20"
        }
        ("monday", "3,4 курс") => {
            "<b>Понедельник • 3,4 курс</b>\n\n<b>1 смена</b>\n1п: 8:00 – 9:30\n2п: 9:40 – 11:10 (Обед 11:10 – 11:50)\n3п: 11:50 – 13:20\n\n<b>2 смена</b>\n4п: 14:10 – 15:40\nОбед: 15:40 – 16:10\n5п: 16:10 – 17:40\n6п: 17:50 – 19:20"
        }
        ("tuesday", "1 курс") => {
            "<b>Вторник • 1 курс</b>\n\n<b>1 смена</b>\n1п: 8:00 – 9:30\nОбед: 9:30 – 10:10\n2п: 10:10 – 11:40\n3п: 11:50 – 13:20\n\n<b>2 смена</b>\nКлассный час: 13:30 – 14:00\n4п: 14:10 – 15:40\nОбед: 15:40 – 16:10\n5п: 16:10 – 17:40\n6п: 17:50 – 19:20"
        }
        ("tuesday", "2 курс") => {
            "<b>Вторник • 2 курс</b>\n\n<b>1 смена</b>\n1п: 8:00 – 9:30\n2п: 9:40–10:10 / 10:40–11:40 (Обед 10:10 – 10:40)\n3п: 11:50 – 13:20\nКлассный час: 13:30 – 14:00\n\n<b>2 смена</b>\nКлассный час: 13:30 – 14:00\n4п: 14:10 – 15:40\nОбед: 15:40 – 16:10\n5п: 16:10 – 17:40\n6п: 17:50 – 19:20"
        }
        ("tuesday", "3,4 курс") => {
            "<b>Вторник • 3,4 курс</b>\n\n<b>1 смена</b>\n1п: 8:00 – 9:30\n2п: 9:40 – 11:10 (Обед 11:10 – 11:50)\n3п: 11:50 – 13:20\nКлассный час: 13:30 – 14:00\n\n<b>2 смена</b>\nКлассный час: 13:30 – 14:00\n4п: 14:10 – 15:40\nОбед: 15:40 – 16:10\n5п: 16:10 – 17:40\n6п: 17:50 – 19:20"
        }
        ("wednesday", "1 курс") => {
            "<b>Среда • 1 курс</b>\n\n<b>1 смена</b>\n1п: 8:00 – 9:30\nОбед: 9:30 – 10:10\n2п: 10:10 – 11:40\n3п: 11:50 – 13:20\nКлассный час: 13:30 – 14:00\n\n<b>2 смена</b>\n4п: 14:10 – 15:40\nОбед: 15:40 – 16:10\n5п: 16:10 – 17:40\n6п: 17:50 – 19:20"
        }
        ("wednesday", "2 курс") => {
            "<b>Среда • 2 курс</b>\n\n<b>1 смена</b>\n1п: 8:00 – 9:30\n2п: 9:40–10:10 / 10:40–11:40 (Обед 10:10 – 10:40)\n3п: 11:50 – 13:20\n\n<b>2 смена</b>\n4п: 14:10 – 15:40\nОбед: 15:40 – 16:10\n5п: 16:10 – 17:40\n6п: 17:50 – 19:20"
        }
        ("wednesday", "3,4 курс") => {
            "<b>Среда • 3,4 курс</b>\n\n<b>1 смена</b>\n1п: 8:00 – 9:30\n2п: 9:40 – 11:10 (Обед 11:10 – 11:40)\n3п: 11:40 – 13:10\n\n<b>2 смена</b>\n4п: 14:10 – 15:40\nОбед: 15:40 – 16:10\n5п: 16:10 – 17:40\n6п: 17:50 – 19:20"
        }
        ("thursday_saturday", "1 курс") => {
            "<b>Четверг–Суббота • 1 курс</b>\n\n<b>1 смена</b>\n1п: 8:00 – 9:30\nОбед: 9:30 – 10:10\n2п: 10:10 – 11:40\n3п: 11:50 – 13:20\n\n<b>2 смена</b>\n4п: 13:30 – 15:00\nОбед: 15:00 – 15:30\n5п: 15:30 – 17:00\n6п: 17:10 – 18:40"
        }
        ("thursday_saturday", "2 курс") => {
            "<b>Четверг–Суббота • 2 курс</b>\n\n<b>1 смена</b>\n1п: 8:00 – 9:30\n2п: 9:40–10:10 / 10:40–11:40 (Обед 10:10 – 10:40)\n3п: 11:50 – 13:20\n\n<b>2 смена</b>\n4п: 13:30 – 15:00\nОбед: 15:00 – 15:30\n5п: 15:30 – 17:00\n6п: 17:10 – 18:40"
        }
        ("thursday_saturday", "3,4 курс") => {
            "<b>Четверг–Суббота • 3,4 курс</b>\n\n<b>1 смена</b>\n1п: 8:00 – 9:30\n2п: 9:40 – 11:10 (Обед 11:10 – 11:40)\n3п: 11:40 – 13:10\n\n<b>2 смена</b>\n4п: 13:30 – 15:00\nОбед: 15:00 – 15:30\n5п: 15:30 – 17:00\n6п: 17:10 – 18:40"
        }
        _ => return None,
    })
}

/// Ключ дня STATIC_ZVONKI по дню недели (services.py get_zvonki_text).
fn static_day_key(weekday: Weekday) -> &'static str {
    match weekday {
        Weekday::Mon | Weekday::Sun => "monday",
        Weekday::Tue => "tuesday",
        Weekday::Wed => "wednesday",
        _ => "thursday_saturday",
    }
}

pub struct ScheduleService {
    pub db: Arc<Db>,
    pub vk: VkClient,
    /// date_str -> group -> Vec<(content, type)>. Заполняется load_ram().
    pub archive_cache: RwLock<HashMap<String, HashMap<String, Vec<(String, String)>>>>,
    /// date_str -> {курс: строки звонков} из docx.
    pub zvonki_cache: RwLock<HashMap<String, BTreeMap<String, Vec<String>>>>,
    /// date_str -> url/id фото звонков.
    pub bells_photo_cache: RwLock<HashMap<String, String>>,
    /// Токен VK (VkClient его не отдаёт; update_check без токена
    /// возвращает пустое множество, как в Python).
    vk_token: String,
    /// date_str -> файлы (как files_cache в Python). В контракте этого
    /// поля нет, но без него синхронные get_file_for_date /
    /// has_file_for_date / get_active_zvonki_files не реализовать.
    files_cache: RwLock<HashMap<String, Vec<ScheduleFile>>>,
    /// Дедупликация постов (processed_posts в Python, cap 200).
    processed_posts: Mutex<ProcessedPosts>,
    /// Обработанные вложения (processed_files в Python).
    processed_files: Mutex<HashSet<String>>,
}

// ---------------------------------------------------------------------------
// Локальные сканеры/помощники (зеркало приватных regex Python без lookaround)
// ---------------------------------------------------------------------------

fn lower_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn is_cyr(c: char) -> bool {
    ('\u{0400}'..='\u{04FF}').contains(&c)
}

fn is_alpha_cyr_lat(c: char) -> bool {
    is_cyr(c) || c.is_ascii_alphabetic()
}

/// _norm_archive_content (services.py:2408).
fn norm_archive_content(text: &str) -> String {
    text.replace("🕒 ", "").replace('🕒', "").trim().to_string()
}

/// RE_BASE_FILE_UID: отрезать суффикс _ДД.ММ.ГГГГ.
fn base_file_uid(uid: &str) -> Option<String> {
    let b = uid.as_bytes();
    if b.len() < 12 || b[b.len() - 11] != b'_' {
        return None;
    }
    let tail = &uid[uid.len() - 10..];
    let t: Vec<char> = tail.chars().collect();
    if t.len() == 10
        && t[0].is_ascii_digit()
        && t[1].is_ascii_digit()
        && t[2] == '.'
        && t[3].is_ascii_digit()
        && t[4].is_ascii_digit()
        && t[5] == '.'
        && t[6].is_ascii_digit()
        && t[7].is_ascii_digit()
        && t[8].is_ascii_digit()
        && t[9].is_ascii_digit()
    {
        Some(uid[..uid.len() - 11].to_string())
    } else {
        None
    }
}

/// JSON звонков из архива (__ZVONKI__) -> {курс: строки}.
fn parse_zvonki_json(content: &str) -> Option<BTreeMap<String, Vec<String>>> {
    let v: serde_json::Value = serde_json::from_str(content).ok()?;
    let obj = v.as_object()?;
    if obj.is_empty() {
        return None;
    }
    let mut out = BTreeMap::new();
    for (k, val) in obj {
        let lines: Vec<String> = val
            .as_array()?
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
        out.insert(k.clone(), lines);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Выбор строк звонков по курсу — как в get_group_bells_end_time /
/// get_tomorrow_first_bell_info / _bells_lines (api.rs).
fn zvonki_lines_for_course(
    parsed: &BTreeMap<String, Vec<String>>,
    course: &str,
) -> Vec<String> {
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
    if let Some(lines) = parsed.get(course) {
        return lines.clone();
    }
    parsed.get("2 курс").cloned().unwrap_or_default()
}

/// Строка изменения — отмена пары? `\b(?:нет|отмен\w*|снят\w*)\b`, IGNORECASE.
fn is_cancel_change(text: &str) -> bool {
    let lower: String = text.chars().map(lower_char).collect();
    for word in lower.split(|c: char| !c.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        if word == "нет" || word.starts_with("отмен") || word.starts_with("снят") {
            return true;
        }
    }
    false
}

/// Убрать группы (...) без вложенности (для поиска времён в звонках).
fn remove_paren_groups(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                if depth > 0 {
                    depth -= 1;
                }
            }
            _ => {
                if depth == 0 {
                    out.push(c);
                }
            }
        }
    }
    out
}

/// Все "H:MM" в строке (как re.findall(r"(\d{1,2}):(\d{2})", s)).
fn find_hhmm(s: &str) -> Vec<(i32, i32)> {
    let ch: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i].is_ascii_digit() {
            let mut j = i;
            while j < ch.len() && ch[j].is_ascii_digit() && j - i < 2 {
                j += 1;
            }
            if j < ch.len() && ch[j] == ':' {
                let h: i32 = ch[i..j].iter().collect::<String>().parse().unwrap_or(-1);
                let mut k = j + 1;
                while k < ch.len() && ch[k].is_ascii_digit() && k - (j + 1) < 2 {
                    k += 1;
                }
                if k - (j + 1) == 2 {
                    let m: i32 = ch[j + 1..k].iter().collect::<String>().parse().unwrap_or(-1);
                    if (0..=23).contains(&h) && (0..=59).contains(&m) {
                        out.push((h, m));
                    }
                    i = k;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

/// Номер пары из "Nп:" (re.search(r"(\d+)\s*п:", line)).
fn pair_number_colon(line: &str) -> Option<i32> {
    let ch: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i].is_ascii_digit() {
            let mut j = i;
            while j < ch.len() && ch[j].is_ascii_digit() {
                j += 1;
            }
            let mut k = j;
            while k < ch.len() && ch[k].is_whitespace() {
                k += 1;
            }
            if k < ch.len() && ch[k] == 'п' && k + 1 < ch.len() && ch[k + 1] == ':' {
                return ch[i..j].iter().collect::<String>().parse().ok();
            }
        }
        i += 1;
    }
    None
}

/// Время начала пары `target` ("1") в строке "1п: 8:00 ..." с
/// lookbehind (?<!\d). Возвращает "8:00".
fn pair_start_time(line: &str, target: &str) -> Option<String> {
    let ch: Vec<char> = line.chars().collect();
    let t: Vec<char> = target.chars().collect();
    if t.is_empty() || ch.len() < t.len() {
        return None;
    }
    for i in 0..=ch.len() - t.len() {
        if ch[i..i + t.len()] != t[..] {
            continue;
        }
        if i > 0 && ch[i - 1].is_ascii_digit() {
            continue;
        }
        let mut p = i + t.len();
        while p < ch.len() && ch[p].is_whitespace() {
            p += 1;
        }
        if p >= ch.len() || ch[p] != 'п' {
            continue;
        }
        p += 1;
        if p >= ch.len() || ch[p] != ':' {
            continue;
        }
        p += 1;
        while p < ch.len() && ch[p].is_whitespace() {
            p += 1;
        }
        let rest: String = ch[p..].iter().collect();
        let times = find_hhmm(&rest);
        if let Some((h, m)) = times.first() {
            return Some(format!("{h}:{m:02}"));
        }
        return None;
    }
    None
}

/// Первая пара и её время в строке: (номер, "8:00").
fn first_pair_start(line: &str) -> Option<(String, String)> {
    let ch: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i].is_ascii_digit() {
            let mut j = i;
            while j < ch.len() && ch[j].is_ascii_digit() {
                j += 1;
            }
            let num: String = ch[i..j].iter().collect();
            let mut k = j;
            while k < ch.len() && ch[k].is_whitespace() {
                k += 1;
            }
            if k < ch.len() && ch[k] == 'п' && k + 1 < ch.len() && ch[k + 1] == ':' {
                let rest: String = ch[k + 2..].iter().collect();
                if let Some((h, m)) = find_hhmm(&rest).first() {
                    return Some((num, format!("{h}:{m:02}")));
                }
                return None;
            }
        }
        i += 1;
    }
    None
}

/// _is_valid_image_payload (services.py:521) без content-type:
/// VkClient.download_bytes не отдаёт заголовки, поэтому ветка
/// content_type.startswith("image/") недоступна.
fn is_valid_image_payload(data: &[u8]) -> bool {
    if data.is_empty() || data.len() < 128 || data.len() > 10 * 1024 * 1024 {
        return false;
    }
    let head: Vec<u8> = data
        .iter()
        .take(32)
        .skip_while(|b| b.is_ascii_whitespace())
        .cloned()
        .collect();
    let head_lower: Vec<u8> = head.iter().map(|b| b.to_ascii_lowercase()).collect();
    if head_lower.starts_with(b"<html")
        || head_lower.starts_with(b"<!doctype")
        || head_lower.starts_with(b"<?xml")
        || head_lower.starts_with(b"{")
    {
        return false;
    }
    data.starts_with(b"\xff\xd8\xff")
        || data.starts_with(b"\x89PNG\r\n\x1a\n")
        || data.starts_with(b"GIF8")
        || (data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP")
}

// ---------------------------------------------------------------------------
// Rich Messages: хелперы (зеркало regex Python без lookbehind/lookahead)
// ---------------------------------------------------------------------------

/// HTML-escape (как html.escape в Python).
fn esc(s: &str) -> String {
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

/// Подсветка маркеров пар в уже экранированном тексте — как
/// `RE_PAIR_LIST_MARKER.sub(r"<mark><b>\g<0></b></mark>", text)` (services.py:102).
/// Маркер: список номеров 1–8 через `,`/`–`/`—`/`−`/`-`/`и`, затем `п.`/`п`/`пар…`.
pub fn highlight_pair_list_marker(escaped: &str) -> String {
    let ch: Vec<char> = escaped.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < ch.len() {
        // Lookbehind (?<![\d.-]): перед маркером — не цифра, точка, дефис.
        let lookbehind_ok = i == 0
            || !(ch[i - 1].is_ascii_digit() || ch[i - 1] == '.' || ch[i - 1] == '-');
        if lookbehind_ok {
            if let Some(end) = pair_list_marker_end(&ch, i) {
                out.push_str("<mark><b>");
                out.extend(ch[i..end].iter());
                out.push_str("</b></mark>");
                i = end;
                continue;
            }
        }
        out.push(ch[i]);
        i += 1;
    }
    out
}

/// Конец маркера пар, начинающегося в `start` (цифра 1–8), или None.
fn pair_list_marker_end(ch: &[char], start: usize) -> Option<usize> {
    if start >= ch.len() || !('1'..='8').contains(&ch[start]) {
        return None;
    }
    // Список номеров: цифра, затем (разделитель + цифра)*.
    let mut j = start + 1;
    loop {
        let mut p = j;
        while p < ch.len() && ch[p].is_whitespace() {
            p += 1;
        }
        let is_sep = p < ch.len()
            && (ch[p] == ','
                || ch[p] == '–'
                || ch[p] == '—'
                || ch[p] == '−'
                || ch[p] == '-'
                || ch[p] == 'и'
                || ch[p] == 'И');
        if !is_sep {
            break;
        }
        p += 1;
        while p < ch.len() && ch[p].is_whitespace() {
            p += 1;
        }
        if p < ch.len() && ('1'..='8').contains(&ch[p]) {
            j = p + 1;
        } else {
            break;
        }
    }
    // После списка — пробелы и слово пары.
    let mut p = j;
    while p < ch.len() && ch[p].is_whitespace() {
        p += 1;
    }
    if p >= ch.len() {
        return None;
    }
    let c = ch[p];
    if c != 'п' && c != 'П' && c != 'p' && c != 'P' {
        return None;
    }
    // Вариант «пар…»: п + а + р (+ а/ы/у/е), дальше — не буква.
    if p + 2 < ch.len()
        && (ch[p + 1] == 'а' || ch[p + 1] == 'А')
        && (ch[p + 2] == 'р' || ch[p + 2] == 'Р')
    {
        let mut end = p + 3;
        if end < ch.len()
            && (ch[end] == 'а'
                || ch[end] == 'А'
                || ch[end] == 'ы'
                || ch[end] == 'Ы'
                || ch[end] == 'у'
                || ch[end] == 'У'
                || ch[end] == 'е'
                || ch[end] == 'Е')
        {
            end += 1;
        }
        if end >= ch.len() || !is_alpha_cyr_lat(ch[end]) {
            return Some(end);
        }
        return None;
    }
    // Вариант «п.»: дальше — не буква.
    if p + 1 < ch.len() && ch[p + 1] == '.' {
        if p + 2 >= ch.len() || !is_alpha_cyr_lat(ch[p + 2]) {
            return Some(p + 2);
        }
        return None;
    }
    // Вариант «п»: дальше — не буква и не точка.
    if p + 1 >= ch.len() || (!is_alpha_cyr_lat(ch[p + 1]) && ch[p + 1] != '.') {
        return Some(p + 1);
    }
    None
}

/// "H:MM" в позиции p (H — 1–2 цифры, MM — ровно 2) → (ч, м, конец).
fn parse_hhmm(ch: &[char], p: usize) -> Option<(i32, i32, usize)> {
    let mut j = p;
    while j < ch.len() && ch[j].is_ascii_digit() && j - p < 2 {
        j += 1;
    }
    if j == p || j >= ch.len() || ch[j] != ':' {
        return None;
    }
    let h: i32 = ch[p..j].iter().collect::<String>().parse().ok()?;
    let mut k = j + 1;
    while k < ch.len() && ch[k].is_ascii_digit() && k - (j + 1) < 2 {
        k += 1;
    }
    if k - (j + 1) != 2 {
        return None;
    }
    let m: i32 = ch[j + 1..k].iter().collect::<String>().parse().ok()?;
    Some((h, m, k))
}

/// re.findall(r"(\d{1,2}):(\d{2})", s) — все пары «ч:мм» без проверки диапазона.
fn find_hhmm_candidates(s: &str) -> Vec<(i32, i32)> {
    let ch: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i].is_ascii_digit() {
            if let Some((h, m, end)) = parse_hhmm(&ch, i) {
                out.push((h, m));
                i = end;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// <tg-time unix="..." format="r"> — как _make_tg_time (services.py:1998).
/// base_date по умолчанию: завтра при воскресенье, иначе сегодня (now).
/// Пара ещё не началась — отсчёт до её начала; идёт сейчас — до звонка.
pub fn make_tg_time(
    time_str: &str,
    base_date: Option<NaiveDate>,
    now: DateTime<FixedOffset>,
) -> String {
    let times = find_hhmm_candidates(time_str);
    if times.is_empty() {
        return "—".to_string();
    }
    let base = base_date.unwrap_or_else(|| {
        if now.weekday() == Weekday::Sun {
            now.date_naive() + chrono::Duration::days(1)
        } else {
            now.date_naive()
        }
    });
    let (h1, m1) = times[0];
    if !(0..=23).contains(&h1) || !(0..=59).contains(&m1) {
        return "—".to_string();
    }
    let dt_start = tyumen_datetime(
        base,
        NaiveTime::from_hms_opt(h1 as u32, m1 as u32, 0).unwrap(),
    );
    if times.len() >= 2 {
        let (h2, m2) = *times.last().unwrap();
        if (0..=23).contains(&h2) && (0..=59).contains(&m2) {
            let dt_end = tyumen_datetime(
                base,
                NaiveTime::from_hms_opt(h2 as u32, m2 as u32, 0).unwrap(),
            );
            if dt_start <= now && now < dt_end {
                return format!(
                    "<tg-time unix=\"{}\" format=\"r\">до {h2}:{m2:02}</tg-time>",
                    dt_end.timestamp()
                );
            }
        }
    }
    format!(
        "<tg-time unix=\"{}\" format=\"r\">{h1}:{m1:02}</tg-time>",
        dt_start.timestamp()
    )
}

/// Как re.sub(r"</?(?:table|tr|th|td|details|summary|h3)[^>]*>", " ", s)
/// + схлопывание пробелов/табов (get_zvonki_resp, services.py:3095).
pub fn strip_rich_table_tags(s: &str) -> String {
    const TAGS: [&str; 7] = ["table", "tr", "th", "td", "details", "summary", "h3"];
    let ch: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i] == '<' {
            let mut j = i + 1;
            if j < ch.len() && ch[j] == '/' {
                j += 1;
            }
            let name_start = j;
            while j < ch.len() && ch[j].is_ascii_alphanumeric() {
                j += 1;
            }
            let name: String = ch[name_start..j].iter().collect::<String>().to_lowercase();
            if TAGS.iter().any(|t| name.starts_with(t)) {
                // Пропустить до '>'; без '>' — тег не считается (как в regex).
                let mut k = j;
                while k < ch.len() && ch[k] != '>' {
                    k += 1;
                }
                if k < ch.len() {
                    out.push(' ');
                    i = k + 1;
                    continue;
                }
            }
        }
        out.push(ch[i]);
        i += 1;
    }
    // re.sub(r"[ \t]+", " ", s).strip()
    let mut collapsed = String::with_capacity(out.len());
    let mut last_space = false;
    for c in out.chars() {
        if c == ' ' || c == '\t' {
            if !last_space {
                collapsed.push(' ');
            }
            last_space = true;
        } else {
            collapsed.push(c);
            last_space = false;
        }
    }
    collapsed.trim().to_string()
}

/// re.search(r"(\d{1,2}:\d{2}\s*[-–—]\s*\d{1,2}:\d{2})", s) — диапазон времён.
fn extract_time_range(s: &str) -> Option<String> {
    let ch: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i].is_ascii_digit() {
            if let Some((_, _, end1)) = parse_hhmm(&ch, i) {
                let mut p = end1;
                while p < ch.len() && ch[p].is_whitespace() {
                    p += 1;
                }
                if p < ch.len() && (ch[p] == '-' || ch[p] == '–' || ch[p] == '—') {
                    p += 1;
                    while p < ch.len() && ch[p].is_whitespace() {
                        p += 1;
                    }
                    if let Some((_, _, end2)) = parse_hhmm(&ch, p) {
                        return Some(ch[i..end2].iter().collect());
                    }
                }
                i = end1;
                continue;
            }
        }
        i += 1;
    }
    None
}

/// re.search(r"(\d+)\s*п:\s*(\d{1,2}):(\d{2})\s*[-–—]\s*(\d{1,2}):(\d{2})", s)
/// → (номер пары, ч1, м1, ч2, м2).
fn parse_pair_time_range(s: &str) -> Option<(i32, i32, i32, i32, i32)> {
    let ch: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i].is_ascii_digit() {
            let mut j = i;
            while j < ch.len() && ch[j].is_ascii_digit() {
                j += 1;
            }
            let num: i32 = ch[i..j].iter().collect::<String>().parse().ok()?;
            let mut p = j;
            while p < ch.len() && ch[p].is_whitespace() {
                p += 1;
            }
            if p < ch.len() && ch[p] == 'п' && p + 1 < ch.len() && ch[p + 1] == ':' {
                p += 2;
                while p < ch.len() && ch[p].is_whitespace() {
                    p += 1;
                }
                if let Some((h1, m1, after1)) = parse_hhmm(&ch, p) {
                    let mut q = after1;
                    while q < ch.len() && ch[q].is_whitespace() {
                        q += 1;
                    }
                    if q < ch.len() && (ch[q] == '-' || ch[q] == '–' || ch[q] == '—') {
                        q += 1;
                        while q < ch.len() && ch[q].is_whitespace() {
                            q += 1;
                        }
                        if let Some((h2, m2, _)) = parse_hhmm(&ch, q) {
                            return Some((num, h1, m1, h2, m2));
                        }
                    }
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

/// re.match(r"^(\d+)п:", s) — номер пары в начале строки.
fn pair_number_colon_start(line: &str) -> Option<i32> {
    let ch: Vec<char> = line.chars().collect();
    let mut j = 0usize;
    while j < ch.len() && ch[j].is_ascii_digit() {
        j += 1;
    }
    if j == 0 {
        return None;
    }
    let mut p = j;
    while p < ch.len() && ch[p].is_whitespace() {
        p += 1;
    }
    if p < ch.len() && ch[p] == 'п' && p + 1 < ch.len() && ch[p + 1] == ':' {
        ch[..j].iter().collect::<String>().parse().ok()
    } else {
        None
    }
}

/// _is_special_line (services.py:2066): строка «Разговоры» / «Классный час».
fn is_special_zvonki_line(line: &str) -> bool {
    let stripped = line.trim_start_matches(|c| c == '✨' || c == ' ');
    let lower = stripped.to_lowercase();
    lower.starts_with("разговоры") || lower.starts_with("классный час")
}

/// re.search(r"(\d+)\s*п:\s*([0-9:–—\s/]+)(?:\((.*?)\))?", s):
/// (номер пары, время, примечание) для штатных звонков.
fn parse_zvonki_pair_row(line: &str) -> Option<(String, String, String)> {
    let ch: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    while i < ch.len() {
        if !ch[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < ch.len() && ch[j].is_ascii_digit() {
            j += 1;
        }
        let num: String = ch[i..j].iter().collect();
        let mut p = j;
        while p < ch.len() && ch[p].is_whitespace() {
            p += 1;
        }
        if p < ch.len() && ch[p] == 'п' && p + 1 < ch.len() && ch[p + 1] == ':' {
            let mut q = p + 2;
            while q < ch.len() && ch[q].is_whitespace() {
                q += 1;
            }
            let t_start = q;
            while q < ch.len()
                && (ch[q].is_ascii_digit()
                    || ch[q] == ':'
                    || ch[q] == '–'
                    || ch[q] == '—'
                    || ch[q].is_whitespace()
                    || ch[q] == '/')
            {
                q += 1;
            }
            let raw_time: String = ch[t_start..q].iter().collect::<String>().trim().to_string();
            if !raw_time.is_empty() {
                let mut r = q;
                while r < ch.len() && ch[r].is_whitespace() {
                    r += 1;
                }
                if r == ch.len() {
                    return Some((num, raw_time, String::new()));
                }
                if ch[r] == '(' && ch.last() == Some(&')') {
                    let inner: String = ch[r + 1..ch.len() - 1].iter().collect();
                    return Some((num, raw_time, inner.trim().to_string()));
                }
                // Остаток не подходит — как неудача re.search в этой позиции.
            }
        }
        i = j;
    }
    None
}

/// re.match(r"^(\d+)п:\s*([^(]+?)(?:\((.*?)\))?$", s):
/// (номер, время, примечание) для распарсенных звонков.
fn parse_rich_pair_row(line: &str) -> Option<(String, String, String)> {
    let ch: Vec<char> = line.chars().collect();
    let mut i = 0usize;
    while i < ch.len() && ch[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let num: String = ch[..i].iter().collect();
    let mut p = i;
    if p >= ch.len() || ch[p] != 'п' {
        return None;
    }
    p += 1;
    if p >= ch.len() || ch[p] != ':' {
        return None;
    }
    p += 1;
    while p < ch.len() && ch[p].is_whitespace() {
        p += 1;
    }
    let rest: String = ch[p..].iter().collect();
    if rest.trim().is_empty() {
        return None;
    }
    match rest.find('(') {
        Some(open) => {
            if rest.ends_with(')') {
                let time = rest[..open].trim().to_string();
                let inner = rest[open + 1..rest.len() - 1].trim().to_string();
                if time.is_empty() {
                    return None;
                }
                Some((num, time, inner))
            } else {
                None
            }
        }
        None => Some((num, rest.trim().to_string(), String::new())),
    }
}

/// Одна таблица звонков (Пара/Время/Отсчёт) — build_table в
/// format_parsed_zvonki_rich (services.py:2072).
fn build_zvonki_table(
    lines: &[String],
    target_date: NaiveDate,
    now: DateTime<FixedOffset>,
) -> String {
    let mut t_lines: Vec<String> = vec![
        "<table bordered striped compact>".to_string(),
        "  <tr>".to_string(),
        "    <th align=\"center\">Пара</th>".to_string(),
        "    <th align=\"center\">Время</th>".to_string(),
        "    <th align=\"center\">Отсчёт</th>".to_string(),
        "  </tr>".to_string(),
    ];
    let mut lunch_notes: Vec<String> = Vec::new();
    for l in lines {
        let l_clean = l.trim();
        if let Some((p_num, raw_time, p_note)) = parse_rich_pair_row(l_clean) {
            let tg_timer = make_tg_time(&raw_time, Some(target_date), now);
            if p_note.to_lowercase().contains("обед") {
                lunch_notes.push(esc(&p_note));
            }
            t_lines.push(format!(
                "  <tr><td align=\"center\"><b>{}п</b></td><td align=\"center\">{}</td><td align=\"center\">{}</td></tr>",
                esc(&p_num),
                esc(&raw_time),
                tg_timer
            ));
        } else if l_clean.starts_with('🍽') || l_clean.to_lowercase().starts_with("обед") {
            let raw_t = match l_clean.split_once(':') {
                Some((_, rest)) => rest.trim().to_string(),
                None => l_clean.to_string(),
            };
            let tg_timer = make_tg_time(&raw_t, Some(target_date), now);
            t_lines.push(format!(
                "  <tr><td align=\"center\">Обед</td><td align=\"center\">{}</td><td align=\"center\">{}</td></tr>",
                esc(&raw_t),
                tg_timer
            ));
        } else if l_clean.starts_with('✨') || is_special_zvonki_line(l_clean) {
            let without_star = l_clean.replace('✨', "");
            let parts: Vec<&str> = without_star.splitn(2, ':').collect();
            let lbl = esc(parts[0].trim());
            let raw_t = if parts.len() > 1 {
                parts[1].trim().to_string()
            } else {
                String::new()
            };
            let tg_timer = make_tg_time(&raw_t, Some(target_date), now);
            t_lines.push(format!(
                "  <tr><td align=\"center\">{lbl}</td><td align=\"center\">{}</td><td align=\"center\">{}</td></tr>",
                esc(&raw_t),
                tg_timer
            ));
        }
    }
    t_lines.push("</table>".to_string());
    if !lunch_notes.is_empty() {
        t_lines.push(format!("<i>{}</i>", lunch_notes.join("; ")));
    }
    t_lines.join("\n")
}

/// Разбивка строк звонков по сменам — build_shift_sections
/// (services.py:2113). 1-я смена раскрыта при group_shift==1, 2-я — при 2.
fn build_zvonki_shift_sections(
    lines: &[String],
    group_shift: i32,
    target_date: NaiveDate,
    now: DateTime<FixedOffset>,
) -> String {
    let mut shift1: Vec<String> = Vec::new();
    let mut shift2: Vec<String> = Vec::new();
    let mut in_shift2 = false;
    for (idx_l, l) in lines.iter().enumerate() {
        if let Some(n) = pair_number_colon_start(l.trim()) {
            if n >= 4 {
                in_shift2 = true;
            }
        } else if !in_shift2
            && is_special_zvonki_line(l)
            && idx_l + 1 < lines.len()
            && pair_number_colon_start(lines[idx_l + 1].trim())
                .map(|n| (4..=8).contains(&n))
                .unwrap_or(false)
            && lines[..idx_l]
                .iter()
                .any(|prev| pair_number_colon_start(prev.trim()) == Some(3))
        {
            if lines[..=idx_l]
                .iter()
                .filter(|prev| is_special_zvonki_line(prev))
                .count()
                >= 2
            {
                in_shift2 = true;
            }
        }
        if in_shift2 {
            shift2.push(l.clone());
        } else {
            shift1.push(l.clone());
        }
    }
    if !shift1.is_empty() && !shift2.is_empty() {
        let t1 = build_zvonki_table(&shift1, target_date, now);
        let t2 = build_zvonki_table(&shift2, target_date, now);
        let open1 = if group_shift == 1 { " open" } else { "" };
        let open2 = if group_shift == 2 { " open" } else { "" };
        format!(
            "<details{open1}>\n<summary><b>1 смена</b></summary>\n\n{t1}\n</details>\n\n<details{open2}>\n<summary><b>2 смена</b></summary>\n\n{t2}\n</details>"
        )
    } else {
        build_zvonki_table(lines, target_date, now)
    }
}

/// render_compact_section_table (services.py:3110): секция штатных звонков
/// как компактная Rich-таблица (Пара/Время/Отсчёт).
fn render_compact_section_table(
    section_title: &str,
    lines: &[String],
    wrap_details: bool,
    is_open: bool,
    base_date: NaiveDate,
    now: DateTime<FixedOffset>,
) -> String {
    let mut table_lines: Vec<String> = vec![
        "<table bordered striped compact>".to_string(),
        "  <tr>".to_string(),
        "    <th align=\"center\">Пара</th>".to_string(),
        "    <th align=\"center\">Время</th>".to_string(),
        "    <th align=\"center\">Отсчёт</th>".to_string(),
        "  </tr>".to_string(),
    ];
    let mut has_rows = false;
    let mut lunch_note = String::new();
    for line in lines {
        let line_clean = line.trim();
        if line_clean.is_empty() {
            continue;
        }
        if let Some((p_num, raw_time, p_note)) = parse_zvonki_pair_row(line_clean) {
            has_rows = true;
            let tg_timer = make_tg_time(&raw_time, Some(base_date), now);
            if p_note.to_lowercase().contains("обед") {
                lunch_note = esc(&p_note);
            }
            table_lines.push(format!(
                "  <tr><td align=\"center\"><b>{}п</b></td><td align=\"center\">{}</td><td align=\"center\">{}</td></tr>",
                esc(&p_num),
                esc(&raw_time),
                tg_timer
            ));
        } else if line_clean.contains("Разговоры") || line_clean.contains("Классный час") {
            has_rows = true;
            let raw_t = extract_time_range(line_clean).unwrap_or_default();
            let tg_timer = make_tg_time(&raw_t, Some(base_date), now);
            let lbl = if line_clean.contains("Разговоры") {
                "Разговоры"
            } else {
                "Кл. час"
            };
            table_lines.push(format!(
                "  <tr><td align=\"center\">{lbl}</td><td align=\"center\">{}</td><td align=\"center\">{}</td></tr>",
                esc(&raw_t),
                tg_timer
            ));
        } else if line_clean.contains("Обед") {
            has_rows = true;
            let raw_t = extract_time_range(line_clean).unwrap_or_default();
            let tg_timer = make_tg_time(&raw_t, Some(base_date), now);
            table_lines.push(format!(
                "  <tr><td align=\"center\">Обед</td><td align=\"center\">{}</td><td align=\"center\">{}</td></tr>",
                esc(&raw_t),
                tg_timer
            ));
        }
    }
    table_lines.push("</table>".to_string());
    if !lunch_note.is_empty() {
        table_lines.push(format!("<i>{lunch_note}</i>"));
    }
    if !has_rows {
        return String::new();
    }
    let content = table_lines.join("\n");
    if wrap_details && !section_title.is_empty() {
        let clean_st = section_title
            .replace("<b>", "")
            .replace("</b>", "")
            .trim()
            .to_string();
        let open_attr = if is_open { " open" } else { "" };
        format!(
            "<details{open_attr}>\n<summary><b>{}</b></summary>\n\n{content}\n</details>",
            esc(&clean_st)
        )
    } else {
        let st = if section_title.is_empty() {
            String::new()
        } else {
            format!("<b>{section_title}</b>\n")
        };
        format!("{st}{content}")
    }
}

// --- RE_MDK_CODE (локальная копия: в parser.rs сканер приватный) --------

const MDK_KEYWORDS: &[&str] = &["огсэ", "мдк", "оп", "уп", "пп", "пм", "ен", "од", "фк"];

/// Убрать коды МДК/ОП/... вида "МДК.01.02" (RE_MDK_CODE).
fn remove_mdk_codes_local(text: &str) -> String {
    let ch: Vec<char> = text.chars().collect();
    let low: Vec<char> = ch.iter().map(|&c| lower_char(c)).collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < ch.len() {
        let mut matched_end: Option<usize> = None;
        let at_boundary = i == 0 || !(ch[i - 1].is_alphanumeric() || ch[i - 1] == '_');
        if at_boundary {
            for kw in MDK_KEYWORDS {
                let kwc: Vec<char> = kw.chars().collect();
                if i + kwc.len() > low.len() || low[i..i + kwc.len()] != kwc[..] {
                    continue;
                }
                let mut p = i + kwc.len();
                while p < low.len() && low[p].is_whitespace() {
                    p += 1;
                }
                if p < low.len() && low[p] == '.' {
                    p += 1;
                }
                while p < low.len() && low[p].is_whitespace() {
                    p += 1;
                }
                let d1 = p;
                while p < low.len() && low[p].is_ascii_digit() && p - d1 < 2 {
                    p += 1;
                }
                if p == d1 {
                    continue;
                }
                while p < low.len() && low[p].is_whitespace() {
                    p += 1;
                }
                if p >= low.len() || low[p] != '.' {
                    continue;
                }
                p += 1;
                while p < low.len() && low[p].is_whitespace() {
                    p += 1;
                }
                let d2 = p;
                while p < low.len() && low[p].is_ascii_digit() && p - d2 < 2 {
                    p += 1;
                }
                if p == d2 {
                    continue;
                }
                let mut q = p;
                while q < low.len() && low[q].is_whitespace() {
                    q += 1;
                }
                if q < low.len() && low[q] == '.' {
                    let mut r = q + 1;
                    while r < low.len() && low[r].is_whitespace() {
                        r += 1;
                    }
                    let d3 = r;
                    while r < low.len() && low[r].is_ascii_digit() && r - d3 < 2 {
                        r += 1;
                    }
                    if r > d3 {
                        p = r;
                    }
                }
                matched_end = Some(p);
                break;
            }
        }
        if let Some(end) = matched_end {
            out.push(' ');
            i = end;
        } else {
            out.push(ch[i]);
            i += 1;
        }
    }
    out
}

impl ScheduleService {
    pub fn new(db: Arc<Db>, vk_token: String, vk_domain: String) -> Self {
        let vk = VkClient::new(vk_token.clone(), vk_domain);
        Self {
            db,
            vk,
            archive_cache: RwLock::new(HashMap::new()),
            zvonki_cache: RwLock::new(HashMap::new()),
            bells_photo_cache: RwLock::new(HashMap::new()),
            vk_token,
            files_cache: RwLock::new(HashMap::new()),
            processed_posts: Mutex::new(ProcessedPosts::default()),
            processed_files: Mutex::new(HashSet::new()),
        }
    }

    /// Загружает весь архив в RAM (archive_cache + кэши файлов/звонков).
    /// Как load_ram (services.py:3257).
    pub async fn load_ram(&self) -> Result<()> {
        let dump = self.db.get_full_archive_dump().await?;
        let mut archive: HashMap<String, HashMap<String, Vec<(String, String)>>> =
            HashMap::new();
        let mut zvonki: HashMap<String, BTreeMap<String, Vec<String>>> = HashMap::new();
        for (d, g, c, t) in dump {
            if c.is_empty() || c.contains("🔔 Расписание по времени (фото)") {
                continue;
            }
            archive
                .entry(d.clone())
                .or_default()
                .entry(g.clone())
                .or_default()
                .push((c.clone(), t.clone()));
            if g == "__ZVONKI__" && t == "звонки" {
                if let Some(parsed) = parse_zvonki_json(&c) {
                    zvonki.insert(d.clone(), parsed);
                }
            }
        }
        *self.archive_cache.write().unwrap() = archive;
        *self.zvonki_cache.write().unwrap() = zvonki;

        // Файлы — как try/except в Python: ошибка загрузки файлов не
        // роняет весь load_ram.
        let active_files = match self.db.get_all_active_files(14).await {
            Ok(files) => files,
            Err(e) => {
                tracing::error!("Error loading files cache: {e}");
                Vec::new()
            }
        };
        let mut files_cache: HashMap<String, Vec<ScheduleFile>> = HashMap::new();
        let mut bells: HashMap<String, String> = HashMap::new();
        for f in active_files {
            self.register_processed_id(&f.file_unique_id);
            if f.file_type == "zvonki_photo" {
                let value = f.file_id.clone().unwrap_or_else(|| f.vk_url.clone());
                if !value.is_empty() {
                    let replace = match bells.get(&f.date_str) {
                        None => true,
                        Some(existing) => {
                            f.file_id.is_some()
                                && (existing.starts_with("http://")
                                    || existing.starts_with("https://"))
                        }
                    };
                    if replace {
                        bells.insert(f.date_str.clone(), value);
                    }
                }
            }
            files_cache.entry(f.date_str.clone()).or_default().push(f);
        }
        *self.files_cache.write().unwrap() = files_cache;
        *self.bells_photo_cache.write().unwrap() = bells;
        Ok(())
    }

    /// День группы из кэша: (целевая группа, пары, изменения).
    /// Как format_day_data (services.py:2493) поверх archive_cache.
    /// Если даты/группы нет в кэше — пустые пары и изменения, а
    /// target_group — пустая строка (api.rs подставляет исходную группу).
    pub fn format_day_data(&self, group: &str, date_str: &str) -> DayData {
        let clean_g = normalize_group(group, "");
        let cache = self.archive_cache.read().unwrap();
        let Some(day_data) = cache.get(date_str) else {
            return DayData::default();
        };
        let Some(target_g) = day_data
            .keys()
            .find(|ex_g| *ex_g != "__ZVONKI__" && normalize_group(ex_g, "") == clean_g)
            .cloned()
        else {
            return DayData::default();
        };
        let records: Vec<(String, String)> = day_data[&target_g].clone();
        drop(cache);
        let (pairs, changes) = parser_format_day_data(&records);
        DayData {
            target_group: target_g,
            pairs,
            changes,
        }
    }

    /// Быстрое сохранение одной записи в архив + кэш. Как save_fast (services.py:2411).
    pub async fn save_fast(&self, date_str: &str, group: &str, content: &str, g_type: &str) -> Result<bool> {
        if content.contains("🔔 Расписание по времени (фото)") {
            return Ok(false);
        }
        let norm_new = norm_archive_content(content);
        {
            let cache = self.archive_cache.read().unwrap();
            if let Some(items) = cache.get(date_str).and_then(|m| m.get(group)) {
                if items
                    .iter()
                    .any(|(c, _)| norm_archive_content(c) == norm_new)
                {
                    return Ok(false);
                }
            }
        }
        self.db
            .save_to_archive(date_str, group, content, g_type)
            .await?;
        {
            let mut cache = self.archive_cache.write().unwrap();
            let items = cache
                .entry(date_str.to_string())
                .or_default()
                .entry(group.to_string())
                .or_default();
            if g_type == "основное" || g_type == "звонки" {
                items.retain(|(_, t)| t != g_type);
            }
            items.push((content.to_string(), g_type.to_string()));
        }
        if group == "__ZVONKI__" && g_type == "звонки" {
            let mut zc = self.zvonki_cache.write().unwrap();
            match parse_zvonki_json(content) {
                Some(parsed) => {
                    zc.insert(date_str.to_string(), parsed);
                }
                None => {
                    zc.remove(date_str);
                }
            }
        }
        Ok(true)
    }

    /// Внутренняя версия save_fast_batch: возвращает вставленные записи
    /// (нужно process_vk_posts для событий). Python возвращает тот же
    /// список; публичный метод по контракту возвращает ().
    async fn save_fast_batch_inner(
        &self,
        records: &[(String, String, String, String)],
    ) -> Result<Vec<(String, String, String, String)>> {
        let mut to_insert: Vec<(String, String, String, String)> = Vec::new();
        {
            let mut cache = self.archive_cache.write().unwrap();
            for (d_str, group, content, g_type) in records {
                if content.is_empty()
                    || content.contains("🔔 Расписание по времени (фото)")
                {
                    continue;
                }
                let norm_new = norm_archive_content(content);
                let exists = cache
                    .get(d_str)
                    .and_then(|m| m.get(group))
                    .map(|items| {
                        items
                            .iter()
                            .any(|(c, _)| norm_archive_content(c) == norm_new)
                    })
                    .unwrap_or(false);
                if exists {
                    continue;
                }
                let items = cache
                    .entry(d_str.clone())
                    .or_default()
                    .entry(group.clone())
                    .or_default();
                if g_type == "основное" || g_type == "звонки" {
                    items.retain(|(_, t)| t != g_type);
                }
                items.push((content.clone(), g_type.clone()));
                to_insert.push((
                    d_str.clone(),
                    group.clone(),
                    content.clone(),
                    g_type.clone(),
                ));
            }
        }
        if !to_insert.is_empty() {
            self.db.save_to_archive_batch(&to_insert).await?;
            let mut zc = self.zvonki_cache.write().unwrap();
            for (d_str, group, content, g_type) in &to_insert {
                if group == "__ZVONKI__" && g_type == "звонки" {
                    match parse_zvonki_json(content) {
                        Some(parsed) => {
                            zc.insert(d_str.clone(), parsed);
                        }
                        None => {
                            zc.remove(d_str);
                        }
                    }
                }
            }
        }
        Ok(to_insert)
    }

    pub async fn save_fast_batch(&self, records: &[(String, String, String, String)]) -> Result<()> {
        self.save_fast_batch_inner(records).await?;
        Ok(())
    }

    /// _register_processed_id (services.py:433): uid + базовый uid
    /// без суффикса _ДД.ММ.ГГГГ.
    fn register_processed_id(&self, uid: &str) {
        if let Ok(mut set) = self.processed_files.lock() {
            set.insert(uid.to_string());
            if let Some(base) = base_file_uid(uid) {
                set.insert(base);
            }
        }
    }

    fn is_processed_file(&self, uid: &str) -> bool {
        self.processed_files
            .lock()
            .map(|set| set.contains(uid))
            .unwrap_or(false)
    }

    /// save_file_to_cache (services.py:439) — только память; запись в БД
    /// делается отдельным вызовом Db (как в Python: save_file_to_cache
    /// не пишет в БД, это делают вызовы save_schedule_file(s)_batch).
    /// Добавленный метод (в контракте его нет).
    pub fn save_file_to_cache(
        &self,
        file_unique_id: &str,
        date_str: &str,
        file_type: &str,
        title: &str,
        file_id: Option<&str>,
        vk_url: &str,
    ) {
        self.register_processed_id(file_unique_id);
        if file_type == "zvonki_photo"
            && (file_id.map(|f| !f.is_empty()).unwrap_or(false) || !vk_url.is_empty())
        {
            if let Ok(mut bells) = self.bells_photo_cache.try_write() {
                bells.insert(
                    date_str.to_string(),
                    file_id.unwrap_or(vk_url).to_string(),
                );
            }
        }
        if let Ok(mut fc) = self.files_cache.try_write() {
            let items = fc.entry(date_str.to_string()).or_default();
            for item in items.iter_mut() {
                if item.file_unique_id == file_unique_id {
                    if let Some(fid) = file_id {
                        if !fid.is_empty() {
                            item.file_id = Some(fid.to_string());
                        }
                    }
                    return;
                }
            }
            let entry = ScheduleFile {
                file_unique_id: file_unique_id.to_string(),
                date_str: date_str.to_string(),
                file_type: file_type.to_string(),
                title: title.to_string(),
                file_id: file_id.map(str::to_string),
                vk_url: vk_url.to_string(),
            };
            if file_type == "zvonki_photo" {
                items.insert(0, entry);
            } else {
                items.push(entry);
            }
        }
    }

    /// cache_bells_photo_file_id (services.py:486): запомнить Telegram
    /// file_id фото звонков в кэшах и в БД. Добавленный метод.
    pub async fn cache_bells_photo_file_id(&self, date_str: &str, file_id: &str) -> Result<()> {
        if file_id.is_empty() {
            return Ok(());
        }
        {
            let mut bells = self.bells_photo_cache.write().unwrap();
            bells.insert(date_str.to_string(), file_id.to_string());
        }
        {
            let mut fc = self.files_cache.write().unwrap();
            if let Some(items) = fc.get_mut(date_str) {
                for item in items.iter_mut() {
                    if item.file_type == "zvonki_photo" {
                        item.file_id = Some(file_id.to_string());
                    }
                }
            }
        }
        self.db
            .update_schedule_file_id(date_str, "zvonki_photo", file_id, None)
            .await?;
        Ok(())
    }

    /// Распарсенные звонки на дату из docx (или None).
    /// Как get_parsed_zvonki_for_date (services.py:2895): сначала
    /// zvonki_cache, затем архив __ZVONKI__ (последняя валидная запись).
    pub fn get_parsed_zvonki_for_date(&self, date_str: &str) -> Option<BTreeMap<String, Vec<String>>> {
        if let Some(parsed) = self.zvonki_cache.read().unwrap().get(date_str) {
            return Some(parsed.clone());
        }
        let cache = self.archive_cache.read().unwrap();
        let records = cache.get(date_str)?.get("__ZVONKI__")?;
        for (content, rtype) in records.iter().rev() {
            if rtype == "звонки" && !content.is_empty() {
                if let Some(parsed) = parse_zvonki_json(content) {
                    return Some(parsed);
                }
            }
        }
        None
    }

    pub fn has_zvonki_for_date(&self, date_str: &str) -> bool {
        if self.get_parsed_zvonki_for_date(date_str).is_some() {
            return true;
        }
        if self.get_file_for_date(date_str, Some("zvonki")).is_some()
            || self.get_file_for_date(date_str, Some("zvonki_photo")).is_some()
        {
            return true;
        }
        // В воскресенье без спец-звонков занятий нет.
        match parse_date(date_str) {
            Some(d) => d.weekday() != Weekday::Sun,
            // Непарсящаяся дата: как в Python без group-проверки — true.
            None => true,
        }
    }

    /// Текст штатных звонков для группы и даты. Как get_zvonki_text (services.py:2945).
    pub fn get_zvonki_text(&self, group: &str, target: NaiveDate) -> String {
        let day_key = static_day_key(target.weekday());
        let course = get_course_str(group);
        let base_course: &str = if course.contains("1 курс") {
            "1 курс"
        } else {
            course.as_str()
        };
        static_zvonki_text(day_key, base_course)
            .unwrap_or("⚠️ Сетка звонков не найдена")
            .to_string()
    }

    /// (all_pairs, active_pairs) — как _get_group_pairs_for_date_str
    /// (services.py:2723): все пары + реально идущие с учётом отмен.
    fn group_pairs_for_date(&self, group: &str, d_str: &str) -> (HashSet<i32>, HashSet<i32>) {
        let day = self.format_day_data(group, d_str);
        let mut all: HashSet<i32> = day.pairs.keys().copied().collect();
        let mut active = all.clone();
        for c in &day.changes {
            let c_pairs = extract_pair_numbers(c);
            if c_pairs.is_empty() {
                continue;
            }
            all.extend(c_pairs.iter().copied());
            if is_cancel_change(c) {
                active.retain(|p| !c_pairs.contains(p));
            } else {
                active.extend(c_pairs.iter().copied());
            }
        }
        if active.is_empty() && !all.is_empty() {
            active = all.clone();
        }
        (all, active)
    }

    /// Есть ли у группы реальные пары на дату (has_classes_on_date
    /// поверх get_group_day_schedule, services.py:359,655).
    fn group_has_classes(&self, group: &str, date_str: &str) -> bool {
        let day = self.format_day_data(group, date_str);
        let mut cancelled: HashSet<i32> = HashSet::new();
        let mut extra_changes: Vec<&String> = Vec::new();
        for c in &day.changes {
            let nums = extract_pair_numbers(c);
            if is_cancel_change(c) {
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

    /// Даты архива от новых к старым (для fallback в get_group_shift /
    /// get_group_bells_end_time).
    fn archive_dates_desc(&self) -> Vec<String> {
        let cache = self.archive_cache.read().unwrap();
        let mut dates: Vec<String> = cache.keys().cloned().collect();
        dates.sort_by_key(|d| parse_date(d).unwrap_or_default());
        dates.reverse();
        dates
    }

    /// Смена группы на дату (пара №>=5 -> 2). Как get_group_shift (services.py:2749).
    pub fn get_group_shift(&self, group: &str, target: NaiveDate) -> i32 {
        let d_str = fmt_date(target);
        let (all, _) = self.group_pairs_for_date(group, &d_str);
        if !all.is_empty() {
            return if all.iter().any(|p| *p >= 5) { 2 } else { 1 };
        }
        for cand in self.archive_dates_desc() {
            let (cand_all, _) = self.group_pairs_for_date(group, &cand);
            if !cand_all.is_empty() {
                return if cand_all.iter().any(|p| *p >= 5) { 2 } else { 1 };
            }
        }
        1
    }

    /// Конец последней пары группы на дату ("H:MM").
    /// Как get_group_bells_end_time (services.py:2777); Python возвращает
    /// кортеж (час, минута) — по контракту здесь строка.
    pub fn get_group_bells_end_time(&self, group: &str, target: NaiveDate) -> Option<String> {
        let d_str = fmt_date(target);
        let lines: Vec<String> = if let Some(parsed) = self.get_parsed_zvonki_for_date(&d_str) {
            zvonki_lines_for_course(&parsed, &get_course_str(group))
        } else {
            self.get_zvonki_text(group, target)
                .lines()
                .map(str::to_string)
                .collect()
        };

        let mut pair_end_times: BTreeMap<i32, (i32, i32)> = BTreeMap::new();
        let mut shift1_end: Option<(i32, i32)> = None;
        let mut shift2_end: Option<(i32, i32)> = None;
        let mut current_shift = 1;
        for line in &lines {
            let l_clean = line.trim();
            if l_clean.is_empty() {
                continue;
            }
            if l_clean.contains("1 смена") {
                current_shift = 1;
                continue;
            }
            if l_clean.contains("2 смена") {
                current_shift = 2;
                continue;
            }
            let line_no_paren = remove_paren_groups(l_clean);
            let times = find_hhmm(&line_no_paren);
            if times.is_empty() {
                continue;
            }
            let end_hm = *times.last().unwrap();
            if !(0..=23).contains(&end_hm.0) || !(0..=59).contains(&end_hm.1) {
                continue;
            }
            if let Some(p_num) = pair_number_colon(l_clean) {
                pair_end_times.insert(p_num, end_hm);
                if p_num <= 3 {
                    shift1_end = Some(match shift1_end {
                        Some(prev) => prev.max(end_hm),
                        None => end_hm,
                    });
                } else {
                    current_shift = 2;
                    shift2_end = Some(match shift2_end {
                        Some(prev) => prev.max(end_hm),
                        None => end_hm,
                    });
                }
            } else if l_clean.contains("Разговоры") || l_clean.contains("Классный час") {
                if current_shift == 1 && !pair_end_times.contains_key(&4) {
                    shift1_end = Some(match shift1_end {
                        Some(prev) => prev.max(end_hm),
                        None => end_hm,
                    });
                } else {
                    shift2_end = Some(match shift2_end {
                        Some(prev) => prev.max(end_hm),
                        None => end_hm,
                    });
                }
            }
        }

        let default_s1 = pair_end_times.get(&3).copied().unwrap_or((13, 20));
        let default_s2 = pair_end_times.get(&6).copied().unwrap_or_else(|| {
            if target.weekday().num_days_from_monday() >= 3 {
                (18, 40)
            } else {
                (19, 20)
            }
        });
        let shift1_end = shift1_end.unwrap_or(default_s1);
        let shift2_end = shift2_end.unwrap_or(default_s2);

        let group_shift = self.get_group_shift(group, target);
        let shift_end_hm = if group_shift == 2 { shift2_end } else { shift1_end };

        let (_, mut active_pairs) = self.group_pairs_for_date(group, &d_str);
        if active_pairs.is_empty() {
            for cand in self.archive_dates_desc() {
                let (_, cand_active) = self.group_pairs_for_date(group, &cand);
                if !cand_active.is_empty() {
                    active_pairs = cand_active;
                    break;
                }
            }
        }
        if !active_pairs.is_empty() {
            let max_p = *active_pairs.iter().max().unwrap();
            if let Some(last_pair_end) = pair_end_times.get(&max_p) {
                let end = shift_end_hm.min(*last_pair_end);
                return Some(format!("{}:{:02}", end.0, end.1));
            }
        }
        Some(format!("{}:{:02}", shift_end_hm.0, shift_end_hm.1))
    }

    /// Можно ли показать кнопку звонков на завтра. Как
    /// can_open_tomorrow_zvonki (services.py:2877), но по контракту
    /// принимает только дату (завтра) без группы: проверка «не
    /// воскресенье» — по дню перед этой датой, а порог окончания пар
    /// взят 13:20 (конец 1-й смены) — см. шапку файла.
    pub fn can_open_tomorrow_zvonki(&self, date_str: &str) -> bool {
        let Some(tomorrow) = parse_date(date_str) else {
            return false;
        };
        let today = tomorrow - chrono::Duration::days(1);
        if today.weekday() == Weekday::Sun {
            return false;
        }
        if !self.has_zvonki_for_date(date_str) {
            return false;
        }
        let now = tyumen_now();
        (now.hour() as i32, now.minute() as i32) >= (13, 20)
    }

    /// Первая актуальная пара завтра: "Nп H:MM".
    /// Как get_tomorrow_first_bell_info (services.py:2966); Python
    /// возвращает кортеж (метка, время) — по контракту здесь строка.
    pub fn get_tomorrow_first_bell_info(&self, group: &str, tomorrow: NaiveDate) -> Option<String> {
        let d_str = fmt_date(tomorrow);
        let day = self.format_day_data(group, &d_str);
        let target_p: String = if !day.pairs.is_empty() {
            day.pairs.keys().min().unwrap().to_string()
        } else {
            let mut chg_p: HashSet<i32> = HashSet::new();
            for c in &day.changes {
                chg_p.extend(extract_pair_numbers(c));
            }
            if !chg_p.is_empty() {
                chg_p.iter().min().unwrap().to_string()
            } else if self.get_group_shift(group, tomorrow) == 2 {
                "4".to_string()
            } else {
                "1".to_string()
            }
        };

        let lines: Vec<String> = if let Some(parsed) = self.get_parsed_zvonki_for_date(&d_str) {
            zvonki_lines_for_course(&parsed, &get_course_str(group))
        } else if tomorrow.weekday() != Weekday::Sun {
            self.get_zvonki_text(group, tomorrow)
                .lines()
                .map(str::to_string)
                .collect()
        } else {
            Vec::new()
        };

        for line in &lines {
            if let Some(start) = pair_start_time(line, &target_p) {
                return Some(format!("{target_p}п {start}"));
            }
        }
        for line in &lines {
            if let Some((p, start)) = first_pair_start(line) {
                return Some(format!("{p}п {start}"));
            }
        }
        None
    }

    /// Rich-таблица дня — как render_rich_table_html (services.py:2679):
    /// <h3>, <table bordered striped> с <mark> для пар из замен,
    /// <hr/> и <blockquote> для замен; wrap_details — вкладка <details>.
    pub fn render_rich_table_html(
        &self,
        group_name: &str,
        date_str: &str,
        main_pairs: &BTreeMap<i32, (String, String)>,
        changes: &[String],
        target_group_name: Option<&str>,
        wrap_details: bool,
        is_open: bool,
    ) -> String {
        let display_group = target_group_name
            .filter(|g| !g.is_empty())
            .unwrap_or(group_name);
        let weekday_str = match parse_date(date_str) {
            Some(d) => format!(
                " • {}",
                WEEKDAYS[d.weekday().num_days_from_monday() as usize]
            ),
            None => String::new(),
        };
        let header = format!(
            "<h3>Группа {} • {}{}</h3>\n",
            esc(display_group),
            esc(date_str),
            esc(&weekday_str)
        );
        if main_pairs.is_empty() && changes.is_empty() {
            return format!("{header}\n<i>Пар нет</i>");
        }
        let mut changed_pairs: HashSet<i32> = HashSet::new();
        for c in changes {
            changed_pairs.extend(extract_pair_numbers(c));
        }
        let mut body_parts: Vec<String> = Vec::new();
        if !main_pairs.is_empty() {
            let mut table_lines: Vec<String> = vec![
                "<table bordered striped>".to_string(),
                "  <tr>".to_string(),
                "    <th align=\"center\">№</th>".to_string(),
                "    <th>Предмет</th>".to_string(),
                "    <th align=\"center\">Каб.</th>".to_string(),
                "  </tr>".to_string(),
            ];
            for (p_num, (subj, room)) in main_pairs {
                let subj_clean = esc(&subj.split_whitespace().collect::<Vec<_>>().join(" "));
                let room_clean = esc(&room.split_whitespace().collect::<Vec<_>>().join(" "));
                let (pair_cell, room_cell) = if changed_pairs.contains(p_num) {
                    (
                        format!("<mark><b>{p_num}п ⚠️</b></mark>"),
                        format!("<mark>{room_clean}</mark>"),
                    )
                } else {
                    (format!("<b>{p_num}п</b>"), room_clean)
                };
                table_lines.push(format!(
                    "  <tr><td align=\"center\">{pair_cell}</td><td>{subj_clean}</td><td align=\"center\">{room_cell}</td></tr>"
                ));
            }
            table_lines.push("</table>".to_string());
            body_parts.push(format!("\n{}", table_lines.join("\n")));
        }
        if !changes.is_empty() {
            if !main_pairs.is_empty() {
                body_parts.push("\n<hr/>\n".to_string());
            } else {
                body_parts.push("\n".to_string());
            }
            let change_lines: Vec<String> = changes
                .iter()
                .map(|c| {
                    let stripped = c.replace("<b>", "").replace("</b>", "");
                    let c_escaped = esc(stripped.trim());
                    format!("• {}", highlight_pair_list_marker(&c_escaped))
                })
                .collect();
            let change_text = change_lines.join("<br>\n");
            let bq_tag = if change_lines.len() > 4 {
                "blockquote expandable"
            } else {
                "blockquote"
            };
            body_parts.push(format!(
                "<{bq_tag}><b>Замены:</b><br>\n{change_text}</blockquote>"
            ));
        }
        if wrap_details {
            let open_attr = if is_open { " open" } else { "" };
            let mark_badge = if changes.is_empty() { "" } else { " • замены" };
            let inner_content = body_parts.join("").trim().to_string();
            return format!(
                "<details{open_attr}>\n<summary><b>{}{}{}</b></summary>\n\n{header}{inner_content}\n</details>",
                esc(date_str),
                esc(&weekday_str),
                mark_badge
            );
        }
        format!("{header}{}", body_parts.join(""))
    }

    /// Rich-таблица звонков — как format_parsed_zvonki_rich (services.py:2042):
    /// <h3> + <table bordered striped compact> + <details> по сменам
    /// + живой отсчёт <tg-time>. Пустая строка — нет строк для курса.
    pub fn format_parsed_zvonki_rich(
        &self,
        date_str: &str,
        course_str: &str,
        parsed_zvonki: &BTreeMap<String, Vec<String>>,
        group_name: Option<&str>,
        now: DateTime<FixedOffset>,
    ) -> String {
        if parsed_zvonki.is_empty() {
            return String::new();
        }
        let header = format!("<h3>Расписание звонков на {}</h3>\n\n", esc(date_str));
        let target_date = parse_date(date_str).unwrap_or_else(|| {
            if now.weekday() == Weekday::Sun {
                now.date_naive() + chrono::Duration::days(1)
            } else {
                now.date_naive()
            }
        });
        let group_shift = group_name
            .map(|g| self.get_group_shift(g, target_date))
            .unwrap_or(1);

        if let Some(all) = parsed_zvonki.get("all_courses") {
            return format!(
                "{header}{}",
                build_zvonki_shift_sections(all, group_shift, target_date, now)
            );
        }
        if course_str.contains("1 курс") {
            let has_9 = parsed_zvonki
                .get("1 курс_9")
                .filter(|v| !v.is_empty());
            let has_11 = parsed_zvonki
                .get("1 курс_11")
                .filter(|v| !v.is_empty());
            if let Some(lines) = parsed_zvonki.get("1 курс") {
                if has_9.is_none() && has_11.is_none() {
                    return format!(
                        "{header}<b>1 курс</b>\n\n{}",
                        build_zvonki_shift_sections(lines, group_shift, target_date, now)
                    );
                }
            }
            let lines_9 = has_9.cloned().unwrap_or_default();
            let lines_11 = has_11.cloned().unwrap_or_default();
            if course_str.contains("база 9") && !lines_9.is_empty() {
                return format!(
                    "{header}<b>1 курс (база 9 классов)</b>\n\n{}",
                    build_zvonki_shift_sections(&lines_9, group_shift, target_date, now)
                );
            }
            if course_str.contains("база 11") && !lines_11.is_empty() {
                return format!(
                    "{header}<b>1 курс (база 11 классов)</b>\n\n{}",
                    build_zvonki_shift_sections(&lines_11, group_shift, target_date, now)
                );
            }
            let t9 = if lines_9.is_empty() {
                String::new()
            } else {
                build_zvonki_table(&lines_9, target_date, now)
            };
            let t11 = if lines_11.is_empty() {
                String::new()
            } else {
                build_zvonki_table(&lines_11, target_date, now)
            };
            let mut res = header.clone();
            if !t9.is_empty() {
                res.push_str(&format!(
                    "<details open>\n<summary><b>База 9 классов</b></summary>\n\n{t9}\n</details>\n\n"
                ));
            }
            if !t11.is_empty() {
                res.push_str(&format!(
                    "<details>\n<summary><b>База 11 классов</b></summary>\n\n{t11}\n</details>"
                ));
            }
            return res.trim().to_string();
        }
        let lines = parsed_zvonki
            .get(course_str)
            .or_else(|| parsed_zvonki.get("2 курс"));
        match lines {
            Some(lines) if !lines.is_empty() => format!(
                "{header}<b>{}</b>\n\n{}",
                esc(course_str),
                build_zvonki_shift_sections(lines, group_shift, target_date, now)
            ),
            _ => String::new(),
        }
    }

    /// Кнопка «Звонки на завтра» внутри Rich-текста — как
    /// get_tomorrow_bell_button_html (services.py:3023): <tg-button-row>
    /// с живым <tg-time> до начала первой пары завтра.
    pub fn get_tomorrow_bell_button_html(
        &self,
        group_name: &str,
        tomorrow_date: NaiveDate,
        now: DateTime<FixedOffset>,
    ) -> String {
        let d_str = fmt_date(tomorrow_date);
        let short_date = &d_str[..5];
        let btn_label = match self.get_tomorrow_first_bell_info(group_name, tomorrow_date) {
            Some(info) => match info.split_once(' ') {
                Some((p_lbl, start_time)) => {
                    let tg_timer = make_tg_time(start_time, Some(tomorrow_date), now);
                    format!("Звонки на завтра ({short_date}) • {p_lbl} {tg_timer}")
                }
                None => format!("Звонки на завтра ({short_date})"),
            },
            None => format!("Звонки на завтра ({short_date})"),
        };
        format!(
            "<tg-button-row align=\"center\">\n  <tg-button type=\"callback_data\" style=\"primary\" data=\"zvonki_date:{d_str}\">{btn_label}</tg-button>\n</tg-button-row>"
        )
    }

    /// Текущая пара с живым таймером — как _build_current_pair_progress_html
    /// (main.py:243): <blockquote> с <tg-time> до звонка; в воскресенье —
    /// «Воскресенье — выходной»; вне пар — пустая строка.
    pub fn current_pair_progress_rich(
        &self,
        group_name: &str,
        now: DateTime<FixedOffset>,
    ) -> String {
        if now.weekday() == Weekday::Sun {
            return "<blockquote>Воскресенье — выходной</blockquote>".to_string();
        }
        let today_str = fmt_date(now.date_naive());
        let lines: Vec<String> = if let Some(parsed) = self.get_parsed_zvonki_for_date(&today_str)
        {
            let c_str = get_course_str(group_name);
            let mut found: Vec<String> = Vec::new();
            for key in ["all_courses", c_str.as_str(), "1 курс", "2 курс"] {
                if let Some(v) = parsed.get(key) {
                    if !v.is_empty() {
                        found = v.clone();
                        break;
                    }
                }
            }
            found
        } else {
            self.get_zvonki_text(group_name, now.date_naive())
                .lines()
                .map(str::to_string)
                .collect()
        };
        let now_mins = now.hour() as i32 * 60 + now.minute() as i32;
        for line in &lines {
            let Some((p_num, h1, m1, h2, m2)) = parse_pair_time_range(line) else {
                continue;
            };
            let start_m = h1 * 60 + m1;
            let end_m = h2 * 60 + m2;
            if start_m <= now_mins && now_mins < end_m && end_m > start_m {
                let timer_tag = make_tg_time(
                    &format!("{h1}:{m1:02}–{h2}:{m2:02}"),
                    Some(now.date_naive()),
                    now,
                );
                return format!(
                    "<blockquote><b>{p_num} пара</b> • до звонка {timer_tag}</blockquote>"
                );
            }
        }
        String::new()
    }

    /// Полный ответ "Звонки" — (fallback, rich), как get_zvonki_resp
    /// (services.py:3052). target=None — сегодня (завтра при воскресенье);
    /// include_tomorrow_button работает только при target=None.
    /// Ветка без rich-частей возвращает (fb, Some(fb)) — как в Python.
    pub fn get_zvonki_resp_rich(
        &self,
        group: &str,
        target: Option<NaiveDate>,
        include_tomorrow_button: bool,
        now: DateTime<FixedOffset>,
    ) -> (String, Option<String>) {
        let base_date = target.unwrap_or_else(|| {
            if now.weekday() == Weekday::Sun {
                now.date_naive() + chrono::Duration::days(1)
            } else {
                now.date_naive()
            }
        });
        let group_shift = self.get_group_shift(group, base_date);
        let tomorrow_btn_block = if include_tomorrow_button && target.is_none() {
            let tomorrow_date = now.date_naive() + chrono::Duration::days(1);
            self.get_tomorrow_bell_button_html(group, tomorrow_date, now)
        } else {
            String::new()
        };
        let d_str = fmt_date(base_date);
        if let Some(parsed) = self.get_parsed_zvonki_for_date(&d_str) {
            let course_str = get_course_str(group);
            let rich_z =
                self.format_parsed_zvonki_rich(&d_str, &course_str, &parsed, Some(group), now);
            if !rich_z.is_empty() {
                let fb_z = strip_rich_table_tags(&rich_z);
                let rich_z = if tomorrow_btn_block.is_empty() {
                    rich_z
                } else {
                    format!("{rich_z}\n\n{tomorrow_btn_block}")
                };
                return (fb_z, Some(rich_z));
            }
        }

        let raw_text = self.get_zvonki_text(group, base_date);
        let raw_lines: Vec<&str> = raw_text.split('\n').collect();
        let raw_first_line = raw_lines.first().map(|l| l.trim()).unwrap_or("");
        let mut sections: Vec<(String, Vec<String>)> = Vec::new();
        let mut current_title = String::new();
        let mut current_lines: Vec<String> = Vec::new();
        for (idx_r, line) in raw_lines.iter().enumerate() {
            let line_clean = line.trim();
            if line_clean.is_empty() {
                continue;
            }
            if ["1 смена", "2 смена", "База 9", "База 11"]
                .iter()
                .any(|k| line_clean.contains(k))
            {
                if !current_lines.is_empty() {
                    sections.push((
                        std::mem::take(&mut current_title),
                        std::mem::take(&mut current_lines),
                    ));
                }
                current_title = line_clean.to_string();
            } else if idx_r == 0 {
                current_title = line_clean.to_string();
            } else {
                current_lines.push(line_clean.to_string());
            }
        }
        if !current_lines.is_empty() {
            sections.push((current_title, current_lines));
        }

        let date_suffix = if target.is_some() {
            format!(" ({})", &d_str[..5])
        } else {
            String::new()
        };
        let mut fb_lines: Vec<String> = Vec::new();
        for (idx_l, line) in raw_lines.iter().enumerate() {
            let line_no_note = remove_paren_groups(line);
            if idx_l == 0 && !date_suffix.is_empty() {
                fb_lines.push(format!("{line}{date_suffix}"));
            } else if idx_l > 0 && extract_time_range(&line_no_note).is_some() {
                let tg_timer = make_tg_time(&line_no_note, Some(base_date), now);
                fb_lines.push(format!("{line} ({tg_timer})"));
            } else {
                fb_lines.push(line.to_string());
            }
        }
        let fb_text = fb_lines.join("\n");

        let clean_header = raw_first_line
            .replace('🔔', "")
            .replace("<b>", "")
            .replace("</b>", "")
            .trim()
            .to_string();
        let clean_header = if !clean_header.is_empty() && !date_suffix.is_empty() {
            format!("{clean_header}{date_suffix}")
        } else {
            clean_header
        };
        let main_header = if clean_header.is_empty() {
            String::new()
        } else {
            format!("<h3>{}</h3>", esc(&clean_header))
        };
        let mut result_parts: Vec<String> = Vec::new();
        if !main_header.is_empty() {
            result_parts.push(main_header);
        }
        let is_multi_section = sections.len() > 1;
        for (idx, (title, l_list)) in sections.iter().enumerate() {
            let sec_title = if title == raw_first_line { "" } else { title.as_str() };
            let sec_open = if sec_title.contains("2 смена") {
                group_shift == 2
            } else if sec_title.contains("1 смена") {
                group_shift == 1
            } else {
                idx == 0
            };
            let t_html = render_compact_section_table(
                sec_title,
                l_list,
                is_multi_section,
                sec_open,
                base_date,
                now,
            );
            if !t_html.is_empty() {
                result_parts.push(t_html);
            }
        }
        if !tomorrow_btn_block.is_empty() {
            result_parts.push(tomorrow_btn_block);
        }
        if !result_parts.is_empty() {
            let rich_html = result_parts
                .iter()
                .filter(|p| !p.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join("\n\n");
            (fb_text, Some(rich_html))
        } else {
            (fb_text.clone(), Some(fb_text))
        }
    }

    /// Полный ответ "Звонки" для бота — только fallback-текст.
    /// Rich-часть — в get_zvonki_resp_rich.
    pub fn get_zvonki_resp(&self, group: &str, target: NaiveDate) -> String {
        self.get_zvonki_resp_rich(group, Some(target), false, tyumen_now())
            .0
    }

    /// URL/id фото звонков на дату (или None).
    /// Как get_bells_photo_for_date (services.py:474).
    pub fn get_bells_photo_for_date(&self, date_str: &str) -> Option<String> {
        let cached: Option<String> = self
            .bells_photo_cache
            .read()
            .unwrap()
            .get(date_str)
            .cloned();
        if let Some(ref c) = cached {
            if !c.starts_with("http://") && !c.starts_with("https://") {
                return Some(c.clone());
            }
        }
        let f = self.get_file_for_date(date_str, Some("zvonki_photo"));
        if let Some(f) = f {
            let fid = f
                .file_id
                .clone()
                .or_else(|| cached.clone())
                .or_else(|| {
                    if f.vk_url.is_empty() {
                        None
                    } else {
                        Some(f.vk_url.clone())
                    }
                });
            if let Some(fid) = fid {
                if let Ok(mut bells) = self.bells_photo_cache.try_write() {
                    bells.insert(date_str.to_string(), fid.clone());
                }
                return Some(fid);
            }
        }
        cached
    }

    pub fn get_file_for_date(&self, date_str: &str, file_type: Option<&str>) -> Option<ScheduleFile> {
        let fc = self.files_cache.read().unwrap();
        let files = fc.get(date_str)?;
        if let Some(ft) = file_type {
            files.iter().find(|f| f.file_type == ft).cloned()
        } else {
            files.first().cloned()
        }
    }

    pub fn has_file_for_date(&self, date_str: &str) -> bool {
        let fc = self.files_cache.read().unwrap();
        fc.get(date_str).map(|files| !files.is_empty()).unwrap_or(false)
    }

    pub fn get_active_zvonki_files(&self) -> Vec<ScheduleFile> {
        let fc = self.files_cache.read().unwrap();
        let mut out: Vec<ScheduleFile> = fc
            .values()
            .flatten()
            .filter(|f| f.file_type == "zvonki" || f.file_type == "zvonki_photo")
            .cloned()
            .collect();
        out.sort_by(|a, b| {
            b.date_str
                .cmp(&a.date_str)
                .then_with(|| a.file_unique_id.cmp(&b.file_unique_id))
        });
        out
    }

    /// Скачать фото (байты) с VK CDN. Как download_photo_bytes (services.py:537).
    /// Невалидный URL или невалидный payload — ошибка (в Python — None).
    pub async fn download_photo_bytes(&self, url: &str) -> Result<Vec<u8>> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(AppError::vk(format!("bad download url: {url}")));
        }
        let bytes = self.vk.download_bytes(url).await?;
        if !is_valid_image_payload(&bytes) {
            return Err(AppError::vk(format!(
                "invalid image payload (size={}) for {url}",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

}

/// Фото звонков для отправки: готовый Telegram file_id или байты.
/// Как resolve_photo_input (services.py:610): file_id — если захвачен,
/// иначе скачанные байты с VK CDN.
#[derive(Debug, Clone)]
pub enum BellsPhotoInput {
    FileId(String),
    Bytes(Vec<u8>),
}

impl ScheduleService {
    /// Фото звонков для отправки в Telegram: file_id или байты.
    /// Как get_bells_photo_input (services.py:648). None — фото нет или
    /// скачать не удалось (в Python — тоже None при ошибке скачивания).
    pub async fn get_bells_photo_input(&self, date_str: &str) -> Result<Option<BellsPhotoInput>> {
        let Some(r) = self.get_bells_photo_for_date(date_str) else {
            return Ok(None);
        };
        if r.starts_with("http://") || r.starts_with("https://") {
            match self.download_photo_bytes(&r).await {
                Ok(bytes) => Ok(Some(BellsPhotoInput::Bytes(bytes))),
                Err(e) => {
                    tracing::warn!("get_bells_photo_input({date_str}): {e}");
                    Ok(None)
                }
            }
        } else {
            Ok(Some(BellsPhotoInput::FileId(r)))
        }
    }
}

impl std::fmt::Debug for ScheduleService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduleService").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// update_check / process_vk_posts
// ---------------------------------------------------------------------------

impl ScheduleService {
    /// Проверка VK на новые посты; возвращает события (группа, дата, тип)
    /// для рассылки. Как update_check (services.py:1237), count=5 по умолчанию.
    pub async fn update_check(&self, count: usize) -> Result<HashSet<(String, String, String)>> {
        if self.vk_token.is_empty() {
            tracing::error!("❌ VK_TOKEN не найден в переменных окружения!");
            return Ok(HashSet::new());
        }
        if self.vk.is_rate_limited() {
            tracing::warn!(
                "⚠️ VK rate limited, backoff {}s — пропуск проверки",
                self.vk.backoff_delay()
            );
            return Ok(HashSet::new());
        }
        match self.vk.wall_get(count).await {
            Err(e) => {
                tracing::error!("VK Error: {e}");
                Ok(HashSet::new())
            }
            Ok(posts) => {
                // VK отдаёт сначала новые — разворот в хронологический
                // порядок, как list(reversed(...)) в Python.
                let mut posts = posts;
                posts.reverse();
                self.process_vk_posts(&posts).await
            }
        }
    }

    /// _compute_post_signature (services.py:404). В Rust вместо
    /// owner_id_id вложений — kind:url/title (см. шапку файла).
    fn compute_post_signature(post: &VkPost) -> PostSignature {
        let mut hasher = DefaultHasher::new();
        post.text.hash(&mut hasher);
        let text_hash = hasher.finish();
        let att_ids: Vec<String> = post
            .attachments
            .iter()
            .map(|att| {
                let id_part = att
                    .url
                    .clone()
                    .or_else(|| att.title.clone())
                    .unwrap_or_default();
                format!("{}:{id_part}", att.kind)
            })
            .collect();
        (post.date, text_hash, att_ids)
    }

    /// Разбор постов VK в архив; возвращает события (группа, дата, тип).
    /// Как process_vk_posts (services.py:1291).
    pub async fn process_vk_posts(&self, posts: &[VkPost]) -> Result<HashSet<(String, String, String)>> {
        let mut updated_events: HashSet<(String, String, String)> = HashSet::new();
        if posts.is_empty() {
            return Ok(updated_events);
        }

        let mut pending: Vec<(&VkPost, PostSignature)> = Vec::new();
        for post in posts {
            let sig = Self::compute_post_signature(post);
            let already = self
                .processed_posts
                .lock()
                .map(|pp| pp.get(post.id).map(|s| s == &sig).unwrap_or(false))
                .unwrap_or(false);
            if already {
                continue;
            }
            pending.push((post, sig));
        }
        // Все пришедшие посты уже в кэше — завершаемся до get_all_chats().
        if pending.is_empty() {
            return Ok(updated_events);
        }

        let now_dt = tyumen_now();
        let today = now_dt.date_naive();
        let all_chats = self.db.get_all_chats().await?;
        let mut chats_by_group: HashMap<String, Vec<(i64, String)>> = HashMap::new();
        for (c_id, g_name) in &all_chats {
            chats_by_group
                .entry(normalize_group(g_name, ""))
                .or_default()
                .push((*c_id, g_name.clone()));

        }
        for (post, sig) in pending {
            match self
                .process_single_post(post, &all_chats, &chats_by_group, today, &mut updated_events)
                .await
            {
                Ok(had_retryable_error) => {
                    if !had_retryable_error {
                        if let Ok(mut pp) = self.processed_posts.lock() {
                            pp.remember(post.id, sig);
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("⚠️ Ошибка обработки поста ВК {}: {e}", post.id);
                }
            }
        }
        Ok(updated_events)
    }

    /// Один пост: текст, docx расписания, docx звонков, фото звонков.
    /// Возвращает Ok(true), если была retryable-ошибка (пост не
    /// запоминается как обработанный). Ошибки отдельных вложений
    /// изолируются, как в Python.
    #[allow(clippy::too_many_arguments)]
    async fn process_single_post(
        &self,
        post: &VkPost,
        all_chats: &[(i64, String)],
        chats_by_group: &HashMap<String, Vec<(i64, String)>>,
        today: NaiveDate,
        updated_events: &mut HashSet<(String, String, String)>,
    ) -> Result<bool> {
        let mut post_had_retryable_error = false;
        let post_text = post.text.clone();

        let first_line = post_text.lines().next().unwrap_or("");
        let main_date_str =
            normalize_date(first_line).unwrap_or_else(|| fmt_date(today));
        let post_dates = extract_all_dates_from_post(&post_text, "");

        // --- Текст поста -> изменения ---
        let text_changes = self
            .parse_text_complex(&post_text, &main_date_str, all_chats)
            .await;
        if !text_changes.is_empty() {
            let text_batch: Vec<(String, String, String, String)> = text_changes
                .iter()
                .map(|(t_date, t_group, t_content)| {
                    (
                        t_date.clone(),
                        t_group.clone(),
                        t_content.clone(),
                        "изменения".to_string(),
                    )
                })
                .collect();
            let inserted = self.save_fast_batch_inner(&text_batch).await?;
            for (t_date, t_group, _, _) in inserted {
                updated_events.insert((t_group, t_date, "change".to_string()));
            }
        }

        let lower_text = post_text.to_lowercase();
        let is_post_zvonki = ["звонк", "врем", "сетк", "смен"]
            .iter()
            .any(|w| lower_text.contains(w));

        // --- Классификация вложений ---
        let mut photo_urls: Vec<String> = Vec::new();
        let mut zvonki_docs: Vec<(String, String)> = Vec::new(); // (title_lower, url)
        let mut schedule_docs: Vec<(String, String)> = Vec::new();
        for att in &post.attachments {
            match att.kind.as_str() {
                "photo" => {
                    if let Some(url) = &att.url {
                        photo_urls.push(url.clone());
                    }
                }
                "doc" => {
                    let title = att.title.clone().unwrap_or_default().to_lowercase();
                    let Some(url) = &att.url else { continue };
                    let is_zv_doc = ["звонк", "врем", "сетк", "время", "звонки"]
                        .iter()
                        .any(|w| title.contains(w));
                    if is_zv_doc {
                        zvonki_docs.push((title, url.clone()));
                    } else if title.ends_with(".doc")
                        || title.ends_with(".docx")
                        || title.contains("docx")
                        || title.contains("doc")
                        || url.to_lowercase().ends_with(".docx")
                        || url.to_lowercase().ends_with(".doc")
                    {
                        schedule_docs.push((title, url.clone()));
                    }
                }
                _ => {}
            }
        }

        let mut post_has_schedule_docx = false;
        let mut post_schedule_dates: Vec<String> = Vec::new();

        // === 1. Расписание пар (.DOC/.DOCX) ===
        for (title, doc_url) in &schedule_docs {
            // file_unique_id: в Python f"{owner_id}_{id}"; в Rust url
            // вложения (vk.rs не отдаёт owner_id/id) — см. шапку файла.
            let file_unique_id = doc_url.clone();
            if self.is_processed_file(&file_unique_id) {
                post_has_schedule_docx = true;
                continue;
            }
            let doc_dates = extract_all_dates_from_post(&post_text, title);
            let doc_date = doc_dates
                .first()
                .cloned()
                .or_else(|| normalize_date(title))
                .unwrap_or_else(|| main_date_str.clone());

            let content = match self.vk.download_bytes(doc_url).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!(
                        "⚠️ Ошибка загрузки вложения {file_unique_id} ({title}): {e}"
                    );
                    post_had_retryable_error = true;
                    continue;
                }
            };
            if content.is_empty() {
                tracing::warn!("⚠️ Пустое вложение расписания: {file_unique_id} ({title})");
                self.register_processed_id(&file_unique_id);
                continue;
            }
            let parsed_by_date = match parse_docx_pure(&content) {
                Ok(parsed) => parsed,
                Err(e) => {
                    tracing::warn!(
                        "⚠️ Не удалось извлечь таблицу расписания из {file_unique_id} ({title}): {e}"
                    );
                    continue;
                }
            };
            if parsed_by_date.is_empty() {
                tracing::warn!(
                    "⚠️ Не удалось извлечь таблицу расписания из {file_unique_id} ({title})"
                );
                self.register_processed_id(&file_unique_id);
                continue;
            }

            post_has_schedule_docx = true;
            let target_dates: Vec<String> = if doc_dates.is_empty() {
                vec![doc_date.clone()]
            } else {
                doc_dates.clone()
            };
            for t_d in &target_dates {
                if !post_schedule_dates.contains(t_d) {
                    post_schedule_dates.push(t_d.clone());
                }
            }
            let mut sched_batch: Vec<(String, String, String, String)> = Vec::new();
            for (d_key, g, lines) in &parsed_by_date {
                if !post_schedule_dates.contains(d_key) {
                    post_schedule_dates.push(d_key.clone());
                }
                sched_batch.push((
                    d_key.clone(),
                    g.clone(),
                    lines.join("\n"),
                    "основное".to_string(),
                ));
            }
            match self.save_fast_batch_inner(&sched_batch).await {
                Ok(inserted) => {
                    for (d_key, g, _, _) in inserted {
                        updated_events.insert((g, d_key, "schedule".to_string()));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "⚠️ Ошибка обработки документа расписания в посте {}: {e}",
                        post.id
                    );
                    continue;
                }
            }

            let mut file_records: Vec<ScheduleFile> = Vec::new();
            for t_d in &post_schedule_dates {
                let composite_uid = format!("{file_unique_id}_{t_d}");
                self.save_file_to_cache(
                    &composite_uid,
                    t_d,
                    "schedule",
                    title,
                    None,
                    doc_url,
                );
                file_records.push(ScheduleFile {
                    file_unique_id: composite_uid,
                    date_str: t_d.clone(),
                    file_type: "schedule".to_string(),
                    title: title.clone(),
                    file_id: None,
                    vk_url: doc_url.clone(),
                });
            }
            if !file_records.is_empty() {
                if let Err(e) = self.db.save_schedule_files_batch(&file_records).await {
                    tracing::warn!("⚠️ save_schedule_files_batch: {e}");
                }
            }
            self.register_processed_id(&file_unique_id);
        }

        // === 2. Документы звонков (.DOCX) ===
        for (title, doc_url) in &zvonki_docs {
            let file_unique_id = doc_url.clone();
            if self.is_processed_file(&file_unique_id) {
                continue;
            }
            let doc_dates = extract_all_dates_from_post(&post_text, title);
            let doc_date = doc_dates
                .first()
                .cloned()
                .or_else(|| normalize_date(title))
                .unwrap_or_else(|| main_date_str.clone());

            let content = match self.vk.download_bytes(doc_url).await {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!(
                        "⚠️ Ошибка загрузки звонков {file_unique_id}: {e}"
                    );
                    post_had_retryable_error = true;
                    continue;
                }
            };
            if content.is_empty() {
                self.register_processed_id(&file_unique_id);
                continue;
            }
            let parsed_zvonki = match parse_zvonki_docx(&content) {
                Ok(parsed) => parsed,
                Err(e) => {
                    tracing::warn!(
                        "⚠️ Ошибка обработки документа звонков в посте {}: {e}",
                        post.id
                    );
                    continue;
                }
            };

            let is_zv_past = parse_date(&doc_date).map(|d| d < today).unwrap_or(false);

            // Чаты, у которых в этот день есть пары.
            let mut eligible_groups: Vec<String> = Vec::new();
            for chat_list in chats_by_group.values() {
                let Some(sample_g) = chat_list.first().map(|(_, g)| g.clone()) else {
                    continue;
                };
                if self.group_has_classes(&sample_g, &doc_date) {
                    for (_, g_name) in chat_list {
                        if !eligible_groups.contains(g_name) {
                            eligible_groups.push(g_name.clone());
                        }
                    }
                }
            }

            if !parsed_zvonki.is_empty() && !is_zv_past {
                match serde_json::to_string(&parsed_zvonki) {
                    Ok(json) => {
                        if let Err(e) = self
                            .save_fast(&doc_date, "__ZVONKI__", &json, "звонки")
                            .await
                        {
                            tracing::warn!(
                                "⚠️ Ошибка сохранения звонков в посте {}: {e}",
                                post.id
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("⚠️ Сериализация звонков: {e}");
                    }
                }
                // Прямая рассылка docx/rich-алерта по чатам в Python —
                // в Rust не делается (нет Bot-хендла, см. шапку файла).
            }

            let zv_change_batch: Vec<(String, String, String, String)> = eligible_groups
                .iter()
                .map(|g| {
                    (
                        doc_date.clone(),
                        g.clone(),
                        "Расписание звонков обновлено".to_string(),
                        "изменения".to_string(),
                    )
                })
                .collect();
            if !zv_change_batch.is_empty() {
                if let Err(e) = self.save_fast_batch_inner(&zv_change_batch).await {
                    tracing::warn!(
                        "⚠️ Ошибка сохранения изменений звонков в посте {}: {e}",
                        post.id
                    );
                }
            }

            let file = ScheduleFile {
                file_unique_id: file_unique_id.clone(),
                date_str: doc_date.clone(),
                file_type: "zvonki".to_string(),
                title: title.clone(),
                file_id: None,
                vk_url: doc_url.clone(),
            };
            if let Err(e) = self.db.save_schedule_file(&file).await {
                tracing::warn!("⚠️ save_schedule_file: {e}");
            }
            self.save_file_to_cache(
                &file_unique_id,
                &doc_date,
                "zvonki",
                title,
                None,
                doc_url,
            );
        }

        // === 3. Фото сокращённых звонков ===
        if !photo_urls.is_empty() && (is_post_zvonki || post_has_schedule_docx) {
            let target_photo_dates: Vec<String> = if !post_schedule_dates.is_empty() {
                post_schedule_dates.clone()
            } else if !post_dates.is_empty() {
                post_dates.clone()
            } else {
                vec![main_date_str.clone()]
            };
            let primary_photo_date = target_photo_dates
                .first()
                .cloned()
                .unwrap_or_else(|| main_date_str.clone());

            for photo_url in &photo_urls {
                let photo_unique_id = format!("photo_{photo_url}");
                if self.is_processed_file(&photo_unique_id) {
                    continue;
                }
                let mut photo_file_records: Vec<ScheduleFile> = Vec::new();
                for p_date in &target_photo_dates {
                    let p_uid = if target_photo_dates.len() == 1 {
                        photo_unique_id.clone()
                    } else {
                        format!("{photo_unique_id}_{p_date}")
                    };
                    self.save_file_to_cache(
                        &p_uid,
                        p_date,
                        "zvonki_photo",
                        "Расписание по времени (фото)",
                        None,
                        photo_url,
                    );
                    photo_file_records.push(ScheduleFile {
                        file_unique_id: p_uid,
                        date_str: p_date.clone(),
                        file_type: "zvonki_photo".to_string(),
                        title: "Расписание по времени (фото)".to_string(),
                        file_id: None,
                        vk_url: photo_url.clone(),
                    });
                }
                self.register_processed_id(&photo_unique_id);
                if !photo_file_records.is_empty() {
                    if let Err(e) = self
                        .db
                        .save_schedule_files_batch(&photo_file_records)
                        .await
                    {
                        tracing::warn!("⚠️ save_schedule_files_batch (фото): {e}");
                    }
                }

                // Прогрев байтов картинки с VK CDN в RAM.
                if let Err(e) = self.download_photo_bytes(photo_url).await {
                    tracing::warn!("⚠️ Не удалось скачать фото с VK ({e}): {photo_url}");
                }

                // Прямая рассылка фото по чатам в Python — в Rust не
                // делается (нет Bot-хендла): сохраняем ссылку в кэш,
                // доставка — через get_bells_photo_input в bot.rs.
                if let Ok(mut bells) = self.bells_photo_cache.try_write() {
                    if post_has_schedule_docx {
                        for p_date in &target_photo_dates {
                            bells.insert(p_date.clone(), photo_url.clone());
                        }
                    } else {
                        bells.insert(primary_photo_date.clone(), photo_url.clone());
                    }
                }
            }
        }

        Ok(post_had_retryable_error)
    }

    /// parse_text_complex (services.py:941): текст поста -> записи
    /// (дата, группа, строка) для архива типа "изменения".
    async fn parse_text_complex(
        &self,
        text: &str,
        default_date: &str,
        all_chats: &[(i64, String)],
    ) -> Vec<(String, String, String)> {
        let mut results: Vec<(String, String, String)> = Vec::new();
        let known_groups_map: HashMap<String, String> = all_chats
            .iter()
            .map(|(_, g)| (normalize_group(g, ""), g.clone()))
            .collect();
        let mut current_date = default_date.to_string();
        let mut active_target_groups: Vec<String> = Vec::new();

        for line in text.lines() {
            let line_clean = line.trim();
            if line_clean.is_empty() {
                continue;
            }
            if let Some(new_date) = standalone_date_line(line_clean) {
                current_date = new_date;
                active_target_groups.clear();
                continue;
            }

            let mut line_for_search = translate_latin(line_clean);
            line_for_search = remove_mdk_codes_local(&line_for_search);
            line_for_search = line_for_search
                .chars()
                .map(|c| match c {
                    '–' | '—' | '−' => '-',
                    other => other,
                })
                .collect();

            let (groups_alpha, spans) = search_alpha_groups(&line_for_search);
            let mut line_for_digit_chars: Vec<char> = line_for_search.chars().collect();
            for (s, e) in spans.iter().rev() {
                for idx in *s..*e {
                    if idx < line_for_digit_chars.len() {
                        line_for_digit_chars[idx] = ' ';
                    }
                }
            }
            let line_for_digit: String = line_for_digit_chars.iter().collect();
            let groups_digit = search_digit_groups(&line_for_digit);

            let mut all_detected_raw: Vec<String> = groups_alpha;
            all_detected_raw.extend(groups_digit);

            if !all_detected_raw.is_empty() {
                active_target_groups.clear();
                for g in &all_detected_raw {
                    let g_norm = normalize_group(g, "");
                    if g_norm.is_empty() {
                        continue;
                    }
                    // Голое '26-21' без ТД/ЮР — уведомляем обе группы.
                    if g_norm == "26-21" {
                        for special_g in ["ТД-26-21", "ЮР-26-21"] {
                            let target = known_groups_map
                                .get(special_g)
                                .cloned()
                                .unwrap_or_else(|| special_g.to_string());
                            if !active_target_groups.contains(&target) {
                                active_target_groups.push(target);
                            }
                        }
                    } else {
                        let target = known_groups_map
                            .get(&g_norm)
                            .cloned()
                            .unwrap_or_else(|| g_norm.clone());
                        if !active_target_groups.contains(&target) {
                            active_target_groups.push(target);
                        }
                    }
                }
                if !active_target_groups.is_empty() && !current_date.is_empty() {
                    for g in &active_target_groups {
                        results.push((
                            current_date.clone(),
                            g.clone(),
                            format!("⚠️ {line_clean}"),
                        ));
                    }
                }
            } else if !active_target_groups.is_empty() && !current_date.is_empty() {
                for g in &active_target_groups {
                    results.push((
                        current_date.clone(),
                        g.clone(),
                        format!("ℹ️ {line_clean}"),
                    ));
                }
            }
        }
        results
    }
}

// --- Сканеры для parse_text_complex (RE_SEARCH_ALPHA / RE_SEARCH_DIGIT) --

const EXCLUDED_ALPHA_WORDS: &[&str] = &[
    "МДК", "ОП", "УП", "ПП", "ПМ", "НА", "ДО", "ОТ", "ИЗ", "ЗА", "ПО", "ВО", "СО",
];

/// RE_SEARCH_ALPHA: `(?<![а-яёa-z0-9])([а-яё]{1,5})\s*[-− ]?\s*([2-9]\d)
/// \s*[-. \s−]\s*(\d{1,2})([а-яё])?(?![а-яё0-9])`, IGNORECASE.
/// Возвращает (группы "СЛОВО-NN-MM[б]", спэны совпадений в символах).
fn search_alpha_groups(line: &str) -> (Vec<String>, Vec<(usize, usize)>) {
    let ch: Vec<char> = line.chars().collect();
    let mut groups: Vec<String> = Vec::new();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < ch.len() {
        let prev_ok = i == 0 || !is_alpha_cyr_lat(ch[i - 1]) && !ch[i - 1].is_ascii_digit();
        if prev_ok && is_cyr(ch[i]) {
            // Максимальный забег кириллицы (до 5 букв — иначе нет совпадения).
            let mut j = i;
            while j < ch.len() && is_cyr(ch[j]) && j - i < 6 {
                j += 1;
            }
            let run_len = j - i;
            if run_len >= 1 && run_len <= 5 && (j >= ch.len() || !is_cyr(ch[j])) {
                let word: String = ch[i..j].iter().collect();
                let word_upper: String = word.chars().map(|c| {
                    c.to_uppercase().next().unwrap_or(c)
                }).collect();
                if !EXCLUDED_ALPHA_WORDS.contains(&word_upper.as_str()) {
                    let mut p = j;
                    while p < ch.len() && ch[p].is_whitespace() {
                        p += 1;
                    }
                    if p < ch.len() && (ch[p] == '-' || ch[p] == '−' || ch[p] == ' ') {
                        p += 1;
                    }
                    while p < ch.len() && ch[p].is_whitespace() {
                        p += 1;
                    }
                    if p + 1 < ch.len()
                        && ('2'..='9').contains(&ch[p])
                        && ch[p + 1].is_ascii_digit()
                    {
                        let g1: String = ch[p..p + 2].iter().collect();
                        let mut q = p + 2;
                        while q < ch.len() && ch[q].is_whitespace() {
                            q += 1;
                        }
                        if q < ch.len()
                            && (ch[q] == '-' || ch[q] == '.' || ch[q] == '−'
                                || ch[q].is_whitespace())
                        {
                            q += 1;
                            while q < ch.len() && ch[q].is_whitespace() {
                                q += 1;
                            }
                            let d_start = q;
                            while q < ch.len() && ch[q].is_ascii_digit() && q - d_start < 2 {
                                q += 1;
                            }
                            if q > d_start {
                                let g2: String = ch[d_start..q].iter().collect();
                                let mut suffix = String::new();
                                let mut end = q;
                                if q < ch.len() && is_cyr(ch[q]) {
                                    suffix.push(ch[q]);
                                    end = q + 1;
                                }
                                if end >= ch.len()
                                    || (!is_alpha_cyr_lat(ch[end])
                                        && !ch[end].is_ascii_digit())
                                {
                                    groups.push(format!("{word}-{g1}-{g2}{suffix}"));
                                    spans.push((i, end));
                                    i = end;
                                    continue;
                                }
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }
    (groups, spans)
}

/// RE_SEARCH_DIGIT: `(?<![\d.])([2-9]\d)\s*[-−]\s*(\d{1,2})(?![\d.])`.
fn search_digit_groups(line: &str) -> Vec<String> {
    let ch: Vec<char> = line.chars().collect();
    let mut groups: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i + 1 < ch.len() {
        let prev_ok = i == 0 || (!ch[i - 1].is_ascii_digit() && ch[i - 1] != '.');
        if prev_ok && ('2'..='9').contains(&ch[i]) && ch[i + 1].is_ascii_digit() {
            let g1: String = ch[i..i + 2].iter().collect();
            let mut p = i + 2;
            while p < ch.len() && ch[p].is_whitespace() {
                p += 1;
            }
            if p < ch.len() && (ch[p] == '-' || ch[p] == '−') {
                p += 1;
                while p < ch.len() && ch[p].is_whitespace() {
                    p += 1;
                }
                let d_start = p;
                while p < ch.len() && ch[p].is_ascii_digit() && p - d_start < 2 {
                    p += 1;
                }
                if p > d_start
                    && (p >= ch.len() || (!ch[p].is_ascii_digit() && ch[p] != '.'))
                {
                    let g2: String = ch[d_start..p].iter().collect();
                    groups.push(format!("{g1}-{g2}"));
                    i = p;
                    continue;
                }
            }
        }
        i += 1;
    }
    groups
}

/// _is_standalone_date_line (services.py:916).
fn standalone_date_line(line_clean: &str) -> Option<String> {
    if line_clean.chars().count() > 35 {
        return None;
    }
    let line_norm = translate_latin(line_clean);
    if remove_mdk_codes_local(&line_norm) != line_norm {
        return None;
    }
    if find_single_pair_marker(line_clean).is_some()
        || cell_has_pair_marker(line_clean)
    {
        return None;
    }
    let dash_norm: String = line_norm
        .chars()
        .map(|c| match c {
            '–' | '—' | '−' => '-',
            other => other,
        })
        .collect();
    if !search_digit_groups(&dash_norm).is_empty() {
        return None;
    }
    // Дата с текстовым месяцем.
    let lower = line_norm.to_lowercase();
    for stem in [
        "янв", "фев", "мар", "апр", "мая", "май", "июн", "июл", "авг", "сен", "окт",
        "ноя", "дек",
    ] {
        if lower.contains(stem) {
            return normalize_date(&line_norm);
        }
    }
    // Числовая дата с годом.
    if has_numeric_date_with_year(&line_norm) {
        return normalize_date(&line_norm);
    }
    // ^(?:на\s+|расписание\s+на\s+|изменения\s+на\s+)?\d{1,2}\.\d{1,2}
    // (?:\s*\([а-яёa-z]+\))?[:.]?$
    if matches_bare_date_line(&line_norm) {
        return normalize_date(&line_norm);
    }
    None
}

/// `\b\d{1,2}\.\d{1,2}\.\d{2,4}\b` в строке.
fn has_numeric_date_with_year(s: &str) -> bool {
    let ch: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i].is_ascii_digit() {
            let d_start = i;
            while i < ch.len() && ch[i].is_ascii_digit() && i - d_start < 2 {
                i += 1;
            }
            if i < ch.len() && ch[i] == '.' {
                let mut j = i + 1;
                let m_start = j;
                while j < ch.len() && ch[j].is_ascii_digit() && j - m_start < 2 {
                    j += 1;
                }
                if j > m_start && j < ch.len() && ch[j] == '.' {
                    let mut k = j + 1;
                    let y_start = k;
                    while k < ch.len() && ch[k].is_ascii_digit() && k - y_start < 4 {
                        k += 1;
                    }
                    if k - y_start >= 2
                        && (k >= ch.len() || !ch[k].is_ascii_digit())
                        && (d_start == 0 || !ch[d_start - 1].is_ascii_digit())
                    {
                        return true;
                    }
                }
            }
        } else {
            i += 1;
        }
    }
    false
}

/// `^(?:на\s+|расписание\s+на\s+|изменения\s+на\s+)?\d{1,2}\.\d{1,2}
/// (?:\s*\([а-яёa-z]+\))?[:.]?$` (без учёта регистра).
fn matches_bare_date_line(s: &str) -> bool {
    let t = s.trim().to_lowercase();
    let ch: Vec<char> = t.chars().collect();
    let mut p = 0usize;
    for prefix in ["изменения на", "расписание на", "на"] {
        let pc: Vec<char> = prefix.chars().collect();
        if ch.len() >= pc.len() && ch[..pc.len()] == pc[..] {
            // После префикса должен быть пробел (или конец — тогда не дата).
            if ch.len() > pc.len() && ch[pc.len()].is_whitespace() {
                p = pc.len();
                break;
            }
        }
    }
    while p < ch.len() && ch[p].is_whitespace() {
        p += 1;
    }
    let d_start = p;
    while p < ch.len() && ch[p].is_ascii_digit() && p - d_start < 2 {
        p += 1;
    }
    if p == d_start || p >= ch.len() || ch[p] != '.' {
        return false;
    }
    p += 1;
    let m_start = p;
    while p < ch.len() && ch[p].is_ascii_digit() && p - m_start < 2 {
        p += 1;
    }
    if p == m_start {
        return false;
    }
    while p < ch.len() && ch[p].is_whitespace() {
        p += 1;
    }
    if p < ch.len() && ch[p] == '(' {
        p += 1;
        let w_start = p;
        while p < ch.len() && (is_cyr(ch[p]) || ch[p].is_ascii_alphabetic()) {
            p += 1;
        }
        if p == w_start || p >= ch.len() || ch[p] != ')' {
            return false;
        }
        p += 1;
    }
    if p < ch.len() && (ch[p] == ':' || ch[p] == '.') {
        p += 1;
    }
    p == ch.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vk::{VkAttachment, VkPost};

    async fn temp_service() -> (Arc<ScheduleService>, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("settings.db");
        let db = Arc::new(Db::open(db_path.to_str().unwrap()).unwrap());
        db.init().await.unwrap();
        let svc = Arc::new(ScheduleService::new(db, String::new(), String::new()));
        (svc, dir)
    }

    /// Копия фикстуры tests/fixtures/settings.db (только чтение оригинала!).
    async fn fixture_service() -> (Arc<ScheduleService>, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("settings.db");
        std::fs::copy("tests/fixtures/settings.db", &db_path).unwrap();
        let db = Arc::new(Db::open(db_path.to_str().unwrap()).unwrap());
        db.init().await.unwrap();
        let svc = Arc::new(ScheduleService::new(db, String::new(), String::new()));
        svc.load_ram().await.unwrap();
        (svc, dir)
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn dead_chat_errors_full_list() {
        assert_eq!(DEAD_CHAT_ERRORS.len(), 15);
        for marker in [
            "topic_closed",
            "chat not found",
            "bot was blocked",
            "user is deactivated",
            " have no rights to send a message",
            "group chat was upgraded to a supergroup chat",
            "migrated to a supergroup",
            "peer_id_invalid",
        ] {
            assert!(DEAD_CHAT_ERRORS.contains(&marker), "{marker} missing");
        }
    }

    #[tokio::test]
    async fn fixture_day_yur_18_11() {
        let (svc, _dir) = fixture_service().await;
        let day = svc.format_day_data("ЮР-26-21", "18.11.2026");
        assert_eq!(day.target_group, "ЮР-26-21");
        assert_eq!(day.pairs.len(), 3);
        assert!(day.pairs.get(&1).unwrap().0.contains("безопасности"));
        assert_eq!(
            day.pairs.get(&2).unwrap().0,
            "География каб. 60".to_string()
        );
        assert_eq!(
            day.pairs.get(&3).unwrap().0,
            "Обществознание каб. 403".to_string()
        );
        assert!(day.changes.is_empty());

        // Неизвестная группа/дата — пустые пары и изменения.
        let empty = svc.format_day_data("ЮР-26-21", "01.01.2030");
        assert!(empty.pairs.is_empty() && empty.changes.is_empty());
        assert_eq!(empty.target_group, "");
    }

    #[tokio::test]
    async fn load_ram_fills_caches() {
        // Фикстура + свежие файлы (created_at = now): 14-дневное окно
        // get_all_active_files(14) иначе зависит от даты запуска теста.
        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("settings.db");
        std::fs::copy("tests/fixtures/settings.db", &db_path).unwrap();
        let db = Arc::new(Db::open(db_path.to_str().unwrap()).unwrap());
        db.init().await.unwrap();
        db.save_schedule_file(&ScheduleFile {
            file_unique_id: "test-uid-1".to_string(),
            date_str: "18.11.2026".to_string(),
            file_type: "schedule".to_string(),
            title: "18.11.2026.docx".to_string(),
            file_id: None,
            vk_url: "https://cdn.example/18.docx".to_string(),
        })
        .await
        .unwrap();
        db.save_schedule_file(&ScheduleFile {
            file_unique_id: "photo-test-1".to_string(),
            date_str: "19.11.2026".to_string(),
            file_type: "zvonki_photo".to_string(),
            title: "Расписание по времени (фото)".to_string(),
            file_id: None,
            vk_url: "https://cdn.example/bells.jpg".to_string(),
        })
        .await
        .unwrap();
        let svc = Arc::new(ScheduleService::new(db, String::new(), String::new()));
        svc.load_ram().await.unwrap();

        let cache = svc.archive_cache.read().unwrap();
        let groups: HashSet<&String> = cache
            .values()
            .flat_map(|m| m.keys())
            .filter(|g| *g != "__ZVONKI__")
            .collect();
        assert!(groups.len() > 0, "групп в кэше нет");
        assert!(groups.contains(&"ЮР-26-21".to_string()));
        drop(cache);

        assert!(svc.has_file_for_date("18.11.2026"));
        let f = svc
            .get_file_for_date("18.11.2026", Some("schedule"))
            .unwrap();
        assert_eq!(f.date_str, "18.11.2026");
        assert_eq!(f.file_type, "schedule");
        assert!(svc.get_file_for_date("18.11.2026", Some("zvonki")).is_none());
        let zv = svc.get_active_zvonki_files();
        assert!(zv.iter().any(|x| x.file_type == "zvonki_photo"
            && x.date_str == "19.11.2026"));
        let photo = svc.get_bells_photo_for_date("19.11.2026");
        assert_eq!(photo.as_deref(), Some("https://cdn.example/bells.jpg"));
        assert!(svc.get_bells_photo_for_date("17.11.2026").is_none());
    }

    #[tokio::test]
    async fn group_shift_from_pairs_and_fallback() {
        let (svc, _dir) = temp_service().await;
        svc.save_fast_batch(&[
            (
                "20.11.2026".to_string(),
                "ТД-26-21".to_string(),
                "<b>1п.</b> | Математика (каб. 1)\n<b>2п.</b> | Физика (каб. 2)".to_string(),
                "основное".to_string(),
            ),
            (
                "21.11.2026".to_string(),
                "ТД-26-21".to_string(),
                "<b>5п.</b> | МДК.01.01 (лаб. 207)\n<b>6п.</b> | МДК.01.01 (лаб. 207)".to_string(),
                "основное".to_string(),
            ),
        ])
        .await
        .unwrap();
        assert_eq!(svc.get_group_shift("ТД-26-21", date(2026, 11, 20)), 1);
        assert_eq!(svc.get_group_shift("ТД-26-21", date(2026, 11, 21)), 2);
        // Нет данных на 22.11 — fallback на последнюю дату архива (21.11).
        assert_eq!(svc.get_group_shift("ТД-26-21", date(2026, 11, 22)), 2);
        // Группа без данных вообще — 1.
        assert_eq!(svc.get_group_shift("26-99", date(2026, 11, 20)), 1);
    }

    #[test]
    fn zvonki_text_static_matches_python() {
        let svc = ScheduleService::new(
            Arc::new(Db::open(":memory:").unwrap()),
            String::new(),
            String::new(),
        );
        // 18.11.2026 — среда, ЮР-26-21 — 1 курс.
        let wed = svc.get_zvonki_text("ЮР-26-21", date(2026, 11, 18));
        assert!(wed.starts_with("<b>Среда • 1 курс</b>"), "{wed}");
        assert!(wed.contains("1п: 8:00 – 9:30"));
        assert!(wed.contains("Классный час: 13:30 – 14:00"));
        assert!(wed.contains("6п: 17:50 – 19:20"));
        // Понедельник.
        let mon = svc.get_zvonki_text("ЮР-26-21", date(2026, 11, 16));
        assert!(mon.contains("Понедельник • 1 курс"));
        assert!(mon.contains("Разговоры о важном: 13:30 – 14:00"));
        // Воскресенье — та же сетка, что в понедельник (Python: w in (0,6)).
        let sun = svc.get_zvonki_text("ЮР-26-21", date(2026, 11, 22));
        assert!(sun.contains("Понедельник • 1 курс"));
        // Четверг, 3,4 курс: 2-я смена 13:30–15:00, 3п 11:40–13:10.
        let thu = svc.get_zvonki_text("24-27", date(2026, 11, 19));
        assert!(thu.contains("Четверг–Суббота • 3,4 курс"));
        assert!(thu.contains("3п: 11:40 – 13:10"));
        assert!(thu.contains("4п: 13:30 – 15:00"));
        assert!(thu.contains("6п: 17:10 – 18:40"));
        // Вторник, 2 курс: классный час в обеих сменах.
        let tue = svc.get_zvonki_text("25-01", date(2026, 11, 17));
        assert!(tue.contains("Вторник • 2 курс"));
        assert_eq!(tue.matches("Классный час: 13:30 – 14:00").count(), 2);
    }

    #[tokio::test]
    async fn zvonki_resp_and_has_zvonki() {
        let (svc, _dir) = fixture_service().await;
        // Воскресенье без спец-звонков — нет звонков.
        assert!(!svc.has_zvonki_for_date("22.11.2026"));
        // Будний день — штатная сетка есть.
        assert!(svc.has_zvonki_for_date("18.11.2026"));
        let resp = svc.get_zvonki_resp("ЮР-26-21", date(2026, 11, 18));
        assert!(resp.contains("Среда • 1 курс</b> (18.11)"), "{resp}");
        assert!(resp.contains("1п: 8:00 – 9:30"));
        // Конец пар ЮР-26-21 18.11 (пары 1–3, 1-я смена) — 13:20.
        assert_eq!(
            svc.get_group_bells_end_time("ЮР-26-21", date(2026, 11, 18))
                .as_deref(),
            Some("13:20")
        );
        // Первая пара завтра без данных — 1-я смена -> 1п 8:00.
        let (svc2, _dir2) = temp_service().await;
        assert_eq!(
            svc2
                .get_tomorrow_first_bell_info("ЮР-26-21", date(2026, 11, 19))
                .as_deref(),
            Some("1п 8:00")
        );
    }

    #[tokio::test]
    async fn save_fast_dedup_and_photo_guard() {
        let (svc, _dir) = temp_service().await;
        assert!(svc
            .save_fast("20.11.2026", "ЮР-26-21", "<b>1п.</b> | Химия (каб. 206)", "основное")
            .await
            .unwrap());
        // Дубликат (с точностью до 🕒) не вставляется.
        assert!(!svc
            .save_fast("20.11.2026", "ЮР-26-21", "🕒 <b>1п.</b> | Химия (каб. 206)", "основное")
            .await
            .unwrap());
        // Строка с фото-маркером не сохраняется.
        assert!(!svc
            .save_fast(
                "20.11.2026",
                "ЮР-26-21",
                "🔔 Расписание по времени (фото) x",
                "основное"
            )
            .await
            .unwrap());
        let day = svc.format_day_data("ЮР-26-21", "20.11.2026");
        assert_eq!(day.pairs.len(), 1);
    }

    #[tokio::test]
    async fn update_check_without_token_is_empty() {
        let (svc, _dir) = temp_service().await;
        let events = svc.update_check(5).await.unwrap();
        assert!(events.is_empty());
    }

    fn fake_post(id: i64, text: &str) -> VkPost {
        VkPost {
            id,
            date: 1793123456,
            text: text.to_string(),
            attachments: vec![],
        }
    }

    #[tokio::test]
    async fn process_vk_posts_text_changes_and_dedup() {
        let (svc, _dir) = temp_service().await;
        svc.db.set_group(1, "ЮР-26-21").await.unwrap();
        svc.db.set_group(2, "ТД-26-21").await.unwrap();

        let posts = vec![
            fake_post(101, "Расписание на 20.11.2026\nЮР-26-21 1п. отмена"),
            fake_post(
                102,
                "Замена 20.11.2026\nТД-26-21 2п. перенос\n3п. в каб. 302",
            ),
        ];
        let events = svc.process_vk_posts(&posts).await.unwrap();
        assert!(events.contains(&(
            "ЮР-26-21".to_string(),
            "20.11.2026".to_string(),
            "change".to_string()
        )));
        assert!(events.contains(&(
            "ТД-26-21".to_string(),
            "20.11.2026".to_string(),
            "change".to_string()
        )));

        // Записи попали в архив и видны через format_day_data.
        let day = svc.format_day_data("ЮР-26-21", "20.11.2026");
        assert!(day.changes.iter().any(|c| c.contains("1п. отмена")));
        let day_td = svc.format_day_data("ТД-26-21", "20.11.2026");
        assert_eq!(day_td.changes.len(), 2);

        // Повторная обработка тех же постов — пустое множество (дедуп).
        let again = svc.process_vk_posts(&posts).await.unwrap();
        assert!(again.is_empty());

        // Пост без групп и дат не даёт событий.
        let none = svc
            .process_vk_posts(&[fake_post(103, "Доброе утро, колледж!")])
            .await
            .unwrap();
        assert!(none.is_empty());

        // Голое "26-21" — события для обеих групп ТД/ЮР-26-21.
        let both = svc
            .process_vk_posts(&[fake_post(
                104,
                "Изменения на 21.11.2026\n26-21 1п. отмена",
            )])
            .await
            .unwrap();
        assert!(both.contains(&(
            "ТД-26-21".to_string(),
            "21.11.2026".to_string(),
            "change".to_string()
        )));
        assert!(both.contains(&(
            "ЮР-26-21".to_string(),
            "21.11.2026".to_string(),
            "change".to_string()
        )));
    }

    #[tokio::test]
    async fn process_vk_posts_docx_attachment_offline() {
        // Docx-вложение без сети: download_bytes упадёт (нет URL-доступа),
        // пост не запоминается как обработанный (retryable), событий нет.
        let (svc, _dir) = temp_service().await;
        svc.db.set_group(1, "ЮР-26-21").await.unwrap();
        let post = VkPost {
            id: 201,
            date: 1793123456,
            text: "Замена 20.11.2026".to_string(),
            attachments: vec![VkAttachment {
                kind: "doc".to_string(),
                // 127.0.0.1:1 — connection refused сразу, без DNS-таймаутов.
                url: Some("https://127.0.0.1:1/20.11.2026.docx".to_string()),
                title: Some("20.11.2026.docx".to_string()),
            }],
        };
        let events = svc.process_vk_posts(std::slice::from_ref(&post)).await.unwrap();
        assert!(events.is_empty());
        // Пост не запомнен — повторная обработка снова пытается скачать.
        let again = svc.process_vk_posts(std::slice::from_ref(&post)).await.unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn standalone_date_and_alpha_search() {
        assert_eq!(
            standalone_date_line("Расписание на 20.11.2026").as_deref(),
            Some("20.11.2026")
        );
        assert_eq!(
            standalone_date_line("20 ноября").as_deref(),
            Some("20.11.2026")
        );
        // Строка с группой — не заголовок даты.
        assert_eq!(standalone_date_line("26-21 1п. отмена"), None);
        // Код МДК — не заголовок даты.
        assert_eq!(standalone_date_line("МДК.01.02"), None);
        let (groups, _) = search_alpha_groups("ЮР-26-21 1п. отмена");
        assert_eq!(groups, vec!["ЮР-26-21".to_string()]);
        let (groups, _) = search_alpha_groups("26-21б 1п. отмена");
        assert!(groups.is_empty() || groups == vec!["26-21".to_string()]);
        assert_eq!(search_digit_groups("24-27 и 26-14"), vec!["24-27", "26-14"]);
    }

    /// Группировка дампа архива по (дата, группа) — общая основа тестов
    /// по всем группам фикстуры (пропуск пустых/фото-строк, как load_ram).
    fn group_dump(
        dump: &[(String, String, String, String)],
    ) -> BTreeMap<(String, String), Vec<(String, String)>> {
        let mut by_key: BTreeMap<(String, String), Vec<(String, String)>> =
            BTreeMap::new();
        for (d, g, c, t) in dump {
            if c.is_empty() || c.contains("🔔 Расписание по времени (фото)") {
                continue;
            }
            by_key
                .entry((d.clone(), g.clone()))
                .or_default()
                .push((c.clone(), t.clone()));
        }
        by_key
    }

    #[tokio::test]
    async fn format_day_data_all_groups_fixture() {
        let (svc, _dir) = fixture_service().await;
        let dump = svc.db.get_full_archive_dump().await.unwrap();
        let by_key = group_dump(&dump);
        // 3529 комбинаций дата×группа в фикстуре.
        assert!(by_key.len() > 3000, "мало комбинаций: {}", by_key.len());
        let mut checked = 0usize;
        for ((date, group), records) in &by_key {
            if group == "__ZVONKI__" {
                continue;
            }
            let day = svc.format_day_data(group, date);
            let (exp_pairs, exp_changes) = parser_format_day_data(records);
            assert_eq!(day.target_group, *group, "target_group {date} {group}");
            assert_eq!(day.pairs, exp_pairs, "pairs {date} {group}");
            assert_eq!(day.changes, exp_changes, "changes {date} {group}");
            checked += 1;
        }
        assert!(checked > 3000, "checked={checked}");
    }

    #[tokio::test]
    async fn group_shift_all_groups_fixture() {
        let (svc, _dir) = fixture_service().await;
        let dump = svc.db.get_full_archive_dump().await.unwrap();
        let by_key = group_dump(&dump);
        let mut checked = 0usize;
        let mut shift2 = 0usize;
        let mut shift1 = 0usize;
        for ((date, group), records) in &by_key {
            if group == "__ZVONKI__" {
                continue;
            }
            let (pairs, changes) = parser_format_day_data(records);
            let mut all: HashSet<i32> = pairs.keys().copied().collect();
            for ch in &changes {
                all.extend(extract_pair_numbers(ch));
            }
            if all.is_empty() {
                // Пустой день — смена берётся из fallback на последнюю
                // дату архива, из одной записи её не предсказать.
                continue;
            }
            let expected = if all.iter().any(|p| *p >= 5) { 2 } else { 1 };
            let got = svc.get_group_shift(group, parse_date(date).unwrap());
            assert_eq!(got, expected, "shift {date} {group}: all={all:?}");
            checked += 1;
            if expected == 2 {
                shift2 += 1;
            } else {
                shift1 += 1;
            }
        }
        assert!(checked > 3000, "checked={checked}");
        assert!(shift2 >= 600, "2-я смена почти не покрыта: {shift2}");
        assert!(shift1 >= 2000, "1-я смена почти не покрыта: {shift1}");
    }

    #[tokio::test]
    async fn bells_end_time_all_groups_18_11() {
        let (svc, _dir) = fixture_service().await;
        // Точные значения для известных случаев 18.11.2026 (среда):
        // 23-10 и 24-01 — пары 4–6, 2-я смена -> 19:20;
        // 25-05 — только 1п -> 9:30; 26-14 — пары 1–3 -> 13:20.
        assert_eq!(
            svc.get_group_bells_end_time("23-10", date(2026, 11, 18))
                .as_deref(),
            Some("19:20")
        );
        assert_eq!(
            svc.get_group_bells_end_time("24-01", date(2026, 11, 18))
                .as_deref(),
            Some("19:20")
        );
        assert_eq!(
            svc.get_group_bells_end_time("25-05", date(2026, 11, 18))
                .as_deref(),
            Some("9:30")
        );
        assert_eq!(
            svc.get_group_bells_end_time("26-14", date(2026, 11, 18))
                .as_deref(),
            Some("13:20")
        );
        // Все группы 18.11 с парами — валидное время в пределах дня.
        let dump = svc.db.get_full_archive_dump().await.unwrap();
        let by_key = group_dump(&dump);
        let mut checked = 0usize;
        for ((d, group), records) in &by_key {
            if d != "18.11.2026" || group == "__ZVONKI__" {
                continue;
            }
            let (pairs, _) = parser_format_day_data(records);
            if pairs.is_empty() {
                continue;
            }
            let end = svc
                .get_group_bells_end_time(group, date(2026, 11, 18))
                .unwrap_or_else(|| panic!("нет конца пар: {group}"));
            let (h, m) = end.split_once(':').unwrap();
            let mins: i32 = h.parse::<i32>().unwrap() * 60 + m.parse::<i32>().unwrap();
            assert!(
                (8 * 60..=19 * 60 + 20).contains(&mins),
                "конец пар вне учебного дня: {group} -> {end}"
            );
            checked += 1;
        }
        assert!(checked >= 70, "групп 18.11 с парами: {checked}");
    }

    #[test]
    fn zvonki_text_courses_across_groups() {
        let svc = ScheduleService::new(
            Arc::new(Db::open(":memory:").unwrap()),
            String::new(),
            String::new(),
        );
        // Группы разных годов поступления -> разные курсы:
        // 23-xx и 24-xx — 3,4 курс; 25-xx — 2 курс; 26-xx — 1 курс.
        let cases: &[(&str, &str)] = &[
            ("23-05", "3,4 курс"),
            ("24-27", "3,4 курс"),
            ("25-05", "2 курс"),
            ("26-14", "1 курс"),
            ("ТД-26-21", "1 курс"),
            ("ЮР-26-21", "1 курс"),
            ("26-21", "1 курс"),
        ];
        let days: &[(NaiveDate, &str)] = &[
            (date(2026, 11, 16), "Понедельник"),
            (date(2026, 11, 17), "Вторник"),
            (date(2026, 11, 18), "Среда"),
            (date(2026, 11, 19), "Четверг–Суббота"),
        ];
        for (group, course) in cases {
            for (d, day_name) in days {
                let text = svc.get_zvonki_text(group, *d);
                assert!(
                    text.contains(&format!("{day_name} • {course}")),
                    "{group} {d}: {text}"
                );
                let resp = svc.get_zvonki_resp(group, *d);
                assert!(
                    resp.contains(&format!("{day_name} • {course}")),
                    "resp {group} {d}: {resp}"
                );
            }
        }
    }

    #[tokio::test]
    async fn load_ram_covers_all_archive_groups() {
        let (svc, _dir) = fixture_service().await;
        let dump = svc.db.get_full_archive_dump().await.unwrap();
        let mut exp_dates: HashSet<String> = HashSet::new();
        let mut exp_groups: HashSet<String> = HashSet::new();
        let mut exp_records = 0usize;
        for (d, g, c, _) in &dump {
            if c.is_empty() || c.contains("🔔 Расписание по времени (фото)") {
                continue;
            }
            exp_dates.insert(d.clone());
            exp_groups.insert(g.clone());
            exp_records += 1;
        }
        // В фикстуре нет ни пустых, ни фото-строк — все 3811 записей в кэше.
        assert_eq!(exp_records, 3811);
        assert_eq!(exp_groups.len(), 81);
        let cache = svc.archive_cache.read().unwrap();
        let cache_dates: HashSet<String> = cache.keys().cloned().collect();
        assert_eq!(cache_dates, exp_dates);
        let cache_groups: HashSet<String> =
            cache.values().flat_map(|m| m.keys().cloned()).collect();
        assert_eq!(cache_groups, exp_groups);
        let cache_records: usize = cache
            .values()
            .flat_map(|m| m.values())
            .map(|v| v.len())
            .sum();
        assert_eq!(cache_records, exp_records);
        drop(cache);
        assert_eq!(svc.db.get_archive_groups().await.unwrap().len(), 81);
    }

    #[tokio::test]
    async fn process_vk_posts_multiple_groups() {
        let (svc, _dir) = temp_service().await;
        for (id, g) in [(1, "25-05"), (2, "24-27"), (3, "23-10")] {
            svc.db.set_group(id, g).await.unwrap();
        }
        let post = fake_post(
            301,
            "Изменения на 21.11.2026\n25-05 1п. отмена\n24-27 2п. перенос\n23-10 и 25-05 3п. в каб. 100",
        );
        let events = svc
            .process_vk_posts(std::slice::from_ref(&post))
            .await
            .unwrap();
        for g in ["25-05", "24-27", "23-10"] {
            assert!(
                events.contains(&(
                    g.to_string(),
                    "21.11.2026".to_string(),
                    "change".to_string()
                )),
                "нет события для {g}: {events:?}"
            );
        }
        // 25-05 упомянута в двух строках — две записи изменений.
        let day = svc.format_day_data("25-05", "21.11.2026");
        assert_eq!(day.changes.len(), 2);
        assert!(day.changes.iter().any(|c| c.contains("1п. отмена")));
        assert!(day.changes.iter().any(|c| c.contains("3п. в каб. 100")));
    }

    // ---------- Rich Messages ----------

    fn tyumen_dt(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<FixedOffset> {
        use chrono::TimeZone;
        crate::tyumen::tyumen_offset()
            .from_local_datetime(
                &NaiveDate::from_ymd_opt(y, mo, d)
                    .unwrap()
                    .and_hms_opt(h, mi, 0)
                    .unwrap(),
            )
            .unwrap()
    }

    #[test]
    fn make_tg_time_before_and_during_pair() {
        // Среда 18.11.2026, 08:30 — пара 8:00–9:30 идёт: отсчёт до 9:30.
        let now = tyumen_dt(2026, 11, 18, 8, 30);
        let tag = make_tg_time("8:00–9:30", Some(date(2026, 11, 18)), now);
        let end_unix = crate::tyumen::tyumen_datetime(
            date(2026, 11, 18),
            chrono::NaiveTime::from_hms_opt(9, 30, 0).unwrap(),
        )
        .timestamp();
        assert_eq!(
            tag,
            format!("<tg-time unix=\"{end_unix}\" format=\"r\">до 9:30</tg-time>")
        );
        // Пара ещё не началась — отсчёт до её начала.
        let tag2 = make_tg_time("10:10–11:40", Some(date(2026, 11, 18)), now);
        let start_unix = crate::tyumen::tyumen_datetime(
            date(2026, 11, 18),
            chrono::NaiveTime::from_hms_opt(10, 10, 0).unwrap(),
        )
        .timestamp();
        assert_eq!(
            tag2,
            format!("<tg-time unix=\"{start_unix}\" format=\"r\">10:10</tg-time>")
        );
        // Нет времён / невалидное время — «—».
        assert_eq!(make_tg_time("обед", None, now), "—");
        assert_eq!(
            make_tg_time("25:00–26:00", Some(date(2026, 11, 18)), now),
            "—"
        );
        // Воскресенье без base_date — база завтра (понедельник).
        let sun = tyumen_dt(2026, 11, 22, 10, 0);
        let tag3 = make_tg_time("8:00", None, sun);
        let mon_unix = crate::tyumen::tyumen_datetime(
            date(2026, 11, 23),
            chrono::NaiveTime::from_hms_opt(8, 0, 0).unwrap(),
        )
        .timestamp();
        assert_eq!(
            tag3,
            format!("<tg-time unix=\"{mon_unix}\" format=\"r\">8:00</tg-time>")
        );
    }

    #[test]
    fn highlight_pair_list_marker_cases() {
        assert_eq!(
            highlight_pair_list_marker("1, 2п. — Химия"),
            "<mark><b>1, 2п.</b></mark> — Химия"
        );
        assert_eq!(
            highlight_pair_list_marker("1-3п. отмена"),
            "<mark><b>1-3п.</b></mark> отмена"
        );
        assert_eq!(
            highlight_pair_list_marker("3п. Математика"),
            "<mark><b>3п.</b></mark> Математика"
        );
        assert_eq!(
            highlight_pair_list_marker("1 и 2 пары"),
            "<mark><b>1 и 2 пары</b></mark>"
        );
        // 10-я пара — не маркер (0 не входит в [1-8]).
        assert_eq!(highlight_pair_list_marker("10п. — нет"), "10п. — нет");
        // Двузначный диапазон — не маркер.
        assert_eq!(
            highlight_pair_list_marker("21-23п. перенос"),
            "21-23п. перенос"
        );
        // «парк» — не «пар».
        assert_eq!(highlight_pair_list_marker("1 парк"), "1 парк");
        // Несколько маркеров в одной строке.
        assert_eq!(
            highlight_pair_list_marker("1п. Химия, 2п. Физика"),
            "<mark><b>1п.</b></mark> Химия, <mark><b>2п.</b></mark> Физика"
        );
    }

    #[tokio::test]
    async fn render_rich_table_matches_python() {
        let (svc, _dir) = fixture_service().await;
        let pairs: BTreeMap<i32, (String, String)> = BTreeMap::from([
            (1, ("Химия".to_string(), "206".to_string())),
            (2, ("Физика".to_string(), "101".to_string())),
            (3, ("Математика".to_string(), "—".to_string())),
        ]);
        let changes = vec!["<b>Замена</b>: 1 пара — Химия".to_string()];
        let rich = svc.render_rich_table_html(
            "24-27",
            "18.11.2026",
            &pairs,
            &changes,
            Some("ЮР-26-21"),
            false,
            false,
        );
        // Заголовок — display_group из target_group_name (как в Python).
        assert!(
            rich.starts_with("<h3>Группа ЮР-26-21 • 18.11.2026 • Среда</h3>\n"),
            "{rich}"
        );
        assert!(rich.contains("<table bordered striped>"));
        // Изменённая пара подсвечена <mark>.
        assert!(rich.contains("<mark><b>1п ⚠️</b></mark>"), "{rich}");
        assert!(rich.contains("<mark>206</mark>"), "{rich}");
        assert!(rich.contains("<b>2п</b>"), "{rich}");
        // Замены: <b> срезан, маркер «1 пара» подсвечен.
        assert!(
            rich.contains(
                "<blockquote><b>Замены:</b><br>\n• Замена: <mark><b>1 пара</b></mark> — Химия</blockquote>"
            ),
            "{rich}"
        );
        assert!(rich.contains("\n<hr/>\n"), "{rich}");
        // wrap_details: вкладка с бейджем «замены».
        let wrapped = svc.render_rich_table_html(
            "24-27",
            "18.11.2026",
            &pairs,
            &changes,
            Some("ЮР-26-21"),
            true,
            true,
        );
        assert!(
            wrapped.starts_with(
                "<details open>\n<summary><b>18.11.2026 • Среда • замены</b></summary>"
            ),
            "{wrapped}"
        );
        // Пусто — «Пар нет».
        let empty = svc.render_rich_table_html(
            "24-27",
            "18.11.2026",
            &BTreeMap::new(),
            &[],
            None,
            false,
            false,
        );
        assert_eq!(
            empty,
            "<h3>Группа 24-27 • 18.11.2026 • Среда</h3>\n\n<i>Пар нет</i>"
        );
    }

    #[tokio::test]
    async fn zvonki_resp_rich_raw_and_parsed() {
        let (svc, _dir) = fixture_service().await;
        let now = tyumen_dt(2026, 11, 18, 8, 0);
        // Штатная сетка (raw-ветка): fb — с tg-time в скобках, rich — таблица.
        let (fb, rich) =
            svc.get_zvonki_resp_rich("ЮР-26-21", Some(date(2026, 11, 18)), false, now);
        assert!(fb.contains("Среда • 1 курс</b> (18.11)"), "{fb}");
        assert!(fb.contains("1п: 8:00 – 9:30 (<tg-time"), "{fb}");
        let rich = rich.expect("rich");
        assert!(rich.contains("<h3>Среда • 1 курс (18.11)</h3>"), "{rich}");
        assert!(rich.contains("<table bordered striped compact>"), "{rich}");
        assert!(rich.contains("<tg-time"), "{rich}");
        // Распарсенные звонки (parsed-ветка): fb выводится из rich.
        let mut parsed = BTreeMap::new();
        parsed.insert(
            "all_courses".to_string(),
            vec![
                "1п: 8:00 – 9:30".to_string(),
                "2п: 10:10 – 11:40".to_string(),
                "Обед: 9:30 – 10:10".to_string(),
            ],
        );
        svc.zvonki_cache
            .write()
            .unwrap()
            .insert("18.11.2026".to_string(), parsed);
        let (fb2, rich2) =
            svc.get_zvonki_resp_rich("ЮР-26-21", Some(date(2026, 11, 18)), false, now);
        let rich2 = rich2.expect("rich2");
        assert!(
            rich2.contains("<h3>Расписание звонков на 18.11.2026</h3>"),
            "{rich2}"
        );
        assert!(
            rich2.contains("<b>1п</b></td><td align=\"center\">8:00 – 9:30</td>"),
            "{rich2}"
        );
        assert!(rich2.contains("<td align=\"center\">Обед</td>"), "{rich2}");
        // fb — та же таблица без тегов table/tr/td.
        assert!(!fb2.contains("<table"), "{fb2}");
        assert!(fb2.contains("1п"), "{fb2}");
        assert!(fb2.contains("8:00 – 9:30"), "{fb2}");
    }

    #[tokio::test]
    async fn format_parsed_zvonki_rich_shifts() {
        let (svc, _dir) = fixture_service().await;
        let now = tyumen_dt(2026, 11, 18, 8, 0);
        let mut parsed = BTreeMap::new();
        parsed.insert(
            "all_courses".to_string(),
            vec![
                "1п: 8:00 – 9:30".to_string(),
                "2п: 10:10 – 11:40".to_string(),
                "5п: 14:00 – 15:30".to_string(),
                "6п: 15:40 – 17:10".to_string(),
            ],
        );
        let rich = svc.format_parsed_zvonki_rich(
            "18.11.2026",
            "1 курс",
            &parsed,
            Some("ЮР-26-21"),
            now,
        );
        // 2-я смена (5–6 пары) — в отдельном <details>, 1-я раскрыта (смена 1).
        assert!(
            rich.contains("<details open>\n<summary><b>1 смена</b></summary>"),
            "{rich}"
        );
        assert!(
            rich.contains("<details>\n<summary><b>2 смена</b></summary>"),
            "{rich}"
        );
        assert!(rich.contains("5п</b>"), "{rich}");
        // Нет строк для курса — пусто.
        let empty = svc.format_parsed_zvonki_rich(
            "18.11.2026",
            "3,4 курс",
            &BTreeMap::new(),
            None,
            now,
        );
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn tomorrow_bell_button_and_progress() {
        let (svc, _dir) = fixture_service().await;
        let now = tyumen_dt(2026, 11, 18, 12, 0);
        let btn = svc.get_tomorrow_bell_button_html("ЮР-26-21", date(2026, 11, 19), now);
        assert!(btn.contains("data=\"zvonki_date:19.11.2026\""), "{btn}");
        assert!(btn.contains("Звонки на завтра (19.11)"), "{btn}");
        assert!(btn.starts_with("<tg-button-row"), "{btn}");
        // Прогресс: среда 08:30 — 1-я пара идёт.
        let progress =
            svc.current_pair_progress_rich("ЮР-26-21", tyumen_dt(2026, 11, 18, 8, 30));
        assert!(
            progress.contains("<b>1 пара</b> • до звонка <tg-time"),
            "{progress}"
        );
        // Ночью пар нет — пусто.
        assert!(svc
            .current_pair_progress_rich("ЮР-26-21", tyumen_dt(2026, 11, 18, 23, 0))
            .is_empty());
        // Воскресенье — выходной.
        assert_eq!(
            svc.current_pair_progress_rich("ЮР-26-21", tyumen_dt(2026, 11, 22, 10, 0)),
            "<blockquote>Воскресенье — выходной</blockquote>"
        );
    }

    #[tokio::test]
    async fn bells_photo_input_typed() {
        let (svc, _dir) = fixture_service().await;
        // Нет фото — None.
        assert!(svc
            .get_bells_photo_input("17.11.2026")
            .await
            .unwrap()
            .is_none());
        // Захваченный file_id — возвращается как FileId, без скачивания.
        svc.bells_photo_cache
            .write()
            .unwrap()
            .insert("20.11.2026".to_string(), "FILEID123".to_string());
        match svc.get_bells_photo_input("20.11.2026").await.unwrap() {
            Some(BellsPhotoInput::FileId(fid)) => assert_eq!(fid, "FILEID123"),
            other => panic!("unexpected {other:?}"),
        }
        // URL с недоступным хостом — скачивание не удалось → None.
        svc.bells_photo_cache
            .write()
            .unwrap()
            .insert(
                "21.11.2026".to_string(),
                "http://127.0.0.1:1/bells.jpg".to_string(),
            );
        assert!(svc
            .get_bells_photo_input("21.11.2026")
            .await
            .unwrap()
            .is_none());
    }
}
