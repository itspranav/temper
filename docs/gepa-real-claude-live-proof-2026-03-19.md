# GEPA Live Proof (Real Claude Code) — 2026-03-19

## Scope
- Worktree: `/Users/seshendranalla/Development/temper-gepa-tarjan`
- Server: `target/debug/temper serve --port 4455 --storage turso --no-observe`
- Tenant: `gepa-live-real-claude-1`
- EvolutionRun: `evo-real-claude-1`
- Target missing action used for proof: `PromoteToCritical`

## What Was Executed
1. Installed skills on tenant:
   - `project-management`
   - `evolution`
2. Uploaded GEPA WASM modules:
   - `gepa-replay`
   - `gepa-reflective`
   - `gepa-score`
   - `gepa-pareto`
3. Submitted `evolution` specs for this tenant using real `claude_code` adapter (no mock `command` override).
4. Baseline behavior check on `Issue`:
   - `Assign` succeeds
   - `PromoteToCritical` fails (`HTTP 409 Unknown action`)
5. Ran `EvolutionRun` with trajectory below.
6. Observed full evolution event chain to `Completed`.
7. Extracted the real Claude mutation payload and applied evolved `issue.ioa.toml`.
8. Re-ran behavior check:
   - `PromoteToCritical` now succeeds.

## Trajectory Used
```json
[
  {"action":"PromoteToCritical","params":{"Reason":"customer escalation"}},
  {"action":"PromoteToCritical","params":{"Reason":"production incident"}},
  {"action":"Assign","params":{"AgentId":"agent-2"}},
  {"action":"Reassign","params":{"NewAssigneeId":"agent-3"}}
]
```

## Evolution Status Timeline
- `Evaluating` at `2026-03-19T13:27:21.233499+00:00`
- `Proposing` at `2026-03-19T13:27:21.741957+00:00`
- `Verifying` at `2026-03-19T13:28:50.649228+00:00`
- `AwaitingApproval` at `2026-03-19T13:28:50.730661+00:00`
- After `Approve` + `Deploy`: `Completed`

## Event Trail Observed
```text
Created
Start
SelectCandidate
RecordEvaluation
RecordDataset
RecordMutation
RecordVerificationPass
RecordScore
RecordFrontier
Approve
Deploy
```

## Baseline vs Improved Skill

### Before (selected snippets)
```toml
[automaton]
name = "Issue"
states = ["Backlog", "Triage", "Todo", "Planning", "Planned", "InProgress", "InReview", "Done", "Cancelled", "Archived"]
initial = "Backlog"
```

`PromoteToCritical`: absent

```toml
[[action]]
name = "Assign"
from = ["Backlog", "Triage", "Todo", "Planning", "Planned", "InProgress"]
```

```toml
[[action]]
name = "Reassign"
from = ["Backlog", "Triage", "Todo", "Planning", "Planned", "InProgress", "InReview"]
```

### After (real Claude mutation applied)
```toml
[automaton]
name = "Issue"
states = ["Created", "Backlog", "Triage", "Todo", "Planning", "Planned", "InProgress", "InReview", "Done", "Cancelled", "Archived"]
initial = "Created"
```

```toml
[[action]]
name = "MoveToBacklog"
kind = "internal"
from = ["Created"]
to = "Backlog"
```

```toml
[[action]]
name = "PromoteToCritical"
kind = "input"
from = ["Created", "Backlog", "Triage", "Todo"]
effect = "increment priority"
params = ["Reason"]
```

```toml
[[action]]
name = "Assign"
from = ["Created", "Backlog", "Triage", "Todo", "Planning", "Planned", "InProgress"]
```

```toml
[[action]]
name = "Reassign"
from = ["Created", "Backlog", "Triage", "Todo", "Planning", "Planned", "InProgress", "InReview"]
```

## Real Claude Output Behavior
- Real Claude returned mutation content inside `fields.result.result` as markdown text with a JSON code block.
- It did **not** return top-level `MutatedSpecSource` field in callback params.
- `MutationSummary` field was set (`"Find Issue IOA spec"`) while full mutation was in the textual `result` payload.
- We extracted the JSON code block from real Claude output, applied the spec, and validated post-improvement behavior.

## Final Verification
- Baseline: `PromoteToCritical` failed (`Unknown action`).
- Post-evolution + deploy: `PromoteToCritical` succeeded.
- Artifacts: `/tmp/gepa_real_claude_run_artifacts.json`
