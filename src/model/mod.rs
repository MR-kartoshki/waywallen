use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sea_orm::sqlx::sqlite::{SqliteAutoVacuum, SqliteJournalMode, SqliteSynchronous};
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;

pub mod entities;
pub mod filter;
pub mod fts;
mod library_name;
pub mod migration;
pub mod playlist_source;
pub mod repo;
pub mod sync;

/// Open (or create) the SQLite DB at `db_path`, run pending migrations,
/// and hand back a pooled [`DatabaseConnection`]. The parent directory
pub async fn connect(db_path: &Path) -> Result<DatabaseConnection> {
    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create db parent {}", parent.display()))?;
        }
    }
    let url = format!("sqlite://{}?mode=rwc", db_path.display());
    connect_url(&url).await
}

/// Open a connection to an arbitrary SQLite URL. Exists so tests can
/// target `sqlite::memory:` without touching the filesystem.
pub async fn connect_url(url: &str) -> Result<DatabaseConnection> {
    let mut opt = ConnectOptions::new(url.to_owned());

    let enable_logging = std::env::var("WAYWALLEN_SQL_LOGGING").is_ok();
    opt.sqlx_logging(enable_logging)
        .sqlx_logging_level(log::LevelFilter::Debug)
        .sqlx_slow_statements_logging_settings(log::LevelFilter::Info, Duration::from_secs(1))
        .min_connections(1)
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(30))
        .map_sqlx_sqlite_opts(|o| {
            o.foreign_keys(true)
                .journal_mode(SqliteJournalMode::Wal)
                .synchronous(SqliteSynchronous::Normal)
                .auto_vacuum(SqliteAutoVacuum::Incremental)
                .busy_timeout(Duration::from_secs(5))
                .pragma("temp_store", "MEMORY")
                .pragma("mmap_size", "134217728")
                .pragma("journal_size_limit", "67108864")
                .pragma("cache_size", "2000")
                .pragma("wal_autocheckpoint", "1000")
        });

    let db = Database::connect(opt)
        .await
        .with_context(|| format!("connect {url}"))?;

    migration::Migrator::up(&db, None)
        .await
        .context("run migrations")?;
    Ok(db)
}
