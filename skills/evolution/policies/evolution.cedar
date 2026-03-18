// Evolution Skill Cedar Policies
//
// Governs the autonomy slider for spec evolution: who can start, approve,
// reject, and deploy evolution candidates.

// Anyone can start an evolution run.
permit(principal, action == Action::"Start", resource is EvolutionRun);

// Only humans can approve in full-human mode (default).
permit(principal, action == Action::"Approve", resource is EvolutionRun)
  when { principal.type == "Human" };

// Verified agents can approve low-risk mutations in supervised mode.
permit(principal, action == Action::"Approve", resource is EvolutionRun)
  when { principal.agentTypeVerified == true && resource.autonomy_level == "auto" };

// CRITICAL: Self-approval prohibition — proposer cannot approve own mutation.
forbid(principal, action == Action::"Approve", resource is EvolutionRun)
  when { resource.proposer_agent_id == principal.id };

// Anyone can reject a candidate.
permit(principal, action == Action::"Reject", resource is EvolutionRun);

// Only the system or verified agents can deploy.
permit(principal, action == Action::"Deploy", resource is EvolutionRun)
  when { principal.agentTypeVerified == true };

// Sentinel monitoring — anyone can trigger checks.
permit(principal, action == Action::"CheckSentinel", resource is SentinelMonitor);
permit(principal, action == Action::"AlertsFound", resource is SentinelMonitor);
permit(principal, action == Action::"NoAlerts", resource is SentinelMonitor);
permit(principal, action == Action::"CreateEvolutionRun", resource is SentinelMonitor);
