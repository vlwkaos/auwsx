//! Global project profiles.

use crate::Result;
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: i64,
    pub name: String,
    pub ord: i64,
    pub created_at: i64,
}

impl Profile {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        Ok(Profile {
            id: row.try_get("id")?,
            name: row.try_get("name")?,
            ord: row.try_get("ord")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

pub async fn list(pool: &SqlitePool) -> Result<Vec<Profile>> {
    let rows = sqlx::query("SELECT * FROM profiles ORDER BY ord, id")
        .fetch_all(pool)
        .await?;
    rows.iter().map(Profile::from_row).collect()
}

pub async fn create(pool: &SqlitePool, name: &str, now: i64) -> Result<i64> {
    let ord = next_ord(pool).await?;
    let id: i64 = sqlx::query(
        "INSERT INTO profiles (name, ord, created_at)
         VALUES (?, ?, ?)
         RETURNING id",
    )
    .bind(name)
    .bind(ord)
    .bind(now)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

pub async fn rename(pool: &SqlitePool, id: i64, name: &str) -> Result<()> {
    let n = sqlx::query("UPDATE profiles SET name = ? WHERE id = ?")
        .bind(name)
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    ensure_found(n, id)
}

async fn next_ord(pool: &SqlitePool) -> Result<i64> {
    let max: Option<i64> = sqlx::query_scalar("SELECT MAX(ord) FROM profiles")
        .fetch_one(pool)
        .await?;
    Ok(max.unwrap_or(0) + 1)
}

fn ensure_found(rows_affected: u64, id: i64) -> Result<()> {
    if rows_affected == 0 {
        return Err(anyhow!("profile {id} not found"));
    }
    Ok(())
}
