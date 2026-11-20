//! Данные преподавателей и маппинг групп display ↔ ключ — только на сервере.
//!
//! Источник данных — Python-оригинал Mini App (только чтение):
//! - `schedule-miniapp/work/teachers_map.js` — TEACHERS (предмет →
//!   [Фамилия И.О.]), TEACHER_ALIASES;
//! - `schedule-miniapp/work/spec_map.js` — SPEC_TEACHERS
//!   (специальность → предмет → преподаватели).
//! А group_spec / spec_names / group_display — из таблицы образовательных
//! программ `assets/programs_table.txt` (ИСТОЧНИК ПРАВДЫ: 94 display-группы,
//! 18 специальностей, 10 неоднозначных голых ключей); конвертер
//! `scripts/convert_teachers.py` строит их из таблицы и сверяет group_spec
//! 1-в-1 с GROUP_SPEC из spec_map.js. Названия специальностей — полные
//! официальные из таблицы (напр. «Экономика и бухгалтерский учет
//! (по отраслям)»), квалификация из скобок в JSON не хранится.
//! Перестройка без капа — `scripts/rebuild/` (копии исходных скриптов
//! с убранным `len(lst) < 3` и без diff-фильтра SPEC_TEACHERS):
//! parse из 18 PDF (pdftotext) -> teachers_all.json (414 записей,
//! байт-в-байт как оригинал) -> карты. Конвертер
//! `scripts/convert_teachers.py` (только stdlib) генерирует из них
//! закоммиченные ассеты `assets/teachers.json` и `assets/spec_teachers.json`;
//! здесь они грузятся через `include_str!` + `serde_json` один раз на старте
//! (OnceLock). В `webapp/index.html` этих данных быть не должно.
//!
//! Кап «максимум 3 на предмет» УБРАН полностью: раньше в TEACHERS было
//! 80 уникальных из 85, максимум 3 на предмет; теперь — все.
//! ВАЖНО про «90»: в teachers_clean.json 90 сырых строк имён, но 5 из них —
//! склеенные записи вида «Шамухаметов Р.Р. Шапенова К.К.» (два человека
//! в одной строке PDF), посчитанные как отдельные «преподаватели».
//! Реальных людей (ФИО из 3 частей, склеенные записи разделены) — 85,
//! коллизий «Фамилия И.О.» нет, потерь в пайплайне нет (union полных
//! карт специальностей == union TEACHERS == 85). Поэтому в обоих файлах —
//! 85 уникальных, а не 90: 90 недостижимо без выдумывания людей.
//! SPEC_TEACHERS — ПОЛНЫЕ карты (diff-фильтр «только отличия от
//! глобального» убран): иначе 15 преподавателей, чей список совпадает
//! с глобальным, в файле отсутствовали бы в принципе. На результат
//! teacherFor это не влияет (где списки совпадают, возвращается тот же
//! список). Известно: в данных есть мусорный ключ «выс» (обрывок
//! «Высшее образование» из PDF, 10 фамилий) — он был и в исходной карте,
//! точные совпадения предметов он не затрагивает.
//!
//! Логика — точная копия JS из `webapp/index.html`:
//! - `teacherFor` / `teacherForUncached` / `specTeachersFor` / `specForGroup` /
//!   `normSubj` / `normGroup`;
//! - `groupPrefix` — только для определения, есть ли у ключа префикс.
//!
//! Маппинг групп:
//! - В БД ключ голый — «24-27» (в schedule_archive 30 строк «24-27»,
//!   «ИП-24-27» там нет); в Mini App показывается с префиксом — «ИП-24-27»
//!   (`group_display`, однозначные ключи берутся из `group_display` в
//!   ассете: голый ключ, у которого в GROUP_SPEC есть своя запись и ровно
//!   один display-вариант с тем же кодом специальности).
//! - ИСКЛЮЧЕНИЕ — 26-21: в БД лежат ПОЛНЫЕ ключи с префиксом (chat_settings:
//!   ТД-26-21 — 22, ЮР-26-21 — 22, голой «26-21» — 0; schedule_archive:
//!   ТД-26-21 — 45, ЮР-26-21 — 50). Для них display == key.
//! - Голая «26-21» (3 строки в архиве — остатки неоднозначного парсинга)
//!   отдельной группой НЕ считается: `group_display("26-21") == "26-21"`,
//!   `spec_for_group("26-21") == None`, а запрос расписания по ней —
//!   по-прежнему 422 с choices (это решает api.rs, не этот модуль).
//! - 10 голых ключей без собственной записи в GROUP_SPEC (26-25, 24-21,
//!   26-22, 25-22, 26-23, 25-24, 25-25, 26-21, 24-23, 26-24) имеют по два
//!   display с РАЗНЫМИ специальностями — для них display = key и spec = None:
//!   сервер не может знать специальность по голому ключу.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

#[derive(Debug, Deserialize)]
struct TeachersData {
    /// [[предмет (lowercase), [Фамилия И.О.]]] — массив, а не объект:
    /// порядок ключей важен для резервного поиска teacherFor
    /// (префикс/подстрока, первый по порядку), а serde_json без
    /// preserve_order порядок объекта не сохраняет.
    teachers: Vec<(String, Vec<String>)>,
    aliases: HashMap<String, Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct SpecData {
    group_spec: HashMap<String, String>,
    spec_names: HashMap<String, String>,
    spec_teachers: HashMap<String, HashMap<String, Vec<String>>>,
    /// Голый ключ -> display, только однозначные (см. шапку модуля).
    group_display: HashMap<String, String>,
}

struct Teachers {
    /// Порядок — как в исходном TEACHERS (для резервного поиска).
    ordered: Vec<(String, Vec<String>)>,
    by_subject: HashMap<String, Vec<String>>,
    aliases: HashMap<String, Vec<String>>,
}

struct Specs {
    group_spec: HashMap<String, String>,
    spec_names: HashMap<String, String>,
    spec_teachers: HashMap<String, HashMap<String, Vec<String>>>,
    group_display: HashMap<String, String>,
}

fn teachers_data() -> &'static Teachers {
    static DATA: OnceLock<Teachers> = OnceLock::new();
    DATA.get_or_init(|| {
        let raw: TeachersData = serde_json::from_str(include_str!("../assets/teachers.json"))
            .expect("assets/teachers.json: невалидный JSON");
        let by_subject: HashMap<String, Vec<String>> =
            raw.teachers.iter().cloned().collect();
        Teachers {
            ordered: raw.teachers,
            by_subject,
            aliases: raw.aliases,
        }
    })
}

fn specs_data() -> &'static Specs {
    static DATA: OnceLock<Specs> = OnceLock::new();
    DATA.get_or_init(|| {
        let raw: SpecData =
            serde_json::from_str(include_str!("../assets/spec_teachers.json"))
                .expect("assets/spec_teachers.json: невалидный JSON");
        Specs {
            group_spec: raw.group_spec,
            spec_names: raw.spec_names,
            spec_teachers: raw.spec_teachers,
            group_display: raw.group_display,
        }
    })
}

/// normSubj из index.html: lowercase, схлопнуть пробелы, trim.
pub fn norm_subject(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// normGroup из index.html: убрать все пробелы, снять один trailing «б»,
/// uppercase.
pub fn norm_group_key(s: &str) -> String {
    let compact: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let trimmed = compact
        .strip_suffix(['б', 'Б'])
        .unwrap_or(&compact)
        .to_string();
    trimmed.to_uppercase()
}

/// Голый ключ display-группы: «ИП-24-27» -> «24-27», «24-27» -> «24-27».
/// Только для тестов (programs_table_counts).
#[cfg(test)]
fn bare_key(key: &str) -> &str {
    match key.split_once('-') {
        Some((_, rest)) if has_prefix(key) => rest,
        _ => key,
    }
}

/// Есть ли у ключа префикс специальности: «ПРЕФИКС-..» (буквы + дефис).
/// «24-27» — нет, «ИП-24-27» / «ЮР-26-21» — да.
pub fn has_prefix(key: &str) -> bool {
    match key.split_once('-') {
        Some((pre, _)) => {
            !pre.is_empty()
                && pre
                    .chars()
                    .all(|c| c.is_alphabetic() && c.is_uppercase())
        }
        None => false,
    }
}

/// specForGroup из index.html: сначала GROUP_SPEC[нормализованная группа],
/// иначе — снять префикс и искать голый ключ.
pub fn spec_for_group(group: &str) -> Option<String> {
    let g = norm_group_key(group);
    if g.is_empty() {
        return None;
    }
    let data = specs_data();
    if let Some(spec) = data.group_spec.get(&g) {
        return Some(spec.clone());
    }
    // Группа с префиксом, которой нет в таблице как есть:
    // «ПРЕФИКС-24-27» -> «24-27».
    if let Some((_, rest)) = g.split_once('-') {
        if let Some(spec) = data.group_spec.get(rest) {
            return Some(spec.clone());
        }
    }
    None
}

pub fn spec_name(spec: &str) -> Option<&'static str> {
    specs_data().spec_names.get(spec).map(|s| s.as_str())
}

/// Display-имя группы для Mini App.
/// - «ЮР-26-21» / «ТД-26-21» (и любой ключ с префиксом) — как есть;
/// - голый ключ с однозначным display — «ИП-24-27»;
/// - иначе (нет в таблице / неоднозначен, напр. «26-21», «26-25») — key.
pub fn group_display(key: &str) -> String {
    if has_prefix(key) {
        return key.to_string();
    }
    specs_data()
        .group_display
        .get(key)
        .cloned()
        .unwrap_or_else(|| key.to_string())
}

/// Снять trailing « (...)» — как `q.replace(/\s*\([^)]*\)\s*$/, "")` в JS.
fn strip_trailing_parens(q: &str) -> &str {
    let t = q.trim_end();
    if !t.ends_with(')') {
        return q;
    }
    match t.rfind('(') {
        Some(i) if !t[i + 1..t.len() - 1].contains(')') => t[..i].trim_end(),
        _ => q,
    }
}

/// specTeachersFor из index.html: предмет/алиасы/без скобок — только
/// в переопределениях специальности.
fn spec_teachers_for(q: &str, spec: &str) -> Option<Vec<String>> {
    let data = specs_data();
    let by_spec = data.spec_teachers.get(spec)?;
    if let Some(v) = by_spec.get(q) {
        return Some(v.clone());
    }
    if let Some(aliases) = data_aliases().get(q) {
        let mut out: Vec<String> = Vec::new();
        for k in aliases {
            if let Some(v) = by_spec.get(k) {
                for t in v {
                    if !out.contains(t) {
                        out.push(t.clone());
                    }
                }
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    let q2 = strip_trailing_parens(q);
    if q2 != q {
        if let Some(v) = by_spec.get(q2) {
            return Some(v.clone());
        }
    }
    // Голый код («МДК.02.03») в расписании, а в переопределениях — полное
    // имя («мдк.02.03 уголовное право»): префиксный поиск внутри программы.
    // Несколько разных «мдк.02.03 *» в одной программе — неоднозначно, None.
    let mut found: Option<Vec<String>> = None;
    for (k, v) in by_spec {
        if k.chars().count() >= 5 && (k.starts_with(q) || q.starts_with(k.as_str())) {
            match &found {
                None => found = Some(v.clone()),
                Some(prev) if prev == v => {}
                Some(_) => return None,
            }
        }
    }
    found
}

/// Резервный поиск по TEACHERS: первый ключ НЕ годится — голый код
/// («мдк.02.03») это разные предметы в разных программах («оценка
/// качества» — Муравьева, «уголовное право» — Степанова). Возвращаем
/// преподавателей, только если все подошедшие ключи дают один и тот же
/// список; иначе None — лучше никого, чем чужого.
fn fuzzy_unique(
    q: &str,
    ordered: &[(String, Vec<String>)],
    pred: impl Fn(&str, &str) -> bool,
) -> Option<Vec<String>> {
    let mut found: Option<Vec<String>> = None;
    for (k, v) in ordered {
        if k.chars().count() < 5 || !pred(k, q) {
            continue;
        }
        match &found {
            None => found = Some(v.clone()),
            Some(prev) if prev == v => {}
            Some(_) => return None,
        }
    }
    found
}

fn data_aliases() -> &'static HashMap<String, Vec<String>> {
    &teachers_data().aliases
}

/// Результат teacherFor: специальность группы + преподаватели предмета.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeachersFor {
    pub spec: Option<String>,
    pub spec_name: Option<String>,
    pub teachers: Vec<String>,
}

/// teacherForUncached из index.html: сначала переопределения программы
/// группы (SPEC_TEACHERS), иначе общий TEACHERS, с учётом TEACHER_ALIASES,
/// затем резервный поиск по префиксу/подстроке (только ключи длиной >= 5).
pub fn teachers_for(subject: &str, group: Option<&str>) -> TeachersFor {
    let q = norm_subject(subject);
    if q.is_empty() {
        return TeachersFor {
            spec: None,
            spec_name: None,
            teachers: Vec::new(),
        };
    }
    let spec = group.and_then(spec_for_group);
    if let Some(s) = &spec {
        if let Some(v) = spec_teachers_for(&q, s) {
            return TeachersFor {
                spec_name: spec_name(s).map(str::to_string),
                spec: Some(s.clone()),
                teachers: v,
            };
        }
    }
    let data = teachers_data();
    let mut found: Option<Vec<String>> = data.by_subject.get(&q).cloned();
    if found.is_none() {
        if let Some(aliases) = data.aliases.get(&q) {
            let mut out: Vec<String> = Vec::new();
            for k in aliases {
                if let Some(v) = data.by_subject.get(k) {
                    for t in v {
                        if !out.contains(t) {
                            out.push(t.clone());
                        }
                    }
                }
            }
            if !out.is_empty() {
                found = Some(out);
            }
        }
    }
    if found.is_none() {
        let q2 = strip_trailing_parens(&q);
        if q2 != q {
            found = data.by_subject.get(q2).cloned();
        }
    }
    if found.is_none() && q.chars().count() >= 5 {
        // Резервный поиск — как в JS, но без «первого попавшегося» ключа:
        // голый код («мдк.02.03») — разные предметы в разных программах.
        found = fuzzy_unique(&q, &data.ordered, |k, q| {
            k.starts_with(q) || q.starts_with(k)
        });
        if found.is_none() {
            found = fuzzy_unique(&q, &data.ordered, |k, q| {
                k.contains(q) || q.contains(k)
            });
        }
    }
    TeachersFor {
        spec_name: spec.as_ref().and_then(|s| spec_name(s)).map(str::to_string),
        spec,
        teachers: found.unwrap_or_default(),
    }
}

/// Преподаватели предмета без привязки к группе: общий список, а если
/// предмета нет в общем списке — объединение переопределений всех
/// специальностей (для /api/search: предмет может вестись только в части
/// программ). Порядок детерминирован (коды специальностей по возрастанию).
pub fn teachers_for_any(subject: &str) -> Vec<String> {
    let q = norm_subject(subject);
    if q.is_empty() {
        return Vec::new();
    }
    let global = teachers_for(subject, None).teachers;
    if !global.is_empty() {
        return global;
    }
    let data = specs_data();
    let mut specs: Vec<&String> = data.spec_teachers.keys().collect();
    specs.sort();
    let mut out: Vec<String> = Vec::new();
    for spec in &specs {
        if let Some(by_spec) = data.spec_teachers.get(*spec) {
            if let Some(v) = by_spec.get(&q) {
                for t in v {
                    if !out.contains(t) {
                        out.push(t.clone());
                    }
                }
            }
        }
    }
    // Алиасы — тоже по всем специальностям.
    if out.is_empty() {
        if let Some(aliases) = data_aliases().get(&q) {
            for spec in &specs {
                if let Some(by_spec) = data.spec_teachers.get(*spec) {
                    for k in aliases {
                        if let Some(v) = by_spec.get(k) {
                            for t in v {
                                if !out.contains(t) {
                                    out.push(t.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

/// Все группы программной таблицы (GROUP_SPEC) — для тестов маппинга:
/// (display, код специальности).
pub fn all_program_groups() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = specs_data()
        .group_spec
        .iter()
        .map(|(k, s)| (k.clone(), s.clone()))
        .collect();
    v.sort();
    v
}

/// Все специальности таблицы программ: (код, полное название).
pub fn all_spec_names() -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = specs_data()
        .spec_names
        .iter()
        .map(|(k, s)| (k.clone(), s.clone()))
        .collect();
    v.sort();
    v
}

/// Уникальные преподаватели по всем данным (общий список + переопределения).
pub fn all_teachers() -> HashSet<String> {
    let mut out: HashSet<String> = HashSet::new();
    for (_, v) in &teachers_data().ordered {
        out.extend(v.iter().cloned());
    }
    for by_spec in specs_data().spec_teachers.values() {
        for v in by_spec.values() {
            out.extend(v.iter().cloned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assets_counts() {
        // Сверено с пересборкой из PDF (scripts/rebuild/, без капа).
        assert_eq!(teachers_data().ordered.len(), 361);
        assert_eq!(teachers_data().aliases.len(), 8);
        assert_eq!(specs_data().group_spec.len(), 168);
        assert_eq!(specs_data().spec_names.len(), 18);
        let pairs: usize = specs_data()
            .spec_teachers
            .values()
            .map(|m| m.len())
            .sum();
        // Полные карты специальностей (без diff-фильтра): 1577 пар.
        assert_eq!(pairs, 1577);
        assert_eq!(specs_data().group_display.len(), 74);
        // Реальных преподавателей — 85 (см. шапку модуля про «90»).
        // Все 85 есть и в общем списке, и суммарно в картах специальностей.
        assert_eq!(all_teachers().len(), 85);
        let teach_unique: HashSet<&String> = teachers_data()
            .ordered
            .iter()
            .flat_map(|(_, v)| v.iter())
            .collect();
        assert_eq!(teach_unique.len(), 85);
        let spec_unique: HashSet<&String> = specs_data()
            .spec_teachers
            .values()
            .flat_map(|m| m.values())
            .flatten()
            .collect();
        assert_eq!(spec_unique.len(), 85);
    }

    /// Таблица программ (assets/programs_table.txt) — источник правды:
    /// ровно 94 display-группы, 18 кодов, 10 неоднозначных голых ключей.
    /// Неоднозначный ключ: голого ключа НЕТ в group_spec (spec_for_group
    /// даёт None, group_display возвращает key как есть) и ровно 2 display
    /// с разными специальностями; group_display не содержит ни одного
    /// из этих 10. Группы набора 2026–2027 (РПО-26-15, ТЭС-26-12б, ...)
    /// в архиве расписания отсутствуют — это нормально, они нужны для
    /// колёс выбора и преподавателей.
    #[test]
    fn programs_table_counts() {
        let groups = all_program_groups();
        let displays: Vec<&(String, String)> =
            groups.iter().filter(|(k, _)| has_prefix(k)).collect();
        assert_eq!(displays.len(), 94, "display-групп в таблице");
        assert_eq!(all_spec_names().len(), 18, "кодов специальностей");
        assert_eq!(specs_data().group_display.len(), 74);

        const AMBIGUOUS: [&str; 10] = [
            "24-21", "24-23", "25-22", "25-24", "25-25", "26-21", "26-22",
            "26-23", "26-24", "26-25",
        ];
        for bare in AMBIGUOUS {
            // Голого ключа нет ни в group_spec, ни в group_display.
            assert_eq!(spec_for_group(bare), None, "{bare}: нет записи");
            assert_eq!(group_display(bare), bare, "{bare}: display = key");
            // Ровно 2 display с разными специальностями.
            let variants: Vec<&(String, String)> = displays
                .iter()
                .filter(|(k, _)| bare_key(k) == bare)
                .copied()
                .collect();
            assert_eq!(variants.len(), 2, "{bare}: два display");
            assert_ne!(variants[0].1, variants[1].1, "{bare}: разные спецы");
        }

        // Набор 2026–2027 есть в карте (хоть и не в архиве расписания).
        assert_eq!(spec_for_group("РПО-26-15").as_deref(), Some("09.02.11"));
        assert_eq!(spec_for_group("ТЭС-26-12б").as_deref(), Some("09.02.12"));
        assert_eq!(group_display("26-12"), "ТЭС-26-12");
        assert_eq!(spec_for_group("ТД-26-21").as_deref(), Some("38.02.08"));
        assert_eq!(spec_for_group("ЮР-26-21").as_deref(), Some("40.02.04"));
        // Полные официальные названия из таблицы (не сокращённые).
        assert_eq!(
            spec_name("38.02.01"),
            Some("Экономика и бухгалтерский учет (по отраслям)")
        );
        assert_eq!(
            spec_name("46.02.02"),
            Some(
                "Обеспечение технологического сопровождения цифровой \
                 трансформации документированных сфер деятельности"
            )
        );
    }

    /// Кап «максимум 3 на предмет» снят: предметы с >3 преподавателями
    /// отдают ВСЕХ (раньше обрезалось до 3).
    #[test]
    fn teachers_for_no_cap() {
        // Математика — 6, Информатика — 5, История — 5, Иностранный — 6.
        assert_eq!(
            teachers_for("Математика", None).teachers,
            vec![
                "Мирошникова В.В.",
                "Оболдина Е.А.",
                "Шапенова К.К.",
                "Рямова М.И.",
                "Скоробогатова М.Н.",
                "Бердюгина Е.В."
            ]
        );
        assert_eq!(
            teachers_for("Информатика", None).teachers,
            vec![
                "Арнович А.Н.",
                "Оболдина Е.А.",
                "Чудмаева Л.С.",
                "Рямова М.И.",
                "Бердюгина Е.В."
            ]
        );
        assert_eq!(
            teachers_for("История", None).teachers,
            vec![
                "Баркова Я.Н.",
                "Ильина Н.В.",
                "Титова Д.А.",
                "Иванова О.А.",
                "Канева Н.В."
            ]
        );
        assert_eq!(
            teachers_for("Иностранный язык", None).teachers,
            vec![
                "Агапова А.Ю.",
                "Таратута И.В.",
                "Антипенская В.Л.",
                "Карнаухова Е.А.",
                "Волобуева С.А.",
                "Сергеева Н.В."
            ]
        );
        // И в переопределении специальности: История у ТД — 4, а не 3.
        assert_eq!(
            teachers_for("История", Some("ТД-26-21")).teachers,
            vec!["Баркова Я.Н.", "Ильина Н.В.", "Канева Н.В.", "Титова Д.А."]
        );
        // Физическая культура — 4.
        assert_eq!(
            teachers_for("Физическая культура", None).teachers,
            vec!["Иванов С.Н.", "Мартьянов К.А.", "Сокол Ю.Я.", "Рачев О.С."]
        );
    }

    /// Голый код МДК — разные предметы в разных программах:
    /// «мдк.02.03 оценка качества» — Муравьева, «мдк.02.03 уголовное
    /// право» — Степанова/Дерман. Учитель ищется ПО ПРОГРАММЕ группы
    /// (префиксный поиск внутри переопределений специальности), а не
    /// первым попавшимся ключом общего списка. Если в программе предмета
    /// нет, а в общем списке он неоднозначен — никого (лучше никого,
    /// чем чужого): ИП-24-27 с «МДК.02.03» получает пусто.
    #[test]
    fn mdk_bare_code_resolves_by_spec() {
        // ЮР (40.02.04): «мдк.02.03 уголовное право» — только Степанова.
        assert_eq!(
            teachers_for("МДК.02.03", Some("ЮР-26-21")).teachers,
            vec!["Степанова В.С."]
        );
        // ПД (40.02.02): то же полное имя — Дерман + Степанова.
        assert_eq!(
            teachers_for("МДК.02.03", Some("ПД-26-06")).teachers,
            vec!["Дерман О.О.", "Степанова В.С."]
        );
        // ИП (09.02.07): в программе МДК.02.03 нет вообще, а общий список
        // неоднозначен (Муравьева vs Степанова/Дерман) — пусто, а не Муравьева.
        assert_eq!(
            teachers_for("МДК.02.03", Some("ИП-24-27")).teachers,
            Vec::<String>::new()
        );
        assert_eq!(
            teachers_for("МДК.02.03", Some("24-27")).teachers,
            Vec::<String>::new()
        );
        // ТХ (19.02.13): «мдк.01.01 организация торгово-сбытовой...» — Муравьева.
        assert_eq!(
            teachers_for("МДК.01.01", Some("ТХ-25-11")).teachers,
            vec!["Муравьева И.Н."]
        );
        // Без группы голый код тоже неоднозначен — пусто.
        assert_eq!(
            teachers_for("МДК.02.03", None).teachers,
            Vec::<String>::new()
        );
    }

    /// 85 уникальных преподавателей в обоих ассетах, ни одной склеенной
    /// строки, максимум списка — 10 и он отдаётся целиком.
    /// Про «90»: 90 сырых строк в teachers_clean.json = 83 чистых + 7
    /// склеенных двойных записей; разбивка даёт ровно 85 разных людей.
    #[test]
    fn no_glued_names_85_unique_max_10() {
        // 85 уникальных — и в teachers.json, и суммарно в spec_teachers.json.
        let teach_unique: HashSet<&String> = teachers_data()
            .ordered
            .iter()
            .flat_map(|(_, v)| v.iter())
            .collect();
        assert_eq!(teach_unique.len(), 85);
        let spec_unique: HashSet<&String> = specs_data()
            .spec_teachers
            .values()
            .flat_map(|m| m.values())
            .flatten()
            .collect();
        assert_eq!(spec_unique.len(), 85);
        assert_eq!(all_teachers().len(), 85);

        // Каждое имя — ровно один шаблон «Фамилия И.О.»: ровно 2 инициала
        // (заглавная кириллица + точка). Склейка вида
        // «Шамухаметов Р.Р. Шапенова К.К.» дала бы 4.
        // (Формат варьируется: «Башлыкова А.Р.» — инициалы слитно,
        // поэтому проверка не по числу слов, а по числу инициалов.)
        fn initials_count(name: &str) -> usize {
            let ch: Vec<char> = name.chars().collect();
            ch.windows(2)
                .filter(|w| {
                    w[1] == '.'
                        && w[0].is_uppercase()
                        && ('\u{0400}'..='\u{04FF}').contains(&w[0])
                })
                .count()
        }
        for name in all_teachers() {
            assert_eq!(initials_count(&name), 2, "склеенное имя: {name}");
        }

        // Люди, существующие только внутри склеек, — в ассетах
        // со своими предметами (проверено родителем по файлам).
        assert!(teachers_for("Физическая культура", None)
            .teachers
            .contains(&"Рачев О.С.".to_string()));
        assert!(teachers_for("Математика", None)
            .teachers
            .contains(&"Скоробогатова М.Н.".to_string()));
        // Обе половины склейки «Шамухаметов + Шапенова» на месте.
        assert!(teachers_for("Основы безопасности и защиты Родины", None)
            .teachers
            .contains(&"Шамухаметов Р.Р.".to_string()));
        assert!(teachers_for("Математика", None)
            .teachers
            .contains(&"Шапенова К.К.".to_string()));

        // Самый длинный список — 10 преподавателей, отдаётся целиком.
        // (Ключ «выс» — мусорный фрагмент «Высшее образование» из PDF,
        // он был и в исходной карте; точные совпадения предметов не трогает.)
        let max = teachers_data()
            .ordered
            .iter()
            .map(|(_, v)| v.len())
            .max()
            .unwrap();
        assert_eq!(max, 10);
        assert_eq!(teachers_for("выс", None).teachers.len(), 10);
    }

    #[test]
    fn spec_for_group_cases() {
        assert_eq!(spec_for_group("ЮР-26-21").as_deref(), Some("40.02.04"));
        assert_eq!(spec_for_group("ТД-26-21").as_deref(), Some("38.02.08"));
        assert_eq!(spec_for_group("ИП-24-27").as_deref(), Some("09.02.07"));
        assert_eq!(spec_for_group("24-27").as_deref(), Some("09.02.07"));
        assert_eq!(spec_for_group("24-27б").as_deref(), Some("09.02.07"));
        assert_eq!(spec_for_group(" 24-27 ").as_deref(), Some("09.02.07"));
        assert_eq!(spec_for_group("26-14").as_deref(), Some("39.02.01"));
        assert_eq!(spec_for_group("СР-26-14").as_deref(), Some("39.02.01"));
        assert_eq!(spec_for_group("23-12").as_deref(), Some("09.02.07"));
        // Неоднозначные голые ключи: специальности нет.
        assert_eq!(spec_for_group("26-21"), None);
        assert_eq!(spec_for_group("26-25"), None);
        assert_eq!(spec_for_group("24-21"), None);
        assert_eq!(spec_for_group(""), None);
        assert_eq!(spec_name("40.02.04"), Some("Юриспруденция"));
        assert_eq!(spec_name("09.02.07"), Some("Информационные системы и программирование"));
        assert_eq!(spec_name("99.99.99"), None);
    }

    #[test]
    fn group_display_cases() {
        // Полные ключи 26-21: display == key.
        assert_eq!(group_display("ЮР-26-21"), "ЮР-26-21");
        assert_eq!(group_display("ТД-26-21"), "ТД-26-21");
        // Голые ключи с однозначным display.
        assert_eq!(group_display("24-27"), "ИП-24-27");
        assert_eq!(group_display("23-12"), "ИВМ-23-12");
        assert_eq!(group_display("26-14"), "СР-26-14");
        assert_eq!(group_display("26-18"), "ОТС-26-18");
        assert_eq!(group_display("24-06"), "ЮР-24-06");
        // Неоднозначные / неизвестные — display = key.
        assert_eq!(group_display("26-21"), "26-21");
        assert_eq!(group_display("26-25"), "26-25");
        assert_eq!(group_display("24-21"), "24-21");
        assert_eq!(group_display("99-99"), "99-99");
        assert!(has_prefix("ЮР-26-21"));
        assert!(has_prefix("ИП-24-27"));
        assert!(!has_prefix("24-27"));
        assert!(!has_prefix("26-21"));
    }

    #[test]
    fn teachers_for_spec_overrides() {
        // История: у ЮР (40.02.04) без Барковой, у ТД (38.02.08) с Барковой.
        let yur = teachers_for("История", Some("ЮР-26-21"));
        assert_eq!(yur.spec.as_deref(), Some("40.02.04"));
        assert_eq!(yur.spec_name.as_deref(), Some("Юриспруденция"));
        assert_eq!(yur.teachers, vec!["Ильина Н.В.", "Канева Н.В.", "Титова Д.А."]);
        let td = teachers_for("История", Some("ТД-26-21"));
        assert_eq!(td.spec.as_deref(), Some("38.02.08"));
        assert_eq!(
            td.teachers,
            vec!["Баркова Я.Н.", "Ильина Н.В.", "Канева Н.В.", "Титова Д.А."]
        );
        // Информатика: ЮР — только Оболдина, ТД — Оболдина+Чудмаева, без группы — пятеро.
        assert_eq!(
            teachers_for("Информатика", Some("ЮР-26-21")).teachers,
            vec!["Оболдина Е.А."]
        );
        assert_eq!(
            teachers_for("Информатика", Some("ТД-26-21")).teachers,
            vec!["Оболдина Е.А.", "Чудмаева Л.С."]
        );
        assert_eq!(
            teachers_for("Информатика", None).teachers,
            vec![
                "Арнович А.Н.",
                "Оболдина Е.А.",
                "Чудмаева Л.С.",
                "Рямова М.И.",
                "Бердюгина Е.В."
            ]
        );
        // Голый ключ группы работает так же, как display.
        assert_eq!(
            teachers_for("Информатика", Some("24-27")).teachers,
            teachers_for("Информатика", Some("ИП-24-27")).teachers
        );
        // Математика: ЮР — Мирошникова+Оболдина (без Шапеновой), без группы — шестеро.
        assert_eq!(
            teachers_for("Математика", Some("ЮР-26-21")).teachers,
            vec!["Мирошникова В.В.", "Оболдина Е.А."]
        );
        assert_eq!(
            teachers_for("Математика", None).teachers,
            vec![
                "Мирошникова В.В.",
                "Оболдина Е.А.",
                "Шапенова К.К.",
                "Рямова М.И.",
                "Скоробогатова М.Н.",
                "Бердюгина Е.В."
            ]
        );
        // Физкультура у ЮР — только Мартьянов (переопределение), без группы — четверо.
        assert_eq!(
            teachers_for("Физическая культура", Some("ЮР-26-21")).teachers,
            vec!["Мартьянов К.А."]
        );
        // Иностранный язык у ЮР — Карнаухова, у 09.02.07 — Агапова.
        assert_eq!(
            teachers_for("Иностранный язык", Some("ЮР-26-21")).teachers,
            vec!["Карнаухова Е.А."]
        );
        assert_eq!(
            teachers_for("Иностранный язык", Some("24-27")).teachers,
            vec!["Агапова А.Ю."]
        );
    }

    #[test]
    fn teachers_for_aliases() {
        // «ОБЖ» -> «основы безопасности и защиты родины».
        assert_eq!(
            teachers_for("ОБЖ", Some("ЮР-26-21")).teachers,
            vec!["Ковылина А.А."]
        );
        assert_eq!(
            teachers_for("ОБЖ", None).teachers,
            vec!["Шамухаметов Р.Р.", "Южанин С.А.", "Ковылина А.А."]
        );
        // «Физкультура» -> «физическая культура» (четверо, без капа).
        assert_eq!(
            teachers_for("Физкультура", None).teachers,
            vec!["Иванов С.Н.", "Мартьянов К.А.", "Сокол Ю.Я.", "Рачев О.С."]
        );
        // «Основы права» -> ТГиП + правовое обеспечение (union, без дублей).
        let pravo = teachers_for("Основы права", Some("ЮР-26-21"));
        assert_eq!(pravo.teachers, vec!["Божко О.А.", "Степанова В.С."]);
        let pravo_global = teachers_for("Основы права", None);
        assert_eq!(
            pravo_global.teachers,
            vec!["Степанова В.С.", "Божко О.А.", "Монтян А.В."]
        );
        // «Индивидуальный проект» — union по четырём каноническим предметам.
        let proj = teachers_for("Индивидуальный проект", None);
        assert!(!proj.teachers.is_empty());
        assert!(proj.teachers.contains(&"Арнович А.Н.".to_string()));
        // Неизвестный предмет — пусто, spec всё равно определяется.
        let unknown = teachers_for("Несуществующий предмет", Some("ЮР-26-21"));
        assert!(unknown.teachers.is_empty());
        assert_eq!(unknown.spec.as_deref(), Some("40.02.04"));
        assert!(teachers_for("", Some("ЮР-26-21")).teachers.is_empty());
        // Регистр и пробелы не важны.
        assert_eq!(
            teachers_for("  иСтОрИя ", Some("ЮР-26-21")).teachers,
            teachers_for("История", Some("ЮР-26-21")).teachers
        );
    }
}
