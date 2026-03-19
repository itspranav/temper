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

## How Trajectories Are Obtained (Current Implementation)
There are two trajectory channels in the codebase:

1. Evolution input trajectory (`TrajectoryActions`) — consumed directly by GEPA replay:
   - `EvolutionRun.SelectCandidate` accepts `TrajectoryActions` (`skills/evolution/evolution_run.ioa.toml`).
   - `gepa-replay` reads `TrajectoryActions` from trigger params/state (`wasm-modules/gepa-replay/src/lib.rs`).
   - `gepa-reflective` converts replay `action_results` into reflective triplets (`wasm-modules/gepa-reflective/src/lib.rs`).
2. Full MCP OTS trajectory (`ots_trajectories`) — capture and persistence path:
   - MCP runtime records each execute turn (`crates/temper-mcp/src/runtime.rs::record_execute_turn`).
   - MCP finalizes and POSTs to `/api/ots/trajectories` (`crates/temper-mcp/src/runtime.rs::finalize_trajectory`).
   - Server persists OTS rows (`crates/temper-server/src/observe/evolution/trajectories.rs::handle_post_ots_trajectory`).

For this specific proof run (`tenant=gepa-live-real-claude-1`, `EvolutionRun=evo-real-claude-1`):
- The trajectory used by evolution was the explicit `TrajectoryActions` array in `SelectCandidate`.
- Database verification showed no OTS rows for this tenant (`ots_trajectories` count = `0`).
- So this run proves GEPA with `TrajectoryActions` input, not an automatic OTS->`TrajectoryActions` conversion pipeline.

## Example Reflective Trajectory Record From This Run
Pulled from persisted `RecordDataset` event payload:

```json
{
  "action": "PromoteToCritical",
  "input": "state=Created, action=PromoteToCritical, params={\"Reason\":\"customer escalation\"}",
  "output": "to_state=Created, success=false",
  "feedback": "Action 'PromoteToCritical' failed from 'Created': evaluate_spec not supported by this host. Validate transition topology and target states.",
  "score": 0.0,
  "trajectory_id": "candidate-real-claude-1",
  "turn_id": 0
}
```

## End-to-End Proof Diagram
```text
Proof input (this run):
  SelectCandidate.TrajectoryActions
        |
        v
  gepa-replay WASM
  -> ReplayResultJson (4 attempted, 0 succeeded in this run)
        |
        v
  gepa-reflective WASM
  -> DatasetJson (4 failure triplets)
        |
        v
  claude_code adapter (real local Claude CLI, non-mock)
  -> RecordMutation (real Claude output)
        |
        v
  RecordVerificationPass -> RecordScore -> RecordFrontier
        |
        v
  Approve -> Deploy -> EvolutionRun Completed
        |
        v
  Apply evolved Issue spec and verify behavior directly
  Baseline: PromoteToCritical = 409 Unknown action
  After evolution: PromoteToCritical = success

Parallel capture path (implemented, not the source for this run):
  temper-mcp OTS capture
        -> POST /api/ots/trajectories
        -> ots_trajectories table
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

## What Was Proven vs Not Proven
Proven in this run:
- Real `claude_code` adapter executed (not mock script).
- Full `EvolutionRun` lifecycle reached `Completed`.
- Real mutation content was produced and applied.
- Skill behavior improved end-to-end (`PromoteToCritical` changed from unknown action to success).

Not proven in this run:
- OTS-driven automatic trajectory selection (no OTS rows were present for the proof tenant).
- Replay host semantic correctness for `evaluate_spec` (recorded replay failures were `evaluate_spec not supported by this host`; behavior proof was therefore confirmed by direct before/after action execution on the live spec).

## Final Verification
- Baseline: `PromoteToCritical` failed (`Unknown action`).
- Post-evolution + deploy: `PromoteToCritical` succeeded.
- Artifacts: `/tmp/gepa_real_claude_run_artifacts.json`
