//! Слой БД: rusqlite, схема ТОЧНО как в Python database.py.
//!
//! Таблицы (init_db):
//! - chat_settings (chat_id INTEGER PRIMARY KEY, group_name TEXT)
//! - schedule_archive (id INTEGER PRIMARY KEY AUTOINCREMENT, date_str TEXT,
//!   group_name TEXT, content TEXT, type TEXT, created_at TIMESTAMP)
//! - pinned_messages (chat_id INTEGER, message_id INTEGER, sent_at TIMESTAMP)
//! - schedule_files (id INTEGER PRIMARY KEY AUTOINCREMENT,
//!   file_unique_id TEXT UNIQUE, date_str TEXT, file_type TEXT, title TEXT,
//!   file_id TEXT, vk_url TEXT, created_at TIMESTAMP)
//! - reminder_settings (chat_id INTEGER PRIMARY KEY, minutes INTEGER NOT NULL DEFAULT 0)
//!   (из пропатченной database.py в schedule-miniapp)
//! - user_notes (user_id INTEGER, subject TEXT, text TEXT,
//!   updated_at TIMESTAMP, PRIMARY KEY(user_id, subject))
//! - user_absences (user_id INTEGER, date_str TEXT, subject TEXT,
//!   pair INTEGER, PRIMARY KEY(user_id, date_str, subject))
//!   (обе — только для Mini App Rust-версии; существующих таблиц и
//!   значений не меняют, создаются отдельными CREATE TABLE IF NOT EXISTS)
//! Индексы: idx_search (date_str, group_name), idx_pinned_chat, idx_pinned_sent,
//! idx_files_date, idx_files_created.
//! init_db также удаляет строки "🔔 Расписание по времени (фото)%".
//!
//! Python использует aiosqlite + WAL + отложенный wal_checkpoint(TRUNCATE)
//! через 1.5 с после записи. Здесь: rusqlite через spawn_blocking
//! (Arc<std::sync::Mutex<Connection>>, PRAGMA journal_mode=WAL и те же
//! PRAGMA из get_db), checkpoint синхронно после каждой записи.
//!
//! ВАЖНО: все SQL-строки — те же, что в database.py (включая
//! datetime('now', '-7 days') и т.п., где Python считает в UTC —
//! SQLite datetime('now') тоже UTC, расхождений нет).
//!
//! Два метода без прямого SQL-аналога в database.py (в Python эти данные
//! читаются из in-memory archive_cache, построенного из
//! get_full_archive_dump):
//! - get_archive_records: SELECT content, type ... ORDER BY created_at ASC
//!   (тот же порядок, что в get_full_archive_dump);
//! - get_archive_groups: SELECT DISTINCT group_name ... WHERE group_name !=
//!   '__ZVONKI__' (та же фильтрация, что в miniapp_api.api_groups).

use crate::{AppError, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct ScheduleFile {
    pub file_unique_id: String,
    pub date_str: String,
    pub file_type: String,
    pub title: String,
    pub file_id: Option<String>,
    pub vk_url: String,
}

#[derive(Clone)]
pub struct Db {
    conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
}

/// Сброс WAL в основной файл — аналог отложенного _delayed_wal_flush из
/// Python, но синхронно после коммита (проще и надёжно).
fn checkpoint(conn: &Connection) -> Result<()> {
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    Ok(())
}

fn row_to_file(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduleFile> {
    Ok(ScheduleFile {
        file_unique_id: row.get(0)?,
        date_str: row.get(1)?,
        file_type: row.get(2)?,
        title: row.get(3)?,
        file_id: row.get(4)?,
        vk_url: row.get(5)?,
    })
}

impl Db {
    /// Открывает (или создаёт) файл БД. Не создаёт таблицы — это делает init().
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        // Те же PRAGMA, что в Python get_db().
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "cache_size", -16000i64)?;
        conn.pragma_update(None, "busy_timeout", 10000i64)?;
        Ok(Self {
            conn: Arc::new(std::sync::Mutex::new(conn)),
        })
    }

    /// Выполняет замыкание на соединении в spawn_blocking (rusqlite — синхронный).
    async fn run<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn
                .lock()
                .map_err(|_| AppError::other("db: mutex poisoned"))?;
            f(&mut guard)
        })
        .await
        .map_err(|e| AppError::other(format!("db: spawn_blocking: {e}")))?
    }

    /// CREATE TABLE IF NOT EXISTS + индексы + чистка фото-строк. Как init_db().
    pub async fn init(&self) -> Result<()> {
        self.run(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS chat_settings (chat_id INTEGER PRIMARY KEY, group_name TEXT);\
                 CREATE TABLE IF NOT EXISTS reminder_settings (chat_id INTEGER PRIMARY KEY, minutes INTEGER NOT NULL DEFAULT 0);\
                 CREATE TABLE IF NOT EXISTS schedule_archive \
                   (id INTEGER PRIMARY KEY AUTOINCREMENT, date_str TEXT, group_name TEXT, \
                    content TEXT, type TEXT, created_at TIMESTAMP);\
                 CREATE TABLE IF NOT EXISTS pinned_messages (chat_id INTEGER, message_id INTEGER, sent_at TIMESTAMP);\
                 CREATE TABLE IF NOT EXISTS schedule_files \
                   (id INTEGER PRIMARY KEY AUTOINCREMENT, file_unique_id TEXT UNIQUE, \
                    date_str TEXT, file_type TEXT, title TEXT, file_id TEXT, \
                    vk_url TEXT, created_at TIMESTAMP);\
                 CREATE TABLE IF NOT EXISTS user_notes \
                   (user_id INTEGER, subject TEXT, text TEXT, updated_at TIMESTAMP, \
                    PRIMARY KEY(user_id, subject));\
                 CREATE TABLE IF NOT EXISTS user_absences \
                   (user_id INTEGER, date_str TEXT, subject TEXT, pair INTEGER, \
                    PRIMARY KEY(user_id, date_str, subject));\
                 CREATE INDEX IF NOT EXISTS idx_search ON schedule_archive (date_str, group_name);\
                 CREATE INDEX IF NOT EXISTS idx_pinned_chat ON pinned_messages (chat_id);\
                 CREATE INDEX IF NOT EXISTS idx_pinned_sent ON pinned_messages (sent_at);\
                 CREATE INDEX IF NOT EXISTS idx_files_date ON schedule_files (date_str);\
                 CREATE INDEX IF NOT EXISTS idx_files_created ON schedule_files (created_at);\
                 DELETE FROM schedule_archive WHERE content LIKE '🔔 Расписание по времени (фото)%';",
            )?;
            checkpoint(conn)
        })
        .await
    }

    pub async fn get_group(&self, chat_id: i64) -> Result<Option<String>> {
        self.run(move |conn| {
            let row = conn
                .query_row(
                    "SELECT group_name FROM chat_settings WHERE chat_id = ?",
                    params![chat_id],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            Ok(row)
        })
        .await
    }

    pub async fn set_group(&self, chat_id: i64, group: &str) -> Result<()> {
        let group = group.to_string();
        self.run(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO chat_settings VALUES (?, ?)",
                params![chat_id, group],
            )?;
            checkpoint(conn)
        })
        .await
    }

    pub async fn delete_chat(&self, chat_id: i64) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "DELETE FROM chat_settings WHERE chat_id = ?",
                params![chat_id],
            )?;
            conn.execute(
                "DELETE FROM pinned_messages WHERE chat_id = ?",
                params![chat_id],
            )?;
            checkpoint(conn)
        })
        .await
    }

    pub async fn get_all_chats(&self) -> Result<Vec<(i64, String)>> {
        self.run(|conn| {
            let mut stmt =
                conn.prepare("SELECT chat_id, group_name FROM chat_settings")?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Пакетная вставка (date_str, group_name, content, type):
    /// сначала DELETE "основное" для тех же (дата, группа), потом INSERT.
    /// Как save_to_archive_batch().
    pub async fn save_to_archive_batch(
        &self,
        records: &[(String, String, String, String)],
    ) -> Result<()> {
        let records = records.to_vec();
        if records.is_empty() {
            return Ok(());
        }
        self.run(move |conn| {
            let tx = conn.transaction()?;
            // Как set comprehension в Python: уникальные (дата, группа)
            // для записей типа "основное".
            let mut seen: HashSet<(String, String)> = HashSet::new();
            let mut main_deletes: Vec<(String, String)> = Vec::new();
            for r in &records {
                if r.3 == "основное" && seen.insert((r.0.clone(), r.1.clone())) {
                    main_deletes.push((r.0.clone(), r.1.clone()));
                }
            }
            for (date_str, group_name) in &main_deletes {
                tx.execute(
                    "DELETE FROM schedule_archive WHERE date_str = ? AND group_name = ? AND type = ?",
                    params![date_str, group_name, "основное"],
                )?;
            }
            for r in &records {
                tx.execute(
                    "INSERT INTO schedule_archive (date_str, group_name, content, type, created_at) VALUES (?, ?, ?, ?, datetime(\"now\"))",
                    params![r.0, r.1, r.2, r.3],
                )?;
            }
            tx.commit()?;
            checkpoint(conn)
        })
        .await
    }

    pub async fn save_to_archive(
        &self,
        date_str: &str,
        group: &str,
        content: &str,
        entry_type: &str,
    ) -> Result<()> {
        self.save_to_archive_batch(&[(
            date_str.to_string(),
            group.to_string(),
            content.to_string(),
            entry_type.to_string(),
        )])
        .await
    }

    /// Удаляет даты, кроме keep_days самых свежих. Как clean_old_archive().
    pub async fn clean_old_archive(&self, keep_days: i64) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "DELETE FROM schedule_archive \
                 WHERE date_str NOT IN (\
                     SELECT date_str FROM (\
                         SELECT date_str FROM schedule_archive \
                         GROUP BY date_str ORDER BY MAX(created_at) DESC LIMIT ?\
                     )\
                 )",
                params![keep_days],
            )?;
            checkpoint(conn)
        })
        .await
    }

    pub async fn save_schedule_files_batch(&self, records: &[ScheduleFile]) -> Result<()> {
        let records = records.to_vec();
        if records.is_empty() {
            return Ok(());
        }
        self.run(move |conn| {
            let tx = conn.transaction()?;
            for f in &records {
                tx.execute(
                    "INSERT OR REPLACE INTO schedule_files \
                     (file_unique_id, date_str, file_type, title, file_id, vk_url, created_at) \
                     VALUES (?, ?, ?, ?, ?, ?, datetime(\"now\"))",
                    params![f.file_unique_id, f.date_str, f.file_type, f.title, f.file_id, f.vk_url],
                )?;
            }
            tx.commit()?;
            checkpoint(conn)
        })
        .await
    }

    pub async fn save_schedule_file(&self, file: &ScheduleFile) -> Result<()> {
        self.save_schedule_files_batch(std::slice::from_ref(file))
            .await
    }

    pub async fn update_schedule_file_id(
        &self,
        date_str: &str,
        file_type: &str,
        file_id: &str,
        file_unique_id: Option<&str>,
    ) -> Result<()> {
        if file_id.is_empty() {
            return Ok(());
        }
        let date_str = date_str.to_string();
        let file_type = file_type.to_string();
        let file_id = file_id.to_string();
        let file_unique_id = file_unique_id.map(str::to_string);
        self.run(move |conn| {
            match file_unique_id {
                Some(uid) => {
                    conn.execute(
                        "UPDATE schedule_files SET file_id = ? WHERE file_unique_id = ? OR (date_str = ? AND file_type = ?)",
                        params![file_id, uid, date_str, file_type],
                    )?;
                }
                None => {
                    conn.execute(
                        "UPDATE schedule_files SET file_id = ? WHERE date_str = ? AND file_type = ?",
                        params![file_id, date_str, file_type],
                    )?;
                }
            }
            checkpoint(conn)
        })
        .await
    }

    /// Файлы за дату не старше 7 дней. Как get_files_for_date().
    pub async fn get_files_for_date(
        &self,
        date_str: &str,
        file_type: Option<&str>,
    ) -> Result<Vec<ScheduleFile>> {
        let date_str = date_str.to_string();
        let file_type = file_type.map(str::to_string);
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT file_unique_id, date_str, file_type, title, file_id, vk_url \
                 FROM schedule_files WHERE date_str = ? AND created_at >= datetime('now', '-7 days') \
                 AND (?2 IS NULL OR file_type = ?2) ORDER BY created_at DESC",
            )?;
            // ?2 IS NULL — file_type передаётся как NULL, когда фильтр не задан:
            // тот же результат, что конкатенация " AND file_type = ?" в Python.
            let rows = stmt
                .query_map(params![date_str, file_type], row_to_file)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    pub async fn get_all_active_files(&self, days: i64) -> Result<Vec<ScheduleFile>> {
        let days_param = format!("-{days}");
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT file_unique_id, date_str, file_type, title, file_id, vk_url FROM schedule_files \
                 WHERE created_at >= datetime('now', ? || ' days') ORDER BY created_at DESC",
            )?;
            let rows = stmt
                .query_map(params![days_param], row_to_file)?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    pub async fn clean_expired_files(&self, days: i64) -> Result<()> {
        let days_param = format!("-{days}");
        self.run(move |conn| {
            conn.execute(
                "DELETE FROM schedule_files WHERE created_at < datetime('now', ? || ' days')",
                params![days_param],
            )?;
            checkpoint(conn)
        })
        .await
    }

    pub async fn get_full_archive_dump(&self) -> Result<Vec<(String, String, String, String)>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT date_str, group_name, content, type FROM schedule_archive ORDER BY created_at ASC",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    pub async fn add_pinned_msg(&self, chat_id: i64, message_id: i64) -> Result<()> {
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO pinned_messages VALUES (?, ?, datetime(\"now\"))",
                params![chat_id, message_id],
            )?;
            checkpoint(conn)
        })
        .await
    }

    pub async fn get_last_pin_for_chat(&self, chat_id: i64) -> Result<Option<i64>> {
        self.run(move |conn| {
            let row = conn
                .query_row(
                    "SELECT message_id FROM pinned_messages WHERE chat_id = ? ORDER BY sent_at DESC LIMIT 1",
                    params![chat_id],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?;
            Ok(row)
        })
        .await
    }

    /// Возвращает пины старше 36ч И удаляет их. Как get_old_pins().
    pub async fn get_old_pins(&self) -> Result<Vec<(i64, i64)>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT chat_id, message_id FROM pinned_messages WHERE sent_at <= datetime('now', '-36 hours')",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if !rows.is_empty() {
                conn.execute(
                    "DELETE FROM pinned_messages WHERE sent_at <= datetime('now', '-36 hours')",
                    [],
                )?;
                checkpoint(conn)?;
            }
            Ok(rows)
        })
        .await
    }

    pub async fn get_reminder_minutes(&self, chat_id: i64) -> Result<i64> {
        self.run(move |conn| {
            let row = conn
                .query_row(
                    "SELECT minutes FROM reminder_settings WHERE chat_id = ?",
                    params![chat_id],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .optional()?;
            // Как в Python: нет строки или NULL/0 — вернуть 0.
            Ok(row.flatten().unwrap_or(0))
        })
        .await
    }

    /// Clamp 0..=120, upsert. Как set_reminder_minutes().
    pub async fn set_reminder_minutes(&self, chat_id: i64, minutes: i64) -> Result<()> {
        let minutes = minutes.clamp(0, 120);
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO reminder_settings (chat_id, minutes) VALUES (?, ?) \
                 ON CONFLICT(chat_id) DO UPDATE SET minutes = excluded.minutes",
                params![chat_id, minutes],
            )?;
            checkpoint(conn)
        })
        .await
    }

    /// Только minutes > 0. Как get_all_reminders().
    pub async fn get_all_reminders(&self) -> Result<Vec<(i64, i64)>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT chat_id, minutes FROM reminder_settings WHERE minutes > 0",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Записи архива (content, type) для даты и группы — для format_day_data.
    pub async fn get_archive_records(
        &self,
        date_str: &str,
        group: &str,
    ) -> Result<Vec<(String, String)>> {
        let date_str = date_str.to_string();
        let group = group.to_string();
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT content, type FROM schedule_archive WHERE date_str = ? AND group_name = ? ORDER BY created_at ASC",
            )?;
            let rows = stmt
                .query_map(params![date_str, group], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Все группы из архива (без "__ZVONKI__") — для /api/groups.
    pub async fn get_archive_groups(&self) -> Result<Vec<String>> {
        self.run(|conn| {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT group_name FROM schedule_archive WHERE group_name != '__ZVONKI__' ORDER BY group_name",
            )?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    // ================= Заметки и пропуски (только Mini App) =================

    /// Заметки пользователя: (subject, text, updated_at), по subject.
    pub async fn get_notes(&self, user_id: i64) -> Result<Vec<(String, String, String)>> {
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT subject, text, updated_at FROM user_notes \
                 WHERE user_id = ? ORDER BY subject",
            )?;
            let rows = stmt
                .query_map(params![user_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Upsert заметки; updated_at — datetime('now') (UTC, как created_at).
    pub async fn upsert_note(&self, user_id: i64, subject: &str, text: &str) -> Result<()> {
        let subject = subject.to_string();
        let text = text.to_string();
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO user_notes (user_id, subject, text, updated_at) \
                 VALUES (?, ?, ?, datetime('now')) \
                 ON CONFLICT(user_id, subject) DO UPDATE \
                 SET text = excluded.text, updated_at = excluded.updated_at",
                params![user_id, subject, text],
            )?;
            checkpoint(conn)
        })
        .await
    }

    pub async fn delete_note(&self, user_id: i64, subject: &str) -> Result<()> {
        let subject = subject.to_string();
        self.run(move |conn| {
            conn.execute(
                "DELETE FROM user_notes WHERE user_id = ? AND subject = ?",
                params![user_id, subject],
            )?;
            checkpoint(conn)
        })
        .await
    }

    /// Пропуски пользователя: (date_str, subject, pair), сначала недавние.
    pub async fn get_absences(&self, user_id: i64) -> Result<Vec<(String, String, i64)>> {
        self.run(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT date_str, subject, pair FROM user_absences \
                 WHERE user_id = ? ORDER BY date_str DESC, pair ASC",
            )?;
            let rows = stmt
                .query_map(params![user_id], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
    }

    /// Отметить пропуск (upsert по (user_id, date_str, subject)).
    pub async fn upsert_absence(
        &self,
        user_id: i64,
        date_str: &str,
        subject: &str,
        pair: i64,
    ) -> Result<()> {
        let date_str = date_str.to_string();
        let subject = subject.to_string();
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO user_absences (user_id, date_str, subject, pair) \
                 VALUES (?, ?, ?, ?) \
                 ON CONFLICT(user_id, date_str, subject) DO UPDATE SET pair = excluded.pair",
                params![user_id, date_str, subject, pair],
            )?;
            checkpoint(conn)
        })
        .await
    }

    /// Снять пропуск.
    pub async fn delete_absence(&self, user_id: i64, date_str: &str, subject: &str) -> Result<()> {
        let date_str = date_str.to_string();
        let subject = subject.to_string();
        self.run(move |conn| {
            conn.execute(
                "DELETE FROM user_absences WHERE user_id = ? AND date_str = ? AND subject = ?",
                params![user_id, date_str, subject],
            )?;
            checkpoint(conn)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    /// Новая временная БД с выполненным init(). Файл живёт, пока живёт кортеж.
    async fn test_db() -> (Db, NamedTempFile) {
        let f = NamedTempFile::new().unwrap();
        let db = Db::open(f.path().to_str().unwrap()).unwrap();
        db.init().await.unwrap();
        (db, f)
    }

    fn raw_conn(path: &std::path::Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(10)).unwrap();
        conn
    }

    fn mk_file(uid: &str, date: &str, ftype: &str) -> ScheduleFile {
        ScheduleFile {
            file_unique_id: uid.to_string(),
            date_str: date.to_string(),
            file_type: ftype.to_string(),
            title: format!("title-{uid}"),
            file_id: Some(format!("tg-{uid}")),
            vk_url: format!("https://vk.com/wall-1_{uid}"),
        }
    }

    #[tokio::test]
    async fn init_creates_python_schema() {
        let (db, f) = test_db().await;
        // init() идемпотентен.
        db.init().await.unwrap();

        let conn = raw_conn(f.path());
        // Таблицы — ровно как в database.py (+ sqlite_sequence от AUTOINCREMENT).
        let tables: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            tables,
            vec![
                "chat_settings",
                "pinned_messages",
                "reminder_settings",
                "schedule_archive",
                "schedule_files",
                "user_absences",
                "user_notes",
            ]
        );
        // Колонки каждой таблицы.
        let cols = |table: &str| -> Vec<(String, String)> {
            conn.prepare(&format!("PRAGMA table_info({table})"))
                .unwrap()
                .query_map([], |r| {
                    Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?))
                })
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        assert_eq!(
            cols("chat_settings"),
            vec![
                ("chat_id".to_string(), "INTEGER".to_string()),
                ("group_name".to_string(), "TEXT".to_string()),
            ]
        );
        assert_eq!(
            cols("reminder_settings"),
            vec![
                ("chat_id".to_string(), "INTEGER".to_string()),
                ("minutes".to_string(), "INTEGER".to_string()),
            ]
        );
        assert_eq!(
            cols("schedule_archive"),
            vec![
                ("id".to_string(), "INTEGER".to_string()),
                ("date_str".to_string(), "TEXT".to_string()),
                ("group_name".to_string(), "TEXT".to_string()),
                ("content".to_string(), "TEXT".to_string()),
                ("type".to_string(), "TEXT".to_string()),
                ("created_at".to_string(), "TIMESTAMP".to_string()),
            ]
        );
        assert_eq!(
            cols("pinned_messages"),
            vec![
                ("chat_id".to_string(), "INTEGER".to_string()),
                ("message_id".to_string(), "INTEGER".to_string()),
                ("sent_at".to_string(), "TIMESTAMP".to_string()),
            ]
        );
        assert_eq!(
            cols("schedule_files"),
            vec![
                ("id".to_string(), "INTEGER".to_string()),
                ("file_unique_id".to_string(), "TEXT".to_string()),
                ("date_str".to_string(), "TEXT".to_string()),
                ("file_type".to_string(), "TEXT".to_string()),
                ("title".to_string(), "TEXT".to_string()),
                ("file_id".to_string(), "TEXT".to_string()),
                ("vk_url".to_string(), "TEXT".to_string()),
                ("created_at".to_string(), "TIMESTAMP".to_string()),
            ]
        );
        // Новые таблицы Mini App (только CREATE TABLE IF NOT EXISTS в init()).
        assert_eq!(
            cols("user_notes"),
            vec![
                ("user_id".to_string(), "INTEGER".to_string()),
                ("subject".to_string(), "TEXT".to_string()),
                ("text".to_string(), "TEXT".to_string()),
                ("updated_at".to_string(), "TIMESTAMP".to_string()),
            ]
        );
        assert_eq!(
            cols("user_absences"),
            vec![
                ("user_id".to_string(), "INTEGER".to_string()),
                ("date_str".to_string(), "TEXT".to_string()),
                ("subject".to_string(), "TEXT".to_string()),
                ("pair".to_string(), "INTEGER".to_string()),
            ]
        );
        // Индексы — ровно как в database.py.
        let indexes: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            indexes,
            vec![
                "idx_files_created",
                "idx_files_date",
                "idx_pinned_chat",
                "idx_pinned_sent",
                "idx_search",
            ]
        );
        // init() удаляет фото-строки "🔔 Расписание по времени (фото)%".
        conn.execute(
            "INSERT INTO schedule_archive (date_str, group_name, content, type, created_at) \
             VALUES ('01.01.2026', 'ЮР-26-21', '🔔 Расписание по времени (фото) тест', 'основное', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO schedule_archive (date_str, group_name, content, type, created_at) \
             VALUES ('01.01.2026', 'ЮР-26-21', 'обычное расписание', 'основное', datetime('now'))",
            [],
        )
        .unwrap();
        db.init().await.unwrap();
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schedule_archive WHERE content LIKE '🔔 Расписание по времени (фото)%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
        let kept: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schedule_archive WHERE content = 'обычное расписание'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, 1);
    }

    #[tokio::test]
    async fn chats_set_get_delete() {
        let (db, _f) = test_db().await;
        assert_eq!(db.get_group(111).await.unwrap(), None);

        db.set_group(111, "ЮР-26-21").await.unwrap();
        db.set_group(222, "ТД-26-21").await.unwrap();
        assert_eq!(
            db.get_group(111).await.unwrap().as_deref(),
            Some("ЮР-26-21")
        );
        // INSERT OR REPLACE: повторная установка меняет группу.
        db.set_group(111, "26-14").await.unwrap();
        assert_eq!(db.get_group(111).await.unwrap().as_deref(), Some("26-14"));

        let mut chats = db.get_all_chats().await.unwrap();
        chats.sort();
        assert_eq!(chats, vec![(111, "26-14".to_string()), (222, "ТД-26-21".to_string())]);

        // delete_chat удаляет и группу, и пины чата.
        db.add_pinned_msg(111, 555).await.unwrap();
        db.delete_chat(111).await.unwrap();
        assert_eq!(db.get_group(111).await.unwrap(), None);
        assert_eq!(db.get_last_pin_for_chat(111).await.unwrap(), None);
        assert_eq!(
            db.get_all_chats().await.unwrap(),
            vec![(222, "ТД-26-21".to_string())]
        );
    }

    #[tokio::test]
    async fn archive_batch_dedup_and_dump() {
        let (db, _f) = test_db().await;
        db.save_to_archive_batch(&[
            ("18.11.2026".to_string(), "ЮР-26-21".to_string(), "старое основное".to_string(), "основное".to_string()),
            ("18.11.2026".to_string(), "ЮР-26-21".to_string(), "замена 1".to_string(), "замена".to_string()),
        ])
        .await
        .unwrap();
        // Второй пакет с "основное" удаляет старое основное, но не замены.
        db.save_to_archive_batch(&[(
            "18.11.2026".to_string(),
            "ЮР-26-21".to_string(),
            "новое основное".to_string(),
            "основное".to_string(),
        )])
        .await
        .unwrap();
        let recs = db
            .get_archive_records("18.11.2026", "ЮР-26-21")
            .await
            .unwrap();
        assert_eq!(
            recs,
            vec![
                ("замена 1".to_string(), "замена".to_string()),
                ("новое основное".to_string(), "основное".to_string()),
            ]
        );
        // Пустой пакет — no-op, без ошибки.
        db.save_to_archive_batch(&[]).await.unwrap();

        // save_to_archive — одиночная запись.
        db.save_to_archive("19.11.2026", "ЮР-26-21", "пары", "основное")
            .await
            .unwrap();
        assert_eq!(
            db.get_archive_records("19.11.2026", "ЮР-26-21")
                .await
                .unwrap(),
            vec![("пары".to_string(), "основное".to_string())]
        );

        // get_full_archive_dump: порядок created_at ASC.
        let dump = db.get_full_archive_dump().await.unwrap();
        assert_eq!(dump.len(), 3);
        assert_eq!(dump[0].0, "18.11.2026");
        assert_eq!(dump[2].0, "19.11.2026");

        // get_archive_groups: без "__ZVONKI__".
        db.save_to_archive("18.11.2026", "__ZVONKI__", "{}", "звонки")
            .await
            .unwrap();
        assert_eq!(
            db.get_archive_groups().await.unwrap(),
            vec!["ЮР-26-21".to_string()]
        );
    }

    #[tokio::test]
    async fn clean_old_archive_keeps_n_dates() {
        let (db, f) = test_db().await;
        // Явные created_at, чтобы порядок MAX(created_at) был детерминирован.
        let conn = raw_conn(f.path());
        for (i, date) in ["01.01.2026", "02.01.2026", "03.01.2026"]
            .iter()
            .enumerate()
        {
            conn.execute(
                "INSERT INTO schedule_archive (date_str, group_name, content, type, created_at) \
                 VALUES (?1, 'ЮР-26-21', 'x', 'основное', datetime('2026-01-0' || ?2 || ' 00:00:00'))",
                params![date, i + 1],
            )
            .unwrap();
        }
        drop(conn);
        db.clean_old_archive(2).await.unwrap();
        let dump = db.get_full_archive_dump().await.unwrap();
        let dates: Vec<&str> = dump.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(dates, vec!["02.01.2026", "03.01.2026"]);
    }

    #[tokio::test]
    async fn files_roundtrip_and_expiry() {
        let (db, f) = test_db().await;
        db.save_schedule_file(&mk_file("u1", "18.11.2026", "photo"))
            .await
            .unwrap();
        db.save_schedule_files_batch(&[
            mk_file("u2", "18.11.2026", "zvonki"),
            mk_file("u3", "19.11.2026", "photo"),
        ])
        .await
        .unwrap();
        // INSERT OR REPLACE по file_unique_id.
        db.save_schedule_file(&mk_file("u1", "18.11.2026", "photo"))
            .await
            .unwrap();
        assert_eq!(db.get_all_active_files(14).await.unwrap().len(), 3);

        let by_date = db
            .get_files_for_date("18.11.2026", None)
            .await
            .unwrap();
        assert_eq!(by_date.len(), 2);
        let photos = db
            .get_files_for_date("18.11.2026", Some("photo"))
            .await
            .unwrap();
        assert_eq!(photos.len(), 1);
        assert_eq!(photos[0].file_unique_id, "u1");

        db.update_schedule_file_id("18.11.2026", "photo", "tg-new", Some("u1"))
            .await
            .unwrap();
        let photos = db
            .get_files_for_date("18.11.2026", Some("photo"))
            .await
            .unwrap();
        assert_eq!(photos[0].file_id.as_deref(), Some("tg-new"));
        // Пустой file_id — no-op (как в Python).
        db.update_schedule_file_id("18.11.2026", "photo", "", Some("u1"))
            .await
            .unwrap();
        let photos = db
            .get_files_for_date("18.11.2026", Some("photo"))
            .await
            .unwrap();
        assert_eq!(photos[0].file_id.as_deref(), Some("tg-new"));

        // Протухший файл (created_at 20 дней назад): не виден в активных,
        // удаляется clean_expired_files(14).
        let conn = raw_conn(f.path());
        conn.execute(
            "INSERT INTO schedule_files (file_unique_id, date_str, file_type, title, file_id, vk_url, created_at) \
             VALUES ('old1', '01.11.2026', 'photo', 't', 'tg-old', 'u', datetime('now', '-20 days'))",
            [],
        )
        .unwrap();
        drop(conn);
        assert_eq!(db.get_all_active_files(14).await.unwrap().len(), 3);
        assert_eq!(db.get_files_for_date("01.11.2026", None).await.unwrap().len(), 0);
        db.clean_expired_files(14).await.unwrap();
        let conn = raw_conn(f.path());
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM schedule_files WHERE file_unique_id = 'old1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
    }

    #[tokio::test]
    async fn reminders_clamp_and_list() {
        let (db, _f) = test_db().await;
        assert_eq!(db.get_reminder_minutes(1).await.unwrap(), 0);

        db.set_reminder_minutes(1, 15).await.unwrap();
        assert_eq!(db.get_reminder_minutes(1).await.unwrap(), 15);
        // upsert: повторная установка перезаписывает.
        db.set_reminder_minutes(1, 30).await.unwrap();
        assert_eq!(db.get_reminder_minutes(1).await.unwrap(), 30);

        // Clamp 0..=120.
        db.set_reminder_minutes(2, 121).await.unwrap();
        assert_eq!(db.get_reminder_minutes(2).await.unwrap(), 120);
        db.set_reminder_minutes(3, -5).await.unwrap();
        assert_eq!(db.get_reminder_minutes(3).await.unwrap(), 0);
        db.set_reminder_minutes(4, 0).await.unwrap();
        assert_eq!(db.get_reminder_minutes(4).await.unwrap(), 0);
        db.set_reminder_minutes(5, 1).await.unwrap();
        db.set_reminder_minutes(6, 120).await.unwrap();

        let mut all = db.get_all_reminders().await.unwrap();
        all.sort();
        // Только minutes > 0: чаты 3 и 4 (0) не попадают.
        assert_eq!(all, vec![(1, 30), (2, 120), (5, 1), (6, 120)]);

        // Выключение (0) убирает чат из списка.
        db.set_reminder_minutes(1, 0).await.unwrap();
        let all = db.get_all_reminders().await.unwrap();
        assert!(!all.iter().any(|(c, _)| *c == 1));
    }

    #[tokio::test]
    async fn notes_absences_crud_and_isolation() {
        let (db, _f) = test_db().await;
        // Пусто для обоих пользователей.
        assert!(db.get_notes(111).await.unwrap().is_empty());
        assert!(db.get_absences(111).await.unwrap().is_empty());

        // Заметки: upsert, перезапись, удаление; изоляция по user_id.
        db.upsert_note(111, "Математика", "повторить §3").await.unwrap();
        db.upsert_note(111, "История", "доклад").await.unwrap();
        db.upsert_note(222, "Математика", "чужая заметка").await.unwrap();
        let notes = db.get_notes(111).await.unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].0, "История");
        assert_eq!(notes[0].1, "доклад");
        assert!(!notes[0].2.is_empty(), "updated_at заполнен");
        assert_eq!(notes[1].0, "Математика");
        assert_eq!(notes[1].1, "повторить §3");
        // Перезапись той же заметки.
        db.upsert_note(111, "Математика", "повторить §3–4").await.unwrap();
        let notes = db.get_notes(111).await.unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(
            notes.iter().find(|n| n.0 == "Математика").unwrap().1,
            "повторить §3–4"
        );
        // У другого пользователя — своя заметка, не тронута.
        assert_eq!(db.get_notes(222).await.unwrap().len(), 1);
        db.delete_note(111, "История").await.unwrap();
        assert_eq!(db.get_notes(111).await.unwrap().len(), 1);
        // Удаление несуществующей — no-op, без ошибки.
        db.delete_note(111, "История").await.unwrap();

        // Пропуски: upsert, перезапись номера пары, удаление, изоляция.
        db.upsert_absence(111, "18.11.2026", "Математика", 1).await.unwrap();
        db.upsert_absence(111, "18.11.2026", "География", 2).await.unwrap();
        db.upsert_absence(111, "19.11.2026", "Математика", 3).await.unwrap();
        db.upsert_absence(222, "18.11.2026", "Математика", 1).await.unwrap();
        let abs = db.get_absences(111).await.unwrap();
        assert_eq!(abs.len(), 3);
        // Сначала недавние (дата DESC), внутри даты — по номеру пары.
        assert_eq!(abs[0], ("19.11.2026".to_string(), "Математика".to_string(), 3));
        assert_eq!(abs[1], ("18.11.2026".to_string(), "Математика".to_string(), 1));
        assert_eq!(abs[2], ("18.11.2026".to_string(), "География".to_string(), 2));
        // Перезапись номера пары той же (дата, предмет).
        db.upsert_absence(111, "18.11.2026", "Математика", 4).await.unwrap();
        let abs = db.get_absences(111).await.unwrap();
        assert_eq!(abs.len(), 3);
        assert_eq!(
            abs.iter().find(|a| a.0 == "18.11.2026" && a.1 == "Математика").unwrap().2,
            4
        );
        assert_eq!(db.get_absences(222).await.unwrap().len(), 1);
        db.delete_absence(111, "18.11.2026", "География").await.unwrap();
        assert_eq!(db.get_absences(111).await.unwrap().len(), 2);
        db.delete_absence(111, "18.11.2026", "География").await.unwrap();
    }

    #[tokio::test]
    async fn pins_last_and_old() {
        let (db, f) = test_db().await;
        assert_eq!(db.get_last_pin_for_chat(7).await.unwrap(), None);
        assert!(db.get_old_pins().await.unwrap().is_empty());

        // Сначала get_old_pins: только пин старше 36ч, свежий не трогается.
        // (Пины с январскими датами для get_last вставляются ПОСЛЕ, иначе
        // они тоже попали бы в старые: январь 2026 старше 36ч от "сейчас".)
        let conn = raw_conn(f.path());
        conn.execute(
            "INSERT INTO pinned_messages VALUES (9, 300, datetime('now', '-40 hours'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pinned_messages VALUES (8, 400, datetime('now', '-1 hours'))",
            [],
        )
        .unwrap();
        drop(conn);

        // get_old_pins: только старше 36ч, и удаляет их.
        let old = db.get_old_pins().await.unwrap();
        assert_eq!(old, vec![(9, 300)]);
        assert!(db.get_old_pins().await.unwrap().is_empty());
        // Свежий пин на месте.
        assert_eq!(db.get_last_pin_for_chat(8).await.unwrap(), Some(400));

        // Пины с явными sent_at — детерминированный порядок для get_last.
        let conn = raw_conn(f.path());
        conn.execute(
            "INSERT INTO pinned_messages VALUES (7, 100, '2026-01-01 00:00:00')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pinned_messages VALUES (7, 200, '2026-01-02 00:00:00')",
            [],
        )
        .unwrap();
        drop(conn);

        assert_eq!(db.get_last_pin_for_chat(7).await.unwrap(), Some(200));
        // add_pinned_msg ставит sent_at = now — становится последним.
        db.add_pinned_msg(7, 500).await.unwrap();
        assert_eq!(db.get_last_pin_for_chat(7).await.unwrap(), Some(500));
    }
}
