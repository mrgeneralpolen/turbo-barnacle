//! Чистые функции парсинга расписания — точная копия логики Python.
//!
//! Эталоны:
//! - `~/workspace/schedule-miniapp/work/extract_real_days.py`
//!   (parse_schedule_line, format_day_data, _extract_pair_numbers,
//!   _clean_change, fix_room — stdlib-копия бота)
//! - `~/workspace/user/files/services.py`: normalize_group (~762),
//!   normalize_date (~728), _infer_year (~717), extract_all_dates_from_post
//!   (~836), get_course_str (~2707), get_group_shift (~2749).
//!
//! Правила, которые НЕЛЬЗЯ менять:
//! - parse_schedule_line: номер пары из `(\d+)\s*п`, предмет/кабинет после `|`,
//!   кабинет — из скобок в конце `(...)`; иначе "—"/"—".
//! - format_day_data: дедупликация строк, тип "изменения" -> changes,
//!   слияние дубликатов пар (кабинет "—" заменяется; "A / B" при разных предметах).
//! - _extract_pair_numbers: маркер списка пар 1-8 с разделителями , – — − и,
//!   диапазоны "1-3" раскрываются.
//! - fix_room: кабинет вне скобок ("... каб. 34 (—)") переезжает в room.
//! - normalize_group: префиксы специальностей, "б" в конце, "26-21" неоднозначна.
//! - Смена: пара №>=5 -> 2-я смена, иначе 1-я.
//! - KNOWN_SPECIALTY_PREFIXES — как в services.py:110.
//!
//! Замечание по regex: в Python здесь используются lookahead/lookbehind
//! (`(?<![\d.-])`, `(?![а-яёa-z])` и т.п.), которые крейт `regex` не
//! поддерживает. Поэтому маркеры пар, дат и групп реализованы ручными
//! сканерами по символам с той же семантикой (проверено на примерах
//! из services.py / extract_real_days.py, см. тесты внизу файла).

use crate::tyumen::tyumen_now;
use chrono::Datelike;
use std::collections::{BTreeMap, HashSet};

/// Префиксы специальностей (services.py:110).
pub const KNOWN_SPECIALTY_PREFIXES: &[&str] = &[
    "БН", "БУХ", "ЗУ", "ИБ", "ИВМ", "ИП", "ИР", "ИС", "НАЛ", "ОПД", "ОТС", "ПД", "ПКД",
    "ПСО", "РПО", "СА", "СР", "СТР", "СЭЗ", "ТГ", "ТД", "ТО", "ТХ", "ТЭС", "ФИН", "ЮР",
];

/// День одной группы: пары {№: (предмет, кабинет)} + строки изменений.
/// Аналог (target_g, main_pairs, changes) из ScheduleService.format_day_data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DayData {
    pub target_group: String,
    pub pairs: BTreeMap<i32, (String, String)>,
    pub changes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Вспомогательные сканеры (ручная замена Python-regex с lookaround)
// ---------------------------------------------------------------------------

fn chars_of(s: &str) -> Vec<char> {
    s.chars().collect()
}

/// Таблица замены латинских букв-двойников на кириллицу (services.py: ~55).
/// `_LATIN_TO_CYR_MAP`: "ABCEHKMOPTXYabcehkmoptxy" -> "АВСЕНКМОРТХУавсенкмортху".
fn latin_to_cyr(c: char) -> char {
    match c {
        'A' => 'А',
        'B' => 'В',
        'C' => 'С',
        'E' => 'Е',
        'H' => 'Н',
        'K' => 'К',
        'M' => 'М',
        'O' => 'О',
        'P' => 'Р',
        'T' => 'Т',
        'X' => 'Х',
        'Y' => 'У',
        'a' => 'а',
        'b' => 'в',
        'c' => 'с',
        'e' => 'е',
        'h' => 'н',
        'k' => 'к',
        'm' => 'м',
        'o' => 'о',
        'p' => 'р',
        't' => 'т',
        'x' => 'х',
        'y' => 'у',
        other => other,
    }
}

pub(crate) fn translate_latin(s: &str) -> String {
    s.chars().map(latin_to_cyr).collect()
}

/// Нижний регистр одного символа (Unicode-aware, как str.lower() для 1 символа).
fn lower_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn lower_str(s: &str) -> String {
    s.chars().map(lower_char).collect()
}

fn is_cyr(c: char) -> bool {
    ('\u{0400}'..='\u{04FF}').contains(&c)
}

/// Буква кириллицы или латиницы (для lookahead `[а-яёa-z]` с IGNORECASE).
fn is_alpha_cyr_lat(c: char) -> bool {
    is_cyr(c) || c.is_ascii_alphabetic()
}

/// Символ класса `[а-я.]` (с учётом IGNORECASE; ё включена — безвредное
/// расширение: в Python `ё` в этот класс не входит, но на практике после
/// дня идут только названия месяцев).
fn is_cyr_or_dot(c: char) -> bool {
    let l = lower_char(c);
    ('а'..='я').contains(&l) || l == 'ё' || c == '.'
}

/// Word-символ для `\b` (Python re.UNICODE): буква/цифра/_.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_dash(c: char) -> bool {
    matches!(c, '-' | '–' | '—' | '−')
}

/// Разделитель группы `[-. \s−]`.
fn is_group_sep(c: char) -> bool {
    c == '-' || c == '.' || c == '−' || c.is_whitespace()
}

/// Разрыв вида `\s*[-. \s−]\s*` (RE_GROUP_CLEAN, проверка 26-21):
/// 1+ символов из {\s, -, ., −} с не более чем одним не-пробельным
/// разделителем (пробел сам может быть разделителем). Возвращает
/// позицию конца разрыва или None.
fn group_gap_end(tc: &[char], start: usize) -> Option<usize> {
    let mut q = start;
    let mut non_space = 0;
    while q < tc.len() && is_group_sep(tc[q]) {
        if tc[q] == '-' || tc[q] == '.' || tc[q] == '−' {
            non_space += 1;
        }
        q += 1;
    }
    if q > start && non_space <= 1 {
        Some(q)
    } else {
        None
    }
}



/// Действительная дата (как datetime(y, m, d) без ValueError).
fn valid_date(y: i32, m: u32, d: u32) -> bool {
    chrono::NaiveDate::from_ymd_opt(y, m, d).is_some()
}

/// `_infer_year` (services.py:717). Текущее время — Тюмень (UTC+5).
fn infer_year(month: u32, explicit_year: Option<&str>) -> Option<i32> {
    if let Some(exp) = explicit_year {
        if exp.len() == 4 {
            exp.parse::<i32>().ok()
        } else {
            format!("20{exp}").parse::<i32>().ok()
        }
    } else {
        let now = tyumen_now();
        if now.month() == 12 && month == 1 {
            Some(now.year() + 1)
        } else if now.month() == 1 && month == 12 {
            Some(now.year() - 1)
        } else {
            Some(now.year())
        }
    }
}

// --- RE_MDK_CODE -----------------------------------------------------------
/// `\b(?:мдк|оп|уп|пп|пм|огсэ|ен|од|фк)\s*[.]?\s*\d{1,2}\s*[.]\s*\d{1,2}`
/// `(?:\s*[.]\s*\d{1,2})?` — удаление кодов МДК/ОП/... из текста.
/// Работает на уже приведённом к нижнему регистру тексте (как в
/// normalize_date / extract_all_dates_from_post), но ищет и в исходном
/// регистре через lower-копию для _extract_groups_from_cell.
fn remove_mdk_codes(text: &str) -> String {
    let ch = chars_of(text);
    let low: Vec<char> = ch.iter().map(|&c| lower_char(c)).collect();
    let keywords: &[&str] = &["огсэ", "мдк", "оп", "уп", "пп", "пм", "ен", "од", "фк"];
    let mut out = String::new();
    let mut i = 0usize;
    while i < ch.len() {
        let mut matched_end: Option<usize> = None;
        // \b перед ключевым словом
        let at_boundary = i == 0 || !is_word_char(ch[i - 1]);
        if at_boundary {
            for kw in keywords {
                let kwc: Vec<char> = kw.chars().collect();
                if i + kwc.len() > low.len() {
                    continue;
                }
                if low[i..i + kwc.len()] != kwc[..] {
                    continue;
                }
                // \s*[.]?\s*\d{1,2}\s*[.]\s*\d{1,2}(?:\s*[.]\s*\d{1,2})?
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
                let d1_start = p;
                while p < low.len() && low[p].is_ascii_digit() && p - d1_start < 2 {
                    p += 1;
                }
                if p == d1_start {
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
                let d2_start = p;
                while p < low.len() && low[p].is_ascii_digit() && p - d2_start < 2 {
                    p += 1;
                }
                if p == d2_start {
                    continue;
                }
                // необязательная третья группа .NN
                let mut q = p;
                while q < low.len() && low[q].is_whitespace() {
                    q += 1;
                }
                if q < low.len() && low[q] == '.' {
                    let mut r = q + 1;
                    while r < low.len() && low[r].is_whitespace() {
                        r += 1;
                    }
                    let d3_start = r;
                    while r < low.len() && low[r].is_ascii_digit() && r - d3_start < 2 {
                        r += 1;
                    }
                    if r > d3_start {
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

// --- RE_TIME_OR_ROOM_DOT ---------------------------------------------------
/// `\b(?:сбор\s+в|начало\s+в|в|к|каб\.?|лаб\.?|ауд\.?)\s*\d{1,2}\.\d{2}\b`
/// `(?!\.\d{2,4})` — удаление времени/кабинетов вида "сбор в 9.10".
/// Текст на входе уже в нижнем регистре (как в Python).
fn remove_time_or_room_dot(text: &str) -> String {
    let ch = chars_of(text);
    let low: Vec<char> = ch.iter().map(|&c| lower_char(c)).collect();
    // Ключевые слова — от длинных к коротким.
    let keywords: &[&str] = &[
        "сбор в", "начало в", "каб.", "каб", "лаб.", "лаб", "ауд.", "ауд", "в", "к",
    ];
    let mut out = String::new();
    let mut i = 0usize;
    while i < ch.len() {
        let mut matched_end: Option<usize> = None;
        let at_boundary = i == 0 || !is_word_char(ch[i - 1]);
        if at_boundary {
            for kw in keywords {
                let kwc: Vec<char> = kw.chars().collect();
                if i + kwc.len() > low.len() || low[i..i + kwc.len()] != kwc[..] {
                    continue;
                }
                // "сбор в"/"начало в": между словами \s+ (в kw уже пробел,
                // но допускаем и несколько пробелов — проверяем отдельно)
                let mut p = i + kwc.len();
                // для составных ключевых слов пробел внутри уже учтён как 1 символ;
                // допускаем дополнительные пробелы после "сбор"/"начало":
                if *kw == "сбор в" || *kw == "начало в" {
                    // kw содержит ровно один пробел; разрешаем ещё пробелы
                    while p < low.len() && low[p].is_whitespace() {
                        p += 1;
                    }
                    // 'в' уже в kw; p стоит после 'в' — ok
                }
                while p < low.len() && low[p].is_whitespace() {
                    p += 1;
                }
                let d_start = p;
                while p < low.len() && low[p].is_ascii_digit() && p - d_start < 2 {
                    p += 1;
                }
                if p == d_start || p >= low.len() || low[p] != '.' {
                    continue;
                }
                p += 1;
                let m_start = p;
                while p < low.len() && low[p].is_ascii_digit() && p - m_start < 2 {
                    p += 1;
                }
                if p - m_start != 2 {
                    continue;
                }
                // \b после минут: следующий символ — не word-символ
                if p < low.len() && is_word_char(low[p]) {
                    continue;
                }
                // (?!\.\d{2,4}): после не должно быть ".NN.."
                if p < low.len() && low[p] == '.' {
                    let mut q = p + 1;
                    let y_start = q;
                    while q < low.len() && low[q].is_ascii_digit() && q - y_start < 4 {
                        q += 1;
                    }
                    if q - y_start >= 2 {
                        continue;
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

// --- Месяцы ----------------------------------------------------------------

/// Стволы месяцев в порядке MONTHS_MAP (services.py:116):
/// (ствол, номер месяца). "ма(?:я|й)" представлен двумя стволами.
const MONTH_STEMS: &[(&str, u32)] = &[
    ("янв", 1),
    ("фев", 2),
    ("мар", 3),
    ("апр", 4),
    ("мая", 5),
    ("май", 5),
    ("июн", 6),
    ("июл", 7),
    ("авг", 8),
    ("сен", 9),
    ("окт", 10),
    ("ноя", 11),
    ("дек", 12),
];

fn stem_at(ch: &[char], pos: usize, stem: &str) -> bool {
    let sc: Vec<char> = stem.chars().collect();
    if pos + sc.len() > ch.len() {
        return false;
    }
    ch[pos..pos + sc.len()]
        .iter()
        .zip(sc.iter())
        .all(|(a, b)| lower_char(*a) == *b)
}

/// Проверка суффикса `\s*[а-я.]*\s*<stem>` после дня (позиция day_end —
/// конец цифр дня). Возвращает true, если ствол месяца находится там,
/// где его допускает Python-regex с жадным [а-я.]* и бэктрекингом.
fn suffix_has_stem(ch: &[char], day_end: usize, stem: &str) -> bool {
    let mut p = day_end;
    while p < ch.len() && ch[p].is_whitespace() {
        p += 1;
    }
    let letters_start = p;
    while p < ch.len() && is_cyr_or_dot(ch[p]) {
        p += 1;
    }
    let letters_end = p;
    // жадный [а-я.]* пробует самую длинную позицию первой, затем короче:
    // для существования совпадения достаточно проверить все q.
    for q in letters_start..=letters_end {
        if stem_at(ch, q, stem) {
            return true;
        }
    }
    let mut p2 = letters_end;
    while p2 < ch.len() && ch[p2].is_whitespace() {
        p2 += 1;
    }
    stem_at(ch, p2, stem)
}

/// Поиск даты с текстовым месяцем:
/// `(?<!\d)(\d{1,2})\s*[а-я.]*\s*<stem>` — первое (самое левое) совпадение.
/// Возвращает (день, конец цифр дня).
fn find_month_date(ch: &[char], stem: &str) -> Option<(u32, usize)> {
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i].is_ascii_digit() && (i == 0 || !ch[i - 1].is_ascii_digit()) {
            let d_start = i;
            let mut p = i;
            while p < ch.len() && ch[p].is_ascii_digit() && p - d_start < 2 {
                p += 1;
            }
            let day: u32 = ch[d_start..p].iter().collect::<String>().parse().ok()?;
            if suffix_has_stem(ch, p, stem) {
                return Some((day, p));
            }
        }
        i += 1;
    }
    None
}

// --- Числовая дата ---------------------------------------------------------

/// Одна числовая дата вида ДД.ММ[.ГГГГ] (RE_DATE).
struct NumDate {
    d: u32,
    m: u32,
    year: Option<String>,
    end: usize,
}

/// Парсинг `(\d{1,2})[.](\d{1,2})(?:[.](\d{2,4}))?` с позиции `i`
/// (lookbehind `(?<![\d.])` проверяется вызывающим кодом, кроме случаев,
/// где Python его не требует). Возвращает None, если не совпало,
/// включая провал lookahead `(?![\d.])`.
fn parse_num_date(ch: &[char], i: usize, check_lookahead: bool) -> Option<NumDate> {
    let mut p = i;
    let d_start = p;
    while p < ch.len() && ch[p].is_ascii_digit() && p - d_start < 2 {
        p += 1;
    }
    if p == d_start || p >= ch.len() || ch[p] != '.' {
        return None;
    }
    let d: u32 = ch[d_start..p].iter().collect::<String>().parse().ok()?;
    p += 1; // '.'
    let m_start = p;
    while p < ch.len() && ch[p].is_ascii_digit() && p - m_start < 2 {
        p += 1;
    }
    if p == m_start {
        return None;
    }
    let m: u32 = ch[m_start..p].iter().collect::<String>().parse().ok()?;
    let mut year: Option<String> = None;
    let mut end = p;
    if p < ch.len() && ch[p] == '.' {
        // пробуем длины года 4, 3, 2 (жадный порядок regex)
        let mut ok = false;
        for len in [4usize, 3, 2] {
            let y_start = p + 1;
            let y_end = y_start + len;
            if y_end > ch.len() {
                continue;
            }
            if !ch[y_start..y_end].iter().all(|c| c.is_ascii_digit()) {
                continue;
            }
            if check_lookahead
                && y_end < ch.len()
                && (ch[y_end].is_ascii_digit() || ch[y_end] == '.')
            {
                continue;
            }
            year = Some(ch[y_start..y_end].iter().collect());
            end = y_end;
            ok = true;
            break;
        }
        if !ok {
            return None;
        }
    } else if check_lookahead && p < ch.len() && (ch[p].is_ascii_digit() || ch[p] == '.') {
        return None;
    }
    Some(NumDate { d, m, year, end })
}

fn clean_for_date(text: &str) -> Vec<char> {
    let lowered = lower_str(&translate_latin(text));
    let no_mdk = remove_mdk_codes(&lowered);
    let no_time = remove_time_or_room_dot(&no_mdk);
    let replaced: String = no_time
        .chars()
        .map(|c| if c == '–' || c == '—' || c == '−' { '-' } else { c })
        .collect();
    chars_of(&replaced)
}

// --- Маркеры пар -----------------------------------------------------------

/// Маркер пары в позиции `pos` (после `\s*`):
/// `[пp]\.(?![а-яёa-z]) | [пp](?![а-яёa-z.]) | пар[аыуе]?(?![а-яёa-z])`
/// (RE_PAIR_MARKER / суффикс RE_PAIR_LIST_MARKER, IGNORECASE).
/// Возвращает позицию конца маркера.
fn pair_marker_at(ch: &[char], pos: usize) -> Option<usize> {
    let mut p = pos;
    while p < ch.len() && ch[p].is_whitespace() {
        p += 1;
    }
    if p >= ch.len() {
        return None;
    }
    let c0 = lower_char(ch[p]);
    // [пp] — кириллическое п или латинское p
    if c0 == 'п' || c0 == 'p' {
        if p + 1 < ch.len() && ch[p + 1] == '.' {
            // [пp]\.(?![а-яёa-z])
            let after = p + 2;
            if after >= ch.len() || !is_alpha_cyr_lat(ch[after]) {
                return Some(after);
            }
        } else {
            // [пp](?![а-яёa-z.])
            let after = p + 1;
            if after >= ch.len() || (!is_alpha_cyr_lat(ch[after]) && ch[after] != '.') {
                return Some(after);
            }
        }
    }
    // пар[аыуе]? — только кириллица
    if c0 == 'п' && p + 2 < ch.len() && lower_char(ch[p + 1]) == 'а' && lower_char(ch[p + 2]) == 'р'
    {
        let mut end = p + 3;
        if end < ch.len() {
            let lc = lower_char(ch[end]);
            if matches!(lc, 'а' | 'ы' | 'у' | 'е') {
                end += 1;
            }
        }
        if end >= ch.len() || !is_alpha_cyr_lat(ch[end]) {
            return Some(end);
        }
    }
    None
}

/// Одиночный маркер пары (RE_PAIR_MARKER): `(?<![\d.])([1-8])\s*маркер`.
/// Возвращает номер пары из первого совпадения.
pub(crate) fn find_single_pair_marker(text: &str) -> Option<i32> {
    let ch = chars_of(text);
    let mut i = 0usize;
    while i < ch.len() {
        if ('1'..='8').contains(&ch[i]) && (i == 0 || (!ch[i - 1].is_ascii_digit() && ch[i - 1] != '.'))
        {
            if pair_marker_at(&ch, i + 1).is_some() {
                return Some(ch[i].to_digit(10).unwrap() as i32);
            }
        }
        i += 1;
    }
    None
}

/// Есть ли в ячейке маркер пары (списочный или одиночный) — как условие
/// `RE_PAIR_LIST_MARKER.search(c) or RE_PAIR_MARKER.search(c)` в parse_docx_pure.
pub(crate) fn cell_has_pair_marker(cell: &str) -> bool {
    !extract_pair_numbers(cell).is_empty() || find_single_pair_marker(cell).is_some()
}

/// Извлечь номера пар из подстроки-списка `seq` (уже отделённой от маркера):
/// диапазоны `([1-8])\s*[-–—−]\s*([1-8])` раскрываются, плюс все цифры [1-8].
fn pairs_from_seq(seq: &[char]) -> HashSet<i32> {
    let mut out = HashSet::new();
    let mut i = 0usize;
    while i < seq.len() {
        if ('1'..='8').contains(&seq[i]) {
            // диапазон? цифра, пробелы, тире, пробелы, цифра
            let d1 = seq[i].to_digit(10).unwrap() as i32;
            let mut p = i + 1;
            while p < seq.len() && seq[p].is_whitespace() {
                p += 1;
            }
            if p < seq.len() && is_dash(seq[p]) {
                let mut q = p + 1;
                while q < seq.len() && seq[q].is_whitespace() {
                    q += 1;
                }
                if q < seq.len() && ('1'..='8').contains(&seq[q]) {
                    let d2 = seq[q].to_digit(10).unwrap() as i32;
                    if d1 <= d2 {
                        for d in d1..=d2 {
                            out.insert(d);
                        }
                    }
                }
            }
            out.insert(d1);
        }
        i += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Публичные функции
// ---------------------------------------------------------------------------

/// Одна строка расписания -> (номер пары, предмет, кабинет).
/// Точная копия parse_schedule_line (services.py:2457).
pub fn parse_schedule_line(line: &str) -> (i32, String, String) {
    let ch = chars_of(line);
    let mut p_num: i32 = -1;
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
            // Python: re.search(r"(\d+)\s*п", line) — без IGNORECASE,
            // только строчное кириллическое 'п'.
            if k < ch.len() && ch[k] == 'п' {
                p_num = ch[i..j]
                    .iter()
                    .collect::<String>()
                    .parse::<i32>()
                    .unwrap_or(-1);
                break;
            }
        }
        i += 1;
    }

    let clean_line: String = line
        .replace('🕒', "")
        .replace("<b>", "")
        .replace("</b>", "")
        .replace('*', "")
        .trim()
        .to_string();

    let mut subj = "—".to_string();
    let mut room = "—".to_string();
    if let Some(pipe) = clean_line.find('|') {
        let after_pipe = clean_line[pipe + 1..].trim().to_string();
        // m_room = re.search(r"\(([^()]*)\)\s*$", after_pipe)
        let trimmed = after_pipe.trim_end().to_string();
        let mut matched = false;
        if trimmed.ends_with(')') {
            if let Some(open) = trimmed.rfind('(') {
                let interior = &trimmed[open + 1..trimmed.len() - 1];
                if !interior.contains('(') && !interior.contains(')') {
                    room = interior.trim().to_string();
                    subj = trimmed[..open].trim().to_string();
                    matched = true;
                }
            }
        }
        if !matched {
            subj = after_pipe;
        }
    } else {
        subj = clean_line;
    }
    if subj.is_empty() {
        subj = "—".to_string();
    }
    if room.is_empty() {
        room = "—".to_string();
    }
    (p_num, subj, room)
}

/// Записи (content, type) одной даты и группы -> пары + изменения.
/// Точная копия format_day_data (services.py:2493 / extract_real_days.py).
pub fn format_day_data(records: &[(String, String)]) -> (BTreeMap<i32, (String, String)>, Vec<String>) {
    let mut main_pairs: BTreeMap<i32, (String, String)> = BTreeMap::new();
    let mut changes: Vec<String> = Vec::new();
    let mut seen_lines: HashSet<String> = HashSet::new();
    for (content, rtype) in records {
        for raw_line in content.split('\n') {
            let line = raw_line.trim();
            if line.is_empty()
                || seen_lines.contains(line)
                || line.contains("🔔 Расписание по времени (фото)")
            {
                continue;
            }
            seen_lines.insert(line.to_string());
            if rtype == "изменения" {
                changes.push(line.to_string());
            } else {
                let (p_num, subj, room) = parse_schedule_line(line);
                if p_num > 0 {
                    if let Some((prev_subj, prev_room)) = main_pairs.get(&p_num).cloned() {
                        if prev_room == "—" && room != "—" {
                            main_pairs.insert(p_num, (subj, room));
                        } else if prev_subj == subj && room != "—" && !prev_room.contains(&room)
                        {
                            main_pairs.insert(p_num, (subj, format!("{prev_room} / {room}")));
                        } else if prev_subj != subj && subj != "—" {
                            let new_room = if prev_room != room {
                                format!("{prev_room} / {room}")
                            } else {
                                prev_room.clone()
                            };
                            main_pairs.insert(p_num, (format!("{prev_subj} / {subj}"), new_room));
                        }
                    } else {
                        main_pairs.insert(p_num, (subj, room));
                    }
                } else {
                    // clean_change без unescape/эмодзи: только снос </?b>
                    let clean = line.replace("<b>", "").replace("</b>", "");
                    changes.push(clean.trim().to_string());
                }
            }
        }
    }
    (main_pairs, changes)
}

/// Номера пар, упомянутых в строке изменений.
/// Точная копия _extract_pair_numbers (services.py:2549).
pub fn extract_pair_numbers(text: &str) -> HashSet<i32> {
    let ch = chars_of(text);
    let mut out: HashSet<i32> = HashSet::new();
    let mut i = 0usize;
    while i < ch.len() {
        if !('1'..='8').contains(&ch[i]) {
            i += 1;
            continue;
        }
        // lookbehind (?<![\d.-]) перед началом списка
        if i > 0 && (ch[i - 1].is_ascii_digit() || ch[i - 1] == '.' || ch[i - 1] == '-') {
            i += 1;
            continue;
        }
        // Парсим максимальную последовательность цифр с разделителями.
        let seq_start = i;
        let mut pos = i + 1; // конец первой цифры
        loop {
            // пробуем расширение: \s* разделитель \s* цифра 1-8
            let mut p = pos;
            while p < ch.len() && ch[p].is_whitespace() {
                p += 1;
            }
            if p >= ch.len() {
                break;
            }
            let sep = ch[p];
            let is_sep = sep == ','
                || is_dash(sep)
                || lower_char(sep) == 'и';
            if !is_sep {
                break;
            }
            let mut q = p + 1;
            while q < ch.len() && ch[q].is_whitespace() {
                q += 1;
            }
            if q < ch.len() && ('1'..='8').contains(&ch[q]) {
                pos = q + 1;
            } else {
                break;
            }
        }
        let seq_end = pos; // конец последней цифры
        match pair_marker_at(&ch, seq_end) {
            Some(marker_end) => {
                let seq: Vec<char> = ch[seq_start..seq_end].to_vec();
                out.extend(pairs_from_seq(&seq));
                i = marker_end;
            }
            None => {
                i += 1;
            }
        }
    }
    out
}

/// Чистка строки изменения: убрать <b>, ⚠️/ℹ️ в начале, unescape HTML.
/// Точная копия _clean_change из miniapp_api.py.
pub fn clean_change(text: &str) -> String {
    let no_b = text.replace("<b>", "").replace("</b>", "");
    let stripped = no_b.trim();
    let mut rest = stripped;
    // ^(?:⚠️|ℹ️)\s* — ⚠️ = U+26A0 U+FE0F, ℹ️ = U+2139 U+FE0F
    for marker in ["\u{26A0}\u{FE0F}", "\u{2139}\u{FE0F}"] {
        if let Some(after) = rest.strip_prefix(marker) {
            rest = after.trim_start();
            break;
        }
    }
    html_unescape(rest).trim().to_string()
}

/// Минимальный аналог html.unescape: именованные сущности (&amp; &lt; &gt;
/// &quot; &apos; &nbsp;) и числовые (&#NN; / &#xHH;). Неизвестные сущности
/// Python оставляет как есть — здесь тоже.
fn html_unescape(s: &str) -> String {
    let ch = chars_of(s);
    let mut out = String::new();
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i] == '&' {
            let mut j = i + 1;
            while j < ch.len() && j - i <= 10 && ch[j] != ';' {
                j += 1;
            }
            if j < ch.len() && ch[j] == ';' {
                let entity: String = ch[i + 1..j].iter().collect();
                let decoded: Option<String> = match entity.as_str() {
                    "amp" => Some("&".to_string()),
                    "lt" => Some("<".to_string()),
                    "gt" => Some(">".to_string()),
                    "quot" => Some("\"".to_string()),
                    "apos" => Some("'".to_string()),
                    "nbsp" => Some("\u{A0}".to_string()),
                    _ => {
                        if let Some(num) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X")) {
                            u32::from_str_radix(num, 16)
                                .ok()
                                .and_then(char::from_u32)
                                .map(|c| c.to_string())
                        } else if let Some(num) = entity.strip_prefix('#') {
                            num.parse::<u32>()
                                .ok()
                                .and_then(char::from_u32)
                                .map(|c| c.to_string())
                        } else {
                            None
                        }
                    }
                };
                if let Some(d) = decoded {
                    out.push_str(&d);
                    i = j + 1;
                    continue;
                }
            }
        }
        out.push(ch[i]);
        i += 1;
    }
    out
}

/// Кабинет вне скобок ("... каб. 34 (—)") -> в room.
/// Точная копия fix_room из extract_real_days.py.
pub fn fix_room(subj: &str, room: &str) -> (String, String) {
    if room != "—" {
        return (subj.to_string(), room.to_string());
    }
    // ROOM_TAIL = \s+((?:каб|лаб|ауд)\.\s*[\d/]+|к/л\s*[\d/]+|с/з)\s*$
    let trimmed = subj.trim_end();
    let ch = chars_of(trimmed);
    let mut i = 0usize;
    while i < ch.len() {
        // перед группой должен быть пробел (\s+)
        if i > 0 && ch[i - 1].is_whitespace() {
            if let Some((group, _)) = room_tail_at(&ch, i) {
                let new_subj: String = ch[..i].iter().collect::<String>().trim_end().to_string();
                return (new_subj, group);
            }
        }
        i += 1;
    }
    (subj.to_string(), room.to_string())
}

/// Проверка ROOM_TAIL с позиции `i`: возвращает (группа, конец).
/// Группа — без окружающих пробелов, как group(1) в Python.
fn room_tail_at(ch: &[char], i: usize) -> Option<(String, usize)> {
    let rest: String = ch[i..].iter().collect();
    let rest_low = lower_str(&rest);
    let rc: Vec<char> = rest_low.chars().collect();
    // "с/з" до конца
    if rc.len() == 3 && rc[0] == 'с' && rc[1] == '/' && rc[2] == 'з' {
        return Some((rest.trim().to_string(), ch.len()));
    }
    // "к/л" + пробелы + цифры/слэши до конца
    if rc.len() >= 3 && rc[0] == 'к' && rc[1] == '/' && rc[2] == 'л' {
        let mut p = 3;
        while p < rc.len() && rc[p].is_whitespace() {
            p += 1;
        }
        if p < rc.len()
            && rc[p..].iter().all(|c| c.is_ascii_digit() || *c == '/')
            && !rc[p..].is_empty()
        {
            return Some((rest.trim().to_string(), ch.len()));
        }
        return None;
    }
    // ("каб"|"лаб"|"ауд") + "." + пробелы + цифры/слэши до конца
    for word in ["каб", "лаб", "ауд"] {
        let wc: Vec<char> = word.chars().collect();
        if rc.len() > wc.len() && rc[..wc.len()] == wc[..] && rc[wc.len()] == '.' {
            let mut p = wc.len() + 1;
            while p < rc.len() && rc[p].is_whitespace() {
                p += 1;
            }
            if p < rc.len()
                && rc[p..].iter().all(|c| c.is_ascii_digit() || *c == '/')
                && !rc[p..].is_empty()
            {
                return Some((rest.trim().to_string(), ch.len()));
            }
            return None;
        }
    }
    None
}

/// Нормализация названия группы. Python: normalize_group(text, prefix_hint="").
/// Точная копия services.py:762 (включая "б" на конце и неоднозначную "26-21").
///
/// Отличия, продиктованные сигнатурой Rust: вход — только `&str`
/// (в Python допустимы также tuple/list из 4 элементов — в Rust
/// такой вызов невозможен, caller должен заранее склеить части
/// через "-"). Поведение на строках — 1-в-1 с Python, включая то,
/// что префикс специальности ОТБРАСЫВАЕТСЯ для всех групп, кроме
/// ТД/ЮР-26-21: normalize_group("ИП-24-27") == "24-27" (так в Python;
/// сравнение групп в format_day_data нормализует обе стороны).
pub fn normalize_group(text: &str, prefix_hint: &str) -> String {
    let text_clean: String = translate_latin(text)
        .trim()
        .chars()
        .map(|c| if c == '–' || c == '—' || c == '−' { '-' } else { c })
        .collect();
    let hint_upper = translate_latin(prefix_hint).trim().to_uppercase();

    let tc = chars_of(&text_clean);
    // (?<!\d)26\s*[-. \s]\s*21(?!\d)
    // Пробел между "26" и "21" — сам разделитель: \s* [-. \s] \s* означает
    // «пробелы, ОДИН символ-разделитель (пробел/дефис/точка), пробелы»,
    // т.е. разрыв — это 1+ символов из {\s, -, ., −}.
    let mut is_26_21 = false;
    let mut i = 0usize;
    while i + 1 < tc.len() {
        if tc[i] == '2'
            && tc[i + 1] == '6'
            && (i == 0 || !tc[i - 1].is_ascii_digit())
        {
            if let Some(q) = group_gap_end(&tc, i + 2) {
                if q + 1 < tc.len() && tc[q] == '2' && tc[q + 1] == '1' {
                    let after = q + 2;
                    if after >= tc.len() || !tc[after].is_ascii_digit() {
                        is_26_21 = true;
                    }
                }
            }
        }
        i += 1;
    }
    if is_26_21 {
        // RE_26_21_EXPLICIT: (?<![а-яёa-z])(тд|юр)\s*[-. \s−]*26\s*[-. \s−]*21
        if let Some(prefix) = find_26_21_explicit(&tc) {
            return format!("{prefix}-26-21");
        }
        if hint_upper == "ТД" || hint_upper == "ЮР" {
            return format!("{hint_upper}-26-21");
        }
        return "26-21".to_string();
    }

    // RE_GROUP_CLEAN: ([2-9]\d)\s*[-. \s−]\s*(\d{1,2})
    if let Some((g1, g2)) = find_group_clean(&tc) {
        return format!("{g1}-{g2:02}");
    }
    // RE_CLEAN_NON_ALNUM: оставить только [а-яё0-9] в нижнем регистре
    text_clean
        .to_lowercase()
        .chars()
        .filter(|c| {
            let l = lower_char(*c);
            ('а'..='я').contains(&l) || l == 'ё' || c.is_ascii_digit()
        })
        .collect()
}

/// RE_26_21_EXPLICIT: возвращает "ТД" или "ЮР" (верхний регистр).
fn find_26_21_explicit(tc: &[char]) -> Option<String> {
    let mut i = 0usize;
    while i + 1 < tc.len() {
        let prev_ok = i == 0 || !is_alpha_cyr_lat(tc[i - 1]);
        let pair = (lower_char(tc[i]), lower_char(tc[i + 1]));
        let prefix = match pair {
            ('т', 'д') => Some("ТД"),
            ('ю', 'р') => Some("ЮР"),
            _ => None,
        };
        if let Some(pref) = prefix {
            if prev_ok {
                let mut p = i + 2;
                while p < tc.len() && is_group_sep(tc[p]) {
                    p += 1;
                }
                if p + 1 < tc.len() && tc[p] == '2' && tc[p + 1] == '6' {
                    p += 2;
                    while p < tc.len() && is_group_sep(tc[p]) {
                        p += 1;
                    }
                    if p + 1 < tc.len() && tc[p] == '2' && tc[p + 1] == '1' {
                        return Some(pref.to_string());
                    }
                }
            }
        }
        i += 1;
    }
    None
}

/// RE_GROUP_CLEAN: ([2-9]\d)\s*[-. \s−]\s*(\d{1,2}) — первое совпадение.
/// Разрыв между группами цифр — 1+ символов из {\s, -, ., −}
/// (пробел сам может быть разделителем, см. normalize_group).
/// Возвращает (первые 2 цифры, вторые 1-2 цифры как число).
fn find_group_clean(tc: &[char]) -> Option<(String, u32)> {
    let mut i = 0usize;
    while i + 1 < tc.len() {
        if ('2'..='9').contains(&tc[i]) && tc[i + 1].is_ascii_digit() {
            let g1: String = tc[i..i + 2].iter().collect();
            if let Some(p) = group_gap_end(tc, i + 2) {
                let d_start = p;
                let mut p = p;
                while p < tc.len() && tc[p].is_ascii_digit() && p - d_start < 2 {
                    p += 1;
                }
                if p > d_start {
                    let g2: u32 = tc[d_start..p].iter().collect::<String>().parse().ok()?;
                    return Some((g1, g2));
                }
            }
        }
        i += 1;
    }
    None
}

/// Нормализация даты из текста поста -> "ДД.ММ.ГГГГ" или None.
/// Точная копия normalize_date (services.py:728) + _infer_year (services.py:717).
pub fn normalize_date(text: &str) -> Option<String> {
    let ch = clean_for_date(text);
    // 1. Даты с текстовым месяцем (порядок MONTHS_MAP).
    // "ма(?:я|й)" — одна запись в Python; здесь стволы "мая"/"май"
    // идут подряд и оба дают месяц 05.
    for (stem, month_num) in MONTH_STEMS {
        if let Some((day, _)) = find_month_date(&ch, stem) {
            if (1..=31).contains(&day) {
                let y = infer_year(*month_num, None)?;
                if !valid_date(y, *month_num, day) {
                    return None;
                }
                return Some(format!("{day:02}.{month_num:02}.{y}"));
            }
            // день вне 1..31 — Python продолжает перебор паттернов
        }
    }
    // 2. Числовая дата RE_DATE (первое совпадение решает всё).
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i].is_ascii_digit() && (i == 0 || (!ch[i - 1].is_ascii_digit() && ch[i - 1] != '.')) {
            if let Some(nd) = parse_num_date(&ch, i, true) {
                if nd.m < 1 || nd.m > 12 || nd.d < 1 || nd.d > 31 {
                    return None;
                }
                let y = infer_year(nd.m, nd.year.as_deref())?;
                if !valid_date(y, nd.m, nd.d) {
                    return None;
                }
                return Some(format!("{:02}.{:02}.{}", nd.d, nd.m, y));
            }
        }
        i += 1;
    }
    None
}

/// Все даты из текста поста. Точная копия extract_all_dates_from_post (services.py:836).
pub fn extract_all_dates_from_post(text: &str, title: &str) -> Vec<String> {
    let combined = format!("{text}\n{title}");
    let ch = clean_for_date(&combined);
    let mut found: Vec<String> = Vec::new();
    let push = |d: &str, found: &mut Vec<String>| {
        if !found.iter().any(|x| x == d) {
            found.push(d.to_string());
        }
    };

    // 1. Диапазоны дат вида 17.06.2026-23.06.2026 (слева направо, как finditer)
    for d in expand_date_ranges(&ch) {
        push(&d, &mut found);
    }

    // 2. Списки дат и одиночные даты с текстовым месяцем
    for (stem, month_num) in MONTH_STEMS {
        let y = match infer_year(*month_num, None) {
            Some(y) => y,
            None => continue,
        };
        // список: (?:на\s*)?((?:\d{1,2}\s*[,и\s-]\s*)+\d{1,2})\s*[а-я.]*\s*<stem>
        if let Some(days) = find_day_list(&ch, stem) {
            for d in days {
                if (1..=31).contains(&d) && valid_date(y, *month_num, d) {
                    let ds = format!("{d:02}.{month_num:02}.{y}");
                    push(&ds, &mut found);
                }
            }
        }
        // одиночная: (?<!\d)(\d{1,2})\s*[а-я.]*\s*<stem>
        if let Some((day, _)) = find_month_date(&ch, stem) {
            if (1..=31).contains(&day) && valid_date(y, *month_num, day) {
                let ds = format!("{day:02}.{month_num:02}.{y}");
                push(&ds, &mut found);
            }
        }
    }

    // 3. Одиночные даты вида 15.06.2026 (RE_DATE finditer)
    let mut j = 0usize;
    while j < ch.len() {
        if ch[j].is_ascii_digit() && (j == 0 || (!ch[j - 1].is_ascii_digit() && ch[j - 1] != '.')) {
            if let Some(nd) = parse_num_date(&ch, j, true) {
                if (1..=12).contains(&nd.m) && (1..=31).contains(&nd.d) {
                    if let Some(y) = infer_year(nd.m, nd.year.as_deref()) {
                        if valid_date(y, nd.m, nd.d) {
                            let ds = format!("{:02}.{:02}.{}", nd.d, nd.m, y);
                            push(&ds, &mut found);
                        }
                    }
                }
                j = nd.end;
                continue;
            }
        }
        j += 1;
    }
    found
}

/// Раскрытие диапазонов RE_DATE_RANGE из очищенного текста.
/// Порядок — слева направо, как finditer в Python.
fn expand_date_ranges(ch: &[char]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < ch.len() {
        let is_start = ch[i].is_ascii_digit()
            && (i == 0 || (!ch[i - 1].is_ascii_digit() && ch[i - 1] != '.'));
        if !is_start {
            i += 1;
            continue;
        }
        let first = match parse_num_date(ch, i, false) {
            Some(f) => f,
            None => {
                i += 1;
                continue;
            }
        };
        // \s*-\s* между датами
        let mut p = first.end;
        while p < ch.len() && ch[p].is_whitespace() {
            p += 1;
        }
        if p >= ch.len() || ch[p] != '-' {
            i += 1;
            continue;
        }
        p += 1;
        while p < ch.len() && ch[p].is_whitespace() {
            p += 1;
        }
        let second = match parse_num_date(ch, p, true) {
            Some(s) => s,
            None => {
                i += 1;
                continue;
            }
        };
        let y1 = match infer_year(first.m, first.year.as_deref()) {
            Some(y) => y,
            None => {
                i = second.end;
                continue;
            }
        };
        let y2 = match &second.year {
            Some(y) => match infer_year(second.m, Some(y)) {
                Some(v) => v,
                None => {
                    i = second.end;
                    continue;
                }
            },
            None => y1,
        };
        let start = match chrono::NaiveDate::from_ymd_opt(y1, first.m, first.d) {
            Some(d) => d,
            None => {
                i = second.end;
                continue;
            }
        };
        let end = match chrono::NaiveDate::from_ymd_opt(y2, second.m, second.d) {
            Some(d) => d,
            None => {
                i = second.end;
                continue;
            }
        };
        let days = (end - start).num_days();
        if (0..=14).contains(&days) {
            let mut cur = start;
            while cur <= end {
                // Python: curr.weekday() != 6 — воскресенье пропускается
                if cur.weekday() != chrono::Weekday::Sun {
                    out.push(crate::tyumen::fmt_date(cur));
                }
                cur += chrono::Duration::days(1);
            }
        }
        i = second.end;
    }
    out
}

/// Список дней перед текстовым месяцем:
/// `(?:на\s*)?((?:\d{1,2}\s*[,и\s-]\s*)+\d{1,2})\s*[а-я.]*\s*<stem>`.
/// Возвращает дни из первого (самого левого) совпадения.
fn find_day_list(ch: &[char], stem: &str) -> Option<Vec<u32>> {
    let mut i = 0usize;
    while i < ch.len() {
        // кандидат: цифра, либо "на" + пробелы + цифра
        let mut digit_pos: Option<usize> = None;
        if ch[i].is_ascii_digit() {
            digit_pos = Some(i);
        } else if i + 1 < ch.len() && lower_char(ch[i]) == 'н' && lower_char(ch[i + 1]) == 'а' {
            let mut p = i + 2;
            while p < ch.len() && ch[p].is_whitespace() {
                p += 1;
            }
            if p < ch.len() && ch[p].is_ascii_digit() {
                digit_pos = Some(p);
            }
        }
        if let Some(dp) = digit_pos {
            if let Some((days, end)) = parse_day_list_at(ch, dp) {
                if suffix_has_stem(ch, end, stem) {
                    return Some(days);
                }
            }
        }
        i += 1;
    }
    None
}

/// Парсинг списка дней с позиции цифры: день (разделитель день)+, минимум 2 дня.
fn parse_day_list_at(ch: &[char], start: usize) -> Option<(Vec<u32>, usize)> {
    let mut days: Vec<u32> = Vec::new();
    let mut p = start;
    let d_start = p;
    while p < ch.len() && ch[p].is_ascii_digit() && p - d_start < 2 {
        p += 1;
    }
    if p == d_start {
        return None;
    }
    days.push(ch[d_start..p].iter().collect::<String>().parse().ok()?);
    loop {
        // разделители [,и\s-]* (минимум 1 символ), затем цифра
        let mut q = p;
        while q < ch.len()
            && (ch[q].is_whitespace() || ch[q] == ',' || lower_char(ch[q]) == 'и' || is_dash(ch[q]))
        {
            q += 1;
        }
        if q == p || q >= ch.len() || !ch[q].is_ascii_digit() {
            break;
        }
        let nd_start = q;
        while q < ch.len() && ch[q].is_ascii_digit() && q - nd_start < 2 {
            q += 1;
        }
        days.push(ch[nd_start..q].iter().collect::<String>().parse().ok()?);
        p = q;
    }
    if days.len() < 2 {
        return None;
    }
    Some((days, p))
}

/// "1 курс" / "2 курс" / "3,4 курс" по году группы. Точная копия get_course_str (services.py:2707).
pub fn get_course_str(group: &str) -> String {
    let ch = chars_of(group);
    let mut i = 0usize;
    while i + 1 < ch.len() {
        if ('2'..='9').contains(&ch[i]) && ch[i + 1].is_ascii_digit() {
            let entry: i32 = ch[i..i + 2].iter().collect::<String>().parse().unwrap_or(0);
            let now = tyumen_now();
            let current_yr = now.year() % 100;
            let course = (current_yr - entry) + if now.month() >= 9 { 1 } else { 0 };
            if course <= 1 {
                return "1 курс".to_string();
            }
            if course == 2 {
                return "2 курс".to_string();
            }
            return "3,4 курс".to_string();
        }
        i += 1;
    }
    "1 курс".to_string()
}

/// Смена по парам дня: есть пара №>=5 -> 2, иначе 1.
/// Логика get_group_shift (services.py:2749) без fallback на архив.
pub fn group_shift_from_pairs(pairs: &BTreeMap<i32, (String, String)>) -> i32 {
    if pairs.keys().any(|p| *p >= 5) {
        2
    } else {
        1
    }
}

/// Извлечение всех валидных номеров групп из ячейки таблицы DOCX
/// (services.py:799 _extract_groups_from_cell). Используется в docx.rs.
pub(crate) fn extract_groups_from_cell(text: &str, prefix_hint: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let cleaned: String = translate_latin(text)
        .chars()
        .map(|c| if c == '–' || c == '—' || c == '−' { '-' } else { c })
        .collect();
    let cleaned = remove_mdk_codes(&cleaned);
    let ch = chars_of(&cleaned);
    let mut found: Vec<String> = Vec::new();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    // Сначала явные ТД/ЮР-26-21
    let mut i = 0usize;
    while i + 1 < ch.len() {
        let prev_ok = i == 0 || !is_alpha_cyr_lat(ch[i - 1]);
        let pair = (lower_char(ch[i]), lower_char(ch[i + 1]));
        let prefix = match pair {
            ('т', 'д') => Some("ТД"),
            ('ю', 'р') => Some("ЮР"),
            _ => None,
        };
        if let Some(pref) = prefix {
            if prev_ok {
                let mut p = i + 2;
                while p < ch.len() && is_group_sep(ch[p]) {
                    p += 1;
                }
                if p + 1 < ch.len() && ch[p] == '2' && ch[p + 1] == '6' {
                    p += 2;
                    while p < ch.len() && is_group_sep(ch[p]) {
                        p += 1;
                    }
                    if p + 1 < ch.len() && ch[p] == '2' && ch[p + 1] == '1' {
                        let g = format!("{pref}-26-21");
                        if !found.contains(&g) {
                            found.push(g);
                        }
                        spans.push((i, p + 2));
                    }
                }
            }
        }
        i += 1;
    }
    // Вырезаем найденные 26-21 (замена спэна одним пробелом, как в Python)
    let mut rem: Vec<char> = ch.clone();
    for (s, e) in spans.iter().rev() {
        let mut new_rem: Vec<char> = Vec::new();
        new_rem.extend_from_slice(&rem[..*s]);
        new_rem.push(' ');
        new_rem.extend_from_slice(&rem[*e..]);
        rem = new_rem;
    }
    // RE_GROUP_CLEAN по остатку
    let mut j = 0usize;
    while j < rem.len() {
        if let Some((g1, g2)) = find_group_clean(&rem[j..]) {
            // find_group_clean вернул относительные индексы — пересчитываем
            // через повторный поиск с начала: проще — строим кандидата
            // из найденной пары напрямую.
            let cand = format!("{g1}-{g2:02}");
            let norm = normalize_group(&cand, prefix_hint);
            if !norm.is_empty() && !found.contains(&norm) {
                found.push(norm);
            }
            // продвигаемся за найденную пару: ищем её конец в rem
            // (find_group_clean ищет с начала среза — двигаемся на 1,
            //  как finditer: следующее совпадение может пересекаться)
            j += 1;
        } else {
            break;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_schedule_line_basic() {
        assert_eq!(
            parse_schedule_line("<b>1п.</b> | Математика (каб. 216)"),
            (1, "Математика".to_string(), "каб. 216".to_string())
        );
        assert_eq!(
            parse_schedule_line("🕒 <b>2п.</b> | Физика (к/л 101)"),
            (2, "Физика".to_string(), "к/л 101".to_string())
        );
        // скобки внутри предмета сохраняются, кабинет — из последних скобок
        assert_eq!(
            parse_schedule_line("<b>3п.</b> | Учебная практика (по профилю) (каб. 205)"),
            (3, "Учебная практика (по профилю)".to_string(), "каб. 205".to_string())
        );
        // без | — вся строка предмет, номер -1
        assert_eq!(
            parse_schedule_line("⚠️ 1п. отмена"),
            (1, "⚠️ 1п. отмена".to_string(), "—".to_string())
        );
        // кабинет (—) — как в БД 18.11
        assert_eq!(
            parse_schedule_line("<b>1п.</b> | Основы безопасности и защиты Родины каб. 34 (—)"),
            (
                1,
                "Основы безопасности и защиты Родины каб. 34".to_string(),
                "—".to_string()
            )
        );
        assert_eq!(parse_schedule_line("нет пар"), (-1, "нет пар".to_string(), "—".to_string()));
    }

    #[test]
    fn fix_room_moves_room_out_of_brackets() {
        assert_eq!(
            fix_room("Основы безопасности и защиты Родины каб. 34", "—"),
            (
                "Основы безопасности и защиты Родины".to_string(),
                "каб. 34".to_string()
            )
        );
        assert_eq!(
            fix_room("Физика к/л 101", "—"),
            ("Физика".to_string(), "к/л 101".to_string())
        );
        assert_eq!(
            fix_room("Математика", "каб. 216"),
            ("Математика".to_string(), "каб. 216".to_string())
        );
        assert_eq!(
            fix_room("Физкультура с/з", "—"),
            ("Физкультура".to_string(), "с/з".to_string())
        );
        assert_eq!(
            fix_room("Математика каб. 216", "—"),
            ("Математика".to_string(), "каб. 216".to_string())
        );
    }

    #[test]
    fn format_day_data_merges_duplicates() {
        let records = vec![
            (
                "<b>1п.</b> | Математика (—)\n<b>2п.</b> | География (каб. 60)".to_string(),
                "основное".to_string(),
            ),
            (
                "<b>1п.</b> | Математика (каб. 216)".to_string(),
                "основное".to_string(),
            ),
        ];
        let (pairs, changes) = format_day_data(&records);
        // кабинет "—" заменяется при повторе пары
        assert_eq!(pairs.get(&1), Some(&("Математика".to_string(), "каб. 216".to_string())));
        assert_eq!(pairs.get(&2), Some(&("География".to_string(), "каб. 60".to_string())));
        assert!(changes.is_empty());

        // разные предметы в одной паре -> "A / B"
        let records2 = vec![
            ("<b>1п.</b> | Математика (каб. 1)".to_string(), "основное".to_string()),
            ("<b>1п.</b> | Информатика (каб. 2)".to_string(), "основное".to_string()),
        ];
        let (pairs2, _) = format_day_data(&records2);
        assert_eq!(
            pairs2.get(&1),
            Some(&("Математика / Информатика".to_string(), "каб. 1 / каб. 2".to_string()))
        );

        // дедупликация строк и тип "изменения"
        let records3 = vec![
            ("<b>1п.</b> | Математика (каб. 1)\n<b>1п.</b> | Математика (каб. 1)".to_string(), "основное".to_string()),
            ("⚠️ 1п. отмена".to_string(), "изменения".to_string()),
            ("🔔 Расписание по времени (фото)".to_string(), "основное".to_string()),
        ];
        let (pairs3, changes3) = format_day_data(&records3);
        assert_eq!(pairs3.len(), 1);
        assert_eq!(changes3, vec!["⚠️ 1п. отмена".to_string()]);
    }

    #[test]
    fn extract_pair_numbers_lists_and_ranges() {
        let cases: &[(&str, &[i32])] = &[
            ("1, 2п. отмена", &[1, 2]),
            ("1-3п.", &[1, 2, 3]),
            ("3п.", &[3]),
            ("1 и 2 пары", &[1, 2]),
            ("1,2,3п.", &[1, 2, 3]),
            ("5п. и 6п.", &[5, 6]),
            ("1п. 2п.", &[1, 2]),
            ("12п.", &[]),
            ("1, 2, 3 пары", &[1, 2, 3]),
            ("2-4п. замена", &[2, 3, 4]),
            ("1–3 п.", &[1, 2, 3]),
            ("8п.", &[8]),
            ("9п.", &[]),
            ("1п", &[1]),
            ("1 п.", &[1]),
            ("1, 2п", &[1, 2]),
        ];
        for (text, expected) in cases {
            let got = extract_pair_numbers(text);
            let want: HashSet<i32> = expected.iter().copied().collect();
            assert_eq!(got, want, "text={text:?}");
        }
    }

    #[test]
    fn clean_change_strips_tags_and_emoji() {
        assert_eq!(clean_change("<b>1п.</b> отмена"), "1п. отмена");
        assert_eq!(clean_change("⚠️ 1п. отмена"), "1п. отмена");
        assert_eq!(clean_change("ℹ️ расписание"), "расписание");
        assert_eq!(clean_change("A &amp; B &#8212; C"), "A & B — C");
        assert_eq!(clean_change("  <b>текст</b>  "), "текст");
    }

    #[test]
    fn normalize_group_cases() {
        assert_eq!(normalize_group("ЮР-26-21", ""), "ЮР-26-21");
        assert_eq!(normalize_group("ТД-26-21", ""), "ТД-26-21");
        // неоднозначная "26-21" — как есть
        assert_eq!(normalize_group("26-21", ""), "26-21");
        assert_eq!(normalize_group("26-21", "ТД"), "ТД-26-21");
        assert_eq!(normalize_group("26-21", "ЮР"), "ЮР-26-21");
        assert_eq!(normalize_group("26-21б", "ТД"), "ТД-26-21");
        // Python отбрасывает префикс специальности для остальных групп
        assert_eq!(normalize_group("ИП-24-27", ""), "24-27");
        assert_eq!(normalize_group("24-27б", ""), "24-27");
        assert_eq!(normalize_group("24-27", ""), "24-27");
        assert_eq!(normalize_group("БН-26-08", ""), "26-08");
        assert_eq!(normalize_group("ЮР 26 21", ""), "ЮР-26-21");
        assert_eq!(normalize_group("БН", ""), "бн");
    }

    #[test]
    fn normalize_date_cases() {
        assert_eq!(normalize_date("20.11.2026").as_deref(), Some("20.11.2026"));
        assert_eq!(normalize_date("18.11.26").as_deref(), Some("18.11.2026"));
        assert_eq!(normalize_date("18.11").as_deref(), Some("18.11.2026"));
        assert_eq!(normalize_date("ЗАМЕНА на 18 ноября 2026 г. (Среда)").as_deref(), Some("18.11.2026"));
        assert_eq!(normalize_date("31 февраля"), None);
        assert_eq!(normalize_date("18.13.2026"), None);
        assert_eq!(normalize_date("МДК.01.02 18 ноября").as_deref(), Some("18.11.2026"));
        assert_eq!(normalize_date("сбор в 9.10 18 ноября").as_deref(), Some("18.11.2026"));
        assert_eq!(normalize_date("15.06.2026").as_deref(), Some("15.06.2026"));
        assert_eq!(normalize_date("нет даты"), None);
    }

    #[test]
    fn extract_all_dates_cases() {
        let y = tyumen_now().year();
        let got = extract_all_dates_from_post("Расписание на 18, 19 ноября", "");
        assert_eq!(got, vec![format!("18.11.{y}"), format!("19.11.{y}")]);
        let got = extract_all_dates_from_post("17.11.2026-19.11.2026", "");
        assert_eq!(got, vec!["17.11.2026", "18.11.2026", "19.11.2026"]);
        // воскресенье пропускается в диапазоне: 22.11.2026 — воскресенье
        let got = extract_all_dates_from_post("20.11.2026-23.11.2026", "");
        assert_eq!(got, vec!["20.11.2026", "21.11.2026", "23.11.2026"]);
        // "18.11.2026.docx" в заголовке НЕ даёт дату: RE_DATE не
        // совпадает из-за ".docx" после года (lookahead (?![\d.])),
        // так же и в Python.
        let got = extract_all_dates_from_post("Замена 20.11.2026", "18.11.2026.docx");
        assert_eq!(got, vec!["20.11.2026"]);
    }

    #[test]
    fn course_and_shift() {
        // ноябрь 2026: поступившие в 26-м году — 1 курс
        assert_eq!(get_course_str("ЮР-26-21"), "1 курс");
        assert_eq!(get_course_str("24-27"), "3,4 курс");
        assert_eq!(get_course_str("25-01"), "2 курс");
        assert_eq!(get_course_str("группа"), "1 курс");
        let mut pairs: BTreeMap<i32, (String, String)> = BTreeMap::new();
        pairs.insert(1, ("А".into(), "1".into()));
        pairs.insert(4, ("Б".into(), "2".into()));
        assert_eq!(group_shift_from_pairs(&pairs), 1);
        pairs.insert(5, ("В".into(), "3".into()));
        assert_eq!(group_shift_from_pairs(&pairs), 2);
        assert_eq!(group_shift_from_pairs(&BTreeMap::new()), 1);
    }
}
