//! Парсинг docx-таблиц: замены и звонки.
//!
//! Эталон: services.py parse_zvonki_docx (~1830), _extract_docx_table_grid
//! (~2178), parse_docx_pure (~2238). Python использует python-docx (lxml);
//! здесь — zip + quick-xml, тот же результат: сетка таблиц word/document.xml.
//!
//! Ground truth (tests/fixtures):
//! - 18.11.2026.docx: ЮР-26-21 = ОБЖ (каб. 34), География (каб. 60),
//!   Обществознание (каб. 403) — совпадает с settings.db.
//! - 20.11.2026.docx: ЮР-26-21 = Химия (каб. 206), Физика (к/л 101),
//!   Математика (каб. 216) — в БД этого дня нет, docx единственный источник.
//!
//! Отличия от Python, продиктованные сигнатурой Rust:
//! - `parse_docx_pure(bytes)` не принимает `post_dates` / `default_date`
//!   (в Python — необязательные параметры). Дата берётся ТОЛЬКО из
//!   шапки таблицы (первые 3 строки, как в Python в первую очередь);
//!   таблица без распознаваемой даты пропускается.
//! - При нечитаемом zip / отсутствии word/document.xml возвращается
//!   Err (Python в этом случае возвращает пустой dict — здесь ошибка
//!   явная, вызывающий код решает, считать ли вложение пропущенным).
//! - Порядок ключей в parse_zvonki_docx — BTreeMap (по алфавиту),
//!   а не порядок вставки Python-dict; по ключу значения те же.

use crate::parser::{
    extract_groups_from_cell, extract_pair_numbers, find_single_pair_marker, normalize_date,
    translate_latin, KNOWN_SPECIALTY_PREFIXES,
};
use crate::{AppError, Result};
use regex::Regex;
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Cursor, Read};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Мини-DOM поверх quick-xml
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct XmlNode {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<XmlNode>,
    text: String,
}

fn local_name(name: &str) -> &str {
    match name.rfind(':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

fn xml_unescape(s: &str) -> String {
    let mut out = String::new();
    let ch: Vec<char> = s.chars().collect();
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
                    _ => {
                        if let Some(num) = entity
                            .strip_prefix("#x")
                            .or_else(|| entity.strip_prefix("#X"))
                        {
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

fn owned_start(
    e: &quick_xml::events::BytesStart<'_>,
) -> (String, Vec<(String, String)>) {
    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let mut attrs = Vec::new();
    for attr in e.attributes().flatten() {
        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
        let val = String::from_utf8_lossy(attr.value.as_ref()).into_owned();
        attrs.push((key, xml_unescape(&val)));
    }
    (name, attrs)
}

fn parse_element<R: BufRead>(
    reader: &mut quick_xml::Reader<R>,
    name: String,
    attrs: Vec<(String, String)>,
) -> std::result::Result<XmlNode, quick_xml::Error> {
    let mut node = XmlNode {
        name,
        attrs,
        children: Vec::new(),
        text: String::new(),
    };
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e)) => {
                let (n, a) = owned_start(&e);
                node.children.push(parse_element(reader, n, a)?);
            }
            Ok(quick_xml::events::Event::Empty(e)) => {
                let (n, a) = owned_start(&e);
                node.children.push(XmlNode {
                    name: n,
                    attrs: a,
                    children: Vec::new(),
                    text: String::new(),
                });
            }
            Ok(quick_xml::events::Event::End(e)) => {
                let end_name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if local_name(&end_name) == local_name(&node.name) {
                    return Ok(node);
                }
            }
            Ok(quick_xml::events::Event::Text(e)) => {
                node.text.push_str(&xml_unescape(&String::from_utf8_lossy(e.as_ref())));
            }
            Ok(quick_xml::events::Event::CData(e)) => {
                node.text.push_str(&String::from_utf8_lossy(e.as_ref()));
            }
            Ok(quick_xml::events::Event::Eof) => return Ok(node),
            Ok(_) => {}
            Err(err) => return Err(err),
        }
    }
}

fn parse_xml(bytes: &[u8]) -> Result<XmlNode> {
    let mut reader = quick_xml::Reader::from_reader(Cursor::new(bytes));
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e)) => {
                let (n, a) = owned_start(&e);
                return parse_element(&mut reader, n, a)
                    .map_err(|e| AppError::parse(format!("docx xml: {e}")));
            }
            Ok(quick_xml::events::Event::Eof) => {
                return Err(AppError::parse("docx xml: пустой document.xml"));
            }
            Ok(_) => {}
            Err(e) => return Err(AppError::parse(format!("docx xml: {e}"))),
        }
    }
}

fn read_document_xml(bytes: &[u8]) -> Result<XmlNode> {
    let cursor = Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(cursor)
        .map_err(|e| AppError::parse(format!("docx zip: {e}")))?;
    let mut file = zip
        .by_name("word/document.xml")
        .map_err(|e| AppError::parse(format!("docx zip: {e}")))?;
    let mut xml = Vec::new();
    file.read_to_end(&mut xml)?;
    parse_xml(&xml)
}

fn children_named<'a>(node: &'a XmlNode, name: &str) -> Vec<&'a XmlNode> {
    node.children
        .iter()
        .filter(|c| local_name(&c.name) == name)
        .collect()
}

fn descendants_named<'a>(node: &'a XmlNode, name: &str) -> Vec<&'a XmlNode> {
    let mut out = Vec::new();
    for child in &node.children {
        if local_name(&child.name) == name {
            out.push(child);
        }
        out.extend(descendants_named(child, name));
    }
    out
}

fn attr<'a>(node: &'a XmlNode, name: &str) -> Option<&'a str> {
    node.attrs
        .iter()
        .find(|(k, _)| local_name(k) == name)
        .map(|(_, v)| v.as_str())
}

/// Текст ячейки: склейка всех потомков w:t (без пробелов между run'ами,
/// как `"".join(tc.xpath(".//w:t/text()))` в Python).
fn cell_text_raw(tc: &XmlNode) -> String {
    let mut out = String::new();
    for t in descendants_named(tc, "t") {
        out.push_str(&t.text);
    }
    out
}

/// " ".join(s.split()) — нормализация пробелов, как в Python.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Текст ячейки для сетки (_extract_docx_table_grid): замена тире,
/// \xa0 -> пробел, нормализация пробелов.
fn grid_cell_text(tc: &XmlNode) -> String {
    let raw = cell_text_raw(tc);
    let replaced: String = raw
        .chars()
        .map(|c| match c {
            '–' | '—' | '−' => '-',
            '\u{A0}' => ' ',
            other => other,
        })
        .collect();
    normalize_ws(&replaced)
}

/// Текст ячейки для parse_zvonki_docx: только нормализация пробелов.
fn zvonki_cell_text(tc: &XmlNode) -> String {
    normalize_ws(&cell_text_raw(tc))
}

fn tc_pr<'a>(tc: &'a XmlNode) -> Option<&'a XmlNode> {
    children_named(tc, "tcPr").into_iter().next()
}

fn grid_span(tc: &XmlNode) -> usize {
    let span = tc_pr(tc)
        .and_then(|pr| children_named(pr, "gridSpan").into_iter().next())
        .and_then(|gs| attr(gs, "val"))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1);
    span.max(1)
}

/// vMerge ячейки: None — элемента нет; Some(None) — есть без w:val;
/// Some(Some(v)) — есть со значением.
fn vmerge(tc: &XmlNode) -> Option<Option<String>> {
    let pr = tc_pr(tc)?;
    let vm = children_named(pr, "vMerge").into_iter().next()?;
    Some(attr(vm, "val").map(|s| s.to_string()))
}

// ---------------------------------------------------------------------------
// Сетка таблицы (точная копия _extract_docx_table_grid, services.py:2178)
// ---------------------------------------------------------------------------

fn extract_grid(tbl: &XmlNode) -> Vec<Vec<String>> {
    let mut grid: Vec<Vec<String>> = Vec::new();
    let mut vmerge_state: HashMap<usize, String> = HashMap::new();
    for row in children_named(tbl, "tr") {
        let mut grid_row: Vec<String> = Vec::new();
        let mut col_idx = 0usize;
        if let Some(trpr) = children_named(row, "trPr").into_iter().next() {
            if let Some(gb) = children_named(trpr, "gridBefore").into_iter().next() {
                if let Some(v) = attr(gb, "val") {
                    if let Ok(pad_n) = v.parse::<usize>() {
                        for _ in 0..pad_n {
                            grid_row.push(String::new());
                            vmerge_state.remove(&col_idx);
                            col_idx += 1;
                        }
                    }
                }
            }
        }
        for tc in children_named(row, "tc") {
            let mut cell_text = grid_cell_text(tc);
            let span = grid_span(tc);
            match vmerge(tc) {
                Some(vm_val) => {
                    if vm_val.as_deref() != Some("restart") && cell_text.is_empty() {
                        cell_text = vmerge_state.get(&col_idx).cloned().unwrap_or_default();
                    } else {
                        vmerge_state.insert(col_idx, cell_text.clone());
                        for s in 1..span {
                            vmerge_state.insert(col_idx + s, String::new());
                        }
                    }
                }
                None => {
                    for s in 0..span {
                        vmerge_state.remove(&(col_idx + s));
                    }
                }
            }
            grid_row.push(cell_text);
            for _ in 1..span {
                grid_row.push(String::new());
            }
            col_idx += span;
        }
        grid.push(grid_row);
    }
    grid
}

// ---------------------------------------------------------------------------
// _format_compact_time (services.py:344)
// ---------------------------------------------------------------------------

fn re_compact_join() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\d{1,2})\s+(\d{2})").unwrap())
}

fn re_compact_colon() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(\d{1,2})[:.](\d{2})\s*[-–—−]\s*(\d{1,2})[:.](\d{2})").unwrap())
}

fn re_compact_num() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b(\d{3,4})\s*[-–—−]\s*(\d{3,4})\b").unwrap())
}

/// '800 – 930' / '1010 - 1140' / '17 40– 1750' -> '8:00 – 9:30' и т.п.
fn format_compact_time(raw_str: &str) -> String {
    let s = re_compact_join().replace_all(raw_str, "$1$2");
    let s = re_compact_colon().replace_all(&s, "$1:$2 – $3:$4");
    let s = re_compact_num().replace_all(&s, |caps: &regex::Captures| {
        let t1 = &caps[1];
        let t2 = &caps[2];
        let (h1, m1) = if t1.len() >= 3 {
            (&t1[..t1.len() - 2], &t1[t1.len() - 2..])
        } else {
            (t1, "00")
        };
        let (h2, m2) = if t2.len() >= 3 {
            (&t2[..t2.len() - 2], &t2[t2.len() - 2..])
        } else {
            (t2, "00")
        };
        let h1n: i64 = h1.parse().unwrap_or(0);
        let h2n: i64 = h2.parse().unwrap_or(0);
        format!("{h1n}:{m1} – {h2n}:{m2}")
    });
    normalize_ws(&s)
}

// ---------------------------------------------------------------------------
// Публичные функции
// ---------------------------------------------------------------------------

/// Таблицы docx: таблицы -> строки -> ячейки (текст, склеенный из параграфов).
/// Сетка разворачивается с учётом gridSpan / vMerge / gridBefore —
/// как _extract_docx_table_grid (services.py:2178).
pub fn extract_docx_tables(bytes: &[u8]) -> Result<Vec<Vec<Vec<String>>>> {
    let root = read_document_xml(bytes)?;
    let tables = descendants_named(&root, "tbl");
    Ok(tables.iter().map(|t| extract_grid(t)).collect())
}

/// docx со звонками -> {ключ курса: строки звонков}.
///
/// Ключи как в Python: "1 курс", "2 курс", "3,4 курс", "all_courses"
/// (+ варианты "1 курс_9" / "1 курс_11" для баз 9/11).
/// Точная копия parse_zvonki_docx (services.py:1830).
pub fn parse_zvonki_docx(bytes: &[u8]) -> Result<BTreeMap<String, Vec<String>>> {
    let root = read_document_xml(bytes)?;
    let tables = descendants_named(&root, "tbl");
    if tables.is_empty() {
        return Ok(BTreeMap::new());
    }
    let mut course_schedules: BTreeMap<String, Vec<String>> = BTreeMap::from([
        ("1 курс".to_string(), Vec::new()),
        ("1 курс_9".to_string(), Vec::new()),
        ("1 курс_11".to_string(), Vec::new()),
        ("2 курс".to_string(), Vec::new()),
        ("3,4 курс".to_string(), Vec::new()),
        ("all_courses".to_string(), Vec::new()),
    ]);

    for tbl in tables {
        let rows = descendants_named(tbl, "tr");
        if rows.is_empty() {
            continue;
        }
        // Ищем строку заголовков курсов (в первых 6 строках)
        let mut header_row_idx: Option<usize> = None;
        let mut col_map: BTreeMap<usize, String> = BTreeMap::new();
        for (r_idx, r) in rows.iter().take(6).enumerate() {
            let cells_text: Vec<String> = children_named(r, "tc")
                .iter()
                .map(|c| zvonki_cell_text(c))
                .collect();
            // в Python здесь .//w:tc (потомки); прямых потомков достаточно,
            // вложенных таблиц в документах звонков нет
            let mut temp_map: BTreeMap<usize, String> = BTreeMap::new();
            for (idx, h) in cells_text.iter().enumerate() {
                let h_lower = h.to_lowercase();
                if h_lower.contains("утверждаю") || h_lower.contains("директор") {
                    continue;
                }
                if matches_all_courses(&h_lower) {
                    temp_map.insert(idx, "all_courses".to_string());
                } else if h_lower.contains("9 кл") {
                    temp_map.insert(idx, "1 курс_9".to_string());
                } else if h_lower.contains("11 кл") {
                    temp_map.insert(idx, "1 курс_11".to_string());
                } else if h_lower.contains("2 курс") {
                    temp_map.insert(idx, "2 курс".to_string());
                } else if (h_lower.contains('3') || h_lower.contains('4'))
                    && h_lower.contains("курс")
                {
                    temp_map.insert(idx, "3,4 курс".to_string());
                } else if h_lower.contains("1 курс") {
                    temp_map.insert(idx, "1 курс".to_string());
                }
            }
            if !temp_map.is_empty() {
                header_row_idx = Some(r_idx);
                col_map = temp_map;
                break;
            }
        }
        let Some(header_idx) = header_row_idx else {
            continue;
        };
        if col_map.is_empty() {
            continue;
        }
        let min_col = *col_map.keys().next().unwrap();
        let mut current_shift = "1 смена";
        for r in &rows[header_idx + 1..] {
            let cells: Vec<String> = children_named(r, "tc")
                .iter()
                .map(|c| zvonki_cell_text(c))
                .collect();
            if !cells.iter().any(|c| !c.is_empty()) {
                continue;
            }
            let first_cell = cells.first().cloned().unwrap_or_default();
            let first_lower = first_cell.to_lowercase();
            if first_lower.contains("1 смена") {
                current_shift = "1 смена";
                continue;
            }
            if first_lower.contains("2 смена") {
                current_shift = "2 смена";
                continue;
            }
            if first_lower.contains("перемена") || first_lower == "пер." {
                continue;
            }
            let m_p = find_pair_number(&first_lower);
            let is_special_event =
                first_lower.contains("разговоры") || first_lower.contains("классный час");
            let non_empty_cols: Vec<usize> = col_map
                .keys()
                .filter(|c_idx| {
                    **c_idx < cells.len() && !cells[**c_idx].trim().is_empty()
                })
                .copied()
                .collect();
            let is_shared_row = non_empty_cols.len() == 1
                && non_empty_cols[0] == min_col
                && (current_shift == "2 смена" || first_lower.contains("разговоры"));
            for (c_idx, target_key) in &col_map {
                let raw_val = if is_shared_row {
                    cells.get(min_col).cloned().unwrap_or_default()
                } else {
                    cells.get(*c_idx).cloned().unwrap_or_default()
                };
                let raw_val = raw_val.trim().to_string();
                if raw_val.is_empty() {
                    continue;
                }
                // Строка отдельного обеда
                if first_cell.is_empty() && raw_val.to_lowercase().contains("обед") {
                    let t_clean =
                        format_compact_time(&remove_word_ci(&raw_val, "обед"));
                    course_schedules
                        .entry(target_key.clone())
                        .or_default()
                        .push(format!("Обед: {t_clean}"));
                    continue;
                }
                if is_special_event {
                    let t_clean = format_compact_time(&raw_val);
                    let label = if first_lower.contains("разговоры") {
                        "Разговоры о важном"
                    } else {
                        "Классный час"
                    };
                    course_schedules
                        .entry(target_key.clone())
                        .or_default()
                        .push(format!("{label}: {t_clean}"));
                    continue;
                }
                if let Some(p_num) = m_p.as_ref() {
                    let mut lunch_str = String::new();
                    let main_time_raw: String;
                    if raw_val.to_lowercase().contains("обед") {
                        let (before, after) = split_once_ci(&raw_val, "обед");
                        let lunch_fmt = format_compact_time(&after);
                        if !lunch_fmt.is_empty() {
                            lunch_str = format!("Обед {lunch_fmt}");
                        }
                        main_time_raw = before.trim().to_string();
                    } else {
                        main_time_raw = raw_val.clone();
                    }
                    let mut t_fmt = format_compact_time(&main_time_raw);
                    t_fmt = re_two_halves().replace_all(&t_fmt, "$1 / $2").into_owned();
                    let note = if lunch_str.is_empty() {
                        String::new()
                    } else {
                        format!(" ({lunch_str})")
                    };
                    course_schedules
                        .entry(target_key.clone())
                        .or_default()
                        .push(format!("{p_num}п: {t_fmt}{note}"));
                }
            }
        }
    }
    Ok(course_schedules
        .into_iter()
        .filter(|(_, v)| !v.is_empty())
        .collect())
}

fn re_two_halves() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(\d{1,2}:\d{2}\s*–\s*\d{1,2}:\d{2})\s+(\d{1,2}:\d{2}\s*–\s*\d{1,2}:\d{2})")
            .unwrap()
    })
}

/// `1\s*[-–—−]\s*4\s*курс | все\s*курс | общ\w*\s*звонк` (в нижнем регистре).
fn matches_all_courses(h: &str) -> bool {
    let ch: Vec<char> = h.chars().collect();
    // 1 - 4 курс
    let mut i = 0usize;
    while i < ch.len() {
        if ch[i] == '1' {
            let mut p = i + 1;
            while p < ch.len() && ch[p].is_whitespace() {
                p += 1;
            }
            if p < ch.len() && matches!(ch[p], '-' | '–' | '—' | '−') {
                p += 1;
                while p < ch.len() && ch[p].is_whitespace() {
                    p += 1;
                }
                if p < ch.len() && ch[p] == '4' {
                    p += 1;
                    while p < ch.len() && ch[p].is_whitespace() {
                        p += 1;
                    }
                    let rest: String = ch[p..].iter().collect();
                    if rest.starts_with("курс") {
                        return true;
                    }
                }
            }
        }
        i += 1;
    }
    // все курс
    if let Some(pos) = find_substr(&ch, "все") {
        let mut p = pos + 3;
        while p < ch.len() && ch[p].is_whitespace() {
            p += 1;
        }
        let rest: String = ch[p..].iter().collect();
        if rest.starts_with("курс") {
            return true;
        }
    }
    // общ\w* звонк
    if let Some(pos) = find_substr(&ch, "общ") {
        let mut p = pos + 3;
        while p < ch.len() && (ch[p].is_alphanumeric() || ch[p] == '_') {
            p += 1;
        }
        while p < ch.len() && ch[p].is_whitespace() {
            p += 1;
        }
        let rest: String = ch[p..].iter().collect();
        if rest.starts_with("звонк") {
            return true;
        }
    }
    false
}

fn find_substr(hay: &[char], needle: &str) -> Option<usize> {
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() || hay.len() < n.len() {
        return None;
    }
    for i in 0..=hay.len() - n.len() {
        if hay[i..i + n.len()] == n[..] {
            return Some(i);
        }
    }
    None
}

/// `re.search(r"(\d+)\s*п", first_lower)` — номер пары из первой ячейки.
fn find_pair_number(s: &str) -> Option<String> {
    let ch: Vec<char> = s.chars().collect();
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
            if k < ch.len() && ch[k] == 'п' {
                return Some(ch[i..j].iter().collect());
            }
        }
        i += 1;
    }
    None
}

/// Удалить все вхождения слова (без учёта регистра) — как re.sub(word, "", s, IGNORECASE).
fn remove_word_ci(s: &str, word: &str) -> String {
    let ch: Vec<char> = s.chars().collect();
    let w: Vec<char> = word.chars().collect();
    let low: Vec<char> = ch.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < ch.len() {
        if i + w.len() <= low.len() && low[i..i + w.len()] == w[..] {
            i += w.len();
        } else {
            out.push(ch[i]);
            i += 1;
        }
    }
    out
}

/// Разделить по первому вхождению слова (без учёта регистра) — как re.split(word, s, IGNORECASE)[0..2].
fn split_once_ci(s: &str, word: &str) -> (String, String) {
    let ch: Vec<char> = s.chars().collect();
    let w: Vec<char> = word.chars().collect();
    let low: Vec<char> = ch.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    for i in 0..ch.len() {
        if i + w.len() <= low.len() && low[i..i + w.len()] == w[..] {
            let before: String = ch[..i].iter().collect();
            // parts[1] — до следующего "обед" или до конца
            let mut end = ch.len();
            for j in i + w.len()..ch.len() {
                if j + w.len() <= low.len() && low[j..j + w.len()] == w[..] {
                    end = j;
                    break;
                }
            }
            let after: String = ch[i + w.len()..end].iter().collect();
            return (before, after);
        }
    }
    (s.to_string(), String::new())
}

// --- parse_docx_pure -------------------------------------------------------

/// RE_ROOM (services.py): `(каб|лаб|с/з|чит\.?\s*зал|к/л|экскурсия|акт\.?\s*зал|библ|\d{1,3})`
/// IGNORECASE — поиск в строке.
fn room_like(text: &str) -> bool {
    let low = text.to_lowercase();
    if low.contains("каб")
        || low.contains("лаб")
        || low.contains("с/з")
        || low.contains("к/л")
        || low.contains("экскурсия")
        || low.contains("библ")
    {
        return true;
    }
    if low.chars().any(|c| c.is_ascii_digit()) {
        return true;
    }
    let ch: Vec<char> = low.chars().collect();
    for word in ["чит", "акт"] {
        if let Some(pos) = find_substr(&ch, word) {
            let mut p = pos + 3;
            if p < ch.len() && ch[p] == '.' {
                p += 1;
            }
            while p < ch.len() && ch[p].is_whitespace() {
                p += 1;
            }
            let rest: String = ch[p..].iter().collect();
            if rest.starts_with("зал") {
                return true;
            }
        }
    }
    false
}

/// RE_ROOM.fullmatch(val): вся строка — один из вариантов.
fn room_fullmatch(val: &str) -> bool {
    let low = val.to_lowercase();
    match low.as_str() {
        "каб" | "лаб" | "с/з" | "к/л" | "экскурсия" | "библ" => return true,
        _ => {}
    }
    if !low.is_empty()
        && low.len() <= 3
        && low.chars().all(|c| c.is_ascii_digit())
    {
        return true;
    }
    for (word, rest) in [("чит", "зал"), ("акт", "зал")] {
        if let Some(after) = low.strip_prefix(word) {
            let after = after.strip_prefix('.').unwrap_or(after);
            let after = after.trim_start();
            if after == rest {
                return true;
            }
        }
    }
    false
}

fn is_cyr_upper_word(cand: &str) -> bool {
    let len = cand.chars().count();
    (2..=5).contains(&len)
        && cand
            .chars()
            .all(|c| ('А'..='Я').contains(&c) || c == 'Ё')
}

/// docx с заменами -> (дата "ДД.ММ.ГГГГ", группа, строки расписания).
///
/// Используется process_vk_posts для docx-вложений из VK.
/// Точная копия parse_docx_pure (services.py:2238), за вычетом
/// необязательных параметров post_dates/default_date (см. шапку файла).
pub fn parse_docx_pure(bytes: &[u8]) -> Result<Vec<(String, String, Vec<String>)>> {
    let root = read_document_xml(bytes)?;
    let tables = descendants_named(&root, "tbl");
    if tables.is_empty() {
        return Ok(Vec::new());
    }
    // (дата, группа) -> строки; порядок первого появления сохраняется.
    let mut result: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut index_of: HashMap<(String, String), usize> = HashMap::new();

    for tbl in tables {
        let grid_rows = extract_grid(tbl);
        if grid_rows.is_empty() {
            continue;
        }
        let mut current_table_date: Option<String> = None;
        for r_cells in grid_rows.iter().take(3) {
            let r_text = r_cells
                .iter()
                .filter(|c| !c.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join(" ");
            if let Some(d) = normalize_date(&r_text) {
                current_table_date = Some(d);
                break;
            }
        }
        let Some(current_date) = current_table_date else {
            continue;
        };

        let mut current_prefix = String::new();
        let mut last_row_groups: Vec<String> = Vec::new();

        for cells in &grid_rows {
            let idx = match cells
                .iter()
                .position(|c| crate::parser::cell_has_pair_marker(c))
            {
                Some(i) => i,
                None => {
                    if cells.iter().any(|c| !c.is_empty()) {
                        last_row_groups.clear();
                    }
                    continue;
                }
            };

            // Префикс специальности — строго из колонок отделения (перед группой/парой)
            let mut prefix_changed = false;
            let scan_limit = if idx > 0 {
                idx
            } else {
                cells.len().min(3)
            };
            for p_col in 0..scan_limit {
                if cells[p_col].is_empty() {
                    continue;
                }
                let cand = translate_latin(cells[p_col].trim()).to_uppercase();
                if KNOWN_SPECIALTY_PREFIXES.contains(&cand.as_str())
                    || (idx >= 2 && p_col + 1 < idx && is_cyr_upper_word(&cand))
                {
                    if cand != current_prefix {
                        current_prefix = cand;
                        prefix_changed = true;
                    }
                    break;
                }
            }

            // 1. Группа — строго из ячеек ПЕРЕД номером пары
            let group_cell = if idx >= 1 { cells[idx - 1].as_str() } else { "" };
            let mut detected_groups =
                extract_groups_from_cell(group_cell, &current_prefix);
            if detected_groups.is_empty() {
                let pre_str = cells[..idx].join(" ");
                detected_groups = extract_groups_from_cell(&pre_str, &current_prefix);
            }
            if detected_groups.is_empty()
                && !last_row_groups.is_empty()
                && !prefix_changed
                && group_cell.is_empty()
            {
                detected_groups = last_row_groups.clone();
            }
            if detected_groups.is_empty() {
                continue;
            }
            last_row_groups = detected_groups.clone();

            // 2. Предмет и кабинет — из ячеек ПОСЛЕ номера пары
            let after_cells = &cells[idx + 1..];
            let mut subj = "—".to_string();
            let mut room = "—".to_string();
            if after_cells.len() >= 2 {
                let mut raw_subj = normalize_ws(&after_cells[0]);
                let mut raw_room = normalize_ws(&after_cells[1]);
                if after_cells.len() > 2 {
                    let extra = after_cells[1..after_cells.len() - 1]
                        .iter()
                        .filter(|c| !c.trim().is_empty())
                        .map(|c| normalize_ws(c))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !extra.is_empty() && !room_like(&raw_room) {
                        raw_subj = format!("{raw_subj} {extra}").trim().to_string();
                        raw_room = normalize_ws(&after_cells[after_cells.len() - 1]);
                    }
                }
                if !raw_subj.is_empty() {
                    subj = raw_subj;
                }
                if !raw_room.is_empty() {
                    room = raw_room;
                }
            } else if after_cells.len() == 1 {
                let val = normalize_ws(&after_cells[0]);
                if !val.is_empty() {
                    if room_fullmatch(&val) {
                        room = val;
                    } else {
                        subj = val;
                    }
                }
            }

            let raw_pair_str = normalize_ws(&cells[idx]);
            let mut pair_nums: Vec<i32> =
                extract_pair_numbers(&raw_pair_str).into_iter().collect();
            pair_nums.sort_unstable();
            if pair_nums.is_empty() {
                if let Some(single) = find_single_pair_marker(&raw_pair_str) {
                    pair_nums = vec![single];
                }
            }
            let pair_labels: Vec<String> = if pair_nums.is_empty() {
                vec![raw_pair_str.clone()]
            } else {
                pair_nums.iter().map(|p| format!("{p}п.")).collect()
            };

            for g in &detected_groups {
                let key = (current_date.clone(), g.clone());
                let entry_idx = *index_of.entry(key.clone()).or_insert_with(|| {
                    result.push((current_date.clone(), g.clone(), Vec::new()));
                    result.len() - 1
                });
                for p_lbl in &pair_labels {
                    result[entry_idx]
                        .2
                        .push(format!("<b>{p_lbl}</b> | {subj} ({room})"));
                }
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(name: &str) -> Vec<u8> {
        fs::read(format!("tests/fixtures/{name}")).expect("fixture readable")
    }

    #[test]
    fn extract_tables_18_11_shape() {
        let tables = extract_docx_tables(&fixture("18.11.2026.docx")).unwrap();
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].len(), 215);
        // шапка с датой
        assert!(tables[0][0].join(" ").contains("18 ноября 2026"));
        // первая строка данных после gridSpan-развёртки: таблица имеет
        // сетку из 32 колонок ('Математика' — gridSpan 24, 'каб. 1' — 2).
        let mut expected = vec![""; 32];
        expected[1] = "БН";
        expected[3] = "26-08";
        expected[5] = "1п.";
        expected[6] = "Математика";
        expected[30] = "каб. 1";
        assert_eq!(tables[0][2], expected);
    }

    #[test]
    fn parse_pure_18_11_yur() {
        // ВАЖНО: это дословный вывод Python parse_docx_pure. В таблице
        // 18.11 сетка 32-колоночная, и кабинет оказывается в «extra»:
        // Python склеивает его в предмет, а room остаётся "—".
        // Чистый вид (каб. 34) даёт только связка
        // parse_schedule_line + fix_room (см. интеграционный тест).
        let parsed = parse_docx_pure(&fixture("18.11.2026.docx")).unwrap();
        let yur: Vec<_> = parsed
            .iter()
            .filter(|(d, g, _)| d == "18.11.2026" && g == "ЮР-26-21")
            .collect();
        assert_eq!(yur.len(), 1);
        assert_eq!(
            yur[0].2,
            vec![
                "<b>1п.</b> | Основы безопасности и защиты Родины каб. 34 (—)",
                "<b>2п.</b> | География каб. 60 (—)",
                "<b>3п.</b> | Обществознание каб. 403 (—)",
            ]
        );
    }

    #[test]
    fn parse_pure_18_11_other_groups() {
        let parsed = parse_docx_pure(&fixture("18.11.2026.docx")).unwrap();
        let get = |group: &str| -> Vec<String> {
            parsed
                .iter()
                .find(|(d, g, _)| d == "18.11.2026" && g == group)
                .map(|(_, _, lines)| lines.clone())
                .unwrap_or_default()
        };
        assert_eq!(
            get("ТД-26-21"),
            vec![
                "<b>1п.</b> | Стандартизация, метрология и техническое регулирование лаб. 207 (—)",
                "<b>2п.</b> | МДК.04.02 к/л 201 (—)",
                "<b>3п.</b> | Документационное обеспечение управления каб. 302 (—)",
            ]
        );
        // Ключ группы — нормализованный по Python: префикс специальности
        // отбрасывается (normalize_group), поэтому "БН-26-08" -> "26-08";
        // ТД/ЮР-26-21 сохраняют префикс (явная ветка 26-21).
        // У 26-08 на 18.11 три пары (3п. — ОБЖ, как и у ЮР-26-21).
        assert_eq!(
            get("26-08"),
            vec![
                "<b>1п.</b> | Математика каб. 1 (—)",
                "<b>2п.</b> | Математика каб. 1 (—)",
                "<b>3п.</b> | Основы безопасности и защиты Родины каб. 34 (—)",
            ]
        );
    }

    #[test]
    fn parse_pure_20_11_yur() {
        let parsed = parse_docx_pure(&fixture("20.11.2026.docx")).unwrap();
        let yur: Vec<_> = parsed
            .iter()
            .filter(|(d, g, _)| d == "20.11.2026" && g == "ЮР-26-21")
            .collect();
        assert_eq!(yur.len(), 1);
        assert_eq!(
            yur[0].2,
            vec![
                "<b>1п.</b> | Химия (каб. 206)",
                "<b>2п.</b> | Физика (к/л 101)",
                "<b>3п.</b> | Математика (каб. 216)",
            ]
        );
    }

    #[test]
    fn parse_pure_20_11_other_groups() {
        let parsed = parse_docx_pure(&fixture("20.11.2026.docx")).unwrap();
        let get = |group: &str| -> Vec<String> {
            parsed
                .iter()
                .find(|(d, g, _)| d == "20.11.2026" && g == group)
                .map(|(_, _, lines)| lines.clone())
                .unwrap_or_default()
        };
        assert_eq!(
            get("ТД-26-21"),
            vec![
                "<b>4п.</b> | МДК.01.01 (лаб. 207)",
                "<b>5п.</b> | МДК.01.01 (лаб. 207)",
                "<b>6п.</b> | МДК.01.01 (лаб. 207)",
            ]
        );
        assert_eq!(
            get("26-08"),
            vec![
                "<b>2п.</b> | Математика (каб. 1)",
                "<b>3п.</b> | Обществознание (лаб. 209)",
                "<b>4п.</b> | Химия (каб. 5)",
            ]
        );
    }

    #[test]
    fn format_compact_time_cases() {
        assert_eq!(format_compact_time("800 – 930"), "8:00 – 9:30");
        assert_eq!(format_compact_time("1010 - 1140"), "10:10 – 11:40");
        assert_eq!(format_compact_time("17 40– 1750"), "17:40 – 17:50");
        assert_eq!(format_compact_time("8:00-9:30"), "8:00 – 9:30");
        assert_eq!(format_compact_time("930 – 1010"), "9:30 – 10:10");
    }

    #[test]
    fn zvonki_empty_on_replacement_docx() {
        // В docx с заменами нет таблицы звонков — пустой результат.
        let zv = parse_zvonki_docx(&fixture("18.11.2026.docx")).unwrap();
        assert!(zv.is_empty());
    }

    #[test]
    fn bad_bytes_error() {
        assert!(extract_docx_tables(b"not a docx").is_err());
        assert!(parse_docx_pure(b"").is_err());
    }
}
