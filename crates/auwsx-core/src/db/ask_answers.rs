//! `ask_answers` typed row + CRUD. Project-level operator Q&A history.

use crate::Result;
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AskMode {
    Recall,
    Seek,
}

impl AskMode {
    pub fn as_str(self) -> &'static str {
        match self {
            AskMode::Recall => "recall",
            AskMode::Seek => "seek",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "recall" => AskMode::Recall,
            "seek" => AskMode::Seek,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskAnswer {
    pub id: i64,
    pub project_id: i64,
    pub mode: AskMode,
    pub question: String,
    pub answer: String,
    pub context_summary: Option<String>,
    pub log_path: Option<String>,
    pub created_at: i64,
}

impl AskAnswer {
    fn from_row(row: &SqliteRow) -> Result<Self> {
        let mode_raw: String = row.try_get("mode")?;
        Ok(Self {
            id: row.try_get("id")?,
            project_id: row.try_get("project_id")?,
            mode: AskMode::parse(&mode_raw)
                .ok_or_else(|| anyhow::anyhow!("unknown ask_answers.mode {mode_raw:?}"))?,
            question: row.try_get("question")?,
            answer: row.try_get("answer")?,
            context_summary: row.try_get("context_summary")?,
            log_path: row.try_get("log_path")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

pub struct NewAskAnswer<'a> {
    pub project_id: i64,
    pub mode: AskMode,
    pub question: &'a str,
    pub answer: &'a str,
    pub context_summary: Option<&'a str>,
    pub log_path: Option<&'a str>,
}

pub async fn create(pool: &SqlitePool, new: NewAskAnswer<'_>, created_at: i64) -> Result<i64> {
    let id: i64 = sqlx::query(
        "INSERT INTO ask_answers
            (project_id, mode, question, answer, context_summary, log_path, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         RETURNING id",
    )
    .bind(new.project_id)
    .bind(new.mode.as_str())
    .bind(new.question)
    .bind(new.answer)
    .bind(new.context_summary)
    .bind(new.log_path)
    .bind(created_at)
    .fetch_one(pool)
    .await?
    .get("id");
    Ok(id)
}

pub async fn list_by_project(
    pool: &SqlitePool,
    project_id: i64,
    limit: i64,
) -> Result<Vec<AskAnswer>> {
    let rows = sqlx::query(
        "SELECT * FROM ask_answers
         WHERE project_id = ?
         ORDER BY created_at DESC, id DESC
         LIMIT ?",
    )
    .bind(project_id)
    .bind(limit.max(0))
    .fetch_all(pool)
    .await?;
    rows.iter().map(AskAnswer::from_row).collect()
}
