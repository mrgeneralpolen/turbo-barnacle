//! Планировщик напоминаний о парах — точная логика reminders.py.
//!
//! - Настройка в БД (reminder_settings, минут до пары), ставится в Mini App.
//! - Тик каждые 30 с (первый — через 10 с после старта).
//! - Воскресенье пропускается.
//! - Окно отправки: [remind_at, remind_at + 60 с).
//! - Дедупликация (chat_id, дата, № пары) в памяти, чистится при смене дня.
//! - Мёртвый чат (ошибка содержит подстроку из DEAD_CHAT_ERRORS) — удалить.
//! - Текст: "⏰ Через N минут — <b>Предмет</b>\nN пара • HH:MM–HH:MM • каб. X"
//!   с плюрализацией минуту/минуты/минут.
//! - Кнопка "📱 Открыть расписание" (WebApp) — если есть webapp_url.
//!   Кнопку добавляет реализация ReminderSender (teloxide::Bot в bot.rs);
//!   трейт передаёт ей webapp_url как есть.

use crate::api::{day_payload, ApiPair};
use crate::db::Db;
use crate::service::{ScheduleService, DEAD_CHAT_ERRORS};
use crate::tyumen;
use crate::Result;
use chrono::{DateTime, Datelike, Duration, FixedOffset, NaiveTime};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Отправка сообщения — трейт, чтобы тик тестировался офлайн на фейке.
/// Реализация для teloxide::Bot — в bot.rs (или здесь, если удобно).
#[allow(async_fn_in_trait)]
pub trait ReminderSender: Send + Sync {
    async fn send_message(&self, chat_id: i64, text: String, webapp_url: Option<String>) -> Result<()>;
}

/// Плюрализация минут. Точная копия _plural_minutes.
/// (rem_euclid — как % в Python: для отрицательных тоже неотрицательный остаток.)
pub fn plural_minutes(n: i64) -> &'static str {
    let m10 = n.rem_euclid(10);
    let m100 = n.rem_euclid(100);
    if m10 == 1 && m100 != 11 {
        "минуту"
    } else if (2..=4).contains(&m10) && ![12, 13, 14].contains(&m100) {
        "минуты"
    } else {
        "минут"
    }
}

fn html_escape(s: &str) -> String {
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

/// Текст напоминания. Точная копия reminder_text.
pub fn reminder_text(pair: &ApiPair, minutes: i64) -> String {
    let when = match &pair.time {
        Some(t) if !t.start.is_empty() && !t.end.is_empty() => format!("{}–{}", t.start, t.end),
        _ => String::new(),
    };
    let mut lines = vec![format!(
        "⏰ Через {} {} — <b>{}</b>",
        minutes,
        plural_minutes(minutes),
        html_escape(&pair.subject)
    )];
    let meta = [format!("{} пара", pair.n), when, pair.room.clone()]
        .into_iter()
        .filter(|x| !x.is_empty())
        .collect::<Vec<_>>()
        .join(" • ");
    if !meta.is_empty() {
        lines.push(meta);
    }
    lines.join("\n")
}

pub struct ReminderRunner {
    sent: Mutex<HashSet<(i64, String, i32)>>,
}

impl ReminderRunner {
    pub fn new() -> Self {
        Self {
            sent: Mutex::new(HashSet::new()),
        }
    }

    /// Один проход: кому пора напомнить. Точная копия reminder_tick.
    /// `now` — время Тюмени (для тестов можно подставить любое).
    pub async fn tick(
        &self,
        sender: &impl ReminderSender,
        service: &ScheduleService,
        db: &Db,
        webapp_url: Option<&str>,
        now: DateTime<FixedOffset>,
    ) -> Result<()> {
        let today = now.date_naive();
        if today.weekday() == chrono::Weekday::Sun {
            // Воскресенье — пар нет.
            return Ok(());
        }
        let date_str = tyumen::fmt_date(today);
        // Чистим отметки за прошлые дни.
        {
            let mut sent = self.sent.lock().await;
            sent.retain(|(_, d, _)| d == &date_str);
        }
        let reminders = db.get_all_reminders().await?;
        if reminders.is_empty() {
            return Ok(());
        }
        // Кэш payload дня по группе внутри тика.
        let mut payloads: HashMap<String, crate::api::DayPayload> = HashMap::new();
        for (chat_id, minutes) in reminders {
            let group = db.get_group(chat_id).await?;
            let Some(group) = group.filter(|g| !g.is_empty() && g != "26-21") else {
                continue;
            };
            let payload = match payloads.entry(group.clone()) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(day_payload(service, &group, today))
                }
            };
            for pair in &payload.pairs {
                let Some(t) = &pair.time else {
                    continue;
                };
                if t.start.is_empty() {
                    continue;
                }
                let Some(start_mins) = tyumen::hhmm_to_mins(&t.start) else {
                    continue;
                };
                let start = tyumen::tyumen_datetime(
                    today,
                    NaiveTime::from_hms_opt(
                        (start_mins / 60) as u32,
                        (start_mins % 60) as u32,
                        0,
                    )
                    .unwrap_or_else(|| NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
                );
                let remind_at = start - Duration::minutes(minutes);
                let key = (chat_id, date_str.clone(), pair.n);
                if self.sent.lock().await.contains(&key) {
                    continue;
                }
                // Окно отправки: от remind_at до remind_at + 60 сек.
                if remind_at <= now && now < remind_at + Duration::seconds(60) {
                    match sender
                        .send_message(
                            chat_id,
                            reminder_text(pair, minutes),
                            webapp_url.map(|s| s.to_string()),
                        )
                        .await
                    {
                        Ok(()) => {
                            self.sent.lock().await.insert(key);
                        }
                        Err(e) => {
                            // Мёртвый чат — удаляем, чтобы не слать в пустоту.
                            let err_lower = e.to_string().to_lowercase();
                            if DEAD_CHAT_ERRORS.iter().any(|x| err_lower.contains(x)) {
                                db.delete_chat(chat_id).await?;
                            } else {
                                tracing::warn!(
                                    "Напоминания: не удалось написать {}: {}",
                                    chat_id,
                                    e
                                );
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Фоновая задача: sleep 10, затем тик каждые 30 с. Точная копия reminder_loop.
    pub async fn run_loop(
        &self,
        sender: Arc<impl ReminderSender + 'static>,
        service: Arc<ScheduleService>,
        db: Arc<Db>,
        webapp_url: Option<String>,
    ) {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        tracing::info!("⏰ Цикл напоминаний о парах запущен");
        loop {
            if let Err(e) = self
                .tick(
                    sender.as_ref(),
                    &service,
                    &db,
                    webapp_url.as_deref(),
                    tyumen::tyumen_now(),
                )
                .await
            {
                tracing::error!("Ошибка в цикле напоминаний: {}", e);
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    }
}

impl Default for ReminderRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ApiPair, ApiTime};
    use crate::db::Db;

    fn pair(n: i32, subject: &str, room: &str, start: &str, end: &str) -> ApiPair {
        ApiPair {
            n,
            subject: subject.to_string(),
            room: room.to_string(),
            time: Some(ApiTime {
                start: start.to_string(),
                end: end.to_string(),
            }),
            changed: false,
        }
    }

    #[test]
    fn plural_minutes_cases() {
        assert_eq!(plural_minutes(1), "минуту");
        assert_eq!(plural_minutes(2), "минуты");
        assert_eq!(plural_minutes(3), "минуты");
        assert_eq!(plural_minutes(4), "минуты");
        assert_eq!(plural_minutes(5), "минут");
        assert_eq!(plural_minutes(11), "минут");
        assert_eq!(plural_minutes(12), "минут");
        assert_eq!(plural_minutes(14), "минут");
        assert_eq!(plural_minutes(15), "минут");
        assert_eq!(plural_minutes(21), "минуту");
        assert_eq!(plural_minutes(22), "минуты");
        assert_eq!(plural_minutes(25), "минут");
        assert_eq!(plural_minutes(31), "минуту");
        assert_eq!(plural_minutes(32), "минуты");
        assert_eq!(plural_minutes(101), "минуту");
        assert_eq!(plural_minutes(111), "минут");
        assert_eq!(plural_minutes(112), "минут");
    }

    #[test]
    fn reminder_text_format() {
        assert_eq!(
            reminder_text(&pair(1, "Математика", "каб. 216", "08:00", "09:30"), 15),
            "⏰ Через 15 минут — <b>Математика</b>\n1 пара • 08:00–09:30 • каб. 216"
        );
        assert_eq!(
            reminder_text(&pair(1, "Математика", "каб. 216", "08:00", "09:30"), 1),
            "⏰ Через 1 минуту — <b>Математика</b>\n1 пара • 08:00–09:30 • каб. 216"
        );
        assert_eq!(
            reminder_text(&pair(2, "Обществознание", "каб. 403", "10:10", "11:40"), 2),
            "⏰ Через 2 минуты — <b>Обществознание</b>\n2 пара • 10:10–11:40 • каб. 403"
        );
    }

    #[test]
    fn reminder_text_escapes_subject() {
        let p = pair(1, "A&B <C>", "", "08:00", "09:30");
        assert_eq!(
            reminder_text(&p, 5),
            "⏰ Через 5 минут — <b>A&amp;B &lt;C&gt;</b>\n1 пара • 08:00–09:30"
        );
    }

    #[test]
    fn reminder_text_no_time_no_room() {
        let p = ApiPair {
            n: 1,
            subject: "Математика".to_string(),
            room: String::new(),
            time: None,
            changed: false,
        };
        assert_eq!(
            reminder_text(&p, 5),
            "⏰ Через 5 минут — <b>Математика</b>\n1 пара"
        );
    }

    #[test]
    fn reminder_text_second_shift_pairs() {
        // Пары 2-й смены из day_payload: room пустой (кабинет остался
        // в предмете — fix_room не применяется), время из секции «2 смена».
        assert_eq!(
            reminder_text(&pair(5, "Основы философии", "", "16:10", "17:40"), 15),
            "⏰ Через 15 минут — <b>Основы философии</b>\n5 пара • 16:10–17:40"
        );
        assert_eq!(
            reminder_text(&pair(6, "МДК.04.02", "", "17:50", "19:20"), 21),
            "⏰ Через 21 минуту — <b>МДК.04.02</b>\n6 пара • 17:50–19:20"
        );
        assert_eq!(
            reminder_text(&pair(4, "Жилищное право", "", "14:10", "15:40"), 22),
            "⏰ Через 22 минуты — <b>Жилищное право</b>\n4 пара • 14:10–15:40"
        );
        // 2-я пара 2-го курса среды — 09:40–10:10 (разделённая пара с обедом).
        assert_eq!(
            reminder_text(
                &pair(
                    2,
                    "Основы алгоритмизации и программирования",
                    "",
                    "09:40",
                    "10:10"
                ),
                25
            ),
            "⏰ Через 25 минут — <b>Основы алгоритмизации и программирования</b>\n2 пара • 09:40–10:10"
        );
        // 31 минута — «минуту», как 1 и 21.
        assert_eq!(
            reminder_text(&pair(1, "Физика", "", "08:00", "09:30"), 31),
            "⏰ Через 31 минуту — <b>Физика</b>\n1 пара • 08:00–09:30"
        );
    }

    /// Фейковый отправитель: пишет в память; для чатов из `fail` — ошибка.
    struct FakeSender {
        sent: Mutex<Vec<(i64, String)>>,
        fail: HashSet<i64>,
        fail_msg: String,
    }

    impl FakeSender {
        fn new() -> Self {
            Self {
                sent: Mutex::new(Vec::new()),
                fail: HashSet::new(),
                fail_msg: "chat not found".to_string(),
            }
        }

        async fn messages(&self) -> Vec<(i64, String)> {
            self.sent.lock().await.clone()
        }
    }

    impl ReminderSender for FakeSender {
        async fn send_message(
            &self,
            chat_id: i64,
            text: String,
            _webapp_url: Option<String>,
        ) -> Result<()> {
            if self.fail.contains(&chat_id) {
                return Err(crate::AppError::other(self.fail_msg.clone()));
            }
            self.sent.lock().await.push((chat_id, text));
            Ok(())
        }
    }

    /// Тестовый сервис на временной копии фикстурной БД + настройки
    /// напоминаний как в test_reminders.py: 111 — 15 мин, 222 — 30 мин,
    /// 333 — 10 мин (группа "26-21", пропускается).
    /// Плюс группы из разных специальностей (18.11.2026, среда):
    /// 444 — 23-12 (09.02.07, 3,4 курс, 2-я смена: 3п 11:40, 4п 14:10,
    /// 5п 16:10), 15 мин;
    /// 555 — 24-06 (40.02.04, 3,4 курс, 2-я смена: 4п 14:10, 5п 16:10,
    /// 6п 17:50), 30 мин;
    /// 666 — 26-18 (46.02.02, 1 курс: 1п 08:00, 2п 10:10, 3п 11:50), 10 мин;
    /// 777 — 25-14 (10.02.05, 2 курс: 1п 08:00, 2п 09:40, 3п 11:50), 10 мин;
    /// 888 — 23-05 (на 18.11 пар нет вообще), 15 мин.
    async fn test_setup() -> (tempfile::TempDir, Arc<Db>, Arc<ScheduleService>) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("settings.db");
        std::fs::copy(
            format!("{}/tests/fixtures/settings.db", env!("CARGO_MANIFEST_DIR")),
            &db_path,
        )
        .unwrap();
        let db = Arc::new(Db::open(db_path.to_str().unwrap()).unwrap());
        db.init().await.unwrap();
        db.set_group(111, "ЮР-26-21").await.unwrap();
        db.set_group(222, "ЮР-26-21").await.unwrap();
        db.set_group(333, "26-21").await.unwrap();
        db.set_group(444, "23-12").await.unwrap();
        db.set_group(555, "24-06").await.unwrap();
        db.set_group(666, "26-18").await.unwrap();
        db.set_group(777, "25-14").await.unwrap();
        db.set_group(888, "23-05").await.unwrap();
        db.set_reminder_minutes(111, 15).await.unwrap();
        db.set_reminder_minutes(222, 30).await.unwrap();
        db.set_reminder_minutes(333, 10).await.unwrap();
        db.set_reminder_minutes(444, 15).await.unwrap();
        db.set_reminder_minutes(555, 30).await.unwrap();
        db.set_reminder_minutes(666, 10).await.unwrap();
        db.set_reminder_minutes(777, 10).await.unwrap();
        db.set_reminder_minutes(888, 15).await.unwrap();
        let service = Arc::new(ScheduleService::new(db.clone(), String::new(), String::new()));
        service.load_ram().await.unwrap();
        (dir, db, service)
    }

    /// Среда 18.11.2026 в Тюмени. Пары ЮР-26-21 по фикстуре:
    /// 1п 08:00–09:30, 2п 10:10–11:40, 3п 11:50–13:20.
    fn dt(hour: u32, minute: u32) -> DateTime<FixedOffset> {
        tyumen::tyumen_datetime(
            tyumen::parse_date("18.11.2026").unwrap(),
            NaiveTime::from_hms_opt(hour, minute, 0).unwrap(),
        )
    }

    #[tokio::test]
    async fn tick_sends_in_window_no_duplicates() {
        let (_dir, db, service) = test_setup().await;
        let sender = FakeSender::new();
        let runner = ReminderRunner::new();

        // 7:45 — за 15 мин до 1-й пары: только 111 (15 мин).
        runner.tick(&sender, &service, &db, None, dt(7, 45)).await.unwrap();
        let sent = sender.messages().await;
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].0, 111);
        assert!(sent[0].1.contains("15 минут"), "{}", sent[0].1);
        assert!(sent[0].1.contains("безопасности"), "{}", sent[0].1);

        // Повторный проход в то же окно — дублей нет.
        runner.tick(&sender, &service, &db, None, dt(7, 45)).await.unwrap();
        runner.tick(&sender, &service, &db, None, dt(7, 46)).await.unwrap();
        assert_eq!(sender.messages().await.len(), 1);

        // 7:30 — за 30 мин до 1-й пары: 222 (30 мин).
        runner.tick(&sender, &service, &db, None, dt(7, 30)).await.unwrap();
        let sent = sender.messages().await;
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[1].0, 222);
        assert!(sent[1].1.contains("30 минут"), "{}", sent[1].1);

        // 9:40 — за 30 мин до 2-й пары 10:10: 222 про 2-ю пару.
        runner.tick(&sender, &service, &db, None, dt(9, 40)).await.unwrap();
        let sent = sender.messages().await;
        assert_eq!(sent.len(), 3, "{sent:?}");
        assert_eq!(sent[2].0, 222);
        assert!(sent[2].1.contains("География"), "{}", sent[2].1);

        // 9:55 — за 15 мин до 2-й пары: 111 про 2-ю пару.
        runner.tick(&sender, &service, &db, None, dt(9, 55)).await.unwrap();
        let sent = sender.messages().await;
        assert_eq!(sent.len(), 4, "{sent:?}");
        assert_eq!(sent[3].0, 111);
        assert!(sent[3].1.contains("География"), "{}", sent[3].1);
    }

    #[tokio::test]
    async fn tick_outside_window_sends_nothing() {
        let (_dir, db, service) = test_setup().await;
        let sender = FakeSender::new();
        let runner = ReminderRunner::new();
        // 7:44 — рано для 15 мин; 7:47 — уже не в окне [7:45, 7:46).
        runner.tick(&sender, &service, &db, None, dt(7, 44)).await.unwrap();
        assert_eq!(sender.messages().await.len(), 0);
        runner.tick(&sender, &service, &db, None, dt(7, 47)).await.unwrap();
        assert_eq!(sender.messages().await.len(), 0);
    }

    #[tokio::test]
    async fn tick_skips_ambiguous_group() {
        let (_dir, db, service) = test_setup().await;
        let sender = FakeSender::new();
        let runner = ReminderRunner::new();
        // 7:50 — в окне для 333 (10 мин до 08:00), но группа "26-21".
        runner.tick(&sender, &service, &db, None, dt(7, 50)).await.unwrap();
        let sent = sender.messages().await;
        assert!(sent.iter().all(|(c, _)| *c != 333), "{sent:?}");
    }

    #[tokio::test]
    async fn tick_skips_sunday() {
        let (_dir, db, service) = test_setup().await;
        let sender = FakeSender::new();
        let runner = ReminderRunner::new();
        // Воскресенье 22.11.2026, 7:45.
        let sunday = tyumen::tyumen_datetime(
            tyumen::parse_date("22.11.2026").unwrap(),
            NaiveTime::from_hms_opt(7, 45, 0).unwrap(),
        );
        assert_eq!(sunday.weekday(), chrono::Weekday::Sun);
        runner.tick(&sender, &service, &db, None, sunday).await.unwrap();
        assert_eq!(sender.messages().await.len(), 0);
    }

    #[tokio::test]
    async fn tick_dead_chat_is_deleted() {
        let (_dir, db, service) = test_setup().await;
        let mut sender = FakeSender::new();
        sender.fail.insert(111);
        let runner = ReminderRunner::new();
        runner.tick(&sender, &service, &db, None, dt(7, 45)).await.unwrap();
        assert_eq!(sender.messages().await.len(), 0);
        // Мёртвый чат удалён из БД (как в Python delete_chat: chat_settings
        // и пины; строка reminder_settings остаётся, но без группы тик
        // чат пропускает).
        assert_eq!(db.get_group(111).await.unwrap(), None);
        // Повторный тик в то же окно: напоминание для 111 больше не шлётся.
        runner.tick(&sender, &service, &db, None, dt(7, 45)).await.unwrap();
        assert_eq!(sender.messages().await.len(), 0);
    }

    /// Напоминания по нескольким группам из разных специальностей,
    /// включая 2-ю смену, плюс дедупликация и границы окна.
    /// 18.11.2026 (среда): 23-12 — 3п 11:40, 4п 14:10, 5п 16:10;
    /// 24-06 — 4п 14:10, 5п 16:10, 6п 17:50; 26-18 — 1п 08:00, 2п 10:10;
    /// 25-14 — 1п 08:00, 2п 09:40; 23-05 — пар нет.
    #[tokio::test]
    async fn tick_multi_group_second_shift() {
        let (_dir, db, service) = test_setup().await;
        let sender = FakeSender::new();
        let runner = ReminderRunner::new();

        // 13:39 — рано для 555 (окно [13:40, 13:41)).
        runner.tick(&sender, &service, &db, None, dt(13, 39)).await.unwrap();
        assert_eq!(sender.messages().await.len(), 0);

        // 13:40 — за 30 мин до 4-й пары 24-06 (14:10): только 555.
        runner.tick(&sender, &service, &db, None, dt(13, 40)).await.unwrap();
        let sent = sender.messages().await;
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0].0, 555);
        assert!(sent[0].1.contains("Жилищное право"), "{}", sent[0].1);
        assert!(sent[0].1.contains("30 минут"), "{}", sent[0].1);
        assert!(sent[0].1.contains("4 пара • 14:10–15:40"), "{}", sent[0].1);

        // 13:41 — окно прошло, дублей нет.
        runner.tick(&sender, &service, &db, None, dt(13, 41)).await.unwrap();
        assert_eq!(sender.messages().await.len(), 1);

        // 13:55 — за 15 мин до 4-й пары 23-12 (14:10): только 444.
        runner.tick(&sender, &service, &db, None, dt(13, 55)).await.unwrap();
        let sent = sender.messages().await;
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[1].0, 444);
        assert!(sent[1].1.contains("Психология общения"), "{}", sent[1].1);
        assert!(sent[1].1.contains("15 минут"), "{}", sent[1].1);
        assert!(sent[1].1.contains("4 пара • 14:10–15:40"), "{}", sent[1].1);

        // 15:55 — за 15 мин до 5-й пары 23-12 (16:10): 444 про 5-ю пару.
        runner.tick(&sender, &service, &db, None, dt(15, 55)).await.unwrap();
        let sent = sender.messages().await;
        assert_eq!(sent.len(), 3, "{sent:?}");
        assert_eq!(sent[2].0, 444);
        assert!(sent[2].1.contains("Основы философии"), "{}", sent[2].1);
        assert!(sent[2].1.contains("5 пара • 16:10–17:40"), "{}", sent[2].1);

        // 07:50 — за 10 мин до 1-й пары: 666 (26-18) и 777 (25-14).
        runner.tick(&sender, &service, &db, None, dt(7, 50)).await.unwrap();
        let sent = sender.messages().await;
        assert_eq!(sent.len(), 5, "{sent:?}");
        let new: std::collections::HashMap<i64, String> =
            sent[3..].iter().cloned().collect();
        assert_eq!(new.len(), 2);
        assert!(new[&666].contains("безопасности"), "{}", new[&666]);
        assert!(new[&666].contains("10 минут"), "{}", new[&666]);
        assert!(new[&666].contains("1 пара • 08:00–09:30"), "{}", new[&666]);
        assert!(new[&777].contains("Основы алгоритмизации"), "{}", new[&777]);
        assert!(new[&777].contains("10 минут"), "{}", new[&777]);

        // 09:30 — за 10 мин до 2-й пары 25-14 (09:40): только 777, про 2-ю пару.
        runner.tick(&sender, &service, &db, None, dt(9, 30)).await.unwrap();
        let sent = sender.messages().await;
        assert_eq!(sent.len(), 6, "{sent:?}");
        assert_eq!(sent[5].0, 777);
        assert!(sent[5].1.contains("Основы алгоритмизации"), "{}", sent[5].1);
        assert!(sent[5].1.contains("2 пара • 09:40–10:10"), "{}", sent[5].1);

        // Повторные тики в те же окна дублей не дают; 888 (23-05, пар нет)
        // не получил ни одного напоминания за весь день.
        runner.tick(&sender, &service, &db, None, dt(7, 50)).await.unwrap();
        runner.tick(&sender, &service, &db, None, dt(9, 30)).await.unwrap();
        runner.tick(&sender, &service, &db, None, dt(13, 40)).await.unwrap();
        let sent = sender.messages().await;
        assert_eq!(sent.len(), 6, "{sent:?}");
        assert!(sent.iter().all(|(c, _)| *c != 888), "{sent:?}");
        assert!(sent.iter().all(|(c, _)| *c != 333), "{sent:?}");
    }

    #[tokio::test]
    async fn tick_group_without_pairs_sends_nothing() {
        let (_dir, db, service) = test_setup().await;
        let sender = FakeSender::new();
        let runner = ReminderRunner::new();
        // 888 — группа 23-05: на 18.11 у неё нет ни одной пары.
        // Тики в моменты, когда у других групп напоминания точно есть.
        for (h, m) in [(7, 45), (7, 50), (11, 25), (13, 40), (13, 55), (15, 55)] {
            runner.tick(&sender, &service, &db, None, dt(h, m)).await.unwrap();
        }
        let sent = sender.messages().await;
        assert!(!sent.is_empty(), "контроль: другим группам напоминания шли");
        assert!(sent.iter().all(|(c, _)| *c != 888), "{sent:?}");
    }
}
