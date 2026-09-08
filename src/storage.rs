use crate::provider::{Message, Usage};
use anyhow::{Context, Result};
use directories::BaseDirs;
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

pub struct Database {
    connection: Connection,
    path: PathBuf,
}

#[derive(Debug)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub provider: String,
    pub model: String,
    pub compaction_summary: Option<String>,
    pub summarized_upto: usize,
    pub undo_snapshot: Option<String>,
}

pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub message_count: i64,
    pub total_tokens: i64,
    pub updated_at: i64,
}

pub struct SearchHit {
    pub session_id: String,
    pub title: String,
    pub role: String,
    pub content: String,
    pub created_at: i64,
}

#[derive(Default)]
pub struct SessionStats {
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub cached_tokens: i64,
    pub last_input_tokens: Option<i64>,
    pub last_cached_tokens: Option<i64>,
}

/// Per-model token usage within a session, so switching models can be compared.
pub struct ModelStat {
    pub model: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub cached_tokens: i64,
}

pub struct MemoryEntry {
    pub content: String,
}

/// `strftime` groupings behind `/usage`'s two report levels, shared with the per-model token
/// sums that price them so the two can never disagree about what a period is.
const DAY_FORMAT: &str = "%Y-%m-%d";
const MONTH_FORMAT: &str = "%Y-%m";

/// Input and output tokens for one model, across *every* usage kind — a title generation costs
/// money like any other request. `model` is `None` for rows written before Kamui recorded one
/// (`user_version = 5`); such a row can never be priced.
pub struct ModelTokens {
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

/// Token usage aggregated over one calendar period (a day or a month), across every session.
/// `period` is already formatted for display in the user's local timezone.
pub struct UsagePeriod {
    pub period: String,
    pub request_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
    pub cached_tokens: i64,
}

#[derive(Debug)]
pub struct ScheduledJob {
    pub id: String,
    pub command: String,
    pub cwd: String,
    pub interval_secs: Option<i64>,
    pub next_run_at: Option<i64>,
    pub enabled: bool,
    pub status: String,
    pub last_exit_code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
    pub worker_id: Option<String>,
}

fn scheduled_job_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScheduledJob> {
    Ok(ScheduledJob {
        id: row.get(0)?,
        command: row.get(1)?,
        cwd: row.get(2)?,
        interval_secs: row.get(3)?,
        next_run_at: row.get(4)?,
        enabled: row.get(5)?,
        status: row.get(6)?,
        last_exit_code: row.get(7)?,
        stdout: row.get(8)?,
        stderr: row.get(9)?,
        worker_id: row.get(10)?,
    })
}

impl Database {
    pub fn open() -> Result<Self> {
        let data_dir = data_dir()?;
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create {}", data_dir.display()))?;
        let path = data_dir.join("kamui.db");
        let connection = Connection::open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        Self::initialize(connection, path)
    }

    fn initialize(connection: Connection, path: PathBuf) -> Result<Self> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 provider TEXT NOT NULL,
                 model TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                 updated_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE TABLE IF NOT EXISTS messages (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 role TEXT NOT NULL CHECK (role IN ('system', 'user', 'assistant')),
                 content TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE INDEX IF NOT EXISTS messages_session_id ON messages(session_id, id);
             CREATE TABLE IF NOT EXISTS usage_records (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 input_tokens INTEGER NOT NULL,
                 output_tokens INTEGER NOT NULL,
                 total_tokens INTEGER NOT NULL,
                 finish_reason TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT (unixepoch())
             );
             CREATE INDEX IF NOT EXISTS usage_session_id ON usage_records(session_id, id);",
        )?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < 1 {
            connection.execute_batch("PRAGMA user_version = 1;")?;
        }
        if version < 2 {
            connection.execute_batch(
                "ALTER TABLE usage_records
                 ADD COLUMN kind TEXT NOT NULL DEFAULT 'chat';
                 PRAGMA user_version = 2;",
            )?;
        }
        if version < 3 {
            // Rebuild messages to allow the 'tool' role and store tool-call metadata. SQLite cannot
            // alter a CHECK constraint in place, so the table is recreated and its rows copied.
            connection.execute_batch(
                "ALTER TABLE messages RENAME TO messages_pre_tools;
                 CREATE TABLE messages (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                     role TEXT NOT NULL CHECK (role IN ('system', 'user', 'assistant', 'tool')),
                     content TEXT NOT NULL,
                     tool_calls TEXT,
                     tool_call_id TEXT,
                     created_at INTEGER NOT NULL DEFAULT (unixepoch())
                 );
                 INSERT INTO messages (id, session_id, role, content, created_at)
                     SELECT id, session_id, role, content, created_at FROM messages_pre_tools;
                 DROP TABLE messages_pre_tools;
                 CREATE INDEX IF NOT EXISTS messages_session_id ON messages(session_id, id);
                 PRAGMA user_version = 3;",
            )?;
        }
        if version < 4 {
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS settings (
                     key TEXT PRIMARY KEY,
                     value TEXT NOT NULL
                 );
                 PRAGMA user_version = 4;",
            )?;
        }
        if version < 5 {
            connection.execute_batch(
                "ALTER TABLE usage_records ADD COLUMN model TEXT;
                 PRAGMA user_version = 5;",
            )?;
        }
        if version < 6 {
            // Global, permanent facts the model has been explicitly asked to remember. Not scoped
            // to a session or project — Kamui's database is already one global file (data_dir(),
            // not per-project), so a remembered fact is visible from any project the user opens
            // Kamui in afterward.
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS memory (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     content TEXT NOT NULL,
                     created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                     updated_at INTEGER NOT NULL DEFAULT (unixepoch())
                 );
                 PRAGMA user_version = 6;",
            )?;
        }
        if version < 7 {
            // `/index`'s semantic-search store: one row per chunk of an indexed file, with its
            // embedding vector (little-endian f32s) and the file's content hash so re-indexing can
            // skip unchanged files. Unlike `memory`, this is project-scoped in spirit but lives in
            // the same global database as everything else (Kamui has one DB file, not one per
            // project) — `path` is project-relative, so chunks from different projects indexed into
            // the same database would collide; this is an accepted v1 limitation (see CLAUDE.md).
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS indexed_files (
                     path TEXT PRIMARY KEY,
                     hash TEXT NOT NULL,
                     indexed_at INTEGER NOT NULL DEFAULT (unixepoch())
                 );
                 CREATE TABLE IF NOT EXISTS code_chunks (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     path TEXT NOT NULL,
                     start_line INTEGER NOT NULL,
                     end_line INTEGER NOT NULL,
                     content TEXT NOT NULL,
                     embedding BLOB NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS code_chunks_path ON code_chunks(path);
                 PRAGMA user_version = 7;",
            )?;
        }
        if version < 8 {
            // Scope the semantic-search store to one project, lifting the v7 limitation above:
            // `path` is project-relative, so indexing two projects into this one global database
            // made their identically-named files collide. Existing rows record no project, so they
            // cannot be attributed to one and are dropped rather than silently mixed — the index is
            // a regenerable cache, and `/index` rebuilds it. `indexed_files` is recreated rather
            // than altered because its primary key becomes (project, path), which SQLite cannot
            // change in place.
            connection.execute_batch(
                "DROP TABLE IF EXISTS code_chunks;
                 DROP TABLE IF EXISTS indexed_files;
                 CREATE TABLE indexed_files (
                     project TEXT NOT NULL,
                     path TEXT NOT NULL,
                     hash TEXT NOT NULL,
                     indexed_at INTEGER NOT NULL DEFAULT (unixepoch()),
                     PRIMARY KEY (project, path)
                 );
                 CREATE TABLE code_chunks (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     project TEXT NOT NULL,
                     path TEXT NOT NULL,
                     start_line INTEGER NOT NULL,
                     end_line INTEGER NOT NULL,
                     content TEXT NOT NULL,
                     embedding BLOB NOT NULL
                 );
                 CREATE INDEX code_chunks_project_path ON code_chunks(project, path);
                 PRAGMA user_version = 8;",
            )?;
        }
        if version < 9 {
            connection.execute_batch(
                "CREATE TABLE scheduled_jobs (
                     id TEXT PRIMARY KEY,
                     command TEXT NOT NULL,
                     cwd TEXT NOT NULL,
                     interval_secs INTEGER,
                     next_run_at INTEGER,
                     enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
                     status TEXT NOT NULL DEFAULT 'scheduled' CHECK (
                         status IN ('scheduled', 'paused', 'running', 'succeeded', 'failed', 'cancelled', 'interrupted')
                     ),
                     last_started_at INTEGER,
                     last_finished_at INTEGER,
                     last_exit_code INTEGER,
                     stdout TEXT NOT NULL DEFAULT '',
                     stderr TEXT NOT NULL DEFAULT '',
                     worker_id TEXT,
                     lease_expires_at INTEGER,
                     created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                     updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
                     CHECK (interval_secs IS NULL OR interval_secs >= 60)
                 );
                 CREATE INDEX scheduled_jobs_due
                     ON scheduled_jobs(enabled, status, next_run_at);
                 PRAGMA user_version = 9;",
            )?;
        }
        if version < 10 {
            connection.execute_batch(
                "ALTER TABLE indexed_files ADD COLUMN embedding_model TEXT;
                 ALTER TABLE code_chunks ADD COLUMN lsh_bucket INTEGER;
                 CREATE INDEX code_chunks_project_lsh ON code_chunks(project, lsh_bucket);
                 CREATE VIRTUAL TABLE code_chunks_fts USING fts5(
                     project UNINDEXED, path, content
                 );
                 INSERT INTO code_chunks_fts(rowid, project, path, content)
                     SELECT id, project, path, content FROM code_chunks;
                 PRAGMA user_version = 10;",
            )?;
        }
        if version < 11 {
            connection.execute_batch(
                "ALTER TABLE usage_records ADD COLUMN cached_tokens INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE sessions ADD COLUMN plan_json TEXT;
                 ALTER TABLE sessions ADD COLUMN plan_status TEXT CHECK (plan_status IN ('pending', 'approved'));
                 PRAGMA user_version = 11;",
            )?;
        }
        if version < 12 {
            connection.execute_batch(
                "ALTER TABLE sessions ADD COLUMN compaction_summary TEXT;
                 ALTER TABLE sessions ADD COLUMN summarized_upto INTEGER NOT NULL DEFAULT 0;
                 PRAGMA user_version = 12;",
            )?;
        }
        if version < 13 {
            connection.execute_batch(
                "ALTER TABLE sessions ADD COLUMN undo_snapshot TEXT;
                 PRAGMA user_version = 13;",
            )?;
        }
        if version < 14 {
            connection.execute_batch(
                "CREATE TABLE tool_executions (
                     id TEXT PRIMARY KEY,
                     session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                     tool_call_id TEXT NOT NULL,
                     tool_name TEXT NOT NULL,
                     arguments TEXT NOT NULL,
                     status TEXT NOT NULL CHECK (status IN ('running', 'completed', 'error', 'interrupted')),
                     output TEXT,
                     started_at INTEGER NOT NULL DEFAULT (unixepoch()),
                     finished_at INTEGER
                 );
                 CREATE INDEX tool_executions_session_id ON tool_executions(session_id, started_at);
                 PRAGMA user_version = 14;",
            )?;
        }
        connection.execute(
            "UPDATE tool_executions SET status = 'interrupted', finished_at = unixepoch()
             WHERE status = 'running'",
            [],
        )?;
        Ok(Self { connection, path })
    }

    #[cfg(test)]
    pub fn open_in_memory_for_tests() -> Self {
        Self::initialize(
            Connection::open_in_memory().unwrap(),
            PathBuf::from(":memory:"),
        )
        .unwrap()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run SQLite's fast integrity check for `kamui doctor` and return its diagnostic text.
    pub fn integrity_check(&self) -> Result<String> {
        self.connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .map_err(Into::into)
    }

    pub fn schema_version(&self) -> Result<i64> {
        self.connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(Into::into)
    }

    pub fn create_scheduled_job(
        &self,
        command: &str,
        cwd: &str,
        next_run_at: i64,
        interval_secs: Option<i64>,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string()[..8].to_string();
        self.connection.execute(
            "INSERT INTO scheduled_jobs
                 (id, command, cwd, interval_secs, next_run_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, command, cwd, interval_secs, next_run_at],
        )?;
        Ok(id)
    }

    pub fn list_scheduled_jobs(&self) -> Result<Vec<ScheduledJob>> {
        let mut statement = self.connection.prepare(
            "SELECT id, command, cwd, interval_secs, next_run_at, enabled, status,
                    last_exit_code, stdout, stderr, worker_id
             FROM scheduled_jobs
             ORDER BY COALESCE(next_run_at, 9223372036854775807), created_at",
        )?;
        let rows = statement.query_map([], scheduled_job_from_row)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Atomically claim one due job. SQLite's `UPDATE ... RETURNING` keeps two worker processes
    /// from receiving the same command.
    pub fn claim_due_job(
        &self,
        now: i64,
        worker_id: &str,
        lease_expires_at: i64,
    ) -> Result<Option<ScheduledJob>> {
        self.connection
            .query_row(
                "UPDATE scheduled_jobs
                 SET status = 'running', last_started_at = ?1, updated_at = ?1,
                     worker_id = ?2, lease_expires_at = ?3
                 WHERE id = (
                     SELECT id FROM scheduled_jobs
                     WHERE enabled = 1 AND status = 'scheduled' AND next_run_at <= ?1
                     ORDER BY next_run_at, created_at LIMIT 1
                 )
                 RETURNING id, command, cwd, interval_secs, next_run_at, enabled, status,
                           last_exit_code, stdout, stderr, worker_id",
                params![now, worker_id, lease_expires_at],
                scheduled_job_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn finish_scheduled_job(
        &self,
        job: &ScheduledJob,
        exit_code: i32,
        stdout: &str,
        stderr: &str,
        finished_at: i64,
    ) -> Result<()> {
        let (enabled, status, next_run_at) = match job.interval_secs {
            Some(interval) => {
                let mut next = job.next_run_at.unwrap_or(finished_at) + interval;
                while next <= finished_at {
                    next += interval;
                }
                (1, "scheduled", Some(next))
            }
            None => (
                0,
                if exit_code == 0 {
                    "succeeded"
                } else {
                    "failed"
                },
                None,
            ),
        };
        let updated = self.connection.execute(
            "UPDATE scheduled_jobs
             SET enabled = ?2, status = ?3, next_run_at = ?4, last_finished_at = ?5,
                 last_exit_code = ?6, stdout = ?7, stderr = ?8, updated_at = ?5,
                 worker_id = NULL, lease_expires_at = NULL
             WHERE id = ?1 AND worker_id = ?9",
            params![
                job.id,
                enabled,
                status,
                next_run_at,
                finished_at,
                exit_code,
                stdout,
                stderr,
                job.worker_id
            ],
        )?;
        if updated == 0 {
            anyhow::bail!(
                "scheduled job '{}' is no longer owned by this worker",
                job.id
            );
        }
        Ok(())
    }

    pub fn interrupt_scheduled_job(&self, id: &str, stderr: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE scheduled_jobs
             SET enabled = 0, status = 'interrupted', stderr = ?2,
                 last_finished_at = unixepoch(), updated_at = unixepoch(),
                 worker_id = NULL, lease_expires_at = NULL
             WHERE id = ?1 AND status = 'running'",
            params![id, stderr],
        )?;
        Ok(())
    }

    pub fn recover_expired_jobs(&self, now: i64) -> Result<usize> {
        self.connection
            .execute(
                "UPDATE scheduled_jobs
                 SET enabled = 0, status = 'interrupted',
                     stderr = CASE WHEN stderr = '' THEN 'worker stopped before completion' ELSE stderr END,
                     last_finished_at = unixepoch(), updated_at = unixepoch(),
                     worker_id = NULL, lease_expires_at = NULL
                 WHERE status = 'running' AND lease_expires_at <= ?1",
                [now],
            )
            .map_err(Into::into)
    }

    pub fn cancel_scheduled_job(&self, id: &str) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE scheduled_jobs
             SET enabled = 0, status = 'cancelled', updated_at = unixepoch()
             WHERE id = ?1 AND status != 'running'",
            [id],
        )? > 0)
    }

    pub fn pause_scheduled_job(&self, id: &str) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE scheduled_jobs
             SET enabled = 0, status = 'paused', updated_at = unixepoch()
             WHERE id = ?1 AND status = 'scheduled'",
            [id],
        )? > 0)
    }

    pub fn resume_scheduled_job(&self, id: &str, now: i64) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE scheduled_jobs
             SET enabled = 1, status = 'scheduled',
                 next_run_at = CASE WHEN next_run_at < ?2 THEN ?2 ELSE next_run_at END,
                 updated_at = unixepoch()
             WHERE id = ?1 AND status = 'paused'",
            params![id, now],
        )? > 0)
    }

    /// Every stored memory entry, oldest first (the order they were learned in).
    pub fn list_memory(&self) -> Result<Vec<MemoryEntry>> {
        let mut statement = self
            .connection
            .prepare("SELECT content FROM memory ORDER BY id")?;
        let rows = statement.query_map([], |row| {
            Ok(MemoryEntry {
                content: row.get(0)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Total bytes across all memory content, used to enforce a cap before adding more (see
    /// `tools::remember`); cheaper than loading every row just to sum lengths.
    pub fn total_memory_bytes(&self) -> Result<i64> {
        self.connection
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(content)), 0) FROM memory",
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Append a new memory entry.
    pub fn remember(&self, content: &str) -> Result<()> {
        self.connection
            .execute("INSERT INTO memory (content) VALUES (?1)", params![content])?;
        Ok(())
    }

    /// Replace the content of an unambiguous entry matched by a case-insensitive substring, so a
    /// superseded fact (e.g. an old preference) can be corrected in place instead of left to
    /// contradict a newer one. Returns `Ok(false)` for no match or an ambiguous (multi-match)
    /// substring, so the caller can ask for something more specific rather than guess.
    pub fn update_memory(&self, substring: &str, new_content: &str) -> Result<bool> {
        let pattern = format!("%{}%", substring.replace('%', "\\%").replace('_', "\\_"));
        let matches: Vec<i64> = self
            .connection
            .prepare("SELECT id FROM memory WHERE content LIKE ?1 ESCAPE '\\'")?
            .query_map([&pattern], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if matches.len() != 1 {
            return Ok(false);
        }
        self.connection.execute(
            "UPDATE memory SET content = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![matches[0], new_content],
        )?;
        Ok(true)
    }

    /// Delete an unambiguous entry matched by a case-insensitive substring. Same ambiguity
    /// handling as `update_memory`.
    pub fn forget(&self, substring: &str) -> Result<bool> {
        let pattern = format!("%{}%", substring.replace('%', "\\%").replace('_', "\\_"));
        let matches: Vec<i64> = self
            .connection
            .prepare("SELECT id FROM memory WHERE content LIKE ?1 ESCAPE '\\'")?
            .query_map([&pattern], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if matches.len() != 1 {
            return Ok(false);
        }
        self.connection
            .execute("DELETE FROM memory WHERE id = ?1", [matches[0]])?;
        Ok(true)
    }

    /// Delete every memory entry.
    pub fn clear_memory(&self) -> Result<usize> {
        Ok(self.connection.execute("DELETE FROM memory", [])?)
    }

    pub fn create_session(&self, provider: &str, model: &str) -> Result<Session> {
        let session = Session {
            id: Uuid::new_v4().to_string(),
            title: "New chat".to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            compaction_summary: None,
            summarized_upto: 0,
            undo_snapshot: None,
        };
        self.connection.execute(
            "INSERT INTO sessions (id, title, provider, model) VALUES (?1, ?2, ?3, ?4)",
            params![session.id, session.title, session.provider, session.model],
        )?;
        Ok(session)
    }

    pub fn find_session(&self, id_prefix: &str) -> Result<Option<Session>> {
        let pattern = format!("{id_prefix}%");
        let mut statement = self.connection.prepare(
            "SELECT id, title, provider, model, compaction_summary, summarized_upto, undo_snapshot FROM sessions
             WHERE id LIKE ?1 ORDER BY updated_at DESC LIMIT 2",
        )?;
        let sessions = statement
            .query_map([pattern], session_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(if sessions.len() == 1 {
            sessions.into_iter().next()
        } else {
            None
        })
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionSummary>> {
        let mut statement = self.connection.prepare(
            "SELECT s.id, s.title,
                    (SELECT COUNT(*) FROM messages WHERE session_id = s.id),
                    (SELECT COALESCE(SUM(total_tokens), 0)
                     FROM usage_records WHERE session_id = s.id),
                    s.updated_at
             FROM sessions s
             WHERE EXISTS (SELECT 1 FROM messages WHERE session_id = s.id)
             ORDER BY s.updated_at DESC, s.rowid DESC",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(SessionSummary {
                id: row.get(0)?,
                title: row.get(1)?,
                message_count: row.get(2)?,
                total_tokens: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn load_messages(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut statement = self.connection.prepare(
            "SELECT role, content, tool_calls, tool_call_id
             FROM messages WHERE session_id = ?1 ORDER BY id",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.map(|row| {
            let (role, content, tool_calls, tool_call_id) = row?;
            let mut message = Message::from_parts(&role, content)?;
            if let Some(json) = tool_calls {
                message.tool_calls =
                    serde_json::from_str(&json).context("failed to parse stored tool calls")?;
            }
            message.tool_call_id = tool_call_id;
            Ok(message)
        })
        .collect()
    }

    pub fn set_compaction_checkpoint(
        &self,
        session_id: &str,
        summary: &str,
        summarized_upto: usize,
    ) -> Result<()> {
        self.connection.execute(
            "UPDATE sessions
             SET compaction_summary = ?2, summarized_upto = ?3, updated_at = unixepoch()
             WHERE id = ?1",
            params![session_id, summary, summarized_upto as i64],
        )?;
        Ok(())
    }

    /// Persist a full turn: every message it produced plus one usage record, atomically. A turn is
    /// usually a user prompt and an assistant answer, but may also include the assistant's tool
    /// requests and the tool results in between.
    pub fn save_turn(
        &self,
        session_id: &str,
        messages: &[Message],
        usage: &Usage,
        model: &str,
        finish_reason: &str,
    ) -> Result<()> {
        self.save_turn_with_undo(session_id, messages, usage, model, finish_reason, None)
    }

    pub fn save_turn_with_undo(
        &self,
        session_id: &str,
        messages: &[Message],
        usage: &Usage,
        model: &str,
        finish_reason: &str,
        undo_snapshot: Option<&str>,
    ) -> Result<()> {
        let input_tokens =
            i64::try_from(usage.prompt_tokens).context("input token count overflow")?;
        let output_tokens =
            i64::try_from(usage.completion_tokens).context("output token count overflow")?;
        let total_tokens =
            i64::try_from(usage.total_tokens).context("total token count overflow")?;
        let cached_tokens =
            i64::try_from(usage.cached_tokens).context("cached token count overflow")?;
        let transaction = self.connection.unchecked_transaction()?;
        for message in messages {
            let tool_calls = if message.tool_calls.is_empty() {
                None
            } else {
                Some(
                    serde_json::to_string(&message.tool_calls)
                        .context("failed to serialize tool calls")?,
                )
            };
            transaction.execute(
                "INSERT INTO messages (session_id, role, content, tool_calls, tool_call_id)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session_id,
                    message.role_name(),
                    message.content,
                    tool_calls,
                    message.tool_call_id
                ],
            )?;
        }
        transaction.execute(
            "INSERT INTO usage_records
             (session_id, input_tokens, output_tokens, total_tokens, cached_tokens, finish_reason, kind, model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'chat', ?7)",
            params![
                session_id,
                input_tokens,
                output_tokens,
                total_tokens,
                cached_tokens,
                finish_reason,
                model
            ],
        )?;
        let title_source = messages
            .iter()
            .find(|message| message.role_name() == "user")
            .map(|message| message.content.as_str())
            .unwrap_or_default();
        transaction.execute(
            "UPDATE sessions SET
                 title = CASE WHEN title = 'New chat' THEN ?2 ELSE title END,
                 undo_snapshot = ?3,
                 updated_at = unixepoch()
             WHERE id = ?1",
            params![session_id, make_title(title_source), undo_snapshot],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn clear_undo_snapshot(&self, session_id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE sessions SET undo_snapshot = NULL, updated_at = unixepoch() WHERE id = ?1",
            [session_id],
        )?;
        Ok(())
    }

    pub fn start_tool_execution(
        &self,
        session_id: &str,
        tool_call_id: &str,
        tool_name: &str,
        arguments: &str,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        self.connection.execute(
            "INSERT INTO tool_executions
                 (id, session_id, tool_call_id, tool_name, arguments, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running')",
            params![id, session_id, tool_call_id, tool_name, arguments],
        )?;
        Ok(id)
    }

    pub fn finish_tool_execution(&self, id: &str, output: &str) -> Result<()> {
        let status = if output.starts_with("Error: ") {
            "error"
        } else {
            "completed"
        };
        self.connection.execute(
            "UPDATE tool_executions
             SET status = ?2, output = ?3, finished_at = unixepoch()
             WHERE id = ?1 AND status = 'running'",
            params![id, status, output],
        )?;
        Ok(())
    }

    pub fn interrupted_tool_executions(&self, session_id: &str) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT tool_name FROM tool_executions
             WHERE session_id = ?1 AND status = 'interrupted'
             ORDER BY started_at, rowid",
        )?;
        let rows = statement.query_map([session_id], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn save_generated_title(
        &self,
        session_id: &str,
        title: &str,
        usage: &Usage,
        model: &str,
        finish_reason: &str,
    ) -> Result<()> {
        let input_tokens =
            i64::try_from(usage.prompt_tokens).context("input token count overflow")?;
        let output_tokens =
            i64::try_from(usage.completion_tokens).context("output token count overflow")?;
        let total_tokens =
            i64::try_from(usage.total_tokens).context("total token count overflow")?;
        let cached_tokens =
            i64::try_from(usage.cached_tokens).context("cached token count overflow")?;
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "UPDATE sessions SET title = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![session_id, title],
        )?;
        transaction.execute(
            "INSERT INTO usage_records
             (session_id, input_tokens, output_tokens, total_tokens, cached_tokens, finish_reason, kind, model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'title', ?7)",
            params![
                session_id,
                input_tokens,
                output_tokens,
                total_tokens,
                cached_tokens,
                finish_reason,
                model
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn session_stats(&self, session_id: &str) -> Result<SessionStats> {
        let mut stats = self.connection.query_row(
            "SELECT COUNT(*) FILTER (WHERE kind = 'chat'), COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0), COALESCE(SUM(total_tokens), 0),
                    COALESCE(SUM(cached_tokens), 0)
             FROM usage_records WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok(SessionStats {
                    request_count: row.get(0)?,
                    input_tokens: row.get(1)?,
                    output_tokens: row.get(2)?,
                    total_tokens: row.get(3)?,
                    cached_tokens: row.get(4)?,
                    last_input_tokens: None,
                    last_cached_tokens: None,
                })
            },
        )?;
        stats.last_input_tokens = self
            .connection
            .query_row(
                "SELECT input_tokens FROM usage_records
                 WHERE session_id = ?1 AND kind = 'chat' ORDER BY id DESC LIMIT 1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        stats.last_cached_tokens = self
            .connection
            .query_row(
                "SELECT cached_tokens FROM usage_records
                 WHERE session_id = ?1 AND kind = 'chat' ORDER BY id DESC LIMIT 1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stats)
    }

    /// Every chat request of one session as `(prompt_tokens, cached_tokens)`, oldest first, for the
    /// prompt-cache report (`crate::cache::report`). Title generation and compaction are left out on
    /// purpose: they are separate requests with their own prefixes, so counting them would misreport
    /// how the conversation itself is landing against the cache.
    pub fn cache_samples(&self, session_id: &str) -> Result<Vec<(i64, i64)>> {
        let mut statement = self.connection.prepare(
            "SELECT input_tokens, cached_tokens FROM usage_records
             WHERE session_id = ?1 AND kind = 'chat' ORDER BY id",
        )?;
        let rows = statement
            .query_map([session_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn rename_session(&self, session_id: &str, title: &str) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE sessions SET title = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![session_id, title],
        )?;
        if changed == 0 {
            anyhow::bail!("session '{session_id}' was not found");
        }
        Ok(())
    }

    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        let pattern = format!("%{}%", escape_like(query));
        let mut statement = self.connection.prepare(
            "SELECT m.session_id, s.title, m.role, m.content, m.created_at
             FROM messages m JOIN sessions s ON s.id = m.session_id
             WHERE m.content LIKE ?1 ESCAPE '\\'
             ORDER BY m.created_at DESC, m.id DESC
             LIMIT ?2",
        )?;
        let hits = statement.query_map(params![pattern, limit as i64], |row| {
            Ok(SearchHit {
                session_id: row.get(0)?,
                title: row.get(1)?,
                role: row.get(2)?,
                content: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;
        hits.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Chat token usage grouped by model for one session, most-used first.
    pub fn model_stats(&self, session_id: &str) -> Result<Vec<ModelStat>> {
        let mut statement = self.connection.prepare(
            "SELECT COALESCE(model, '(unknown)'), COUNT(*),
                    COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(total_tokens), 0), COALESCE(SUM(cached_tokens), 0)
             FROM usage_records
             WHERE session_id = ?1 AND kind = 'chat'
             GROUP BY model
             ORDER BY SUM(total_tokens) DESC",
        )?;
        let rows = statement.query_map([session_id], |row| {
            Ok(ModelStat {
                model: row.get(0)?,
                request_count: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                total_tokens: row.get(4)?,
                cached_tokens: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn delete_session(&self, session_id: &str) -> Result<()> {
        self.connection
            .execute("DELETE FROM sessions WHERE id = ?1", [session_id])?;
        Ok(())
    }

    /// Plan Mode gate (ticket #9): pending/approved plan JSON stored per session.
    /// `plan_json` holds the raw `update_plan` arguments (e.g. `{"plan":[...]}`),
    /// `plan_status` is `pending` until the user approves with `y`.
    pub fn get_plan(&self, session_id: &str) -> Result<Option<(String, String)>> {
        self.connection
            .query_row(
                "SELECT plan_json, plan_status FROM sessions WHERE id = ?1",
                [session_id],
                |row| {
                    let json: Option<String> = row.get(0)?;
                    let status: Option<String> = row.get(1)?;
                    Ok(match (json, status) {
                        (Some(j), Some(s)) => Some((j, s)),
                        _ => None,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn set_plan(&self, session_id: &str, plan_json: &str, status: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE sessions SET plan_json = ?2, plan_status = ?3, updated_at = unixepoch() WHERE id = ?1",
            params![session_id, plan_json, status],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn clear_plan(&self, session_id: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE sessions SET plan_json = NULL, plan_status = NULL, updated_at = unixepoch() WHERE id = ?1",
            [session_id],
        )?;
        Ok(())
    }

    /// Token usage per day across every session, most recent first. Like `session_stats`, the
    /// request count covers only primary chat requests while the token sums include every kind
    /// (notably `title`), so the totals reflect what was actually spent.
    pub fn usage_by_day(&self, limit: usize) -> Result<Vec<UsagePeriod>> {
        self.usage_by_period(DAY_FORMAT, limit)
    }

    /// Token usage per calendar month across every session, most recent first.
    pub fn usage_by_month(&self, limit: usize) -> Result<Vec<UsagePeriod>> {
        self.usage_by_period(MONTH_FORMAT, limit)
    }

    /// Group `usage_records` by a `strftime` format applied to `created_at`. Periods are computed
    /// in local time so a report lines up with the timestamps `/sessions` already displays.
    fn usage_by_period(&self, format: &str, limit: usize) -> Result<Vec<UsagePeriod>> {
        let mut statement = self.connection.prepare(
            "SELECT strftime(?1, created_at, 'unixepoch', 'localtime') AS period,
                    COUNT(*) FILTER (WHERE kind = 'chat'),
                    COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(total_tokens), 0), COALESCE(SUM(cached_tokens), 0)
             FROM usage_records
             GROUP BY period
             ORDER BY period DESC
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![format, limit as i64], |row| {
            Ok(UsagePeriod {
                period: row.get(0)?,
                request_count: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                total_tokens: row.get(4)?,
                cached_tokens: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Lifetime totals across every session, for the footer of a usage report.
    pub fn usage_total(&self) -> Result<UsagePeriod> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FILTER (WHERE kind = 'chat'),
                        COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0),
                        COALESCE(SUM(total_tokens), 0), COALESCE(SUM(cached_tokens), 0)
                 FROM usage_records",
                [],
                |row| {
                    Ok(UsagePeriod {
                        period: "all time".to_string(),
                        request_count: row.get(0)?,
                        input_tokens: row.get(1)?,
                        output_tokens: row.get(2)?,
                        total_tokens: row.get(3)?,
                        cached_tokens: row.get(4)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    /// Per-model token totals for one session, for optional cost reporting. Unlike `model_stats`,
    /// which powers the per-model *request* breakdown, this counts every usage kind, matching the
    /// convention that token sums include title generation while request counts do not.
    pub fn session_model_tokens(&self, session_id: &str) -> Result<Vec<ModelTokens>> {
        let mut statement = self.connection.prepare(
            "SELECT model, COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
             FROM usage_records WHERE session_id = ?1 GROUP BY model",
        )?;
        let rows = statement.query_map([session_id], model_tokens)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Per-model token totals across every session, to price a usage report's lifetime row.
    pub fn usage_model_tokens(&self) -> Result<Vec<ModelTokens>> {
        let mut statement = self.connection.prepare(
            "SELECT model, COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
             FROM usage_records GROUP BY model",
        )?;
        let rows = statement.query_map([], model_tokens)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Per-model token totals per day, keyed by the same period string `usage_by_day` produces so
    /// a report can look each of its rows up.
    pub fn usage_model_tokens_by_day(&self) -> Result<HashMap<String, Vec<ModelTokens>>> {
        self.usage_model_tokens_by_period(DAY_FORMAT)
    }

    /// Per-model token totals per calendar month, keyed like `usage_by_month`'s periods.
    pub fn usage_model_tokens_by_month(&self) -> Result<HashMap<String, Vec<ModelTokens>>> {
        self.usage_model_tokens_by_period(MONTH_FORMAT)
    }

    /// Group tokens by period and model in one scan. Deliberately unlimited, unlike
    /// `usage_by_period`: a caller only looks up the periods it is already showing, and one
    /// grouped scan is cheaper than a windowed query repeated per row.
    fn usage_model_tokens_by_period(
        &self,
        format: &str,
    ) -> Result<HashMap<String, Vec<ModelTokens>>> {
        let mut statement = self.connection.prepare(
            "SELECT strftime(?1, created_at, 'unixepoch', 'localtime') AS period, model,
                    COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0)
             FROM usage_records
             GROUP BY period, model",
        )?;
        let rows = statement.query_map(params![format], |row| {
            Ok((
                row.get::<_, String>(0)?,
                ModelTokens {
                    model: row.get(1)?,
                    input_tokens: row.get(2)?,
                    output_tokens: row.get(3)?,
                },
            ))
        })?;
        let mut grouped: HashMap<String, Vec<ModelTokens>> = HashMap::new();
        for row in rows {
            let (period, tokens) = row?;
            grouped.entry(period).or_default().push(tokens);
        }
        Ok(grouped)
    }

    /// A durable key-value store for small UI state, such as the active provider profile.
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        self.connection
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(Into::into)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// The content hash `/index` stored for a project-relative path last time it was indexed, or
    /// `None` if the path has never been indexed in this project. Compared against the file's
    /// current hash to skip re-embedding unchanged files.
    pub fn indexed_file_hash(&self, project: &str, path: &str) -> Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT hash FROM indexed_files WHERE project = ?1 AND path = ?2",
                params![project, path],
                |row| row.get(0),
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn indexed_file_is_current(
        &self,
        project: &str,
        path: &str,
        hash: &str,
        embedding_model: &str,
    ) -> Result<bool> {
        self.connection
            .query_row(
                "SELECT hash = ?3 AND embedding_model = ?4
                 FROM indexed_files WHERE project = ?1 AND path = ?2",
                params![project, path, hash, embedding_model],
                |row| row.get(0),
            )
            .optional()
            .map(|matched| matched.unwrap_or(false))
            .map_err(Into::into)
    }

    /// Record (or update) the hash `/index` last saw for a path in this project.
    #[cfg(test)]
    pub fn set_indexed_file(&self, project: &str, path: &str, hash: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO indexed_files (project, path, hash, indexed_at)
             VALUES (?1, ?2, ?3, unixepoch())
             ON CONFLICT(project, path)
                 DO UPDATE SET hash = excluded.hash, indexed_at = excluded.indexed_at",
            params![project, path, hash],
        )?;
        Ok(())
    }

    /// Every file currently indexed for a project, so `/index` can tell which ones no longer exist
    /// on disk and the startup staleness check can compare each one against its file's mtime.
    pub fn indexed_files(&self, project: &str) -> Result<Vec<IndexedFile>> {
        let mut statement = self
            .connection
            .prepare("SELECT path, indexed_at FROM indexed_files WHERE project = ?1")?;
        let rows = statement.query_map([project], |row| {
            Ok(IndexedFile {
                path: row.get(0)?,
                indexed_at: row.get(1)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn delete_indexed_file(&self, project: &str, path: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM indexed_files WHERE project = ?1 AND path = ?2",
            params![project, path],
        )?;
        Ok(())
    }

    /// Drop every chunk previously indexed for a path, before re-chunking it (or because it no
    /// longer exists).
    pub fn delete_chunks_for_path(&self, project: &str, path: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM code_chunks_fts WHERE rowid IN (
                 SELECT id FROM code_chunks WHERE project = ?1 AND path = ?2
             )",
            params![project, path],
        )?;
        self.connection.execute(
            "DELETE FROM code_chunks WHERE project = ?1 AND path = ?2",
            params![project, path],
        )?;
        Ok(())
    }

    #[cfg(test)]
    pub fn insert_chunk(
        &self,
        project: &str,
        path: &str,
        start_line: usize,
        end_line: usize,
        content: &str,
        embedding: &[f32],
    ) -> Result<()> {
        self.connection.execute(
            "INSERT INTO code_chunks
                 (project, path, start_line, end_line, content, embedding, lsh_bucket)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                project,
                path,
                start_line as i64,
                end_line as i64,
                content,
                encode_embedding(embedding),
                embedding_signature(embedding)
            ],
        )?;
        let id = self.connection.last_insert_rowid();
        self.connection.execute(
            "INSERT INTO code_chunks_fts(rowid, project, path, content)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, project, path, content],
        )?;
        Ok(())
    }

    /// Atomically replace one file's complete index after all embeddings have been prepared.
    /// A provider failure therefore leaves the old searchable rows untouched.
    pub fn replace_file_index(
        &self,
        project: &str,
        path: &str,
        hash: &str,
        embedding_model: &str,
        chunks: &[NewCodeChunk],
    ) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute(
            "DELETE FROM code_chunks_fts WHERE rowid IN (
                 SELECT id FROM code_chunks WHERE project = ?1 AND path = ?2
             )",
            params![project, path],
        )?;
        transaction.execute(
            "DELETE FROM code_chunks WHERE project = ?1 AND path = ?2",
            params![project, path],
        )?;
        for chunk in chunks {
            transaction.execute(
                "INSERT INTO code_chunks
                     (project, path, start_line, end_line, content, embedding, lsh_bucket)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    project,
                    path,
                    chunk.start_line as i64,
                    chunk.end_line as i64,
                    chunk.content,
                    encode_embedding(&chunk.embedding),
                    embedding_signature(&chunk.embedding)
                ],
            )?;
            let id = transaction.last_insert_rowid();
            transaction.execute(
                "INSERT INTO code_chunks_fts(rowid, project, path, content)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, project, path, chunk.content],
            )?;
        }
        transaction.execute(
            "INSERT INTO indexed_files (project, path, hash, indexed_at, embedding_model)
             VALUES (?1, ?2, ?3, unixepoch(), ?4)
             ON CONFLICT(project, path) DO UPDATE SET
                 hash = excluded.hash,
                 indexed_at = excluded.indexed_at,
                 embedding_model = excluded.embedding_model",
            params![project, path, hash, embedding_model],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// The number of chunks currently indexed for a project, for a quick `/index`/status summary
    /// without loading every embedding into memory.
    pub fn chunk_count(&self, project: &str) -> Result<i64> {
        self.connection
            .query_row(
                "SELECT COUNT(*) FROM code_chunks WHERE project = ?1",
                [project],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Every indexed chunk in a project, for `search_code` to score against a query embedding.
    /// Loaded in full (no vector index) — a brute-force scan is simple and fast enough at the scale
    /// a single project's chunks reach; see CLAUDE.md for the tradeoff.
    pub fn all_chunks(&self, project: &str) -> Result<Vec<CodeChunk>> {
        let mut statement = self.connection.prepare(
            "SELECT path, start_line, end_line, content, embedding FROM code_chunks
             WHERE project = ?1",
        )?;
        let rows = statement.query_map([project], |row| {
            let start_line: i64 = row.get(1)?;
            let end_line: i64 = row.get(2)?;
            let embedding: Vec<u8> = row.get(4)?;
            Ok(CodeChunk {
                path: row.get(0)?,
                start_line: start_line as usize,
                end_line: end_line as usize,
                content: row.get(3)?,
                embedding: decode_embedding(&embedding),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Candidate retrieval for large indexes: lexical FTS and nearby LSH buckets are unioned,
    /// then the caller performs exact cosine scoring only over this bounded set. Small projects
    /// retain exhaustive scoring for maximum recall.
    pub fn candidate_chunks(
        &self,
        project: &str,
        query: &str,
        lsh_buckets: &[i64],
    ) -> Result<Vec<CodeChunk>> {
        const EXHAUSTIVE_LIMIT: i64 = 2_000;
        const CANDIDATE_LIMIT: usize = 1_024;
        if self.chunk_count(project)? <= EXHAUSTIVE_LIMIT {
            return self.all_chunks(project);
        }

        let mut candidates: HashMap<i64, CodeChunk> = HashMap::new();
        if let Some(fts_query) = fts_query(query) {
            let mut statement = self.connection.prepare(
                "SELECT c.id, c.path, c.start_line, c.end_line, c.content, c.embedding
                 FROM code_chunks_fts f
                 JOIN code_chunks c ON c.id = f.rowid
                 WHERE f.project = ?1 AND code_chunks_fts MATCH ?2
                 ORDER BY bm25(code_chunks_fts)
                 LIMIT 256",
            )?;
            let rows = statement.query_map(params![project, fts_query], code_chunk_with_id)?;
            for row in rows {
                let (id, chunk) = row?;
                candidates.insert(id, chunk);
            }
        }

        if !lsh_buckets.is_empty() && candidates.len() < CANDIDATE_LIMIT {
            let placeholders = (0..lsh_buckets.len())
                .map(|index| format!("?{}", index + 2))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT id, path, start_line, end_line, content, embedding
                 FROM code_chunks
                 WHERE project = ?1 AND lsh_bucket IN ({placeholders})
                 LIMIT {CANDIDATE_LIMIT}"
            );
            let mut values = Vec::<rusqlite::types::Value>::with_capacity(lsh_buckets.len() + 1);
            values.push(project.to_string().into());
            values.extend(lsh_buckets.iter().copied().map(Into::into));
            let mut statement = self.connection.prepare(&sql)?;
            let rows =
                statement.query_map(rusqlite::params_from_iter(values), code_chunk_with_id)?;
            for row in rows {
                let (id, chunk) = row?;
                if candidates.len() >= CANDIDATE_LIMIT {
                    break;
                }
                candidates.insert(id, chunk);
            }
        }

        if candidates.is_empty()
            && let Some(signature) = lsh_buckets.first()
        {
            let mut statement = self.connection.prepare(
                "SELECT DISTINCT lsh_bucket FROM code_chunks
                 WHERE project = ?1 AND lsh_bucket IS NOT NULL",
            )?;
            let rows = statement.query_map([project], |row| row.get::<_, i64>(0))?;
            let mut nearest = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            nearest.sort_unstable_by_key(|bucket| (bucket ^ signature).count_ones());
            nearest.truncate(8);
            if !nearest.is_empty() {
                let placeholders = (0..nearest.len())
                    .map(|index| format!("?{}", index + 2))
                    .collect::<Vec<_>>()
                    .join(",");
                let sql = format!(
                    "SELECT id, path, start_line, end_line, content, embedding
                     FROM code_chunks
                     WHERE project = ?1 AND lsh_bucket IN ({placeholders})
                     LIMIT {CANDIDATE_LIMIT}"
                );
                let mut values = Vec::<rusqlite::types::Value>::with_capacity(nearest.len() + 1);
                values.push(project.to_string().into());
                values.extend(nearest.into_iter().map(Into::into));
                let mut statement = self.connection.prepare(&sql)?;
                let rows =
                    statement.query_map(rusqlite::params_from_iter(values), code_chunk_with_id)?;
                for row in rows {
                    let (id, chunk) = row?;
                    candidates.insert(id, chunk);
                }
            }
        }

        // Legacy rows from before schema v10 have no LSH bucket. They remain searchable until
        // `/index` upgrades them, even though that one transitional query is exhaustive.
        if candidates.is_empty() {
            return self.all_chunks(project);
        }
        Ok(candidates.into_values().collect())
    }
}

/// One file `/index` has embedded, with the time it was last indexed so the startup staleness
/// check can spot files modified since without re-reading and re-hashing them.
pub struct IndexedFile {
    pub path: String,
    pub indexed_at: i64,
}

/// One chunk of an indexed file, as scored by `search_code`.
pub struct CodeChunk {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
    pub embedding: Vec<f32>,
}

pub struct NewCodeChunk {
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
    pub embedding: Vec<f32>,
}

fn code_chunk_with_id(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, CodeChunk)> {
    let embedding: Vec<u8> = row.get(5)?;
    Ok((
        row.get(0)?,
        CodeChunk {
            path: row.get(1)?,
            start_line: row.get::<_, i64>(2)? as usize,
            end_line: row.get::<_, i64>(3)? as usize,
            content: row.get(4)?,
            embedding: decode_embedding(&embedding),
        },
    ))
}

const LSH_BITS: usize = 12;

pub fn embedding_signature(vector: &[f32]) -> i64 {
    let mut signature = 0i64;
    for bit in 0..LSH_BITS {
        let projection: f32 = vector
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let mixed = (index as u64)
                    .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                    .rotate_left(bit as u32 + 1)
                    ^ (bit as u64).wrapping_mul(0xbf58_476d_1ce4_e5b9);
                if mixed.count_ones().is_multiple_of(2) {
                    *value
                } else {
                    -*value
                }
            })
            .sum();
        if projection >= 0.0 {
            signature |= 1 << bit;
        }
    }
    signature
}

pub fn lsh_probe_buckets(signature: i64) -> Vec<i64> {
    let mut buckets = Vec::with_capacity(1 + LSH_BITS + (LSH_BITS * (LSH_BITS - 1) / 2));
    buckets.push(signature);
    for first in 0..LSH_BITS {
        buckets.push(signature ^ (1 << first));
    }
    for first in 0..LSH_BITS {
        for second in first + 1..LSH_BITS {
            buckets.push(signature ^ (1 << first) ^ (1 << second));
        }
    }
    buckets
}

fn fts_query(query: &str) -> Option<String> {
    let tokens = query
        .split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .filter(|token| token.len() >= 2)
        .take(12)
        .map(|token| format!("\"{}\"*", token.replace('"', "\"\"")))
        .collect::<Vec<_>>();
    (!tokens.is_empty()).then(|| tokens.join(" OR "))
}

/// Encode an embedding vector as little-endian `f32` bytes for the `code_chunks.embedding` BLOB
/// column — simpler than pulling in a serialization crate for a fixed, self-describing layout.
fn encode_embedding(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn decode_embedding(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().expect("chunks(4) yields 4 bytes")))
        .collect()
}

fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("KAMUI_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    BaseDirs::new()
        .map(|dirs| dirs.data_local_dir().join("kamui"))
        .context("could not determine the operating system data directory")
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        title: row.get(1)?,
        provider: row.get(2)?,
        model: row.get(3)?,
        compaction_summary: row.get(4)?,
        summarized_upto: row.get::<_, i64>(5)?.max(0) as usize,
        undo_snapshot: row.get(6)?,
    })
}

/// Row mapper shared by the per-model token queries, which differ only in what they group over.
fn model_tokens(row: &rusqlite::Row<'_>) -> rusqlite::Result<ModelTokens> {
    Ok(ModelTokens {
        model: row.get(0)?,
        input_tokens: row.get(1)?,
        output_tokens: row.get(2)?,
    })
}

fn make_title(content: &str) -> String {
    let mut title: String = content.chars().take(40).collect();
    if content.chars().count() > 40 {
        title.push_str("...");
    }
    title
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ToolCall;

    fn database() -> Database {
        Database::initialize(
            Connection::open_in_memory().unwrap(),
            PathBuf::from(":memory:"),
        )
        .unwrap()
    }

    #[test]
    fn persists_and_reloads_a_tool_turn() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_turn(
                &session.id,
                &[
                    Message::user("read it"),
                    Message::tool_request(
                        "",
                        vec![ToolCall {
                            id: "c1".to_string(),
                            name: "read_file".to_string(),
                            arguments: r#"{"path":"a.rs"}"#.to_string(),
                        }],
                    ),
                    Message::tool_result("c1", "fn main() {}"),
                    Message::assistant("It defines main."),
                ],
                &Usage::default(),
                "model",
                "stop",
            )
            .unwrap();

        let messages = database.load_messages(&session.id).unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[1].role_name(), "assistant");
        assert_eq!(messages[1].tool_calls.len(), 1);
        assert_eq!(messages[1].tool_calls[0].name, "read_file");
        assert_eq!(messages[2].role_name(), "tool");
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(messages[2].content, "fn main() {}");
        // The turn counts as a single request despite its extra messages.
        assert_eq!(
            database.session_stats(&session.id).unwrap().request_count,
            1
        );
    }

    #[test]
    fn compaction_checkpoint_survives_resume() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .set_compaction_checkpoint(&session.id, "earlier work", 7)
            .unwrap();

        let resumed = database.find_session(&session.id).unwrap().unwrap();
        assert_eq!(resumed.compaction_summary.as_deref(), Some("earlier work"));
        assert_eq!(resumed.summarized_upto, 7);
        assert_eq!(database.schema_version().unwrap(), 14);
    }

    #[test]
    fn undo_snapshot_is_saved_with_the_turn_and_can_be_cleared() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_turn_with_undo(
                &session.id,
                &[Message::user("edit"), Message::assistant("done")],
                &Usage::default(),
                "model",
                "stop",
                Some(r#"[["/tmp/a.txt","original"]]"#),
            )
            .unwrap();

        let resumed = database.find_session(&session.id).unwrap().unwrap();
        assert_eq!(
            resumed.undo_snapshot.as_deref(),
            Some(r#"[["/tmp/a.txt","original"]]"#)
        );
        database.clear_undo_snapshot(&session.id).unwrap();
        assert!(
            database
                .find_session(&session.id)
                .unwrap()
                .unwrap()
                .undo_snapshot
                .is_none()
        );
    }

    #[test]
    fn tool_execution_journal_records_outcomes_and_recovers_running_rows() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        let completed = database
            .start_tool_execution(&session.id, "call-1", "patch_file", "{}")
            .unwrap();
        database
            .finish_tool_execution(&completed, "patched file")
            .unwrap();
        let running = database
            .start_tool_execution(&session.id, "call-2", "run_command", "{}")
            .unwrap();
        assert_eq!(
            database
                .connection
                .query_row(
                    "SELECT status FROM tool_executions WHERE id = ?1",
                    [&completed],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "completed"
        );

        let Database { connection, path } = database;
        let database = Database::initialize(connection, path).unwrap();
        assert_eq!(
            database
                .connection
                .query_row(
                    "SELECT status FROM tool_executions WHERE id = ?1",
                    [&running],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "interrupted"
        );
        assert_eq!(database.schema_version().unwrap(), 14);
    }

    #[test]
    fn migration_preserves_messages_and_enables_the_tool_role() {
        let connection = Connection::open_in_memory().unwrap();
        // Reconstruct the pre-tool (user_version 2) schema with one existing message.
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE sessions (
                     id TEXT PRIMARY KEY, title TEXT NOT NULL, provider TEXT NOT NULL,
                     model TEXT NOT NULL, created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                     updated_at INTEGER NOT NULL DEFAULT (unixepoch()));
                 CREATE TABLE messages (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                     role TEXT NOT NULL CHECK (role IN ('system', 'user', 'assistant')),
                     content TEXT NOT NULL,
                     created_at INTEGER NOT NULL DEFAULT (unixepoch()));
                 CREATE INDEX messages_session_id ON messages(session_id, id);
                 CREATE TABLE usage_records (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                     input_tokens INTEGER NOT NULL, output_tokens INTEGER NOT NULL,
                     total_tokens INTEGER NOT NULL, finish_reason TEXT NOT NULL,
                     kind TEXT NOT NULL DEFAULT 'chat',
                     created_at INTEGER NOT NULL DEFAULT (unixepoch()));
                 INSERT INTO sessions (id, title, provider, model) VALUES ('s1', 't', 'test', 'm');
                 INSERT INTO messages (session_id, role, content) VALUES ('s1', 'user', 'hi');
                 PRAGMA user_version = 2;",
            )
            .unwrap();

        let database = Database::initialize(connection, PathBuf::from(":memory:")).unwrap();

        // The existing message survives the rebuild.
        let messages = database.load_messages("s1").unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content, "hi");

        // The relaxed CHECK now accepts a tool turn.
        database
            .save_turn(
                "s1",
                &[Message::tool_result("c1", "body")],
                &Usage::default(),
                "model",
                "stop",
            )
            .unwrap();
        let messages = database.load_messages("s1").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role_name(), "tool");
    }

    #[test]
    fn cache_samples_are_chat_turns_in_order() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        for (prompt, cached) in [(100u64, 0u64), (300, 285)] {
            database
                .save_turn(
                    &session.id,
                    &[Message::user("hi"), Message::assistant("hello")],
                    &Usage {
                        prompt_tokens: prompt,
                        completion_tokens: 5,
                        total_tokens: prompt + 5,
                        cached_tokens: cached,
                    },
                    "model",
                    "stop",
                )
                .unwrap();
        }
        // Title generation is its own request with its own prefix; counting it would misreport how
        // the conversation lands against the cache.
        database
            .save_generated_title(
                &session.id,
                "a title",
                &Usage {
                    prompt_tokens: 40,
                    completion_tokens: 3,
                    total_tokens: 43,
                    cached_tokens: 0,
                },
                "model",
                "stop",
            )
            .unwrap();

        let samples = database.cache_samples(&session.id).unwrap();
        assert_eq!(samples, vec![(100, 0), (300, 285)]);
    }

    #[test]
    fn persists_session_messages_and_usage() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_turn(
                &session.id,
                &[
                    Message::user("Explain Rust ownership"),
                    Message::assistant("Ownership tracks values."),
                ],
                &Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: 0,
                },
                "model",
                "stop",
            )
            .unwrap();

        let messages = database.load_messages(&session.id).unwrap();
        let stats = database.session_stats(&session.id).unwrap();
        let resumed = database.find_session(&session.id).unwrap().unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(stats.request_count, 1);
        assert_eq!(stats.total_tokens, 15);
        assert_eq!(stats.last_input_tokens, Some(10));
        assert_eq!(resumed.title, "Explain Rust ownership");
    }

    #[test]
    fn deleting_session_cascades_related_data() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_turn(
                &session.id,
                &[Message::user("hello"), Message::assistant("hi")],
                &Usage::default(),
                "model",
                "stop",
            )
            .unwrap();

        database.delete_session(&session.id).unwrap();

        assert!(database.find_session(&session.id).unwrap().is_none());
        assert!(database.load_messages(&session.id).unwrap().is_empty());
    }

    #[test]
    fn session_summary_does_not_multiply_usage_by_message_count() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_turn(
                &session.id,
                &[Message::user("hello"), Message::assistant("hi")],
                &Usage {
                    prompt_tokens: 4,
                    completion_tokens: 2,
                    total_tokens: 6,
                    cached_tokens: 0,
                },
                "model",
                "stop",
            )
            .unwrap();

        let summaries = database.list_sessions().unwrap();

        assert_eq!(summaries[0].message_count, 2);
        assert_eq!(summaries[0].total_tokens, 6);
    }

    #[test]
    fn renames_session_and_updates_summary() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_turn(
                &session.id,
                &[Message::user("hello"), Message::assistant("hi")],
                &Usage::default(),
                "model",
                "stop",
            )
            .unwrap();

        database
            .rename_session(&session.id, "Custom title")
            .unwrap();

        let resumed = database.find_session(&session.id).unwrap().unwrap();
        assert_eq!(resumed.title, "Custom title");
        assert_eq!(database.list_sessions().unwrap()[0].title, "Custom title");
    }

    #[test]
    fn renaming_missing_session_is_an_error() {
        let database = database();
        assert!(database.rename_session("missing", "title").is_err());
    }

    #[test]
    fn search_matches_message_content_and_ignores_wildcards() {
        let database = database();
        let session = database.create_session("test", "model").unwrap();
        database
            .save_turn(
                &session.id,
                &[
                    Message::user("How does ownership work in Rust"),
                    Message::assistant("Ownership tracks each value's owner."),
                ],
                &Usage::default(),
                "model",
                "stop",
            )
            .unwrap();

        let hits = database.search_messages("ownership", 20).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| hit.session_id == session.id));

        // A literal percent must not behave as a wildcard.
        assert!(database.search_messages("%", 20).unwrap().is_empty());
    }

    #[test]
    fn model_stats_break_down_usage_by_model() {
        let database = database();
        let session = database.create_session("test", "sol").unwrap();
        let turn = |model: &str, total: u64| {
            database
                .save_turn(
                    &session.id,
                    &[Message::user("hi"), Message::assistant("yo")],
                    &Usage {
                        prompt_tokens: total,
                        completion_tokens: 0,
                        total_tokens: total,
                        cached_tokens: 0,
                    },
                    model,
                    "stop",
                )
                .unwrap();
        };
        turn("gpt-5.6-sol", 15);
        turn("codeqwen:latest", 6);
        turn("gpt-5.6-sol", 4);

        let stats = database.model_stats(&session.id).unwrap();

        // Two distinct models, ordered by total tokens descending.
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].model, "gpt-5.6-sol");
        assert_eq!(stats[0].request_count, 2);
        assert_eq!(stats[0].total_tokens, 19);
        assert_eq!(stats[1].model, "codeqwen:latest");
        assert_eq!(stats[1].request_count, 1);
        assert_eq!(stats[1].total_tokens, 6);
    }

    #[test]
    fn settings_round_trip_and_overwrite() {
        let database = database();
        assert_eq!(database.get_setting("active_profile").unwrap(), None);

        database.set_setting("active_profile", "ollama").unwrap();
        assert_eq!(
            database.get_setting("active_profile").unwrap().as_deref(),
            Some("ollama")
        );

        database.set_setting("active_profile", "openai").unwrap();
        assert_eq!(
            database.get_setting("active_profile").unwrap().as_deref(),
            Some("openai")
        );
    }

    #[test]
    fn integrity_check_reports_ok_and_current_schema() {
        let database = database();
        assert_eq!(database.integrity_check().unwrap(), "ok");
        assert!(database.schema_version().unwrap() >= 9);
    }

    #[test]
    fn scheduled_jobs_are_claimed_once_and_persist_results() {
        let database = database();
        let id = database
            .create_scheduled_job("cargo test", "/tmp", 100, None)
            .unwrap();

        assert!(database.claim_due_job(99, "w1", 200).unwrap().is_none());
        let job = database.claim_due_job(100, "w1", 200).unwrap().unwrap();
        assert_eq!(job.id, id);
        assert!(database.claim_due_job(100, "w2", 200).unwrap().is_none());

        database
            .finish_scheduled_job(&job, 0, "ok", "", 101)
            .unwrap();
        let jobs = database.list_scheduled_jobs().unwrap();
        assert_eq!(jobs[0].status, "succeeded");
        assert_eq!(jobs[0].last_exit_code, Some(0));
        assert_eq!(jobs[0].stdout, "ok");
        assert!(!jobs[0].enabled);
    }

    #[test]
    fn recurring_jobs_advance_without_backfilling_missed_runs() {
        let database = database();
        database
            .create_scheduled_job("echo hi", "/tmp", 100, Some(60))
            .unwrap();
        let job = database.claim_due_job(400, "w1", 500).unwrap().unwrap();
        database.finish_scheduled_job(&job, 0, "", "", 400).unwrap();

        let jobs = database.list_scheduled_jobs().unwrap();
        assert_eq!(jobs[0].next_run_at, Some(460));
        assert_eq!(jobs[0].status, "scheduled");
        assert!(jobs[0].enabled);
    }

    #[test]
    fn only_expired_worker_leases_are_recovered() {
        let database = database();
        database
            .create_scheduled_job("echo hi", "/tmp", 100, None)
            .unwrap();
        database.claim_due_job(100, "worker", 150).unwrap().unwrap();

        assert_eq!(database.recover_expired_jobs(149).unwrap(), 0);
        assert_eq!(database.recover_expired_jobs(150).unwrap(), 1);
        assert_eq!(
            database.list_scheduled_jobs().unwrap()[0].status,
            "interrupted"
        );
    }

    #[test]
    fn session_list_hides_empty_sessions() {
        let database = database();
        database.create_session("test", "model").unwrap();

        assert!(database.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn remember_and_list_memory_round_trip_in_insertion_order() {
        let database = database();
        database.remember("Prefers bun over node.").unwrap();
        database.remember("Prefers uv over pip.").unwrap();

        let entries = database.list_memory().unwrap();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].content, "Prefers bun over node.");
        assert_eq!(entries[1].content, "Prefers uv over pip.");
    }

    #[test]
    fn total_memory_bytes_sums_all_entries() {
        let database = database();
        database.remember("abc").unwrap();
        database.remember("de").unwrap();

        assert_eq!(database.total_memory_bytes().unwrap(), 5);
    }

    #[test]
    fn update_memory_replaces_an_unambiguous_match() {
        let database = database();
        database.remember("Prefers node over bun.").unwrap();

        let updated = database
            .update_memory("node over bun", "bun over node.")
            .unwrap();

        assert!(updated);
        let entries = database.list_memory().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "bun over node.");
    }

    #[test]
    fn update_memory_fails_on_no_match_or_ambiguous_match() {
        let database = database();
        database.remember("Prefers bun.").unwrap();
        database.remember("Prefers uv.").unwrap();

        assert!(!database.update_memory("nonexistent", "x").unwrap());
        // Both entries contain "Prefers", so this is ambiguous.
        assert!(!database.update_memory("Prefers", "x").unwrap());
        let entries = database.list_memory().unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn forget_removes_an_unambiguous_match() {
        let database = database();
        database.remember("Prefers bun over node.").unwrap();
        database.remember("Prefers uv over pip.").unwrap();

        assert!(database.forget("bun").unwrap());

        let entries = database.list_memory().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "Prefers uv over pip.");
    }

    #[test]
    fn forget_fails_on_no_match_or_ambiguous_match() {
        let database = database();
        database.remember("Prefers bun.").unwrap();
        database.remember("Prefers uv.").unwrap();

        assert!(!database.forget("nonexistent").unwrap());
        assert!(!database.forget("Prefers").unwrap());
        assert_eq!(database.list_memory().unwrap().len(), 2);
    }

    #[test]
    fn clear_memory_removes_every_entry_and_reports_the_count() {
        let database = database();
        database.remember("one").unwrap();
        database.remember("two").unwrap();

        assert_eq!(database.clear_memory().unwrap(), 2);
        assert!(database.list_memory().unwrap().is_empty());
    }

    #[test]
    fn memory_matching_is_case_insensitive_and_escapes_like_wildcards() {
        let database = database();
        database.remember("100% sure about this_thing").unwrap();

        // '%' and '_' are SQL LIKE wildcards; a literal search for them must not match everything.
        assert!(!database.forget("50%").unwrap());
        assert!(database.forget("100% SURE").unwrap());
    }

    #[test]
    fn usage_reports_group_by_day_and_month_and_total() {
        let database = database();
        let session = database.create_session("test", "m").unwrap();
        // Two chat turns plus one title generation, all "today" from SQLite's point of view.
        for _ in 0..2 {
            database
                .save_turn(
                    &session.id,
                    &[Message::user("hi"), Message::assistant("hello")],
                    &Usage {
                        prompt_tokens: 10,
                        completion_tokens: 5,
                        total_tokens: 15,
                        cached_tokens: 0,
                    },
                    "m",
                    "stop",
                )
                .unwrap();
        }
        database
            .save_generated_title(
                &session.id,
                "A title",
                &Usage {
                    prompt_tokens: 4,
                    completion_tokens: 1,
                    total_tokens: 5,
                    cached_tokens: 0,
                },
                "m",
                "stop",
            )
            .unwrap();

        let daily = database.usage_by_day(30).unwrap();
        assert_eq!(daily.len(), 1);
        // Requests count only chat turns; tokens include the title call's usage.
        assert_eq!(daily[0].request_count, 2);
        assert_eq!(daily[0].total_tokens, 35);
        assert_eq!(daily[0].period.len(), 10); // YYYY-MM-DD

        let monthly = database.usage_by_month(12).unwrap();
        assert_eq!(monthly.len(), 1);
        assert_eq!(monthly[0].period.len(), 7); // YYYY-MM
        assert_eq!(monthly[0].total_tokens, 35);

        let total = database.usage_total().unwrap();
        assert_eq!(total.request_count, 2);
        assert_eq!(total.input_tokens, 24);
        assert_eq!(total.output_tokens, 11);
        assert_eq!(total.total_tokens, 35);
    }

    #[test]
    fn session_model_tokens_cover_every_usage_kind() {
        let database = database();
        let session = database.create_session("test", "sol").unwrap();
        database
            .save_turn(
                &session.id,
                &[Message::user("hi"), Message::assistant("yo")],
                &Usage {
                    prompt_tokens: 10,
                    completion_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: 0,
                },
                "gpt-5.6-sol",
                "stop",
            )
            .unwrap();
        database
            .save_generated_title(
                &session.id,
                "A title",
                &Usage {
                    prompt_tokens: 4,
                    completion_tokens: 1,
                    total_tokens: 5,
                    cached_tokens: 0,
                },
                "codeqwen:latest",
                "stop",
            )
            .unwrap();

        let mut tokens = database.session_model_tokens(&session.id).unwrap();
        tokens.sort_by(|a, b| a.model.cmp(&b.model));

        // Both models appear, including the one that only generated a title — it costs money too.
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].model.as_deref(), Some("codeqwen:latest"));
        assert_eq!(tokens[0].input_tokens, 4);
        assert_eq!(tokens[0].output_tokens, 1);
        assert_eq!(tokens[1].model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(tokens[1].input_tokens, 10);
        assert_eq!(tokens[1].output_tokens, 5);
    }

    #[test]
    fn usage_model_tokens_are_grouped_by_period_and_model() {
        let database = database();
        let session = database.create_session("test", "sol").unwrap();
        let turn = |model: &str, input: u64, output: u64| {
            database
                .save_turn(
                    &session.id,
                    &[Message::user("hi")],
                    &Usage {
                        prompt_tokens: input,
                        completion_tokens: output,
                        total_tokens: input + output,
                        cached_tokens: 0,
                    },
                    model,
                    "stop",
                )
                .unwrap();
        };
        turn("gpt-5.6-sol", 10, 5);
        turn("gpt-5.6-sol", 20, 1);
        turn("codeqwen:latest", 7, 3);

        let daily = database.usage_model_tokens_by_day().unwrap();
        let day = database.usage_by_day(30).unwrap();

        // Keyed by exactly the period strings a usage report prints, so rows can be looked up.
        assert_eq!(daily.len(), 1);
        let mut rows = daily.into_values().next().unwrap();
        rows.sort_by(|a, b| a.model.cmp(&b.model));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(rows[1].input_tokens, 30);
        assert_eq!(rows[1].output_tokens, 6);
        assert!(
            database
                .usage_model_tokens_by_day()
                .unwrap()
                .contains_key(&day[0].period)
        );

        let monthly = database.usage_model_tokens_by_month().unwrap();
        let month = database.usage_by_month(12).unwrap();
        assert!(monthly.contains_key(&month[0].period));

        let lifetime = database.usage_model_tokens().unwrap();
        assert_eq!(lifetime.len(), 2);
        assert_eq!(lifetime.iter().map(|row| row.input_tokens).sum::<i64>(), 37);
    }

    #[test]
    fn usage_reports_are_empty_without_any_usage() {
        let database = database();
        assert!(database.usage_by_day(30).unwrap().is_empty());
        assert!(database.usage_by_month(12).unwrap().is_empty());
        assert_eq!(database.usage_total().unwrap().total_tokens, 0);
    }

    /// Stand-in project keys; the real ones are canonical root paths (`ProjectContext::key`).
    const PROJECT: &str = "/home/dev/alpha";
    const OTHER_PROJECT: &str = "/home/dev/beta";

    #[test]
    fn embedding_round_trips_through_the_blob_column() {
        let database = database();
        let vector = vec![0.5_f32, -1.25, 0.0, 3.0];
        database
            .insert_chunk(PROJECT, "src/main.rs", 1, 10, "fn main() {}", &vector)
            .unwrap();

        let chunks = database.all_chunks(PROJECT).unwrap();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].path, "src/main.rs");
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 10);
        assert_eq!(chunks[0].content, "fn main() {}");
        assert_eq!(chunks[0].embedding, vector);
    }

    #[test]
    fn indexed_file_hash_tracks_the_most_recent_hash() {
        let database = database();
        assert_eq!(database.indexed_file_hash(PROJECT, "a.rs").unwrap(), None);

        database.set_indexed_file(PROJECT, "a.rs", "hash1").unwrap();
        assert_eq!(
            database.indexed_file_hash(PROJECT, "a.rs").unwrap(),
            Some("hash1".to_string())
        );

        database.set_indexed_file(PROJECT, "a.rs", "hash2").unwrap();
        assert_eq!(
            database.indexed_file_hash(PROJECT, "a.rs").unwrap(),
            Some("hash2".to_string())
        );
    }

    #[test]
    fn deleting_a_path_removes_its_chunks_and_index_entry() {
        let database = database();
        database.set_indexed_file(PROJECT, "a.rs", "hash1").unwrap();
        database
            .insert_chunk(PROJECT, "a.rs", 1, 5, "content", &[0.1])
            .unwrap();

        database.delete_chunks_for_path(PROJECT, "a.rs").unwrap();
        database.delete_indexed_file(PROJECT, "a.rs").unwrap();

        assert!(database.all_chunks(PROJECT).unwrap().is_empty());
        assert_eq!(database.indexed_file_hash(PROJECT, "a.rs").unwrap(), None);
        assert!(database.indexed_files(PROJECT).unwrap().is_empty());
    }

    #[test]
    fn chunk_count_reflects_inserted_chunks() {
        let database = database();
        assert_eq!(database.chunk_count(PROJECT).unwrap(), 0);
        database
            .insert_chunk(PROJECT, "a.rs", 1, 5, "x", &[0.1])
            .unwrap();
        database
            .insert_chunk(PROJECT, "a.rs", 6, 10, "y", &[0.2])
            .unwrap();
        assert_eq!(database.chunk_count(PROJECT).unwrap(), 2);
    }

    /// The whole point of the `user_version = 8` migration: two projects sharing Kamui's one global
    /// database must not see each other's chunks, even for an identical project-relative path.
    #[test]
    fn the_index_is_isolated_per_project() {
        let database = database();
        database
            .insert_chunk(PROJECT, "src/main.rs", 1, 5, "alpha", &[0.1])
            .unwrap();
        database
            .insert_chunk(OTHER_PROJECT, "src/main.rs", 1, 5, "beta", &[0.2])
            .unwrap();
        database
            .set_indexed_file(PROJECT, "src/main.rs", "hash-alpha")
            .unwrap();
        database
            .set_indexed_file(OTHER_PROJECT, "src/main.rs", "hash-beta")
            .unwrap();

        let chunks = database.all_chunks(PROJECT).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].content, "alpha");
        assert_eq!(database.chunk_count(PROJECT).unwrap(), 1);
        assert_eq!(
            database.indexed_file_hash(PROJECT, "src/main.rs").unwrap(),
            Some("hash-alpha".to_string())
        );

        // Re-indexing one project leaves the other's rows untouched.
        database
            .delete_chunks_for_path(PROJECT, "src/main.rs")
            .unwrap();
        database
            .delete_indexed_file(PROJECT, "src/main.rs")
            .unwrap();

        assert!(database.all_chunks(PROJECT).unwrap().is_empty());
        assert_eq!(database.all_chunks(OTHER_PROJECT).unwrap().len(), 1);
        assert_eq!(
            database
                .indexed_file_hash(OTHER_PROJECT, "src/main.rs")
                .unwrap(),
            Some("hash-beta".to_string())
        );
    }

    #[test]
    fn indexed_files_reports_only_the_requested_project() {
        let database = database();
        database.set_indexed_file(PROJECT, "a.rs", "h1").unwrap();
        database.set_indexed_file(PROJECT, "b.rs", "h2").unwrap();
        database
            .set_indexed_file(OTHER_PROJECT, "c.rs", "h3")
            .unwrap();

        let mut paths: Vec<String> = database
            .indexed_files(PROJECT)
            .unwrap()
            .into_iter()
            .map(|file| file.path)
            .collect();
        paths.sort();

        assert_eq!(paths, vec!["a.rs".to_string(), "b.rs".to_string()]);
        assert!(
            database.indexed_files(PROJECT).unwrap()[0].indexed_at > 0,
            "indexed_at should be populated for the staleness check"
        );
    }

    #[test]
    fn replacing_a_file_index_tracks_the_embedding_model() {
        let database = database();
        let chunks = [NewCodeChunk {
            start_line: 1,
            end_line: 2,
            content: "fn alpha() {}".to_string(),
            embedding: vec![1.0, 0.0],
        }];
        database
            .replace_file_index(PROJECT, "a.rs", "hash", "embed-v1", &chunks)
            .unwrap();

        assert!(
            database
                .indexed_file_is_current(PROJECT, "a.rs", "hash", "embed-v1")
                .unwrap()
        );
        assert!(
            !database
                .indexed_file_is_current(PROJECT, "a.rs", "hash", "embed-v2")
                .unwrap()
        );
    }

    #[test]
    fn lsh_probes_include_nearby_signatures_without_duplicates() {
        let signature = embedding_signature(&[1.0, -0.5, 0.25]);
        let probes = lsh_probe_buckets(signature);
        let unique: std::collections::HashSet<i64> = probes.iter().copied().collect();
        assert_eq!(probes.len(), 79);
        assert_eq!(unique.len(), probes.len());
        assert_eq!(probes[0], signature);
    }

    #[test]
    fn fts_query_keeps_code_identifiers_and_escapes_syntax() {
        assert_eq!(
            fts_query("find read_project_file()"),
            Some("\"find\"* OR \"read_project_file\"*".to_string())
        );
        assert_eq!(fts_query("!"), None);
    }

    #[test]
    fn large_index_candidate_search_is_bounded_and_keeps_lexical_hits() {
        let database = database();
        for index in 0..2_001 {
            let content = if index == 1_900 {
                "fn special_identifier() {}".to_string()
            } else {
                format!("fn routine_{index}() {{}}")
            };
            database
                .insert_chunk(
                    PROJECT,
                    "src/lib.rs",
                    index + 1,
                    index + 1,
                    &content,
                    &[1.0],
                )
                .unwrap();
        }
        let signature = embedding_signature(&[1.0]);
        let chunks = database
            .candidate_chunks(PROJECT, "special_identifier", &lsh_probe_buckets(signature))
            .unwrap();

        assert!(chunks.len() <= 1_024);
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.content.contains("special_identifier"))
        );
    }
}
