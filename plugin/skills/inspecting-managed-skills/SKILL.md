---
name: inspecting-managed-skills
description: 'Use when listing or reading agent-managed automation skills or viewing automation run artifacts without mutating them.'
---

# Inspecting managed skills and automation output

The daemon automation loop (skill writer, memory curator, session reflector) drafts managed skills and records durable run artifacts. This skill is the read-only window into that state; every lifecycle change (approve, disable, archive, install) goes through the `tracedecay automation` CLI or the dashboard instead.

## Workflow

1. **List managed skills → `tracedecay_skill_list`** (`state?`: filter by lifecycle state, `include_body?`): metadata, lifecycle state, usage summary, and stale/archive/improvement evidence for every agent-managed skill in the active profile. Start here to see what automation has produced.
2. **Read one skill → `tracedecay_skill_view`** (`id` required, `include_support_files?`): full metadata, body markdown, usage summary, and support files for a single managed skill. Use before recommending approval, edits, or archival.
3. **Read a run artifact → `tracedecay_automation_run_artifact_view`** (`run_id`, `kind`: e.g. `traces`, `feedback`, `generated_evals`, `validation_gate`, `optimizer_diagnosis`, `codex_handoff`): the hash-verified JSON payload of one durable automation run artifact. Find run ids via `tracedecay automation runs list` when needed.

## Guardrails

- All three tools are read-only; none of them approve, edit, or delete anything. For lifecycle changes hand the user the matching CLI commands: `tracedecay automation skills approve|disable|archive|restore <id>` and `tracedecay automation skills install --target <host> --output <path>`.
- Managed skills are distinct from this bundled skill set: they live in the TraceDecay profile store, not in the plugin. Do not edit bundled plugin skills based on managed-skill evidence.

## Handoff

- Running or configuring the automation jobs themselves → `tracedecay automation run` / `tracedecay automation config` (CLI).
- Inspecting session-reflection fact automation outcomes → `tracedecay automation facts list|view|apply|reject` (CLI; mutate only on explicit request).
- Memory fact curation → `tracedecay:project-memory`.

## If tools are deferred or MCP fails

- Deferred (names listed without schemas): load once with ToolSearch —
  `select:tracedecay_skill_list,tracedecay_skill_view,tracedecay_automation_run_artifact_view`
  — then call normally.
- MCP error/timeout/disconnect: same tool, same args, via shell:
  `tracedecay tool skill_list` (see `tracedecay:using-the-cli`).
  Never query `.tracedecay` databases directly; never abandon the graph over transport.

## Deliverable

Do not end this workflow without: the requested skill list, skill body, or
artifact payload, plus the exact CLI command for any
lifecycle action the user should take next. Report any `tracedecay_metrics:`
line to the user.
