//! Workflow-run indexing.
//!
//! Indexes Claude Code **workflow runs** (`wf_*` directories) and their
//! per-phase **agents** in the per-project `sessions.db`, alongside `sessions`,
//! `session_messages`, and the git-correlation tables from PR #281.
//!
//! Containment mirrors the on-disk layout:
//! `user thread (session) -> subagents -> workflow runs -> workflow agents`.
//! A run's transcript files live under
//! `~/.claude/projects/<slug>/<session_id>/subagents/workflows/<run_id>/`, and
//! the run's meta+result is the sibling `workflows/<run_id>.json`. A run is
//! therefore *owned* by the session that spawned it (`parent_session_id`), so
//! it inherits that session's git spans: "workflows on branch X" resolves to
//! runs whose parent session has a span on X (see [`runs_for_git_scope`]).
//!
//! This module owns the **storage + query** foundation only. The ingest sweep
//! that discovers run directories and parses transcripts, and the
//! `tracedecay_workflows` query surface, build on the APIs defined here.

use libsql::{params, Connection, Value};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use crate::sessions::git_correlation::{GitScopeFilter, MAX_SESSIONS_FOR_LIMIT};

/// Schema version recorded in `session_schema_migrations` under
/// [`MIGRATION_NAME`]. Bump when the workflow tables change shape.
pub const WORKFLOW_INDEX_SCHEMA_VERSION: i64 = 1;

const MIGRATION_NAME: &str = "workflow_indexing";

/// Hard cap on rows returned by run/agent list queries, matching the
/// git-correlation ceiling so the two surfaces page alike.
pub const MAX_WORKFLOW_LIMIT: usize = MAX_SESSIONS_FOR_LIMIT;

/// Errors from the workflow-index store.
///
/// Shaped like [`crate::sessions::git_correlation::GitCorrelationError`] so
/// callers and `?`-conversions read the same across both stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowIndexError {
    /// Underlying database failure.
    Db(String),
    /// Caller-supplied argument was invalid (empty run id, …).
    InvalidArgument(String),
}

impl std::fmt::Display for WorkflowIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Db(message) => write!(f, "workflow index db error: {message}"),
            Self::InvalidArgument(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for WorkflowIndexError {}

impl From<libsql::Error> for WorkflowIndexError {
    fn from(err: libsql::Error) -> Self {
        Self::Db(err.to_string())
    }
}

/// Lifecycle state of a workflow run or agent.
///
/// Mirrors the Claude Code run JSON `status` / agent `state` vocabulary while
/// tolerating unknown strings (forward-compat): anything unrecognized folds to
/// [`WorkflowStatus::Unknown`] rather than failing ingest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    /// Still executing (run dir present, no terminal result yet).
    Running,
    /// Reached a successful terminal result.
    Completed,
    /// Terminated in error / blocked / interrupted.
    Failed,
    /// Status not recorded or not recognized.
    Unknown,
}

impl WorkflowStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    /// Normalizes an on-disk status/state token. Recognizes the Claude Code
    /// run vocabulary (`completed`, `running`, `failed`, `error`, `blocked`,
    /// agent `done`/`in_progress`); everything else becomes `Unknown`.
    pub fn from_disk(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "completed" | "done" | "success" | "succeeded" => Self::Completed,
            "running" | "in_progress" | "started" | "active" | "pending" => Self::Running,
            "failed" | "error" | "errored" | "blocked" | "interrupted" | "cancelled"
            | "canceled" | "timeout" | "timed_out" => Self::Failed,
            _ => Self::Unknown,
        }
    }

    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// One indexed workflow run (`wf_*` directory + its `workflows/<run_id>.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowRun {
    /// `wf_*` run id (also the transcript directory name). Primary key.
    pub run_id: String,
    /// The user-thread session that spawned this run; the run inherits this
    /// session's git spans. May be empty when the parent could not be resolved
    /// from disk (orphan run dir), in which case git-scope joins skip it.
    pub parent_session_id: String,
    /// Workflow name from the run meta (`workflowName` / `meta.name`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Serialized `phases` array from the run meta, verbatim JSON text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_json: Option<String>,
    pub status: WorkflowStatus,
    /// Run start (unix seconds). Derived from `startTime`/`timestamp`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_ts: Option<i64>,
    /// Run end (unix seconds). `started_ts + durationMs` when only a duration
    /// is recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_ts: Option<i64>,
    /// Final run result rendered to a short summary string (the run JSON
    /// `summary`, or a truncated `result`), never the full result blob.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_summary: Option<String>,
    /// Number of agents recorded for the run (`agentCount`), for a cheap
    /// list-view count without joining `workflow_agents`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub agent_count: i64,
}

/// One workflow agent: a single per-phase subagent invocation within a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowAgent {
    pub run_id: String,
    /// Human label from the run's `workflowProgress` (`label`, e.g.
    /// `mine:claude-transcripts`). Unique within a run together with
    /// `agent_id`.
    pub agent_label: String,
    /// Claude agent id (`agentId`, e.g. `a17141dbe5a308242`) — the stem of the
    /// transcript file. Empty when a progress row lacked one.
    pub agent_id: String,
    /// Phase title this agent ran under (`phaseTitle`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Absolute path to the agent's `agent-<id>.jsonl` transcript, when the
    /// file was found on disk. Drill-down reads replay from here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    /// The agent's own session id, when the transcript recorded one distinct
    /// from the parent thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_session_id: Option<String>,
    pub status: WorkflowStatus,
    /// Model that ran the agent (`model`), when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Total tokens (input+output, summed from transcript `usage`), when known.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tokens: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_ts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_ts: Option<i64>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde skip_serializing_if signature
fn is_zero(value: &i64) -> bool {
    *value == 0
}

/// Ensures the workflow-index tables exist in the session store. Version-gated
/// through the shared `session_schema_migrations` table exactly like
/// [`crate::sessions::git_correlation::ensure_git_correlation_schema`], so both
/// stores register under their own migration name in one table.
pub(crate) async fn ensure_workflow_index_schema(
    conn: &Connection,
) -> Result<(), WorkflowIndexError> {
    if schema_version(conn)
        .await
        .is_some_and(|version| version >= WORKFLOW_INDEX_SCHEMA_VERSION)
    {
        return Ok(());
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS session_schema_migrations (
            name TEXT PRIMARY KEY,
            version INTEGER NOT NULL,
            applied_at INTEGER NOT NULL DEFAULT (unixepoch())
        );
        CREATE TABLE IF NOT EXISTS workflow_runs (
            run_id TEXT PRIMARY KEY,
            parent_session_id TEXT NOT NULL DEFAULT '',
            name TEXT,
            description TEXT,
            phase_json TEXT,
            status TEXT NOT NULL DEFAULT 'unknown'
                CHECK(status IN ('running', 'completed', 'failed', 'unknown')),
            started_ts INTEGER,
            ended_ts INTEGER,
            result_summary TEXT,
            agent_count INTEGER NOT NULL DEFAULT 0,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            updated_at INTEGER NOT NULL DEFAULT (unixepoch())
        );
        CREATE INDEX IF NOT EXISTS idx_workflow_runs_parent
            ON workflow_runs(parent_session_id, started_ts);
        CREATE TABLE IF NOT EXISTS workflow_agents (
            run_id TEXT NOT NULL,
            agent_label TEXT NOT NULL,
            agent_id TEXT NOT NULL DEFAULT '',
            phase TEXT,
            transcript_path TEXT,
            agent_session_id TEXT,
            status TEXT NOT NULL DEFAULT 'unknown'
                CHECK(status IN ('running', 'completed', 'failed', 'unknown')),
            model TEXT,
            tokens INTEGER NOT NULL DEFAULT 0,
            started_ts INTEGER,
            ended_ts INTEGER,
            created_at INTEGER NOT NULL DEFAULT (unixepoch()),
            updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
            PRIMARY KEY(run_id, agent_label, agent_id)
        );
        CREATE INDEX IF NOT EXISTS idx_workflow_agents_run
            ON workflow_agents(run_id, phase);
        CREATE TABLE IF NOT EXISTS workflow_index_meta (
            key TEXT PRIMARY KEY,
            value INTEGER NOT NULL,
            updated_at INTEGER NOT NULL DEFAULT (unixepoch())
        );",
    )
    .await?;
    conn.execute(
        "INSERT INTO session_schema_migrations(name, version)
         VALUES (?1, ?2)
         ON CONFLICT(name) DO UPDATE SET
            version = excluded.version,
            applied_at = unixepoch()",
        params![MIGRATION_NAME, WORKFLOW_INDEX_SCHEMA_VERSION],
    )
    .await?;
    Ok(())
}

async fn schema_version(conn: &Connection) -> Option<i64> {
    let mut rows = conn
        .query(
            "SELECT version FROM session_schema_migrations WHERE name = ?1",
            params![MIGRATION_NAME],
        )
        .await
        .ok()?;
    rows.next().await.ok()??.get(0).ok()
}

/// True when both workflow tables are present, so a query against a store that
/// predates this schema can short-circuit to empty instead of hitting a
/// `no such table` error. Mirrors
/// [`crate::sessions::git_correlation::tables_present`].
pub async fn tables_present(conn: &Connection) -> Result<bool, WorkflowIndexError> {
    let mut rows = conn
        .query(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table'
               AND name IN ('workflow_runs', 'workflow_agents')",
            (),
        )
        .await?;
    let Some(row) = rows.next().await? else {
        return Ok(false);
    };
    Ok(row.get::<i64>(0)? == 2)
}

/// `workflow_index_meta` key holding the newest run-file mtime (unix seconds)
/// the ingest sweep has already processed. Runs whose files are no newer than
/// this value are skipped on the next sweep. See
/// [`crate::sessions::workflow_ingest`].
pub const INGEST_WATERMARK_KEY: &str = "ingest_watermark_mtime";

/// Reads the ingest watermark (max processed run-file mtime, unix seconds), or
/// `0` when unset / the schema predates this table. Never errors: a store
/// without the meta table simply reports no watermark, forcing a full sweep.
pub async fn read_ingest_watermark(conn: &Connection, key: &str) -> i64 {
    let Ok(mut rows) = conn
        .query(
            "SELECT value FROM workflow_index_meta WHERE key = ?1",
            params![key.to_string()],
        )
        .await
    else {
        return 0;
    };
    match rows.next().await {
        Ok(Some(row)) => row.get::<i64>(0).unwrap_or(0),
        _ => 0,
    }
}

/// Advances the ingest watermark to `mtime` when it is newer than the stored
/// value (monotonic; a stale re-scan never rewinds it). Requires the schema to
/// exist; callers ensure it before writing.
pub async fn bump_ingest_watermark(
    conn: &Connection,
    key: &str,
    mtime: i64,
) -> Result<(), WorkflowIndexError> {
    conn.execute(
        "INSERT INTO workflow_index_meta(key, value)
         VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET
             value = MAX(value, excluded.value),
             updated_at = unixepoch()",
        params![key.to_string(), mtime],
    )
    .await?;
    Ok(())
}

fn opt_text(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |text| Value::Text(text.to_string()))
}

fn opt_int(value: Option<i64>) -> Value {
    value.map_or(Value::Null, Value::Integer)
}

/// Inserts or updates one run row (idempotent on `run_id`). Re-ingesting a run
/// whose transcripts grew (e.g. a `running` run that later `completed`)
/// overwrites the mutable columns and refreshes `updated_at`. `created_at` is
/// preserved.
pub async fn upsert_run(conn: &Connection, run: &WorkflowRun) -> Result<(), WorkflowIndexError> {
    if run.run_id.trim().is_empty() {
        return Err(WorkflowIndexError::InvalidArgument(
            "workflow run_id must not be empty".to_string(),
        ));
    }
    conn.execute(
        "INSERT INTO workflow_runs(
             run_id, parent_session_id, name, description, phase_json,
             status, started_ts, ended_ts, result_summary, agent_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(run_id) DO UPDATE SET
             parent_session_id = excluded.parent_session_id,
             name = excluded.name,
             description = excluded.description,
             phase_json = excluded.phase_json,
             status = excluded.status,
             started_ts = excluded.started_ts,
             ended_ts = excluded.ended_ts,
             result_summary = excluded.result_summary,
             agent_count = excluded.agent_count,
             updated_at = unixepoch()",
        params![
            run.run_id.clone(),
            run.parent_session_id.clone(),
            opt_text(run.name.as_deref()),
            opt_text(run.description.as_deref()),
            opt_text(run.phase_json.as_deref()),
            run.status.as_str(),
            opt_int(run.started_ts),
            opt_int(run.ended_ts),
            opt_text(run.result_summary.as_deref()),
            run.agent_count,
        ],
    )
    .await?;
    Ok(())
}

/// Inserts or updates one agent row (idempotent on `(run_id, agent_label,
/// agent_id)`).
pub async fn upsert_agent(
    conn: &Connection,
    agent: &WorkflowAgent,
) -> Result<(), WorkflowIndexError> {
    if agent.run_id.trim().is_empty() {
        return Err(WorkflowIndexError::InvalidArgument(
            "workflow agent run_id must not be empty".to_string(),
        ));
    }
    conn.execute(
        "INSERT INTO workflow_agents(
             run_id, agent_label, agent_id, phase, transcript_path,
             agent_session_id, status, model, tokens, started_ts, ended_ts)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(run_id, agent_label, agent_id) DO UPDATE SET
             phase = excluded.phase,
             transcript_path = excluded.transcript_path,
             agent_session_id = excluded.agent_session_id,
             status = excluded.status,
             model = excluded.model,
             tokens = excluded.tokens,
             started_ts = excluded.started_ts,
             ended_ts = excluded.ended_ts,
             updated_at = unixepoch()",
        params![
            agent.run_id.clone(),
            agent.agent_label.clone(),
            agent.agent_id.clone(),
            opt_text(agent.phase.as_deref()),
            opt_text(agent.transcript_path.as_deref()),
            opt_text(agent.agent_session_id.as_deref()),
            agent.status.as_str(),
            opt_text(agent.model.as_deref()),
            agent.tokens,
            opt_int(agent.started_ts),
            opt_int(agent.ended_ts),
        ],
    )
    .await?;
    Ok(())
}

const RUN_COLUMNS: &str = "run_id, parent_session_id, name, description, phase_json,
     status, started_ts, ended_ts, result_summary, agent_count";

fn row_to_run(row: &libsql::Row) -> Result<WorkflowRun, WorkflowIndexError> {
    let status: String = row.get(5)?;
    Ok(WorkflowRun {
        run_id: row.get(0)?,
        parent_session_id: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        phase_json: row.get(4)?,
        status: WorkflowStatus::from_db(&status).unwrap_or(WorkflowStatus::Unknown),
        started_ts: row.get(6)?,
        ended_ts: row.get(7)?,
        result_summary: row.get(8)?,
        agent_count: row.get::<Option<i64>>(9)?.unwrap_or(0),
    })
}

const AGENT_COLUMNS: &str = "run_id, agent_label, agent_id, phase, transcript_path,
     agent_session_id, status, model, tokens, started_ts, ended_ts";

fn row_to_agent(row: &libsql::Row) -> Result<WorkflowAgent, WorkflowIndexError> {
    let status: String = row.get(6)?;
    Ok(WorkflowAgent {
        run_id: row.get(0)?,
        agent_label: row.get(1)?,
        agent_id: row.get(2)?,
        phase: row.get(3)?,
        transcript_path: row.get(4)?,
        agent_session_id: row.get(5)?,
        status: WorkflowStatus::from_db(&status).unwrap_or(WorkflowStatus::Unknown),
        model: row.get(7)?,
        tokens: row.get::<Option<i64>>(8)?.unwrap_or(0),
        started_ts: row.get(9)?,
        ended_ts: row.get(10)?,
    })
}

fn clamp_limit(limit: usize) -> i64 {
    limit.clamp(1, MAX_WORKFLOW_LIMIT) as i64
}

/// Lists workflow runs spawned by one parent session, newest-first. Returns an
/// empty vec (never an error) when the schema is absent.
pub async fn runs_for_session(
    conn: &Connection,
    parent_session_id: &str,
    limit: usize,
) -> Result<Vec<WorkflowRun>, WorkflowIndexError> {
    if !tables_present(conn).await.unwrap_or(false) {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {RUN_COLUMNS}
         FROM workflow_runs
         WHERE parent_session_id = ?1
         ORDER BY COALESCE(started_ts, 0) DESC, run_id DESC
         LIMIT ?2"
    );
    let mut rows = conn
        .query(
            &sql,
            params![parent_session_id.to_string(), clamp_limit(limit)],
        )
        .await?;
    let mut runs = Vec::new();
    while let Some(row) = rows.next().await? {
        runs.push(row_to_run(&row)?);
    }
    Ok(runs)
}

/// Fetches one run by its `wf_*` id, or `None` when absent.
pub async fn run_for_id(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<WorkflowRun>, WorkflowIndexError> {
    if !tables_present(conn).await.unwrap_or(false) {
        return Ok(None);
    }
    let sql = format!("SELECT {RUN_COLUMNS} FROM workflow_runs WHERE run_id = ?1");
    let mut rows = conn.query(&sql, params![run_id.to_string()]).await?;
    match rows.next().await? {
        Some(row) => Ok(Some(row_to_run(&row)?)),
        None => Ok(None),
    }
}

/// Lists the agents of one run, ordered by start time then label so a phase
/// reads top-to-bottom.
pub async fn agents_for_run(
    conn: &Connection,
    run_id: &str,
    limit: usize,
) -> Result<Vec<WorkflowAgent>, WorkflowIndexError> {
    if !tables_present(conn).await.unwrap_or(false) {
        return Ok(Vec::new());
    }
    let sql = format!(
        "SELECT {AGENT_COLUMNS}
         FROM workflow_agents
         WHERE run_id = ?1
         ORDER BY COALESCE(started_ts, 0) ASC, agent_label ASC
         LIMIT ?2"
    );
    let mut rows = conn
        .query(&sql, params![run_id.to_string(), clamp_limit(limit)])
        .await?;
    let mut agents = Vec::new();
    while let Some(row) = rows.next().await? {
        agents.push(row_to_agent(&row)?);
    }
    Ok(agents)
}

/// Runs that ran "on branch X / in worktree Y / for commit Z": a run inherits
/// its parent session's git spans, so this selects runs whose
/// `parent_session_id` matches a session correlated with the given git ref.
///
/// Implemented as an `EXISTS` pushdown against the git-correlation tables
/// ([`session_git_spans`] / [`commit_sessions`]) — the same tables
/// `tracedecay_sessions_for` reads. When either the workflow schema or the
/// git-correlation schema is absent, returns empty (nothing could correlate).
pub async fn runs_for_git_scope(
    conn: &Connection,
    filter: &GitScopeFilter,
    limit: usize,
) -> Result<Vec<WorkflowRun>, WorkflowIndexError> {
    if filter.is_empty() {
        return Err(WorkflowIndexError::InvalidArgument(
            "runs_for_git_scope requires at least one of branch/worktree/commit".to_string(),
        ));
    }
    if !tables_present(conn).await.unwrap_or(false) {
        return Ok(Vec::new());
    }
    // A git-scoped run query against a store written before the correlation
    // schema existed can never match; report empty rather than issuing an
    // EXISTS against missing tables.
    if !crate::sessions::git_correlation::tables_present(conn)
        .await
        .unwrap_or(false)
    {
        return Ok(Vec::new());
    }

    let mut sql = format!(
        "SELECT {RUN_COLUMNS}
         FROM workflow_runs AS r
         WHERE r.parent_session_id <> ''
           AND ("
    );
    let mut params: Vec<Value> = Vec::new();
    let mut first = true;
    let mut push_clause = |sql: &mut String, clause: &str| {
        if !first {
            sql.push_str(" OR ");
        }
        sql.push_str(clause);
        first = false;
    };
    if let Some(branch) = filter.branch.as_deref() {
        params.push(Value::Text(branch.to_string()));
        push_clause(
            &mut sql,
            &format!(
                "EXISTS (SELECT 1 FROM session_git_spans s
                         WHERE s.session_id = r.parent_session_id
                           AND s.branch = ?{})",
                params.len()
            ),
        );
    }
    if let Some(worktree) = filter.worktree.as_deref() {
        params.push(Value::Text(worktree.to_string()));
        push_clause(
            &mut sql,
            &format!(
                "EXISTS (SELECT 1 FROM session_git_spans s
                         WHERE s.session_id = r.parent_session_id
                           AND s.worktree = ?{})",
                params.len()
            ),
        );
    }
    if let Some(commit) = filter.commit.as_deref() {
        params.push(Value::Text(commit.to_string()));
        params.push(Value::Text(format!("{commit}%")));
        push_clause(
            &mut sql,
            &format!(
                "EXISTS (SELECT 1 FROM commit_sessions c
                         WHERE c.session_id = r.parent_session_id
                           AND (c.commit_sha = ?{} OR c.commit_sha LIKE ?{}))",
                params.len() - 1,
                params.len()
            ),
        );
    }
    params.push(Value::Integer(clamp_limit(limit)));
    let _ = write!(
        sql,
        ") ORDER BY COALESCE(r.started_ts, 0) DESC, r.run_id DESC LIMIT ?{}",
        params.len()
    );

    let mut rows = conn.query(&sql, params).await?;
    let mut runs = Vec::new();
    while let Some(row) = rows.next().await? {
        runs.push(row_to_run(&row)?);
    }
    Ok(runs)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sessions::git_correlation::{
        ensure_git_correlation_schema, record_span_observation, SpanObservation, SpanSource,
    };

    async fn mem_conn() -> Connection {
        let db = libsql::Builder::new_local(":memory:")
            .build()
            .await
            .unwrap();
        db.connect().unwrap()
    }

    fn sample_run(run_id: &str, parent: &str) -> WorkflowRun {
        WorkflowRun {
            run_id: run_id.to_string(),
            parent_session_id: parent.to_string(),
            name: Some("triggering-evals".to_string()),
            description: Some("mine + run + score".to_string()),
            phase_json: Some(r#"[{"title":"Mine"},{"title":"Run"}]"#.to_string()),
            status: WorkflowStatus::Completed,
            started_ts: Some(1_700_000_000),
            ended_ts: Some(1_700_000_900),
            result_summary: Some("36 scenarios, 45 runs".to_string()),
            agent_count: 11,
        }
    }

    #[test]
    fn status_from_disk_folds_known_and_unknown() {
        assert_eq!(
            WorkflowStatus::from_disk("completed"),
            WorkflowStatus::Completed
        );
        assert_eq!(WorkflowStatus::from_disk("done"), WorkflowStatus::Completed);
        assert_eq!(
            WorkflowStatus::from_disk("in_progress"),
            WorkflowStatus::Running
        );
        assert_eq!(WorkflowStatus::from_disk("blocked"), WorkflowStatus::Failed);
        assert_eq!(
            WorkflowStatus::from_disk("timed_out"),
            WorkflowStatus::Failed
        );
        assert_eq!(WorkflowStatus::from_disk("banana"), WorkflowStatus::Unknown);
        // Round-trips through the DB spelling.
        for s in [
            WorkflowStatus::Running,
            WorkflowStatus::Completed,
            WorkflowStatus::Failed,
            WorkflowStatus::Unknown,
        ] {
            assert_eq!(WorkflowStatus::from_db(s.as_str()), Some(s));
        }
    }

    #[tokio::test]
    async fn queries_are_empty_before_schema_exists() {
        let conn = mem_conn().await;
        // No tables yet: readers must fail-open to empty/None.
        assert!(!tables_present(&conn).await.unwrap());
        assert!(runs_for_session(&conn, "sess", 10)
            .await
            .unwrap()
            .is_empty());
        assert!(run_for_id(&conn, "wf_x").await.unwrap().is_none());
        assert!(agents_for_run(&conn, "wf_x", 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn upsert_is_idempotent_and_updates_mutable_columns() {
        let conn = mem_conn().await;
        ensure_workflow_index_schema(&conn).await.unwrap();
        assert!(tables_present(&conn).await.unwrap());

        let mut run = sample_run("wf_alpha", "sess-1");
        run.status = WorkflowStatus::Running;
        run.result_summary = None;
        upsert_run(&conn, &run).await.unwrap();

        // Re-ingest the same run once it finished: overwrite, don't duplicate.
        let finished = sample_run("wf_alpha", "sess-1");
        upsert_run(&conn, &finished).await.unwrap();

        let all = runs_for_session(&conn, "sess-1", 10).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0], finished);
        assert_eq!(all[0].status, WorkflowStatus::Completed);
        assert_eq!(
            all[0].result_summary.as_deref(),
            Some("36 scenarios, 45 runs")
        );

        assert_eq!(run_for_id(&conn, "wf_alpha").await.unwrap(), Some(finished));
        assert!(run_for_id(&conn, "wf_missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn empty_run_id_is_rejected() {
        let conn = mem_conn().await;
        ensure_workflow_index_schema(&conn).await.unwrap();
        let mut run = sample_run("   ", "sess");
        run.run_id = "   ".to_string();
        let err = upsert_run(&conn, &run).await.unwrap_err();
        assert!(matches!(err, WorkflowIndexError::InvalidArgument(_)));
    }

    #[tokio::test]
    async fn runs_for_session_orders_newest_first_and_scopes_by_parent() {
        let conn = mem_conn().await;
        ensure_workflow_index_schema(&conn).await.unwrap();

        let mut old = sample_run("wf_old", "sess-1");
        old.started_ts = Some(1_000);
        let mut new = sample_run("wf_new", "sess-1");
        new.started_ts = Some(2_000);
        let other = sample_run("wf_other", "sess-2");
        upsert_run(&conn, &old).await.unwrap();
        upsert_run(&conn, &new).await.unwrap();
        upsert_run(&conn, &other).await.unwrap();

        let s1 = runs_for_session(&conn, "sess-1", 10).await.unwrap();
        let ids: Vec<&str> = s1.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(ids, vec!["wf_new", "wf_old"]);

        let s2 = runs_for_session(&conn, "sess-2", 10).await.unwrap();
        assert_eq!(s2.len(), 1);
        assert_eq!(s2[0].run_id, "wf_other");
    }

    #[tokio::test]
    async fn agents_upsert_and_order_within_run() {
        let conn = mem_conn().await;
        ensure_workflow_index_schema(&conn).await.unwrap();
        upsert_run(&conn, &sample_run("wf_a", "sess"))
            .await
            .unwrap();

        let second = WorkflowAgent {
            run_id: "wf_a".to_string(),
            agent_label: "run:batch2".to_string(),
            agent_id: "a222".to_string(),
            phase: Some("Run".to_string()),
            transcript_path: Some("/tmp/agent-a222.jsonl".to_string()),
            agent_session_id: None,
            status: WorkflowStatus::Completed,
            model: Some("claude-fable-5".to_string()),
            tokens: 4200,
            started_ts: Some(2_000),
            ended_ts: Some(2_500),
        };
        let first = WorkflowAgent {
            agent_label: "mine:claude".to_string(),
            agent_id: "a111".to_string(),
            phase: Some("Mine".to_string()),
            started_ts: Some(1_000),
            ..second.clone()
        };
        upsert_agent(&conn, &second).await.unwrap();
        upsert_agent(&conn, &first).await.unwrap();
        // Idempotent re-ingest of the first agent.
        upsert_agent(&conn, &first).await.unwrap();

        let agents = agents_for_run(&conn, "wf_a", 10).await.unwrap();
        let labels: Vec<&str> = agents.iter().map(|a| a.agent_label.as_str()).collect();
        assert_eq!(labels, vec!["mine:claude", "run:batch2"]);
        assert_eq!(agents[0].tokens, 4200);
        assert_eq!(agents[1].model.as_deref(), Some("claude-fable-5"));
    }

    #[tokio::test]
    async fn runs_for_git_scope_joins_through_parent_session_spans() {
        let conn = mem_conn().await;
        ensure_git_correlation_schema(&conn).await.unwrap();
        ensure_workflow_index_schema(&conn).await.unwrap();

        // A run owned by sess-branch, another owned by sess-other.
        upsert_run(&conn, &sample_run("wf_on_branch", "sess-branch"))
            .await
            .unwrap();
        upsert_run(&conn, &sample_run("wf_elsewhere", "sess-other"))
            .await
            .unwrap();
        // An orphan run with no resolvable parent must never leak into a
        // git-scoped result.
        upsert_run(&conn, &sample_run("wf_orphan", ""))
            .await
            .unwrap();

        // Record a span placing sess-branch on branch `feat/x`.
        record_span_observation(
            &conn,
            &SpanObservation {
                provider: "claude".to_string(),
                session_id: "sess-branch".to_string(),
                thread_id: None,
                branch: Some("feat/x".to_string()),
                worktree: "/repo".to_string(),
                ts: 1_700_000_100,
                source: SpanSource::Ingest,
            },
            super::super::git_correlation::DEFAULT_SPAN_MERGE_GAP_SECS,
        )
        .await
        .unwrap();

        let filter = GitScopeFilter::from_args(Some("feat/x"), None, None).unwrap();
        let hits = runs_for_git_scope(&conn, &filter, 10).await.unwrap();
        let ids: Vec<&str> = hits.iter().map(|r| r.run_id.as_str()).collect();
        assert_eq!(ids, vec!["wf_on_branch"]);

        // A branch with no span yields nothing.
        let none = GitScopeFilter::from_args(Some("feat/absent"), None, None).unwrap();
        assert!(runs_for_git_scope(&conn, &none, 10)
            .await
            .unwrap()
            .is_empty());

        // Empty filter is a caller error.
        let empty = GitScopeFilter::default();
        assert!(matches!(
            runs_for_git_scope(&conn, &empty, 10).await,
            Err(WorkflowIndexError::InvalidArgument(_))
        ));
    }
}
