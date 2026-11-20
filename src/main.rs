//! Бинарь schedule-bot: конфиг из env, БД, сервис, сервер.
//!
//! Вся логика — в библиотеке (см. bot::serve). Здесь только:
//! tracing, Config::from_env, Db::open + init, ScheduleService::new,
//! load_ram, serve.

use schedule_bot::bot;
use schedule_bot::config::Config;
use schedule_bot::db::Db;
use schedule_bot::service::ScheduleService;
use std::sync::Arc;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config = Config::from_env()?;
    let db = Arc::new(Db::open(&config.db_path)?);
    db.init().await?;
    let service = Arc::new(ScheduleService::new(
        db.clone(),
        config.vk_token.clone(),
        config.vk_domain.clone(),
    ));
    service.load_ram().await?;
    bot::serve(config, service, db).await?;
    Ok(())
}
