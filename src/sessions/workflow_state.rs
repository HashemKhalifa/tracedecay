use libsql::{params, Connection};
use serde::Serialize;

use crate::global_db::GlobalDb;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkflowStateItem {
    pub status: String,
    pub provider: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub message_id: String,
    pub ordinal: i64,
    pub evidence: String,
}

pub async fn list_unfinished(
    db: &GlobalDb,
    limit: usize,
) -> Result<Vec<WorkflowStateItem>, String> {
    let conn = db.dashboard_connection();
    query_unfinished(&conn, limit).await
}

async fn query_unfinished(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<WorkflowStateItem>, String> {
    let limit = limit.clamp(1, 250) as i64;
    let mut rows = conn
        .query(
            "SELECT provider, session_id, message_id, ordinal, content,
                    COALESCE(snippet_text, ''), COALESCE(metadata_json, '')
             FROM lcm_raw_messages
             WHERE lower(content) LIKE '%session limit%'
                OR lower(content) LIKE '%blocked%'
                OR lower(content) LIKE '%interrupted%'
                OR lower(content) LIKE '%runs:0%'
                OR lower(content) LIKE '%\"runs\":0%'
             ORDER BY COALESCE(timestamp, 0) DESC, store_id DESC
             LIMIT ?1",
            params![limit],
        )
        .await
        .map_err(|e| e.to_string())?;

    let mut out = Vec::new();
    while let Some(row) = rows.next().await.map_err(|e| e.to_string())? {
        let content: String = row.get(4).map_err(|e| e.to_string())?;
        let snippet: String = row.get(5).map_err(|e| e.to_string())?;
        if let Some((status, evidence)) = classify_evidence(&content, &snippet) {
            let metadata_json: String = row.get(6).map_err(|e| e.to_string())?;
            out.push(WorkflowStateItem {
                status,
                provider: row.get(0).map_err(|e| e.to_string())?,
                session_id: row.get(1).map_err(|e| e.to_string())?,
                message_id: row.get(2).map_err(|e| e.to_string())?,
                ordinal: row.get(3).map_err(|e| e.to_string())?,
                task_id: task_id_from_metadata(&metadata_json),
                evidence,
            });
        }
    }
    Ok(out)
}

fn classify_evidence(content: &str, snippet: &str) -> Option<(String, String)> {
    let status = classify_status(content)?;
    let evidence_source = if snippet.trim().is_empty() {
        content
    } else {
        snippet
    };
    Some((status.to_string(), compact_evidence(evidence_source)))
}

fn classify_status(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    if lower.contains("session limit") {
        Some("session limit")
    } else if lower.contains("runs:0") || lower.contains("\"runs\":0") {
        Some("runs:0")
    } else if lower.contains("blocked") {
        Some("blocked")
    } else if lower.contains("interrupted") {
        Some("interrupted")
    } else {
        None
    }
}

fn compact_evidence(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= 180 {
        one_line
    } else {
        format!("{}...", one_line.chars().take(177).collect::<String>())
    }
}

fn task_id_from_metadata(metadata_json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(metadata_json).ok()?;
    ["task_id", "taskId", "task", "id"]
        .into_iter()
        .find_map(|key| value.get(key)?.as_str())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_workflow_states_from_text() {
        for (text, expected) in [
            (
                "Claude hit the session limit while running task",
                "session limit",
            ),
            ("automation blocked on missing credentials", "blocked"),
            ("task interrupted by compaction", "interrupted"),
            ("worker finished with runs:0", "runs:0"),
            (r#"{"runs":0,"status":"queued"}"#, "runs:0"),
        ] {
            let (status, evidence) = classify_evidence(text, "").expect("status");
            assert_eq!(status, expected);
            assert!(!evidence.is_empty());
        }
    }

    #[test]
    fn extracts_task_id_from_metadata() {
        assert_eq!(
            task_id_from_metadata(r#"{"task_id":"task-123"}"#),
            Some("task-123".to_string())
        );
    }
}
