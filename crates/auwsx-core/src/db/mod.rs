//! sqlx pool + typed queries. Plan Step 6.
//!
//! Single source of truth for persisted state. The daemon owns one [`Db`]
//! instance; UIs talk to the daemon over IPC and never open SQLite directly.
//!
//! DB path resolution (in order):
//!   1. explicit override (test or env: `AUWSX_DB_PATH`)
//!   2. `$XDG_DATA_HOME/auwsx/state.db`           (`directories::ProjectDirs`)
//!   3. `~/.local/share/auwsx/state.db`           (fallback)
//!
//! `Db::open()` creates the parent dir if missing, opens the pool in WAL
//! journal mode (better concurrency than the default `delete`), and runs all
//! embedded migrations under `src/db/migrations/` on first connect.
//!
//! In-process tests use [`Db::open_memory`] to spin a fresh `sqlite::memory:`
//! DB per test — fully isolated, no filesystem touch.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::str::FromStr;

// Typed row + CRUD modules for the core entities that have no behaviour module
// of their own. (backlog_items + steering live in the top-level `backlog` /
// `steering` modules, alongside their gating logic.)
pub mod findings;
pub mod issues;
pub mod projects;
pub mod subtasks;

pub use findings::Finding;
pub use issues::Issue;
pub use projects::Project;
pub use subtasks::Subtask;

/// Embedded migration set. `sqlx::migrate!` reads `./migrations/` at compile
/// time relative to this file, so the SQL is baked into the binary; no
/// runtime filesystem dependency for migrations.
static MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("./src/db/migrations");

/// Daemon-owned database handle.
#[derive(Debug, Clone)]
pub struct Db {
    pool: SqlitePool,
}

impl Db {
    /// Open (and migrate) the on-disk database at the resolved path.
    /// Creates the file and parent directory if missing.
    pub async fn open() -> Result<Self> {
        let path = default_db_path()?;
        Self::open_at(&path).await
    }

    /// Open (and migrate) the database at a specific path. Useful for tests
    /// or non-default deployments.
    pub async fn open_at(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating db parent dir {}", parent.display()))?;
            }
        }

        let url = format!("sqlite://{}", path.display());
        let opts = SqliteConnectOptions::from_str(&url)
            .with_context(|| format!("parsing sqlite url {url}"))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);

        Self::open_with_options(opts).await
    }

    /// Open (and migrate) a fresh in-memory database. Each call returns a
    /// brand-new database — `sqlite::memory:` instances are not shared
    /// between connections by default, so the pool is capped at 1 to keep
    /// all activity in the same SQLite memory area.
    pub async fn open_memory() -> Result<Self> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")
            .context("parsing sqlite::memory: url")?
            .foreign_keys(true);
        Self::open_with_options_capped(opts, 1).await
    }

    async fn open_with_options(opts: SqliteConnectOptions) -> Result<Self> {
        Self::open_with_options_capped(opts, 8).await
    }

    async fn open_with_options_capped(
        opts: SqliteConnectOptions,
        max_connections: u32,
    ) -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .connect_with(opts)
            .await
            .context("connecting to sqlite")?;

        MIGRATIONS
            .run(&pool)
            .await
            .context("running embedded migrations")?;

        Ok(Self { pool })
    }

    /// Borrowed access for query builders. Most callers should add a typed
    /// helper to this module rather than reach for `pool()` directly.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Gracefully drain connections. Optional — `Drop` on `Db` will close
    /// the pool too, but `close().await` waits for in-flight queries.
    pub async fn close(self) {
        self.pool.close().await;
    }
}

/// Resolve the default on-disk path.
fn default_db_path() -> Result<PathBuf> {
    if let Ok(env) = std::env::var("AUWSX_DB_PATH") {
        return Ok(PathBuf::from(env));
    }
    if let Some(dirs) = ProjectDirs::from("", "", "auwsx") {
        return Ok(dirs.data_dir().join("state.db"));
    }
    let home = std::env::var("HOME").context("HOME not set; cannot derive db path")?;
    Ok(PathBuf::from(home).join(".local/share/auwsx/state.db"))
}

#[cfg(test)]
mod tests {
    // Production behaviour (open, migrate, basic insert/query roundtrip)
    // is covered by an integration test under tests/ so we exercise the
    // real tokio runtime + sqlx pool without polluting library compilation
    // with dev-deps. See tests/db_smoke.rs.
}
