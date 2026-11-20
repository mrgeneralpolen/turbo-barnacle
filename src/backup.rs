//! Ручной бэкап базы данных — порт backup.py (34 строки).
//!
//! Поведение 1-в-1 с Python:
//! - источник — `DB_PATH` из env (default `settings.db`), каталог — `backups/`;
//! - имя файла — `backup_ГГГГ-ММ-ДД_ЧЧ-ММ-СС.db`;
//! - копирование — через SQLite backup API (онлайн-бэкап работающей БД),
//!   а не побайтовое копирование файла.
//!
//! Отличия (задокументированы):
//! - время в имени файла — тюменское (как везде в боте), а не локальное
//!   время машины, как `datetime.now()` в Python;
//! - `create_backup` возвращает `Err`, а не делает `sys.exit(1)` — выход
//!   с кодом 1 делает example (см. `examples/backup.rs`).
//! - путь назначения возвращается вызывающему (Python только печатает его).
//!
//! Запуск: `cargo run --offline --example backup`
//! (собранный бинарь бота для этого не нужен и не запускается).

use crate::{AppError, Result};
use rusqlite::backup::Backup;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Создаёт бэкап БД в `backup_dir`, возвращает путь к созданному файлу.
/// Ошибка, если исходной БД нет (как `sys.exit(1)` в Python).
pub fn create_backup(db_path: &Path, backup_dir: &Path) -> Result<PathBuf> {
    if !db_path.exists() {
        return Err(AppError::other(format!(
            "База данных не найдена: {}",
            db_path.display()
        )));
    }
    let src = Connection::open(db_path)?;
    std::fs::create_dir_all(backup_dir)?;
    let stamp = crate::tyumen::tyumen_now().format("%Y-%m-%d_%H-%M-%S");
    let backup_filename = format!("backup_{stamp}.db");
    let dst_path = backup_dir.join(&backup_filename);
    let mut dst = Connection::open(&dst_path)?;
    let backup = Backup::new(&src, &mut dst)?;
    backup.run_to_completion(5, Duration::from_millis(250), None)?;
    Ok(dst_path)
}

/// Имя файла бэкапа для заданного времени (для тестов).
pub fn backup_filename_for(stamp: &str) -> String {
    format!("backup_{stamp}.db")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[tokio::test]
    async fn backup_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let src_path = dir.path().join("settings.db");
        std::fs::copy("tests/fixtures/settings.db", &src_path).unwrap();
        let backup_dir = dir.path().join("backups");
        let out = create_backup(&src_path, &backup_dir).unwrap();
        assert!(out.starts_with(&backup_dir));
        assert!(out
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("backup_20"));
        assert!(out.file_name().unwrap().to_string_lossy().ends_with(".db"));
        // Бэкап — валидная копия: открывается и содержит те же строки архива.
        let src_db = Db::open(src_path.to_str().unwrap()).unwrap();
        let dst_db = Db::open(out.to_str().unwrap()).unwrap();
        let src_rows = src_db.get_full_archive_dump().await.unwrap();
        let dst_rows = dst_db.get_full_archive_dump().await.unwrap();
        assert_eq!(src_rows.len(), dst_rows.len());
        assert_eq!(src_rows, dst_rows);
    }

    #[test]
    fn backup_missing_db_is_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = create_backup(&dir.path().join("nope.db"), &dir.path().join("backups"))
            .unwrap_err();
        assert!(err.to_string().contains("не найдена"));
    }

    #[test]
    fn backup_filename_shape() {
        assert_eq!(
            backup_filename_for("2026-11-20_07-55-01"),
            "backup_2026-11-20_07-55-01.db"
        );
    }
}
