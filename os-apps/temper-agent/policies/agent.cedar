// TemperAgent — Cedar Authorization Policies
//
// Controls who can create, configure, and interact with spec-driven agents.
// Callback actions (SandboxReady, ProcessToolCalls, HandleToolResults, RecordResult)
// are permitted for system agents to enable the dispatch pipeline loop.

// --- Creation and Configuration: supervisors and humans only ---

permit(
  principal,
  action in [Action::"create", Action::"Configure", Action::"Provision"],
  resource is TemperAgent
) when {
  ["supervisor", "human"].contains(principal.agent_type)
};

// --- Read and List: any authenticated agent ---

permit(
  principal,
  action in [Action::"read", Action::"list"],
  resource is TemperAgent
);

// --- Dispatch pipeline callbacks: system agents ---
// These actions are triggered by WASM integration callbacks, not by users.
// The system agent identity drives the callback dispatch.

permit(
  principal,
  action in [
    Action::"SandboxReady",
    Action::"ProcessToolCalls",
    Action::"HandleToolResults",
    Action::"RecordTokenUsage",
    Action::"RecordResult",
    Action::"Complete"
  ],
  resource is TemperAgent
) when {
  principal.agent_type == "system"
};

// --- Failure and cancellation: supervisors, humans, and system ---

permit(
  principal,
  action in [Action::"Fail", Action::"Cancel"],
  resource is TemperAgent
) when {
  ["supervisor", "human", "system"].contains(principal.agent_type)
};
