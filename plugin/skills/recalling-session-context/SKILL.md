---
name: recalling-session-context
description: 'Use when retrieving what happened in past agent sessions: full-text transcript recall, scoped/time-filtered grep, lossless session replay, summary-DAG drill-down, or compaction recovery.'
---

# Recalling session context

Climb this ladder cheapest-first; stop as soon as the question is answered. For durable *decisions and facts* (rather than raw conversation), start with `tracedecay:project-memory` instead.

This skill owns the **FTS → LCM** lane of `tracedecay_message_search`: `message_search` is the entry point into raw-message grep, lossless replay, and summary-DAG drill-down (the ladder below). When `message_search` is instead the entry point into durable *facts*, that is `tracedecay:project-memory`'s FTS → fact lane.

## Retrieval ladder

1. **Fast full-text recall → `tracedecay_message_search`** (`query`, optional `provider`, `scope`: `all`|`parents_only`|`subagents_only`, `limit`): FTS over ingested transcripts; returns messages with their session ids — the entry point for the LCM ladder below.
2. **Scoped/filtered grep → `tracedecay_lcm_grep`** (`query`, `scope`: `current`|`session`|`all` — `current`/`session` require `session_id`; `role`, `source`, `start_time`/`end_time`, `sort`: `recency`|`relevance`|`hybrid`): bounded raw-message snippets plus summary text when FTS recall needs role/time/session precision.
3. **Lossless replay → `tracedecay_lcm_load_session`** (`session_id`, `after_store_id` + `limit` for stable pagination, `roles`, `content_offset`/`content_limit`): ordered raw messages of one session; page with `next_cursor` instead of asking for everything at once.
4. **Summary-DAG drill-down:** `tracedecay_lcm_describe` (`session_id`) for the session's raw/summary shape; `tracedecay_lcm_expand` (`target.kind`: `raw_message`|`summary_node`|`external_payload`) to open one node, paging sources via `source_offset`/`source_limit`; `tracedecay_lcm_expand_query` (`query`) to assemble bounded retrieval context for a prompt in one call.
5. **Store inspection → `tracedecay_lcm_status`** (counts, token estimates, DAG depth/compression ratio) when you need to know what the store contains before searching it.

## Cross-session recovery workflow

Use this when an interrupted/compacted task may have hidden work, delegated
subtasks, stale facts, or unpublished git/GitHub state.

1. **Inventory stores first:** `tracedecay_lcm_status` for the active project
   or profile, then `tracedecay_active_project` / `tracedecay_project_context`
   when branch, worktree, or store routing could be stale. Confirm the served
   project root and branch before trusting any transcript hit.
2. **Find candidate sessions:** `tracedecay_message_search` for task nouns,
   branch names, PR numbers, file paths, error strings, and tool names. Search
   both parent and subagent scope when delegation was used.
3. **Narrow with grep:** `tracedecay_lcm_grep` with `sort: "hybrid"` for
   broad recall, then `sort: "recency"` plus `role`, `source`,
   `start_time`/`end_time`, or `session_id` when you need exact status.
4. **Load decisive evidence:** `tracedecay_lcm_load_session` only for the
   shortlisted session ids. Page with `after_store_id` and bound
   `content_limit`; do not dump whole sessions unless the page is still
   ambiguous.
5. **Drill into summaries:** use `tracedecay_lcm_describe`,
   `tracedecay_lcm_expand`, or `tracedecay_lcm_expand_query` when the raw
   messages are summarized away, sources are needed, or compression replay
   looks incomplete.
6. **Verify live state:** transcript facts are leads, not truth. Check git for
   branches, commits, diffs, tags, and worktrees; check `gh`/GitHub for PR,
   issue, review, and CI state; check current tool output for errors that may
   have changed since the transcript.

## Workflow clusters

Group recovered evidence by workflow, not by search result order:

- **Objective:** user ask, acceptance criteria, branch/worktree, target PR or
  issue, and owning session ids.
- **State:** changed files, commits, staged/uncommitted work, pushed refs,
  open PRs, CI/review status, and any active tool/session ids.
- **Evidence:** exact transcript hits, loaded-message pages, summary-node ids,
  git refs, GitHub URLs or numbers, and command outputs used to verify them.
- **Risk:** stale transcript claims, branch/store drift, missing subagent
  finals, failing tests, unresolved review comments, credentials/production
  blockers, and destructive operations avoided.
- **Next action:** the smallest verified continuation step, or the reason no
  safe continuation is possible.

## Stale-fact checks

- Treat "done", "pushed", "merged", "tests pass", "CI green", and "PR ready"
  in transcripts as stale until verified against live git/GitHub/tool state.
- Prefer `git status --short --branch`, `git log --oneline --decorate -n`,
  `git diff --stat`, `git worktree list --porcelain`, `gh pr view`, `gh pr
  checks`, and targeted test commands over transcript assertions.
- If the transcript mentions an exact command or error, rerun the narrowest
  safe command or inspect current logs before reporting it as current.
- If verification is impossible, label the fact `unverified transcript
  evidence` and keep it out of the "current state" summary.

## Guardrails

- Steps 1–5 are read-only. `tracedecay_lcm_compress`, `tracedecay_lcm_preflight`, and `tracedecay_lcm_session_boundary` are **lifecycle-integration tools for host agents** — never invoke them casually during recall.
- For multi-step recall, dispatch scoped read-only subagents by session id, time window, provider, role, or query variant. Subagents must not call lifecycle or repair tools; the parent agent validates cited messages/summaries and produces the final timeline.
- If the LCM store itself looks wrong (missing sessions, broken FTS, stale counts) → `tracedecay_lcm_doctor` (`mode: "diagnose"` first; `repair`/`clean` mutate and need explicit user intent).
- All LCM tools default to `storage_scope: "project_local"`; only pass `hermes_profile` (with an absolute `hermes_home`) when the user asks about a Hermes profile store.
- Do not query `.tracedecay` SQLite databases directly. Use MCP tools first
  and the `tracedecay tool ...` CLI fallback when MCP routing fails; schemas
  are internal and live stores may be WAL-backed or branch-sharded.
- Do not run repair, clean, compression, boundary, or lifecycle tools to
  "recover context" unless the user explicitly asked for store maintenance.

## Handoff

- Durable decisions/facts and persisting new ones → `tracedecay:project-memory`.

## Output

- The recalled messages/summaries with session ids and timestamps, and which rung answered the question.
- For unfinished workflow recovery, report a compact workflow map:
  `objective`, `current verified state`, `evidence`, `unfinished work`,
  `risks/blockers`, and `next action`. Keep transcript-only claims separated
  from live-verified state.
- If any result includes a `tracedecay_metrics:` line, report the savings to the user.
