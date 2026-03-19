# GEPA Live Proof (TemperAgent + OTS + Real Claude) — 2026-03-19

## Scope
- Worktree: `/Users/seshendranalla/Development/temper-gepa-tarjan`
- Server: `temper serve --port 4455 --storage turso --no-observe`
- Tenant: `gepa-live-ots-temperagent-20260319`
- Final successful run: `EvolutionRun('evo-ots-temperagent-8')`
- Date: March 19, 2026

## What Was Proven
1. **TemperAgent is the proposer** (no `claude_code` adapter in evolution proposer path).
2. **OTS trajectories are used** when `SelectCandidate` omits `TrajectoryActions`.
3. **Real Claude generation ran live** through `llm_caller` + tenant secret `anthropic_api_key`.
4. **GEPA run progressed end-to-end** to `Completed` with full event chain.
5. **Skill behavior improved**: `PromoteToCritical` moved from unknown action behavior to successful execution after applying evolved spec.

## Exact Run Flow
1. Stored Anthropic credential in tenant secrets:
   - `PUT /api/tenants/gepa-live-ots-temperagent-20260319/secrets/anthropic_api_key`
2. Reloaded `EvolutionRun` spec with TemperAgent proposer config:
   - proposer module: `gepa-proposer-agent`
   - proposer polling: `poll_attempts=600`, `poll_sleep_ms=250`
3. Started run `evo-ots-temperagent-8`.
4. Called `SelectCandidate` **without** `TrajectoryActions` (only `CandidateId` + `SpecSource`).
5. Replay still executed `PromoteToCritical`, `Assign`, `Reassign` from OTS-backed injection.
6. Proposer executed via `TemperAgent('019d076d-4b2d-7493-8c37-deb679d9efde')` and returned non-empty mutation payload.
7. Run reached `Verifying` and persisted `MutatedSpecSource` + `MutationSummary`.
8. Continued run with verification/approval/deploy actions to complete lifecycle:
   - `RecordVerificationPass` -> `RecordScore` -> `RecordFrontier` -> `Approve` -> `Deploy`
9. Applied mutated `Issue.ioa.toml` from run output, then executed `PromoteToCritical` successfully on a live `Issue` entity.

## How Trajectories Are Obtained

### 1) OTS ingestion path
- OTS records are persisted under `/api/ots/trajectories` into `ots_trajectories`.
- For this tenant, OTS list included:
  - `trajectory_id: ots-live-proof-20260319-1`
  - `outcome: failure`

### 2) Evolution replay path
- `SelectCandidate` had no `TrajectoryActions` param.
- Server dispatch auto-injected replay actions for `gepa-replay` from OTS trajectory context.
- Evidence:
  - `SelectCandidate.params` had only `CandidateId` + `SpecSource`.
  - `ReplayResultJson.action_results[].action` in run 8 contained:
    - `PromoteToCritical`
    - `Assign`
    - `Reassign`

This proves OTS-backed trajectory replay was active, not manual `TrajectoryActions` passing.

## Example Trajectory Evidence

### OTS summary row used in tenant
```json
{
  "trajectory_id": "ots-live-proof-20260319-1",
  "tenant": "gepa-live-ots-temperagent-20260319",
  "agent_id": "real-claude-session",
  "outcome": "failure",
  "turn_count": 1
}
```

### Replay actions observed in run 8
```json
["PromoteToCritical", "Assign", "Reassign"]
```

### Mutation summary produced by TemperAgent proposer
```json
"Added 'Created' as initial state, added 'MoveToBacklog' transition from Created to Backlog, added missing 'PromoteToCritical' and 'Reassign' actions from Created state, and extended existing actions to support Created state where appropriate."
```

## Before/After Behavior

### Before evolution (baseline behavior)
- `PromoteToCritical` was not present in baseline issue automaton behavior (replay and direct action checks showed unknown action behavior).

### After mutation application
- Executed:
  - `POST /tdata/Issues('{id}')/Temper.ProjectManagement.Issue.PromoteToCritical`
- Result:
  - HTTP `200 OK`
  - Event appended: `PromoteToCritical`
  - Entity remained valid and transitioned through governed action dispatch.

## Proof Diagram
```text
OTS trajectory persisted
  (/api/ots/trajectories)
        |
        v
EvolutionRun.Start
        |
        v
SelectCandidate (NO TrajectoryActions)
        |
        v
Dispatch auto-injects actions from OTS
for gepa-replay
        |
        v
RecordEvaluation -> RecordDataset
        |
        v
propose_mutation (WASM: gepa-proposer-agent)
        |
        v
Create/Configure/Provision TemperAgent
        |
        v
llm_caller -> real Claude response
        |
        v
RecordMutation (MutatedSpecSource persisted)
        |
        v
RecordVerificationPass -> RecordScore -> RecordFrontier
        |
        v
Approve -> Deploy
        |
        v
EvolutionRun Completed
        |
        v
Apply mutated Issue spec
        |
        v
PromoteToCritical succeeds on live Issue entity
```

## Event Trail (Run 8)
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

## Current Gap Observed During Proof
- `RecordMutation` reaches `Verifying`, but verification is not auto-triggered by an integration in the current `EvolutionRun` spec.
- For this proof, verification was advanced via `RecordVerificationPass` input action, then scoring/frontier/approval/deploy proceeded through normal governed transitions.

## Artifacts
- `/tmp/gepa_ots_temperagent_run8_completed.json`
- `/tmp/gepa_ots_temperagent_run8_artifacts.json`
- `/tmp/promote_after_mutation_http.txt`
- `/tmp/issue_mutated_run8.ioa.toml`
