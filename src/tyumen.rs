//! Время Тюмени (UTC+5, фиксированный сдвиг — как `timezone(timedelta(hours=5))`
//! в Python), даты в формате ДД.ММ.ГГГГ, русские дни недели.

use chrono::{DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};

/// Сдвиг Тюмени: UTC+5.
pub fn tyumen_offset() -> FixedOffset {
    FixedOffset::east_opt(5 * 3600).unwrap()
}

pub fn tyumen_now() -> DateTime<FixedOffset> {
    chrono::Utc::now().with_timezone(&tyumen_offset())
}

pub fn tyumen_today() -> NaiveDate {
    tyumen_now().date_naive()
}

/// "ДД.ММ.ГГГГ" -> NaiveDate. Как datetime.strptime(s, "%d.%m.%Y") в Python.
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    NaiveDate::parse_from_str(s, "%d.%m.%Y").ok()
}

/// NaiveDate -> "ДД.ММ.ГГГГ".
pub fn fmt_date(d: NaiveDate) -> String {
    d.format("%d.%m.%Y").to_string()
}

/// "ДД.ММ.ГГГГ HH:MM" — как в /api/me ("now").
pub fn fmt_datetime(dt: DateTime<FixedOffset>) -> String {
    dt.format("%d.%m.%Y %H:%M").to_string()
}

/// Понедельник недели, содержащей дату (Пн–Сб, как в /api/week).
pub fn week_monday(anchor: NaiveDate) -> NaiveDate {
    anchor - chrono::Duration::days(anchor.weekday().num_days_from_monday() as i64)
}

pub const WEEKDAYS: [&str; 7] = [
    "Понедельник",
    "Вторник",
    "Среда",
    "Четверг",
    "Пятница",
    "Суббота",
    "Воскресенье",
];

pub const WEEKDAYS_SHORT: [&str; 7] = ["Пн", "Вт", "Ср", "Чт", "Пт", "Сб", "Вс"];

pub fn weekday_name(d: NaiveDate) -> &'static str {
    WEEKDAYS[d.weekday().num_days_from_monday() as usize]
}

pub fn weekday_short(d: NaiveDate) -> &'static str {
    WEEKDAYS_SHORT[d.weekday().num_days_from_monday() as usize]
}

/// "HH:MM" -> минуты с начала суток.
pub fn hhmm_to_mins(s: &str) -> Option<i32> {
    let (h, m) = s.split_once(':')?;
    let h: i32 = h.trim().parse().ok()?;
    let m: i32 = m.trim().parse().ok()?;
    if (0..24).contains(&h) && (0..60).contains(&m) {
        Some(h * 60 + m)
    } else {
        None
    }
}

/// Дата+время в Тюмени из NaiveDate и "HH:MM".
pub fn tyumen_datetime(d: NaiveDate, t: NaiveTime) -> DateTime<FixedOffset> {
    tyumen_offset()
        .from_local_datetime(&NaiveDateTime::new(d, t))
        .single()
        .unwrap_or_else(|| tyumen_offset().from_local_datetime(&NaiveDateTime::new(d, t)).earliest().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_roundtrip() {
        let d = parse_date("18.11.2026").unwrap();
        assert_eq!(d.day(), 18);
        assert_eq!(fmt_date(d), "18.11.2026");
        assert_eq!(weekday_name(d), "Среда");
        assert_eq!(weekday_short(d), "Ср");
    }

    #[test]
    fn week_monday_works() {
        // 20.11.2026 — пятница; понедельник недели — 16.11.2026
        let fri = parse_date("20.11.2026").unwrap();
        assert_eq!(fmt_date(week_monday(fri)), "16.11.2026");
    }
}
