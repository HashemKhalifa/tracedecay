# Triggering Eval Scorecard

Source: cached `eval-corpus.jsonl`, `eval-results/*.txt`, and local Claude/Codex transcript telemetry referenced by result `session_id`s. No provider reruns.

## Headline

- Corpus cases: 36
- Expected provider-runs: 54 (`sonnet` 36, `codex` 18)
- Raw result files: 43; scored unique provider-runs: 41
- Missing expected provider-runs: 13
- Retry artifacts used for scoring: runner6-ev-024-sonnet-retry.txt, runner6-ev-030-sonnet-retry.txt

| cut | runs | expected-tool hits | hit rate | mean quality | stalls | stall rate | anti-tool runs |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| provider:sonnet | 29 | 20/61 | 32.8% | 0.7 | 9 | 31.0% | 15 |
| provider:codex | 12 | 11/25 | 44.0% | 0.79 | 0 | 0.0% | 10 |
| project:rsbuild-plugin-react-router (bootstrap present) | 22 | 20/47 | 42.6% | 0.97 | 5 | 22.7% | 13 |
| project:tracedecay (bootstrap broken) | 19 | 11/39 | 28.2% | 0.45 | 4 | 21.1% | 12 |

Quality scale: 0-3 heuristic from expected-tool hit fraction, capped at 1.25 when an anti-tool fired; hard errors/rate limits are 0.

## Missing Runs

- ev-026 codex session-recall tracedecay
- ev-028 sonnet fact-recall tracedecay
- ev-029 sonnet memory-store rsbuild-plugin-react-router
- ev-029 codex memory-store rsbuild-plugin-react-router
- ev-030 codex project-registry rsbuild-plugin-react-router
- ev-032 sonnet branch-graph rsbuild-plugin-react-router
- ev-032 codex branch-graph rsbuild-plugin-react-router
- ev-033 sonnet rename-preview rsbuild-plugin-react-router
- ev-033 codex rename-preview rsbuild-plugin-react-router
- ev-034 sonnet test-risk rsbuild-plugin-react-router
- ev-035 sonnet storage-identity tracedecay
- ev-035 codex storage-identity tracedecay
- ev-036 sonnet memory-status tracedecay

## Category Table

| category | runs | expected-tool hits | hit rate | quality | worst |
| --- | ---: | ---: | ---: | ---: | --- |
| affected-tests | 2 | 3/4 | 75.0% | 0.62 | ev-013/sonnet |
| body | 1 | 0/1 | 0.0% | 0.0 | ev-015/sonnet |
| call-chain | 2 | 1/4 | 25.0% | 0.62 | ev-010/sonnet |
| callers | 3 | 0/6 | 0.0% | 0.0 | ev-008/sonnet |
| commit-history | 2 | 0/4 | 0.0% | 0.0 | ev-023/sonnet |
| context | 5 | 6/13 | 46.2% | 1.0 | ev-002/sonnet |
| diff-review | 3 | 0/6 | 0.0% | 0.0 | ev-022/sonnet |
| grep-literal | 2 | 1/3 | 33.3% | 0.62 | ev-004/sonnet |
| health-complexity | 1 | 1/2 | 50.0% | 1.25 | ev-021/sonnet |
| health-dead-code | 1 | 2/2 | 100.0% | 0.0 | ev-020/sonnet |
| health-hotspots | 1 | 1/2 | 50.0% | 1.25 | ev-019/sonnet |
| impact | 3 | 5/7 | 71.4% | 1.75 | ev-012/sonnet |
| outline | 1 | 1/2 | 50.0% | 1.25 | ev-014/sonnet |
| project-registry | 1 | 0/4 | 0.0% | 0.0 | ev-030/sonnet |
| session-recall | 4 | 3/9 | 33.3% | 0.69 | ev-026/sonnet |
| signature-search | 1 | 0/3 | 0.0% | 0.0 | ev-031/sonnet |
| symbol-lookup | 3 | 2/7 | 28.6% | 0.75 | ev-006/codex |
| type-constructors | 2 | 1/2 | 50.0% | 0.62 | ev-016/sonnet |
| type-field-sites | 1 | 1/1 | 100.0% | 3.0 | ev-017/sonnet |
| type-implementations | 2 | 3/4 | 75.0% | 1.38 | ev-018/codex |

## Clearest Triggering Failures

- ev-002 sonnet context: missing `tracedecay_field_sites`; anti `Bash(grep), Grep`; status `error_max_turns`; lever: runner limits: raise turn budget or shorten prompt/tool-result verbosity.
- ev-003 sonnet context: missing `tracedecay_callers_for, tracedecay_outline`; anti `Grep, Read-whole-file`; status `error_max_turns`; lever: bootstrap: fix TraceDecay MCP startup/index context for the tracedecay repo.
- ev-004 sonnet grep-literal: missing `tracedecay_search`; anti `Bash(grep), Grep`; status `success`; lever: tool description/skill trigger: sharpen scenario wording toward the expected graph tool.
- ev-006 codex symbol-lookup: missing `tracedecay_callers, tracedecay_find_exact_symbol`; anti `Grep`; status `success`; lever: agent steering: forbid post-TraceDecay grep/sed verification when graph answer is sufficient.
- ev-008 codex callers: missing `tracedecay_callers_for, tracedecay_find_exact_symbol`; anti `Bash(rg), Bash(sed), Grep`; status `success`; lever: agent steering: forbid post-TraceDecay grep/sed verification when graph answer is sufficient.
- ev-008 sonnet callers: missing `tracedecay_callers_for, tracedecay_find_exact_symbol`; anti `Grep`; status `success`; lever: bootstrap: fix TraceDecay MCP startup/index context for the tracedecay repo.
- ev-009 sonnet callers: missing `tracedecay_callers, tracedecay_field_sites`; anti `Grep`; status `success`; lever: tool description/skill trigger: sharpen scenario wording toward the expected graph tool.
- ev-010 sonnet call-chain: missing `tracedecay_call_chain, tracedecay_callees`; anti `Read-whole-file`; status `error_max_turns`; lever: bootstrap: fix TraceDecay MCP startup/index context for the tracedecay repo.
- ev-013 sonnet affected-tests: missing `tracedecay_affected`; anti `none`; status `error_max_turns`; lever: bootstrap: fix TraceDecay MCP startup/index context for the tracedecay repo.
- ev-015 sonnet body: missing `tracedecay_body`; anti `Grep`; status `success`; lever: bootstrap: fix TraceDecay MCP startup/index context for the tracedecay repo.

## Expected Tools Never Hit

- `tracedecay_body` expected 2x
- `tracedecay_call_chain` expected 2x
- `tracedecay_callers_for` expected 4x
- `tracedecay_changelog` expected 2x
- `tracedecay_commit_context` expected 2x
- `tracedecay_diff_context` expected 3x
- `tracedecay_find_exact_symbol` expected 4x
- `tracedecay_god_class` expected 1x
- `tracedecay_hotspots` expected 1x
- `tracedecay_lcm_grep` expected 4x
- `tracedecay_lcm_load_session` expected 1x
- `tracedecay_pr_context` expected 2x
- `tracedecay_project_list` expected 1x
- `tracedecay_project_search` expected 2x
- `tracedecay_read` expected 1x
- `tracedecay_signature_search` expected 1x
- `tracedecay_type_hierarchy` expected 1x

## Failure Modes

- Sonnet ran more coverage but stalled often: 29/36 present, 9/29 stalls. Missing tail cases indicate the run stopped before ev-028 through ev-036 coverage completed.
- Codex had stronger explicit TraceDecay tool telemetry on attempted runs: 12/18 present, 11/25 expected-tool hits, but 10/12 attempted Codex runs used anti-tools after graph calls.
- Tracedecay repo bootstrap remains a real A/B confound: tracedecay-project runs have lower hit rate and include MCP/bootstrap fallback behavior; rsbuild runs also regress when agents use native grep/sed as confirmation.
- Some categories are unmeasured rather than bad: fact-recall, memory-store, branch-graph, rename-preview, test-risk, storage-identity, and memory-status are mostly missing because runs never happened.
- Diff-review and session-recall are the clearest steering misses: agents reached for `git status`, `gh`, or final-answer memory instead of `tracedecay_diff_context`, `tracedecay_pr_context`, `tracedecay_lcm_grep`, and `tracedecay_message_search`.

## Recommended Fixes

1. Fix generated agent-managed skill frontmatter: Codex logs repeatedly show missing `description` for `tracedecay-tool-fallbacks`, `skill-writer-evidence-validation`, `isolated-worktree-task-flow`, and `tracedecay-code-context-first`.
2. Add a hard post-tool guard: after a successful TraceDecay result, suppress `rg`, `grep`, `sed`, and whole-file `Read` unless the graph result reports truncation, missing index, or unsupported literal search.
3. Make LCM/memory trigger rules lexical: prompts containing prior session, remember, decision, memory, thread, transcript, or last time should trigger `tracedecay_lcm_grep` or `tracedecay_message_search` before native search.
4. Split bootstrap-sensitive scorecards: report rsbuild-plugin-react-router and tracedecay separately until tracedecay's own MCP bootstrap/index path is stable.
5. Rerun only missing provider-runs plus stalled Sonnet cases after the steering fixes; do not rerun successful Codex cases until anti-tool suppression is in place.

## Codex vs Sonnet

- Codex follows explicit TraceDecay skill/tool names better, but often performs native shell verification afterward.
- Sonnet has broader run coverage and better prose answers, but its tool-event compliance is brittle under max-turn pressure and bootstrap failures.
- Codex needs negative steering against redundant Bash search; Sonnet needs stronger first-tool routing plus shorter tool-result budgets.
