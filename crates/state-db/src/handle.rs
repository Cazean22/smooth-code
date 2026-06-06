use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous},
};
use tokio::fs;

use crate::error::StateDbError;

const OPEN_STATUS: &str = "open";
const CLOSED_STATUS: &str = "closed";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadRow {
    pub thread_id: String,
    pub agent_path: Option<String>,
    pub agent_nickname: Option<String>,
    pub prompt_kind: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadSpawnEdgeRow {
    pub parent_thread_id: String,
    pub child_thread_id: String,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct StateDbHandle {
    pool: Arc<SqlitePool>,
}

impl StateDbHandle {
    pub async fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create state db directory {}", parent.display()))?;
        }

        let options = SqliteConnectOptions::new()
            .filename(&path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5));
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .map_err(StateDbError::from)?;
        let handle = Self {
            pool: Arc::new(pool),
        };
        handle.run_migrations().await?;
        Ok(handle)
    }

    pub async fn upsert_thread(
        &self,
        thread_id: &str,
        agent_path: Option<&str>,
        agent_nickname: Option<&str>,
        prompt_kind: Option<&str>,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO threads (thread_id, agent_path, agent_nickname, prompt_kind)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(thread_id) DO UPDATE SET
                agent_path = excluded.agent_path,
                agent_nickname = excluded.agent_nickname,
                prompt_kind = excluded.prompt_kind,
                updated_at = strftime('%s', 'now')
            "#,
        )
        .bind(thread_id)
        .bind(agent_path)
        .bind(agent_nickname)
        .bind(prompt_kind)
        .execute(self.pool.as_ref())
        .await
        .map_err(StateDbError::from)?;
        Ok(())
    }

    pub async fn get_thread(&self, thread_id: &str) -> Result<Option<ThreadRow>> {
        let row = sqlx::query(
            r#"
            SELECT thread_id, agent_path, agent_nickname, prompt_kind, created_at, updated_at
            FROM threads
            WHERE thread_id = ?1
            "#,
        )
        .bind(thread_id)
        .fetch_optional(self.pool.as_ref())
        .await
        .map_err(StateDbError::from)?;
        Ok(row.map(map_thread_row))
    }

    pub async fn upsert_thread_spawn_edge(
        &self,
        parent_thread_id: &str,
        child_thread_id: &str,
        status: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO thread_spawn_edges (parent_thread_id, child_thread_id, status)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(parent_thread_id, child_thread_id) DO UPDATE SET
                status = excluded.status,
                updated_at = strftime('%s', 'now')
            "#,
        )
        .bind(parent_thread_id)
        .bind(child_thread_id)
        .bind(status)
        .execute(self.pool.as_ref())
        .await
        .map_err(StateDbError::from)?;
        Ok(())
    }

    pub async fn set_thread_spawn_edge_status(
        &self,
        parent_thread_id: &str,
        child_thread_id: &str,
        status: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            UPDATE thread_spawn_edges
            SET status = ?3, updated_at = strftime('%s', 'now')
            WHERE parent_thread_id = ?1 AND child_thread_id = ?2
            "#,
        )
        .bind(parent_thread_id)
        .bind(child_thread_id)
        .bind(status)
        .execute(self.pool.as_ref())
        .await
        .map_err(StateDbError::from)?;
        Ok(())
    }

    pub async fn list_thread_spawn_children_with_status(
        &self,
        parent_thread_id: &str,
        status: &str,
    ) -> Result<Vec<ThreadSpawnEdgeRow>> {
        let rows = sqlx::query(
            r#"
            SELECT parent_thread_id, child_thread_id, status, created_at, updated_at
            FROM thread_spawn_edges
            WHERE parent_thread_id = ?1 AND status = ?2
            ORDER BY created_at ASC, child_thread_id ASC
            "#,
        )
        .bind(parent_thread_id)
        .bind(status)
        .fetch_all(self.pool.as_ref())
        .await
        .map_err(StateDbError::from)?;
        Ok(rows.into_iter().map(map_edge_row).collect())
    }

    pub async fn upsert_open_edge(
        &self,
        parent_thread_id: &str,
        child_thread_id: &str,
    ) -> Result<()> {
        self.upsert_thread_spawn_edge(parent_thread_id, child_thread_id, OPEN_STATUS)
            .await
    }

    pub async fn close_edge(&self, parent_thread_id: &str, child_thread_id: &str) -> Result<()> {
        self.set_thread_spawn_edge_status(parent_thread_id, child_thread_id, CLOSED_STATUS)
            .await
    }

    pub async fn list_open_children(
        &self,
        parent_thread_id: &str,
    ) -> Result<Vec<ThreadSpawnEdgeRow>> {
        self.list_thread_spawn_children_with_status(parent_thread_id, OPEN_STATUS)
            .await
    }

    async fn run_migrations(&self) -> Result<()> {
        sqlx::query(include_str!("../migrations/0001_initial.sql"))
            .execute(self.pool.as_ref())
            .await
            .map_err(StateDbError::from)?;
        self.ensure_prompt_kind_column().await?;
        Ok(())
    }

    async fn ensure_prompt_kind_column(&self) -> Result<()> {
        let rows = sqlx::query("PRAGMA table_info(threads)")
            .fetch_all(self.pool.as_ref())
            .await
            .map_err(StateDbError::from)?;
        let mut has_prompt_kind = false;
        let mut has_agent_role = false;
        for row in rows {
            let name: String = row.get("name");
            match name.as_str() {
                "prompt_kind" => has_prompt_kind = true,
                "agent_role" => has_agent_role = true,
                _ => {}
            }
        }
        if !has_prompt_kind {
            sqlx::query("ALTER TABLE threads ADD COLUMN prompt_kind TEXT NULL")
                .execute(self.pool.as_ref())
                .await
                .map_err(StateDbError::from)?;
        }
        if has_agent_role {
            sqlx::query(
                r#"
                UPDATE threads
                SET prompt_kind = CASE agent_role
                    WHEN 'explorer' THEN 'explore'
                    ELSE 'default_subagent'
                END
                WHERE prompt_kind IS NULL AND agent_path IS NOT NULL
                "#,
            )
            .execute(self.pool.as_ref())
            .await
            .map_err(StateDbError::from)?;
        }
        Ok(())
    }
}

fn map_thread_row(row: sqlx::sqlite::SqliteRow) -> ThreadRow {
    ThreadRow {
        thread_id: row.get("thread_id"),
        agent_path: row.get("agent_path"),
        agent_nickname: row.get("agent_nickname"),
        prompt_kind: row.get("prompt_kind"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn map_edge_row(row: sqlx::sqlite::SqliteRow) -> ThreadSpawnEdgeRow {
    ThreadSpawnEdgeRow {
        parent_thread_id: row.get("parent_thread_id"),
        child_thread_id: row.get("child_thread_id"),
        status: row.get("status"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}
