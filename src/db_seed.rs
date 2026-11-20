//! Первичное заполнение БД из внешнего URL (например, settings.db из GitHub).
//!
//! Назначение: бот при первом запуске (пустой Volume на Railway) сам
//! забирает settings.db из репозитория, вместо ручной загрузки файла.
//! Работает ТОЛЬКО как seed:
//! - скачивание — только если файла БД нет или он меньше заголовка SQLite
//!   (< 100 байт — пустой/битый файл);
//! - живая БД из Volume никогда не перезаписывается и не обновляется:
//!   после первого запуска источник правды — она;
//! - чтобы пере-seed'ить (взять свежую копию из репозитория), надо
//!   удалить settings.db из Volume и перезапустить сервис.
//! - любая ошибка (нет сети, не 200, не SQLite) — только warning в логе,
//!   бот стартует с пустой БД и наполняет архив сам через VK watcher.
//!
//! URL задаётся переменной окружения DB_URL (см. config::Config::db_url),
//! например raw-ссылка на settings.db в репозитории:
//! https://raw.githubusercontent.com/<user>/<repo>/main/settings.db

use std::path::Path;
use std::time::Duration;

/// Магические байты заголовка SQLite: "SQLite format 3\0".
const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";
/// Длина заголовка SQLite-файла в байтах.
const SQLITE_HEADER_LEN: u64 = 100;

/// true, если начало байтов — заголовок файла SQLite.
pub fn looks_like_sqlite(bytes: &[u8]) -> bool {
    bytes.len() >= SQLITE_MAGIC.len() && &bytes[..SQLITE_MAGIC.len()] == SQLITE_MAGIC
}

/// Файлу нужна seed-загрузка: его нет или он короче заголовка SQLite.
fn needs_seed(db_path: &Path) -> bool {
    match std::fs::metadata(db_path) {
        Ok(m) => m.len() < SQLITE_HEADER_LEN,
        Err(_) => true,
    }
}

/// Скачивает БД по `url` в `db_path`, если нужен seed. Не падает:
/// при любой проблеме пишет warning и возвращает Ok (бот стартует с пустой БД).
pub async fn ensure_seeded(db_path: &Path, url: Option<&str>) -> anyhow::Result<()> {
    let Some(url) = url.filter(|u| !u.trim().is_empty()) else {
        return Ok(());
    };
    if !needs_seed(db_path) {
        tracing::info!("БД уже есть ({}), seed из URL не нужен", db_path.display());
        return Ok(());
    }
    tracing::info!("БД отсутствует/пуста — скачиваю seed из {}", url);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("seed БД: не удалось скачать {}: {}", url, e);
            return Ok(());
        }
    };
    if !resp.status().is_success() {
        tracing::warn!("seed БД: {} вернул статус {}", url, resp.status());
        return Ok(());
    }
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("seed БД: не удалось прочитать тело ответа: {}", e);
            return Ok(());
        }
    };
    if !looks_like_sqlite(&bytes) {
        tracing::warn!(
            "seed БД: скачанный файл не похож на SQLite ({} байт), игнорирую",
            bytes.len()
        );
        return Ok(());
    }
    // Пишем во временный файл рядом и атомарно переименовываем,
    // чтобы недокачанный файл никогда не стал settings.db.
    let tmp_path = db_path.with_extension("download");
    if let Err(e) = tokio::fs::write(&tmp_path, &bytes).await {
        tracing::warn!("seed БД: не удалось записать {}: {}", tmp_path.display(), e);
        return Ok(());
    }
    if let Err(e) = tokio::fs::rename(&tmp_path, db_path).await {
        tracing::warn!("seed БД: не удалось переместить в {}: {}", db_path.display(), e);
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Ok(());
    }
    tracing::info!("seed БД: скачано {} байт в {}", bytes.len(), db_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_magic_ok() {
        let mut buf = SQLITE_MAGIC.to_vec();
        buf.extend_from_slice(&[0u8; 100]);
        assert!(looks_like_sqlite(&buf));
    }

    #[test]
    fn sqlite_magic_rejects_garbage() {
        assert!(!looks_like_sqlite(b""));
        assert!(!looks_like_sqlite(b"SQLite format 2\0xxxx"));
        assert!(!looks_like_sqlite(b"<!DOCTYPE html><html>"));
        assert!(!looks_like_sqlite(&[0u8; 200]));
    }

    #[test]
    fn needs_seed_missing_and_tiny() {
        let dir = std::env::temp_dir().join("db_seed_test");
        let _ = std::fs::create_dir_all(&dir);
        let missing = dir.join("missing.db");
        let _ = std::fs::remove_file(&missing);
        assert!(needs_seed(&missing));
        let tiny = dir.join("tiny.db");
        std::fs::write(&tiny, b"SQLite").unwrap();
        assert!(needs_seed(&tiny));
        let ok = dir.join("ok.db");
        let mut buf = SQLITE_MAGIC.to_vec();
        buf.extend_from_slice(&[0u8; 4096]);
        std::fs::write(&ok, &buf).unwrap();
        assert!(!needs_seed(&ok));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
