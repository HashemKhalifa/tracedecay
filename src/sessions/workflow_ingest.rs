//! Workflow-run ingest sweep.
//!
//! Scans Claude Code `wf_*` runs, keeps runs whose parent transcript belongs to
//! `project_root`, and upserts bounded run/agent summaries into `sessions.db`.

use std::io::BufRead;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::accounting::parser::parse_timestamp;
use crate::global_db::GlobalDb;
use crate::sessions::shared::path_belongs_to_project;
use crate::sessions::workflow_index::{
    bump_ingest_watermark, read_ingest_watermark, WorkflowAgent, WorkflowRun, WorkflowStatus,
    INGEST_WATERMARK_KEY,
};

const RESULT_SUMMARY_CAP: usize = 600;

/// Depth of the `cwd` probe over a transcript, matching
/// [`crate::sessions::claude`]: the first line is sometimes a meta/summary row
/// without a `cwd`.
const CWD_PROBE_LINES: usize = 8;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowIngestStats {
    pub runs_ingested: u64,
    pub agents_ingested: u64,
}

impl WorkflowIngestStats {
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            runs_ingested: self.runs_ingested.saturating_add(other.runs_ingested),
            agents_ingested: self.agents_ingested.saturating_add(other.agents_ingested),
        }
    }
}

struct DiscoveredRun {
    run_id: String,
    parent_session_id: String,
    meta_path: Option<PathBuf>,
    agents_dir: PathBuf,
}

/// Fail-open at every level: a store that cannot be read, a project whose home
/// cannot be resolved, or an individual malformed run all degrade to "ingest
/// less", never an error. Returns the number of runs and agents upserted.
pub async fn ingest_workflow_runs(db: &GlobalDb, project_root: &Path) -> WorkflowIngestStats {
    let Some(home) = crate::sessions::home_dir() else {
        return WorkflowIngestStats::default();
    };
    ingest_workflow_runs_from(db, project_root, &home.join(".claude").join("projects")).await
}

pub(crate) async fn ingest_workflow_runs_from(
    db: &GlobalDb,
    project_root: &Path,
    projects_dir: &Path,
) -> WorkflowIngestStats {
    let conn = db.dashboard_connection();
    let watermark = read_ingest_watermark(&conn, INGEST_WATERMARK_KEY).await;

    let mut stats = WorkflowIngestStats::default();
    let mut max_mtime = watermark;

    for run in discover_runs(projects_dir) {
        let run_mtime = newest_mtime(&run);
        if run_mtime > 0 && run_mtime <= watermark {
            continue;
        }

        // Scope to this project by the owning session's recorded cwd. A run
        // whose parent thread began in another project is skipped without
        // touching the DB — the same per-session cwd filter ClaudeSource uses.
        // This filter also gates the watermark: `discover_runs` walks every
        // project on the machine, but the watermark is persisted per-store, so
        // only in-scope runs may advance it. Letting an out-of-project run raise
        // this store's watermark could push it past a still-changing target run
        // and strand that run (e.g. a Running run never re-ingested once it
        // completes).
        if !run_belongs_to_project(&run, project_root) {
            continue;
        }
        if run_mtime > max_mtime {
            max_mtime = run_mtime;
        }

        match ingest_one_run(db, &run).await {
            Ok(run_stats) => stats = stats.merge(run_stats),
            Err(err) => {
                tracing::debug!(run_id = %run.run_id, error = %err, "skipping workflow run");
            }
        }
    }

    // Persist the advanced watermark so the next sweep skips everything we just
    // processed. Best-effort: a write failure only means the next sweep does a
    // little redundant (idempotent) work.
    if max_mtime > watermark {
        if let Err(err) = bump_ingest_watermark(&conn, INGEST_WATERMARK_KEY, max_mtime).await {
            tracing::debug!(error = %err, "workflow ingest watermark not advanced");
        }
    }

    stats
}

/// Discover every workflow run under `projects_dir` by walking
/// `<slug>/<session_id>/subagents/workflows/<run_id>/`.
fn discover_runs(projects_dir: &Path) -> Vec<DiscoveredRun> {
    let mut runs = Vec::new();
    let Ok(slugs) = std::fs::read_dir(projects_dir) else {
        return runs;
    };
    for slug in slugs.flatten() {
        let slug_path = slug.path();
        if !slug_path.is_dir() {
            continue;
        }
        let Ok(sessions) = std::fs::read_dir(&slug_path) else {
            continue;
        };
        for session in sessions.flatten() {
            let session_path = session.path();
            if !session_path.is_dir() {
                continue;
            }
            let Some(session_id) = session_path
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            let workflows_dir = session_path.join("subagents").join("workflows");
            let Ok(run_dirs) = std::fs::read_dir(&workflows_dir) else {
                continue;
            };
            for run in run_dirs.flatten() {
                let agents_dir = run.path();
                if !agents_dir.is_dir() {
                    continue;
                }
                let Some(run_id) = agents_dir
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
                else {
                    continue;
                };
                let meta_path = session_path
                    .join("workflows")
                    .join(format!("{run_id}.json"));
                runs.push(DiscoveredRun {
                    run_id,
                    parent_session_id: session_id.clone(),
                    meta_path: meta_path.is_file().then_some(meta_path),
                    agents_dir,
                });
            }
        }
    }
    runs
}

/// Newest mtime (unix seconds) across a run's meta json and its agent-transcript
/// directory, for the incremental watermark. `0` when neither can be stat'd.
fn newest_mtime(run: &DiscoveredRun) -> i64 {
    let mut newest = 0;
    if let Some(meta) = run.meta_path.as_ref() {
        newest = newest.max(file_mtime(meta));
    }
    newest = newest.max(file_mtime(&run.agents_dir));
    newest
}

fn file_mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |dur| i64::try_from(dur.as_secs()).unwrap_or(0))
}

/// Decide whether a run's owning session began inside `project_root`, from the
/// `cwd` recorded in the parent transcript (preferred) or any agent transcript.
fn run_belongs_to_project(run: &DiscoveredRun, project_root: &Path) -> bool {
    let Some(cwd) = run_cwd(run) else {
        // No resolvable cwd: refuse rather than mis-attribute a run to a
        // project it may not belong to. ClaudeSource makes the same choice.
        return false;
    };
    path_belongs_to_project(&cwd, project_root)
}

/// The owning session's working directory, probed from the parent transcript
/// (`<session_id>.jsonl`, two levels above `subagents/workflows/`) or, failing
/// that, an agent transcript in the run dir.
fn run_cwd(run: &DiscoveredRun) -> Option<PathBuf> {
    // Parent transcript sits at <slug>/<session_id>.jsonl. agents_dir is
    // <slug>/<session_id>/subagents/workflows/<run_id>; `ancestors()` yields
    // nth(0)=<run_id> dir, nth(1)=workflows, nth(2)=subagents,
    // nth(3)=<slug>/<session_id>. The parent transcript is that session dir's
    // sibling with a `.jsonl` suffix appended (not `with_extension`, which would
    // mangle a session id that happens to contain a dot).
    let parent_transcript = run.agents_dir.ancestors().nth(3).and_then(|session_dir| {
        let name = session_dir.file_name()?.to_str()?;
        Some(session_dir.with_file_name(format!("{name}.jsonl")))
    });
    if let Some(cwd) = parent_transcript.as_deref().and_then(transcript_cwd) {
        return Some(cwd);
    }
    // Fall back to the first agent transcript that records a cwd.
    for path in agent_transcripts(&run.agents_dir) {
        if let Some(cwd) = transcript_cwd(&path) {
            return Some(cwd);
        }
    }
    None
}

/// Read the `cwd` from an early line of a JSONL transcript. Mirrors the probe in
/// [`crate::sessions::claude`].
fn transcript_cwd(path: &Path) -> Option<PathBuf> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().take(CWD_PROBE_LINES).map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
                if !cwd.is_empty() {
                    return Some(PathBuf::from(cwd));
                }
            }
        }
    }
    None
}

/// Absolute paths to the `agent-<id>.jsonl` transcripts in a run directory,
/// excluding the sibling `.meta.json` files and `journal.jsonl`.
fn agent_transcripts(agents_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            let is_jsonl = path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"));
            let named_agent = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("agent-"));
            is_jsonl && named_agent
        })
        .collect();
    paths.sort();
    paths
}

/// Parse one discovered run and upsert its run row plus every agent row.
async fn ingest_one_run(
    db: &GlobalDb,
    run: &DiscoveredRun,
) -> Result<WorkflowIngestStats, crate::sessions::workflow_index::WorkflowIndexError> {
    let (mut workflow_run, mut agents) = match run.meta_path.as_ref().and_then(read_run_meta) {
        // Finished (or at least meta-written) run: authoritative roster from
        // `workflowProgress[]`.
        Some(meta) => parse_run_from_meta(&run.run_id, &run.parent_session_id, &meta),
        // In-progress / orphan dir with no meta json yet: synthesize a Running
        // run and derive the roster from journal.jsonl + present agent files.
        None => parse_run_from_dir(&run.run_id, &run.parent_session_id, &run.agents_dir),
    };

    // Enrich each agent from its transcript (path, tokens, session id, times)
    // and reconcile the run-level agent count with what we actually recorded.
    for agent in &mut agents {
        enrich_agent_from_transcript(agent, &run.agents_dir);
    }
    if workflow_run.agent_count == 0 {
        workflow_run.agent_count = i64::try_from(agents.len()).unwrap_or(i64::MAX);
    }

    db.workflow_upsert_run(&workflow_run).await?;
    let mut agents_ingested = 0u64;
    for agent in &agents {
        db.workflow_upsert_agent(agent).await?;
        agents_ingested += 1;
    }
    Ok(WorkflowIngestStats {
        runs_ingested: 1,
        agents_ingested,
    })
}

/// Read and JSON-parse a `workflows/<run_id>.json` file, or `None` when it is
/// missing or malformed (fail-open — the run is then treated as dir-only).
fn read_run_meta(path: &PathBuf) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

// ---------------------------------------------------------------------------
// Pure parsing (unit-tested; no disk access below this line).
// ---------------------------------------------------------------------------

/// Build a [`WorkflowRun`] and its agent roster from a parsed run-meta JSON
/// (`workflows/<run_id>.json`).
fn parse_run_from_meta(
    run_id: &str,
    parent_session_id: &str,
    meta: &Value,
) -> (WorkflowRun, Vec<WorkflowAgent>) {
    let run_id = meta
        .get("runId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .unwrap_or(run_id)
        .to_string();

    let name = string_field(meta, "workflowName");
    let description = string_field(meta, "summary").or_else(|| string_field(meta, "description"));
    let phase_json = meta
        .get("phases")
        .filter(|phases| phases.is_array())
        .and_then(|phases| serde_json::to_string(phases).ok());
    let status = meta
        .get("status")
        .and_then(Value::as_str)
        .map_or(WorkflowStatus::Unknown, WorkflowStatus::from_disk);
    let started_ts = run_start_ts(meta);
    let ended_ts = run_end_ts(meta, started_ts);
    let result_summary = run_result_summary(meta);
    let default_model = string_field(meta, "defaultModel");

    let agents = parse_roster(&run_id, meta, default_model.as_deref());
    let agent_count = meta
        .get("agentCount")
        .and_then(Value::as_i64)
        .unwrap_or_else(|| i64::try_from(agents.len()).unwrap_or(i64::MAX));

    (
        WorkflowRun {
            run_id,
            parent_session_id: parent_session_id.to_string(),
            name,
            description,
            phase_json,
            status,
            started_ts,
            ended_ts,
            result_summary,
            agent_count,
        },
        agents,
    )
}

/// Synthesize a Running [`WorkflowRun`] for a dir-only (in-progress / orphan)
/// run and build its roster from `journal.jsonl` plus the agent files present.
fn parse_run_from_dir(
    run_id: &str,
    parent_session_id: &str,
    agents_dir: &Path,
) -> (WorkflowRun, Vec<WorkflowAgent>) {
    let journal = read_journal(agents_dir);
    let agent_ids = roster_agent_ids(agents_dir, &journal);
    let agents: Vec<WorkflowAgent> = agent_ids
        .into_iter()
        .map(|agent_id| WorkflowAgent {
            run_id: run_id.to_string(),
            // No progress row means no human label; the agent id is the stable
            // fallback so drill-down still has a handle.
            agent_label: agent_id.clone(),
            status: journal_agent_status(&journal, &agent_id),
            agent_id,
            phase: None,
            transcript_path: None,
            agent_session_id: None,
            model: None,
            tokens: 0,
            started_ts: None,
            ended_ts: None,
        })
        .collect();

    (
        WorkflowRun {
            run_id: run_id.to_string(),
            parent_session_id: parent_session_id.to_string(),
            name: None,
            description: None,
            phase_json: None,
            status: WorkflowStatus::Running,
            started_ts: None,
            ended_ts: None,
            result_summary: None,
            agent_count: i64::try_from(agents.len()).unwrap_or(i64::MAX),
        },
        agents,
    )
}

/// Extract the agent roster from a run meta's `workflowProgress[]`, keeping only
/// `type == "workflow_agent"` entries (the array also holds `workflow_phase`
/// rows). `default_model` backfills an agent that recorded no `model`.
fn parse_roster(run_id: &str, meta: &Value, default_model: Option<&str>) -> Vec<WorkflowAgent> {
    let Some(progress) = meta.get("workflowProgress").and_then(Value::as_array) else {
        return Vec::new();
    };
    progress
        .iter()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("workflow_agent"))
        .map(|entry| {
            let agent_id = string_field(entry, "agentId").unwrap_or_default();
            let label = string_field(entry, "label")
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| {
                    if agent_id.is_empty() {
                        "agent".to_string()
                    } else {
                        agent_id.clone()
                    }
                });
            let status = entry
                .get("state")
                .and_then(Value::as_str)
                .map_or(WorkflowStatus::Unknown, WorkflowStatus::from_disk);
            WorkflowAgent {
                run_id: run_id.to_string(),
                agent_label: label,
                agent_id,
                phase: string_field(entry, "phaseTitle"),
                transcript_path: None,
                agent_session_id: None,
                status,
                model: string_field(entry, "model").or_else(|| default_model.map(str::to_string)),
                tokens: 0,
                started_ts: ms_field_to_secs(entry, "startedAt"),
                ended_ts: ms_field_to_secs(entry, "lastProgressAt"),
            }
        })
        .collect()
}

/// Run start time in unix seconds: `startTime` is a millisecond epoch; fall back
/// to the ISO-8601 `timestamp`.
fn run_start_ts(meta: &Value) -> Option<i64> {
    ms_field_to_secs(meta, "startTime").or_else(|| {
        meta.get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp)
            .and_then(|secs| i64::try_from(secs).ok())
    })
}

/// Run end time in unix seconds: `started_ts + durationMs/1000` when a duration
/// is recorded, else unknown.
fn run_end_ts(meta: &Value, started_ts: Option<i64>) -> Option<i64> {
    let started = started_ts?;
    let duration_ms = meta.get("durationMs").and_then(Value::as_i64)?;
    Some(started.saturating_add(duration_ms / 1000))
}

/// Prefer the run's dedicated `summary` string; otherwise render `result` (a
/// string or a JSON blob) to a truncated one-line slice, never the whole thing.
fn run_result_summary(meta: &Value) -> Option<String> {
    if let Some(summary) = string_field(meta, "summary") {
        return Some(truncate_one_line(&summary, RESULT_SUMMARY_CAP));
    }
    let result = meta.get("result")?;
    let text = match result {
        Value::Null => return None,
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).ok()?,
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(truncate_one_line(trimmed, RESULT_SUMMARY_CAP))
}

/// Collapse whitespace to single spaces and cap at `max` characters (appending
/// an ellipsis when truncated), so a multi-line result never smears storage.
fn truncate_one_line(text: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let head: String = one_line.chars().take(max.saturating_sub(3)).collect();
        format!("{head}...")
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Read a millisecond-epoch numeric field and convert it to unix seconds.
fn ms_field_to_secs(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64).map(|ms| ms / 1000)
}

// ---------------------------------------------------------------------------
// Agent transcript + journal parsing.
// ---------------------------------------------------------------------------

/// Fill in an agent's transcript-derived fields from
/// `agent-<agentId>.jsonl` when that file exists: absolute `transcript_path`,
/// summed `tokens`, `agent_session_id`, and start/end timestamps. A missing or
/// unreadable transcript leaves the roster-derived values untouched.
fn enrich_agent_from_transcript(agent: &mut WorkflowAgent, agents_dir: &Path) {
    if agent.agent_id.is_empty() {
        return;
    }
    let path = agents_dir.join(format!("agent-{}.jsonl", agent.agent_id));
    if !path.is_file() {
        return;
    }
    agent.transcript_path = Some(path.to_string_lossy().to_string());
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let summary = summarize_transcript(&text);
    if summary.tokens > 0 {
        agent.tokens = summary.tokens;
    }
    if agent.agent_session_id.is_none() {
        agent.agent_session_id = summary.session_id;
    }
    if agent.started_ts.is_none() {
        agent.started_ts = summary.first_ts;
    }
    if summary.last_ts.is_some() {
        agent.ended_ts = summary.last_ts;
    }
}

/// Aggregates extracted from one agent transcript.
#[derive(Debug, Default, PartialEq, Eq)]
struct TranscriptSummary {
    /// Sum of `input_tokens + output_tokens` across assistant `usage` objects.
    tokens: i64,
    session_id: Option<String>,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
}

/// Sum tokens and read the session id / first+last timestamps from a transcript
/// body (one JSON object per line). Malformed lines are skipped.
fn summarize_transcript(body: &str) -> TranscriptSummary {
    let mut summary = TranscriptSummary::default();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        if summary.session_id.is_none() {
            summary.session_id = string_field(&value, "sessionId");
        }
        if let Some(ts) = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp)
            .and_then(|secs| i64::try_from(secs).ok())
        {
            if summary.first_ts.is_none() {
                summary.first_ts = Some(ts);
            }
            summary.last_ts = Some(ts);
        }
        summary.tokens = summary.tokens.saturating_add(line_usage_tokens(&value));
    }
    summary
}

/// Input+output tokens from a transcript line's `message.usage`, or `0` when the
/// line carries no usage (user turns, tool results, meta lines).
fn line_usage_tokens(value: &Value) -> i64 {
    let usage = value
        .get("message")
        .and_then(|message| message.get("usage"))
        .or_else(|| value.get("usage"));
    let Some(usage) = usage else {
        return 0;
    };
    let input = usage
        .get("input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    input.saturating_add(output)
}

/// One `journal.jsonl` event: a `started` / `result` (terminal) marker keyed by
/// `agentId`.
struct JournalEvent {
    event_type: String,
    agent_id: String,
}

/// Parse `journal.jsonl` into its events, skipping malformed lines. Absent
/// journal yields an empty list.
fn read_journal(agents_dir: &Path) -> Vec<JournalEvent> {
    let path = agents_dir.join("journal.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    parse_journal(&text)
}

fn parse_journal(body: &str) -> Vec<JournalEvent> {
    body.lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line.trim()).ok()?;
            let event_type = value.get("type").and_then(Value::as_str)?.to_string();
            let agent_id = value.get("agentId").and_then(Value::as_str)?.to_string();
            if agent_id.is_empty() {
                return None;
            }
            Some(JournalEvent {
                event_type,
                agent_id,
            })
        })
        .collect()
}

/// The set of agent ids for a dir-only run: the union of journal-`started`
/// agents and `agent-<id>.jsonl` files present, so an agent that appears in
/// either source is captured.
fn roster_agent_ids(agents_dir: &Path, journal: &[JournalEvent]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for path in agent_transcripts(agents_dir) {
        if let Some(id) = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .and_then(|stem| stem.strip_prefix("agent-"))
        {
            if !id.is_empty() && !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_string());
            }
        }
    }
    for event in journal {
        if !ids.iter().any(|existing| existing == &event.agent_id) {
            ids.push(event.agent_id.clone());
        }
    }
    ids
}

/// Status of one agent in a dir-only run, inferred from its journal events: a
/// terminal `result` reads as Completed, otherwise Running.
fn journal_agent_status(journal: &[JournalEvent], agent_id: &str) -> WorkflowStatus {
    let mut seen = false;
    for event in journal.iter().filter(|event| event.agent_id == agent_id) {
        seen = true;
        match event.event_type.as_str() {
            "result" | "done" | "completed" => return WorkflowStatus::Completed,
            "error" | "failed" | "blocked" | "interrupted" => return WorkflowStatus::Failed,
            _ => {}
        }
    }
    if seen {
        WorkflowStatus::Running
    } else {
        WorkflowStatus::Unknown
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_meta() -> Value {
        serde_json::json!({
            "runId": "wf_d0bf6fa4-48f",
            "workflowName": "tracedecay-triggering-evals",
            "summary": "Mine real transcripts into a broad eval corpus",
            "status": "completed",
            "startTime": 1_783_142_254_914_i64,
            "durationMs": 983_890_i64,
            "agentCount": 2,
            "defaultModel": "claude-fable-5",
            "phases": [
                {"title": "Mine", "detail": "harvest scenarios"},
                {"title": "Run", "detail": "run it", "model": "fable"}
            ],
            "result": {"scored": 45, "scenarios": 36},
            "workflowProgress": [
                {"type": "workflow_phase", "phaseTitle": "Mine"},
                {
                    "type": "workflow_agent",
                    "label": "mine:claude-transcripts",
                    "phaseTitle": "Mine",
                    "phaseIndex": 1,
                    "agentId": "a17141dbe5a308242",
                    "model": "claude-fable-5",
                    "state": "done",
                    "startedAt": 1_783_142_254_936_i64,
                    "lastProgressAt": 1_783_142_255_936_i64
                },
                {
                    "type": "workflow_agent",
                    "label": "",
                    "phaseTitle": "Run",
                    "agentId": "aa09ec4d07fccc915",
                    "state": "in_progress",
                    "startedAt": 1_783_142_260_000_i64
                }
            ]
        })
    }

    #[test]
    fn parse_run_from_meta_maps_fields_and_folds_status() {
        let (run, agents) = parse_run_from_meta("wf_fallback", "sess-parent", &sample_meta());

        assert_eq!(run.run_id, "wf_d0bf6fa4-48f"); // runId wins over the dir name
        assert_eq!(run.parent_session_id, "sess-parent");
        assert_eq!(run.name.as_deref(), Some("tracedecay-triggering-evals"));
        assert_eq!(run.status, WorkflowStatus::Completed);
        // startTime ms -> secs.
        assert_eq!(run.started_ts, Some(1_783_142_254));
        // started + durationMs/1000.
        assert_eq!(run.ended_ts, Some(1_783_142_254 + 983));
        // agentCount from meta, not roster length.
        assert_eq!(run.agent_count, 2);
        // `summary` present -> used verbatim (one-lined).
        assert_eq!(
            run.result_summary.as_deref(),
            Some("Mine real transcripts into a broad eval corpus")
        );

        // phase_json round-trips as a JSON array of the phases.
        let phases: Value = serde_json::from_str(run.phase_json.as_deref().unwrap()).unwrap();
        assert!(phases.is_array());
        assert_eq!(phases.as_array().unwrap().len(), 2);
        assert_eq!(phases[0]["title"], "Mine");

        // Only the two workflow_agent rows, in order; workflow_phase is dropped.
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].agent_label, "mine:claude-transcripts");
        assert_eq!(agents[0].phase.as_deref(), Some("Mine"));
        assert_eq!(agents[0].status, WorkflowStatus::Completed);
        assert_eq!(agents[0].model.as_deref(), Some("claude-fable-5"));
        assert_eq!(agents[0].started_ts, Some(1_783_142_254));
        assert_eq!(agents[0].ended_ts, Some(1_783_142_255));
        // Empty label falls back to the agent id; missing model backfills from
        // defaultModel; state folds `in_progress` -> Running.
        assert_eq!(agents[1].agent_label, "aa09ec4d07fccc915");
        assert_eq!(agents[1].model.as_deref(), Some("claude-fable-5"));
        assert_eq!(agents[1].status, WorkflowStatus::Running);
    }

    #[test]
    fn result_summary_truncates_json_result_when_no_summary() {
        let mut meta = sample_meta();
        meta.as_object_mut().unwrap().remove("summary");
        let long = "x ".repeat(2000);
        meta.as_object_mut()
            .unwrap()
            .insert("result".to_string(), Value::String(long));
        let summary = run_result_summary(&meta).unwrap();
        assert!(summary.chars().count() <= RESULT_SUMMARY_CAP);
        assert!(summary.ends_with("..."));
        // Whitespace collapsed to single spaces.
        assert!(!summary.contains("  "));
    }

    #[test]
    fn result_summary_prefers_summary_over_result() {
        let meta = sample_meta();
        // Even though `result` is a dict, the `summary` string wins.
        assert_eq!(
            run_result_summary(&meta).as_deref(),
            Some("Mine real transcripts into a broad eval corpus")
        );
    }

    #[test]
    fn roster_extracts_only_workflow_agents() {
        let meta = sample_meta();
        let roster = parse_roster("wf_x", &meta, Some("fallback-model"));
        assert_eq!(roster.len(), 2);
        assert!(roster.iter().all(|agent| agent.run_id == "wf_x"));
        let labels: Vec<&str> = roster.iter().map(|a| a.agent_label.as_str()).collect();
        assert_eq!(labels, vec!["mine:claude-transcripts", "aa09ec4d07fccc915"]);
    }

    #[test]
    fn transcript_tokens_sum_input_and_output() {
        let body = concat!(
            r#"{"type":"user","sessionId":"agent-sess","timestamp":"2026-07-04T05:17:34.967Z","message":{"role":"user","content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-07-04T05:18:00.000Z","message":{"role":"assistant","usage":{"input_tokens":100,"output_tokens":40}}}"#,
            "\n",
            "   \n",
            r#"not json"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-07-04T05:25:32.232Z","message":{"role":"assistant","usage":{"input_tokens":10,"output_tokens":8,"cache_read_input_tokens":999}}}"#,
            "\n",
        );
        let summary = summarize_transcript(body);
        // 100+40 + 10+8 (cache_* excluded).
        assert_eq!(summary.tokens, 158);
        assert_eq!(summary.session_id.as_deref(), Some("agent-sess"));
        assert_eq!(
            summary.first_ts,
            parse_timestamp("2026-07-04T05:17:34.967Z").map(|s| s as i64)
        );
        assert_eq!(
            summary.last_ts,
            parse_timestamp("2026-07-04T05:25:32.232Z").map(|s| s as i64)
        );
    }

    #[test]
    fn dir_only_run_is_running_with_journal_roster() {
        let journal = concat!(
            r#"{"type":"started","agentId":"a1"}"#,
            "\n",
            r#"{"type":"started","agentId":"a2"}"#,
            "\n",
            r#"{"type":"result","agentId":"a1"}"#,
            "\n",
            r#"{"type":"started","agentId":""}"#,
            "\n",
        );
        let events = parse_journal(journal);
        // Empty agentId dropped; three valid events remain.
        assert_eq!(events.len(), 3);
        // a1 has a terminal result -> Completed; a2 only started -> Running.
        assert_eq!(
            journal_agent_status(&events, "a1"),
            WorkflowStatus::Completed
        );
        assert_eq!(journal_agent_status(&events, "a2"), WorkflowStatus::Running);
        assert_eq!(
            journal_agent_status(&events, "absent"),
            WorkflowStatus::Unknown
        );
    }

    #[test]
    fn dir_only_run_from_disk_yields_running_and_roster() {
        let dir = tempfile::tempdir().unwrap();
        let agents_dir = dir.path();
        // Two agent transcripts + a journal naming a3 that has no file yet.
        std::fs::write(
            agents_dir.join("agent-a1.jsonl"),
            format!(
                "{}\n",
                r#"{"sessionId":"s","timestamp":"2026-07-04T05:00:00.000Z","message":{"usage":{"input_tokens":5,"output_tokens":5}}}"#
            ),
        )
        .unwrap();
        std::fs::write(agents_dir.join("agent-a1.meta.json"), "{}").unwrap();
        std::fs::write(agents_dir.join("agent-a2.jsonl"), "\n").unwrap();
        std::fs::write(
            agents_dir.join("journal.jsonl"),
            concat!(
                r#"{"type":"started","agentId":"a1"}"#,
                "\n",
                r#"{"type":"started","agentId":"a3"}"#,
                "\n"
            ),
        )
        .unwrap();

        let (run, mut agents) = parse_run_from_dir("wf_dir", "sess", agents_dir);
        assert_eq!(run.status, WorkflowStatus::Running);
        assert_eq!(run.run_id, "wf_dir");
        // a1, a2 (from files) then a3 (journal-only).
        let mut ids: Vec<String> = agents.iter().map(|a| a.agent_id.clone()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a1", "a2", "a3"]);
        assert_eq!(run.agent_count, 3);

        // Enrichment attaches the transcript path + tokens for a1.
        for agent in &mut agents {
            enrich_agent_from_transcript(agent, agents_dir);
        }
        let a1 = agents.iter().find(|a| a.agent_id == "a1").unwrap();
        assert_eq!(a1.tokens, 10);
        assert!(a1
            .transcript_path
            .as_deref()
            .unwrap()
            .ends_with("agent-a1.jsonl"));
        assert_eq!(a1.agent_session_id.as_deref(), Some("s"));
        // a3 has no file: no transcript path, zero tokens.
        let a3 = agents.iter().find(|a| a.agent_id == "a3").unwrap();
        assert!(a3.transcript_path.is_none());
        assert_eq!(a3.tokens, 0);
    }

    /// Write a `<slug>/<session_id>/` fixture with a parent transcript whose
    /// `cwd` is `project_cwd`, one meta-backed run, and (optionally) one
    /// dir-only run. Returns the `~/.claude/projects` root.
    fn write_fixture(home: &Path, session_id: &str, project_cwd: &Path) -> PathBuf {
        let projects = home.join(".claude").join("projects");
        let slug = projects.join("dummy-slug");
        let session_dir = slug.join(session_id);
        std::fs::create_dir_all(&session_dir).unwrap();

        // Parent transcript records the owning session's cwd.
        std::fs::write(
            slug.join(format!("{session_id}.jsonl")),
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "user",
                    "cwd": project_cwd.to_string_lossy(),
                    "sessionId": session_id,
                    "timestamp": "2026-07-04T05:00:00.000Z",
                })
            ),
        )
        .unwrap();

        // Meta-backed run + one agent transcript.
        let workflows = session_dir.join("workflows");
        std::fs::create_dir_all(&workflows).unwrap();
        std::fs::write(
            workflows.join("wf_meta.json"),
            serde_json::to_string(&sample_meta()).unwrap(),
        )
        .unwrap();
        let run_dir = session_dir
            .join("subagents")
            .join("workflows")
            .join("wf_meta");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::write(
            run_dir.join("agent-a17141dbe5a308242.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "sessionId": "agent-sess",
                    "timestamp": "2026-07-04T05:17:34.967Z",
                    "message": {"usage": {"input_tokens": 100, "output_tokens": 40}},
                })
            ),
        )
        .unwrap();

        // Dir-only (in-progress) run: no workflows/<id>.json.
        let orphan = session_dir
            .join("subagents")
            .join("workflows")
            .join("wf_orphan");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("agent-b1.jsonl"), "\n").unwrap();
        std::fs::write(
            orphan.join("journal.jsonl"),
            format!("{}\n", r#"{"type":"started","agentId":"b1"}"#),
        )
        .unwrap();

        projects
    }

    #[tokio::test]
    async fn sweep_ingests_runs_scoped_to_project_and_is_incremental() {
        let home = tempfile::tempdir().unwrap();
        // `project_root` doubles as the recorded transcript cwd, so path-equality
        // scoping admits the run without needing a real git worktree.
        let project = tempfile::tempdir().unwrap();
        let project_root = project.path().canonicalize().unwrap();
        let projects = write_fixture(home.path(), "sess-1", &project_root);

        let db_file = tempfile::NamedTempFile::new().unwrap();
        let db = GlobalDb::open_at(db_file.path()).await.unwrap();

        let stats = ingest_workflow_runs_from(&db, &project_root, &projects).await;
        // Both the meta run and the dir-only run land.
        assert_eq!(stats.runs_ingested, 2);
        // Meta run: 2 agents; orphan run: 1 agent.
        assert_eq!(stats.agents_ingested, 3);

        // The meta run is owned by sess-1 and reads as completed with its roster.
        let runs = db.workflow_runs_for_session("sess-1", 10).await.unwrap();
        let ids: Vec<&str> = runs.iter().map(|r| r.run_id.as_str()).collect();
        assert!(ids.contains(&"wf_d0bf6fa4-48f")); // runId from meta, not dir name
        assert!(ids.contains(&"wf_orphan"));

        let meta_run = db
            .workflow_run_for_id("wf_d0bf6fa4-48f")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(meta_run.parent_session_id, "sess-1");
        assert_eq!(meta_run.status, WorkflowStatus::Completed);
        let agents = db
            .workflow_agents_for_run("wf_d0bf6fa4-48f", 10)
            .await
            .unwrap();
        assert_eq!(agents.len(), 2);
        // The first agent's transcript enriched tokens (100+40) and its path.
        let enriched = agents
            .iter()
            .find(|a| a.agent_id == "a17141dbe5a308242")
            .unwrap();
        assert_eq!(enriched.tokens, 140);
        assert!(enriched
            .transcript_path
            .as_deref()
            .unwrap()
            .ends_with("agent-a17141dbe5a308242.jsonl"));

        let orphan = db.workflow_run_for_id("wf_orphan").await.unwrap().unwrap();
        assert_eq!(orphan.status, WorkflowStatus::Running);

        // Re-sweep with nothing changed: the watermark short-circuits every run,
        // so no rows are re-ingested.
        let again = ingest_workflow_runs_from(&db, &project_root, &projects).await;
        assert_eq!(again, WorkflowIngestStats::default());
    }

    #[tokio::test]
    async fn sweep_skips_runs_owned_by_a_different_project() {
        let home = tempfile::tempdir().unwrap();
        // The fixture's owning session began in `/somewhere/else`, not the
        // project we sweep for, so its runs must not be ingested.
        let other = tempfile::tempdir().unwrap();
        let projects = write_fixture(home.path(), "sess-x", other.path());

        let target = tempfile::tempdir().unwrap();
        let target_root = target.path().canonicalize().unwrap();

        let db_file = tempfile::NamedTempFile::new().unwrap();
        let db = GlobalDb::open_at(db_file.path()).await.unwrap();

        let stats = ingest_workflow_runs_from(&db, &target_root, &projects).await;
        assert_eq!(stats, WorkflowIngestStats::default());
        assert!(db
            .workflow_runs_for_session("sess-x", 10)
            .await
            .unwrap()
            .is_empty());
    }

    /// Force `path`'s mtime to a fixed unix-second value, so a fixture's
    /// `newest_mtime` is deterministic regardless of wall-clock creation time.
    /// A read-only open covers both files and directories (a write open would
    /// `EISDIR` on a directory).
    fn set_mtime(path: &Path, unix_secs: u64) {
        // `filetime` sets a directory's mtime cross-platform; a read-only
        // `File::open` + `set_times` works on Unix but fails on Windows, where
        // adjusting a directory's timestamps needs backup-semantics access.
        filetime::set_file_mtime(
            path,
            filetime::FileTime::from_unix_time(i64::try_from(unix_secs).unwrap(), 0),
        )
        .unwrap();
    }

    /// Regression: a newer run belonging to a *different* project must not
    /// advance this store's ingest watermark. `discover_runs` walks every
    /// project slug on the machine, but the watermark is persisted per-store; if
    /// an out-of-scope run could raise it, that watermark would leapfrog a
    /// still-changing in-scope run and strand it (a Running run that later
    /// completes would be skipped forever on subsequent sweeps). The watermark
    /// after a sweep must therefore reflect only in-scope runs.
    #[tokio::test]
    async fn other_project_run_does_not_advance_watermark() {
        // Far-future mtime (year ~2100) for the out-of-scope run.
        const FUTURE: u64 = 4_102_444_800;

        let home = tempfile::tempdir().unwrap();

        // Target project: an in-scope owning session recorded at `target_root`.
        let target = tempfile::tempdir().unwrap();
        let target_root = target.path().canonicalize().unwrap();
        let projects = write_fixture(home.path(), "sess-target", &target_root);

        // A second project's session under the same `~/.claude/projects`, owned
        // by a different cwd so it is out of scope for this sweep.
        let other = tempfile::tempdir().unwrap();
        write_fixture(home.path(), "sess-other", other.path());

        // Give the out-of-scope run a far-future mtime. Since `newest_mtime`
        // maxes the meta file in, this run reads as the newest run on disk by a
        // wide margin — exactly the poison the watermark must resist.
        set_mtime(
            &projects
                .join("dummy-slug")
                .join("sess-other")
                .join("workflows")
                .join("wf_meta.json"),
            FUTURE,
        );

        let db_file = tempfile::NamedTempFile::new().unwrap();
        let db = GlobalDb::open_at(db_file.path()).await.unwrap();

        // Sweep the target project only. The in-scope (target) runs are ingested;
        // the out-of-scope (other) runs are not.
        let stats = ingest_workflow_runs_from(&db, &target_root, &projects).await;
        assert_eq!(stats.runs_ingested, 2); // target's wf_meta + wf_orphan
        assert!(db
            .workflow_runs_for_session("sess-other", 10)
            .await
            .unwrap()
            .is_empty());

        // The persisted watermark must reflect only in-scope runs, so it stays
        // well below the out-of-project run's far-future mtime. On the buggy
        // path (watermark advanced before the scope filter) it would equal
        // FUTURE, and the next sweep would strand every target run.
        let watermark =
            read_ingest_watermark(&db.dashboard_connection(), INGEST_WATERMARK_KEY).await;
        assert!(
            watermark > 0 && watermark < i64::try_from(FUTURE).unwrap(),
            "out-of-project run advanced the watermark to {watermark} (>= {FUTURE})"
        );

        // Concretely, the target's still-Running dir-only run is not stranded: a
        // second sweep with an appended agent re-ingests it rather than skipping
        // it on a poisoned watermark.
        let orphan_dir = target_root_orphan_dir(&projects, "sess-target");
        std::fs::write(orphan_dir.join("agent-b2.jsonl"), "\n").unwrap();
        // Bump the run's mtime just past the (correct) watermark so the
        // incremental skip does not legitimately short-circuit it.
        set_mtime(
            &orphan_dir.join("agent-b2.jsonl"),
            u64::try_from(watermark).unwrap() + 60,
        );
        set_mtime(&orphan_dir, u64::try_from(watermark).unwrap() + 60);

        let again = ingest_workflow_runs_from(&db, &target_root, &projects).await;
        assert_eq!(
            again.runs_ingested, 1,
            "the still-Running target run must be re-ingested, not stranded"
        );
    }

    /// Path to a fixture session's dir-only (`wf_orphan`) run directory.
    fn target_root_orphan_dir(projects: &Path, session_id: &str) -> PathBuf {
        projects
            .join("dummy-slug")
            .join(session_id)
            .join("subagents")
            .join("workflows")
            .join("wf_orphan")
    }
}
