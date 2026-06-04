//! sqlx pool + typed queries. Plan Step 6.
//!
//! DB path: `~/.local/share/auwsx/state.db` (overrideable for tests).
//! Migrations: `crates/auwsx-core/src/db/migrations/*.sql` embedded via `sqlx::migrate!`.
//!
//! Sqlx is configured with sqlite + runtime-tokio. No ORM — typed queries via
//! `sqlx::query_as!`. Single writer, multiple reader connections in the pool.

use crate::Result;

// TODO: pub struct Db { pool: sqlx::SqlitePool }
// TODO: Db::open(path: &Path) -> Result<Db> (creates file if missing, runs migrations)
// TODO: typed CRUD helpers per entity (projects, tasks, drafts, followups, routines, main_jobs, iterations)
