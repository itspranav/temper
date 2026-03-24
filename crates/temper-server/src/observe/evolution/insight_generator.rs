//! Trajectory → InsightRecord pipeline.
//!
//! Aggregates trajectory log entries by (entity_type, action), computes
//! success rates and volumes, then generates `InsightRecord`s using the
//! classification and priority scoring from `temper-evolution`.

use std::collections::{BTreeMap, BTreeSet};

use tracing::instrument;

use temper_evolution::insight::{classify_insight, compute_priority_score};
use temper_evolution::records::{
    FeatureRequestDisposition, FeatureRequestRecord, InsightRecord, InsightSignal,
    PlatformGapCategory, RecordHeader, RecordType,
};

use crate::state::trajectory::TrajectorySource;

/// Build an `InsightRecord` from a signal, recommendation, and optional priority override.
fn build_insight(
    signal: InsightSignal,
    recommendation: String,
    priority_override: Option<f64>,
) -> InsightRecord {
    let priority = priority_override.unwrap_or_else(|| compute_priority_score(&signal));
    let category = classify_insight(&signal);
    InsightRecord {
        header: RecordHeader::new(RecordType::Insight, "insight-generator"),
        category,
        signal,
        recommendation,
        priority_score: priority,
    }
}

/// Aggregated trajectory signal for a (entity_type, action) pair.
struct TrajectorySignal {
    entity_type: String,
    action: String,
    total: u64,
    successes: u64,
    failures: u64,
    authz_denials: u64,
    has_entity_not_found: bool,
    has_submit_spec: bool,
}

/// Generate insights from the current trajectory log.
///
/// Reads the trajectory log, aggregates by (entity_type, action), computes
/// classification and priority, and returns `InsightRecord`s. Also correlates
/// EntitySetNotFound 404 trajectories with SubmitSpec events to detect
/// resolved vs open unmet intents.
#[instrument(skip_all, fields(entry_count = entries.len(), insight_count = tracing::field::Empty))]
pub(crate) fn generate_insights(entries: &[crate::state::TrajectoryEntry]) -> Vec<InsightRecord> {
    if entries.is_empty() {
        tracing::debug!("evolution.insight");
        return Vec::new();
    }
    tracing::info!(entry_count = entries.len(), "evolution.insight");

    // Phase 1: Aggregate by (entity_type, action).
    let mut signals: BTreeMap<(String, String), TrajectorySignal> = BTreeMap::new();

    for entry in entries {
        let key = (entry.entity_type.clone(), entry.action.clone());
        let signal = signals.entry(key).or_insert_with(|| TrajectorySignal {
            entity_type: entry.entity_type.clone(),
            action: entry.action.clone(),
            total: 0,
            successes: 0,
            failures: 0,
            authz_denials: 0,
            has_entity_not_found: false,
            has_submit_spec: false,
        });

        signal.total += 1;
        if entry.success {
            signal.successes += 1;
        } else {
            signal.failures += 1;
        }
        if entry.authz_denied == Some(true) {
            signal.authz_denials += 1;
        }
        if let Some(ref err) = entry.error
            && (err.contains("EntitySetNotFound") || err.contains("entity set not found"))
        {
            signal.has_entity_not_found = true;
        }
        if entry.action == "SubmitSpec" {
            signal.has_submit_spec = true;
        }
    }

    // Phase 2: Cross-reference — find entity types with SubmitSpec events.
    let submitted_types: std::collections::BTreeSet<String> = signals
        .values()
        .filter(|s| s.has_submit_spec)
        .map(|s| s.entity_type.clone())
        .collect();
    tracing::info!(
        signal_count = signals.len(),
        submitted_type_count = submitted_types.len(),
        "evolution.insight"
    );

    // Phase 3: Generate insights.
    let mut insights = Vec::new();

    for signal in signals.values() {
        // Skip very low-volume signals.
        if signal.total < 2 {
            continue;
        }

        let success_rate = if signal.total > 0 {
            signal.successes as f64 / signal.total as f64
        } else {
            0.0
        };

        // Special handling for EntitySetNotFound patterns.
        if signal.has_entity_not_found {
            let resolved = submitted_types.contains(&signal.entity_type);
            let intent_str = format!(
                "Entity type '{}' — {}",
                signal.entity_type,
                if resolved {
                    "spec submitted (resolved)"
                } else {
                    "entity type not found (open unmet intent)"
                }
            );
            let recommendation = if resolved {
                format!(
                    "Spec for '{}' has been submitted. Monitor for approval.",
                    signal.entity_type
                )
            } else {
                format!(
                    "Consider creating '{}' entity type. {} attempts, {:.0}% failure rate.",
                    signal.entity_type,
                    signal.total,
                    (1.0 - success_rate) * 100.0,
                )
            };

            let insight_signal = InsightSignal {
                intent: intent_str,
                volume: signal.total,
                success_rate,
                trend: temper_evolution::Trend::Stable,
                growth_rate: None,
            };

            let priority = if resolved {
                compute_priority_score(&insight_signal) * 0.5
            } else {
                compute_priority_score(&insight_signal).max(0.5)
            };
            let severity = if priority >= 0.7 {
                "high"
            } else if priority >= 0.4 {
                "medium"
            } else {
                "low"
            };
            let category = classify_insight(&insight_signal);
            if resolved {
                tracing::info!(
                    entity_type = %signal.entity_type,
                    action = %signal.action,
                    total = signal.total,
                    success_rate,
                    resolved,
                    priority_score = priority,
                    category = ?category,
                    severity,
                    recommendation = %recommendation,
                    "evolution.pattern"
                );
            } else {
                tracing::warn!(
                    entity_type = %signal.entity_type,
                    action = %signal.action,
                    total = signal.total,
                    success_rate,
                    resolved,
                    priority_score = priority,
                    category = ?category,
                    severity,
                    recommendation = %recommendation,
                    "evolution.pattern"
                );
            }
            insights.push(build_insight(
                insight_signal,
                recommendation,
                Some(priority),
            ));
            continue;
        }

        // Special handling for authz denial patterns.
        if signal.authz_denials > 0 && signal.authz_denials as f64 / signal.total as f64 > 0.3 {
            let intent_str = format!(
                "Action '{}' on '{}' denied {} times",
                signal.action, signal.entity_type, signal.authz_denials,
            );
            let recommendation = format!(
                "Consider adding Cedar permit policy for '{}' on '{}'.",
                signal.action, signal.entity_type,
            );

            let insight_signal = InsightSignal {
                intent: intent_str,
                volume: signal.total,
                success_rate,
                trend: temper_evolution::Trend::Stable,
                growth_rate: None,
            };

            let authz_priority = compute_priority_score(&insight_signal);
            let authz_category = classify_insight(&insight_signal);
            let authz_severity = if authz_priority >= 0.7 {
                "high"
            } else if authz_priority >= 0.4 {
                "medium"
            } else {
                "low"
            };
            tracing::warn!(
                entity_type = %signal.entity_type,
                action = %signal.action,
                total = signal.total,
                authz_denials = signal.authz_denials,
                success_rate,
                category = ?authz_category,
                severity = authz_severity,
                recommendation = %recommendation,
                "evolution.pattern"
            );
            insights.push(build_insight(insight_signal, recommendation, None));
            continue;
        }

        // General pattern detection.
        let insight_signal = InsightSignal {
            intent: format!("{}.{}", signal.entity_type, signal.action),
            volume: signal.total,
            success_rate,
            trend: temper_evolution::Trend::Stable,
            growth_rate: None,
        };

        let priority = compute_priority_score(&insight_signal);
        // Only emit insights for meaningful signals.
        if priority < 0.1 {
            continue;
        }

        let category = classify_insight(&insight_signal);
        let recommendation = match category {
            temper_evolution::records::InsightCategory::UnmetIntent => {
                format!(
                    "Action '{}' on '{}' has {:.0}% failure rate ({} attempts). Possible missing feature.",
                    signal.action,
                    signal.entity_type,
                    (1.0 - success_rate) * 100.0,
                    signal.total,
                )
            }
            temper_evolution::records::InsightCategory::Friction => {
                format!(
                    "Action '{}' on '{}' has high volume ({} attempts). Consider simplifying.",
                    signal.action, signal.entity_type, signal.total,
                )
            }
            temper_evolution::records::InsightCategory::Workaround => {
                format!(
                    "Pattern detected on '{}.{}' — {} attempts with {:.0}% success. May be a workaround.",
                    signal.entity_type,
                    signal.action,
                    signal.total,
                    success_rate * 100.0,
                )
            }
            temper_evolution::records::InsightCategory::PlatformGap => {
                format!(
                    "Platform gap: '{}' on '{}' failed {} times. Consider adding this capability.",
                    signal.action, signal.entity_type, signal.total,
                )
            }
        };

        let severity = if priority >= 0.7 {
            "high"
        } else if priority >= 0.4 {
            "medium"
        } else {
            "low"
        };
        tracing::info!(
            entity_type = %signal.entity_type,
            action = %signal.action,
            total = signal.total,
            success_rate,
            priority_score = priority,
            category = ?category,
            severity,
            recommendation = %recommendation,
            "evolution.pattern"
        );

        insights.push(build_insight(
            insight_signal,
            recommendation,
            Some(priority),
        ));
    }

    // Sort by priority (highest first).
    insights.sort_by(|a, b| {
        b.priority_score
            .partial_cmp(&a.priority_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    tracing::Span::current().record("insight_count", insights.len());
    tracing::info!(insight_count = insights.len(), "evolution.insight");
    insights
}

/// A grouped unmet intent from trajectory data.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct UnmetIntent {
    /// Entity type involved.
    pub entity_type: String,
    /// Representative action.
    pub action: String,
    /// Error pattern category.
    pub error_pattern: String,
    /// Number of failures.
    pub failure_count: u64,
    /// First occurrence timestamp.
    pub first_seen: String,
    /// Most recent occurrence timestamp.
    pub last_seen: String,
    /// "open" or "resolved".
    pub status: String,
    /// What resolved it (e.g. spec submission timestamp).
    pub resolved_by: Option<String>,
    /// Recommendation text.
    pub recommendation: String,
    /// Sample request body from the most recent failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_body: Option<serde_json::Value>,
    /// Sample intent from X-Intent header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_intent: Option<String>,
}

/// Accumulator for unmet-intent grouping.
struct UnmetIntentAccum {
    entity_type: String,
    action: String,
    error_pattern: String,
    count: u64,
    first_seen: String,
    last_seen: String,
    sample_body: Option<serde_json::Value>,
    sample_intent: Option<String>,
}

/// Richer unmet-intent evidence derived from recent trajectories.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct IntentEvidenceSummary {
    pub intent_candidates: Vec<IntentCandidate>,
    pub workaround_patterns: Vec<WorkaroundPattern>,
    pub abandonment_patterns: Vec<AbandonmentPattern>,
    pub trajectory_samples: Vec<TrajectorySample>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct IntentCandidate {
    pub intent_key: String,
    pub intent_title: String,
    pub intent_statement: String,
    pub recommended_issue_title: String,
    pub symptom_title: String,
    pub suggested_kind: String,
    pub status: String,
    pub entity_types: Vec<String>,
    pub attempted_actions: Vec<String>,
    pub successful_actions: Vec<String>,
    pub failure_patterns: Vec<String>,
    pub total_count: u64,
    pub failure_count: u64,
    pub success_count: u64,
    pub authz_denials: u64,
    pub workaround_count: u64,
    pub abandonment_count: u64,
    pub success_after_failure_count: u64,
    pub success_rate: f64,
    pub first_seen: String,
    pub last_seen: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_intent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_body: Option<serde_json::Value>,
    pub sample_agents: Vec<String>,
    pub recommendation: String,
    pub problem_statement: String,
    pub logfire_query_hint: serde_json::Value,
    pub evidence_examples: Vec<TrajectorySample>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct WorkaroundPattern {
    pub intent_key: String,
    pub intent_title: String,
    pub failed_actions: Vec<String>,
    pub successful_actions: Vec<String>,
    pub occurrences: u64,
    pub sample_agents: Vec<String>,
    pub last_seen: String,
    pub recommendation: String,
    pub logfire_query_hint: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct AbandonmentPattern {
    pub intent_key: String,
    pub intent_title: String,
    pub failed_actions: Vec<String>,
    pub abandonment_count: u64,
    pub sample_agents: Vec<String>,
    pub first_seen: String,
    pub last_seen: String,
    pub recommendation: String,
    pub logfire_query_hint: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct TrajectorySample {
    pub timestamp: String,
    pub entity_type: String,
    pub action: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

struct IntentCandidateAccum {
    intent_key: String,
    intent_title: String,
    intent_statement: String,
    recommended_issue_title: String,
    symptom_title: String,
    entity_types: BTreeSet<String>,
    attempted_actions: BTreeSet<String>,
    successful_actions: BTreeSet<String>,
    failure_patterns: BTreeSet<String>,
    sample_intent: Option<String>,
    sample_body: Option<serde_json::Value>,
    sample_agents: BTreeSet<String>,
    total_count: u64,
    failure_count: u64,
    success_count: u64,
    authz_denials: u64,
    workaround_count: u64,
    abandonment_count: u64,
    success_after_failure_count: u64,
    first_seen: String,
    last_seen: String,
    evidence_examples: Vec<TrajectorySample>,
}

struct PendingFailure {
    intent_key: String,
    failed_actions: BTreeSet<String>,
    agent_id: Option<String>,
    first_seen: String,
    last_seen: String,
}

struct WorkaroundAccum {
    intent_key: String,
    intent_title: String,
    failed_actions: BTreeSet<String>,
    successful_actions: BTreeSet<String>,
    sample_agents: BTreeSet<String>,
    occurrences: u64,
    last_seen: String,
}

struct AbandonmentAccum {
    intent_key: String,
    intent_title: String,
    failed_actions: BTreeSet<String>,
    sample_agents: BTreeSet<String>,
    abandonment_count: u64,
    first_seen: String,
    last_seen: String,
}

/// Generate unmet intent summaries from trajectory data.
///
/// Groups failed trajectories by error pattern and cross-references with
/// SubmitSpec events to determine open vs resolved status.
///
/// This path is superseded in production by [`generate_unmet_intents_from_aggregated`]
/// (SQL GROUP BY). Retained for unit tests that exercise the aggregation logic
/// against in-memory trajectory slices.
#[cfg_attr(not(test), allow(dead_code))]
#[instrument(skip_all, fields(entry_count = entries.len(), intent_count = tracing::field::Empty))]
pub(crate) fn generate_unmet_intents(
    entries: &[crate::state::TrajectoryEntry],
) -> Vec<UnmetIntent> {
    // Track entity types that have had specs submitted.
    let mut submitted_specs: BTreeMap<String, String> = BTreeMap::new();
    // Track failed patterns by (entity_type, error_pattern).
    let mut failures: BTreeMap<(String, String), UnmetIntentAccum> = BTreeMap::new();

    for entry in entries {
        if entry.action == "SubmitSpec" && entry.success {
            submitted_specs.insert(entry.entity_type.clone(), entry.timestamp.clone());
            continue;
        }

        if !entry.success {
            let error_pattern = categorize_error(entry.error.as_deref());

            // AuthzDenied = governance decision, not a capability gap.
            // These belong in the Decisions view, not Unmet Intents.
            if error_pattern == "AuthzDenied" || entry.authz_denied == Some(true) {
                continue;
            }

            let key = (entry.entity_type.clone(), error_pattern.clone());
            let accum = failures.entry(key).or_insert_with(|| UnmetIntentAccum {
                entity_type: entry.entity_type.clone(),
                action: entry.action.clone(),
                error_pattern,
                count: 0,
                first_seen: entry.timestamp.clone(),
                last_seen: entry.timestamp.clone(),
                sample_body: None,
                sample_intent: None,
            });
            accum.count += 1;
            accum.last_seen = entry.timestamp.clone();
            // Capture the most recent non-None body/intent as sample.
            if entry.request_body.is_some() {
                accum.sample_body = entry.request_body.clone();
            }
            if entry.intent.is_some() {
                accum.sample_intent = entry.intent.clone();
            }
        }
    }

    let intents: Vec<UnmetIntent> = failures
        .into_values()
        .map(|accum| {
            let resolved = submitted_specs.contains_key(&accum.entity_type);
            let resolved_by = submitted_specs.get(&accum.entity_type).cloned();
            let recommendation = if resolved {
                format!("Spec for '{}' has been submitted.", accum.entity_type)
            } else {
                match accum.error_pattern.as_str() {
                    "EntitySetNotFound" => {
                        format!("Consider creating '{}' entity type.", accum.entity_type,)
                    }
                    _ => format!(
                        "Investigate failures for '{}' (pattern: {}).",
                        accum.entity_type, accum.error_pattern,
                    ),
                }
            };

            UnmetIntent {
                entity_type: accum.entity_type,
                action: accum.action,
                error_pattern: accum.error_pattern,
                failure_count: accum.count,
                first_seen: accum.first_seen,
                last_seen: accum.last_seen,
                status: if resolved {
                    "resolved".to_string()
                } else {
                    "open".to_string()
                },
                resolved_by,
                recommendation,
                sample_body: accum.sample_body,
                sample_intent: accum.sample_intent,
            }
        })
        .collect();
    let open_count = intents.iter().filter(|i| i.status == "open").count();
    let resolved_count = intents.iter().filter(|i| i.status == "resolved").count();
    tracing::Span::current().record("intent_count", intents.len());
    if open_count > 0 {
        tracing::warn!(
            entry_count = entries.len(),
            intents_count = intents.len(),
            open_count,
            resolved_count,
            "unmet_intent"
        );
    } else {
        tracing::info!(
            entry_count = entries.len(),
            intents_count = intents.len(),
            open_count,
            resolved_count,
            "unmet_intent"
        );
    }
    intents
}

/// Generate richer, intent-shaped evidence from recent trajectories.
///
/// Unlike `generate_unmet_intents_from_aggregated`, this path intentionally
/// loads bounded raw trajectories so the evolution analyst can reason about:
/// - explicit caller intents (`X-Intent`)
/// - repeated failures around the same intended outcome
/// - workaround sequences (failure followed by alternate success)
/// - abandonment candidates (failed attempts that never recover)
#[instrument(skip_all, fields(entry_count = entries.len(), candidate_count = tracing::field::Empty))]
pub(crate) fn generate_intent_evidence(
    entries: &[crate::state::TrajectoryEntry],
) -> IntentEvidenceSummary {
    if entries.is_empty() {
        return IntentEvidenceSummary {
            intent_candidates: Vec::new(),
            workaround_patterns: Vec::new(),
            abandonment_patterns: Vec::new(),
            trajectory_samples: Vec::new(),
        };
    }

    let mut sorted_entries = entries.to_vec();
    sorted_entries.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    let mut candidates = BTreeMap::<String, IntentCandidateAccum>::new();
    let mut pending_failures = BTreeMap::<(String, String), PendingFailure>::new();
    let mut workarounds = BTreeMap::<String, WorkaroundAccum>::new();
    let mut abandonments = BTreeMap::<String, AbandonmentAccum>::new();

    for entry in &sorted_entries {
        let intent_key = derive_intent_key(entry);
        let intent_title =
            derive_intent_title(entry.intent.as_deref(), &entry.entity_type, &entry.action);
        let intent_statement = entry
            .intent
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| derive_intent_statement(&entry.entity_type, &entry.action));
        let symptom_title = derive_symptom_title(entry);
        let issue_title = derive_issue_title(
            &intent_title,
            entry.intent.as_deref(),
            &entry.entity_type,
            &entry.action,
        );
        let sample = sample_from_entry(entry);
        let accum = candidates
            .entry(intent_key.clone())
            .or_insert_with(|| IntentCandidateAccum {
                intent_key: intent_key.clone(),
                intent_title: intent_title.clone(),
                intent_statement: intent_statement.clone(),
                recommended_issue_title: issue_title.clone(),
                symptom_title: symptom_title.clone(),
                entity_types: BTreeSet::new(),
                attempted_actions: BTreeSet::new(),
                successful_actions: BTreeSet::new(),
                failure_patterns: BTreeSet::new(),
                sample_intent: None,
                sample_body: None,
                sample_agents: BTreeSet::new(),
                total_count: 0,
                failure_count: 0,
                success_count: 0,
                authz_denials: 0,
                workaround_count: 0,
                abandonment_count: 0,
                success_after_failure_count: 0,
                first_seen: entry.timestamp.clone(),
                last_seen: entry.timestamp.clone(),
                evidence_examples: Vec::new(),
            });

        accum.total_count += 1;
        accum.entity_types.insert(entry.entity_type.clone());
        accum.attempted_actions.insert(entry.action.clone());
        accum.last_seen = entry.timestamp.clone();
        if entry.timestamp < accum.first_seen {
            accum.first_seen = entry.timestamp.clone();
        }
        if let Some(agent_id) = entry
            .agent_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            accum.sample_agents.insert(agent_id.to_string());
        }
        if let Some(intent) = entry
            .intent
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            accum.sample_intent = Some(intent.to_string());
        }
        if entry.request_body.is_some() {
            accum.sample_body = entry.request_body.clone();
        }

        if accum.evidence_examples.len() < 4 || !entry.success {
            accum.evidence_examples.push(sample.clone());
            accum.evidence_examples.truncate(4);
        }

        if entry.success {
            accum.success_count += 1;
            accum.successful_actions.insert(entry.action.clone());
        } else {
            accum.failure_count += 1;
            let error_pattern = categorize_error(entry.error.as_deref());
            accum.failure_patterns.insert(error_pattern);
            if entry.authz_denied == Some(true) {
                accum.authz_denials += 1;
            }
        }

        let actor_key = actor_intent_key(entry);
        if entry.success {
            if let Some(pending) = pending_failures.remove(&(actor_key.clone(), intent_key.clone()))
            {
                if pending
                    .failed_actions
                    .iter()
                    .any(|action| action != &entry.action)
                {
                    accum.workaround_count += 1;
                    accum.success_after_failure_count += 1;
                    let workaround_key = format!(
                        "{}::{}",
                        intent_key,
                        normalize_for_key(&format!(
                            "{}->{}",
                            join_set(&pending.failed_actions),
                            entry.action
                        ))
                    );
                    let workaround =
                        workarounds
                            .entry(workaround_key)
                            .or_insert_with(|| WorkaroundAccum {
                                intent_key: intent_key.clone(),
                                intent_title: intent_title.clone(),
                                failed_actions: pending.failed_actions.clone(),
                                successful_actions: BTreeSet::new(),
                                sample_agents: BTreeSet::new(),
                                occurrences: 0,
                                last_seen: entry.timestamp.clone(),
                            });
                    workaround.occurrences += 1;
                    workaround.last_seen = entry.timestamp.clone();
                    workaround.successful_actions.insert(entry.action.clone());
                    if let Some(agent_id) = pending
                        .agent_id
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                    {
                        workaround.sample_agents.insert(agent_id.to_string());
                    }
                    if let Some(agent_id) = entry
                        .agent_id
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                    {
                        workaround.sample_agents.insert(agent_id.to_string());
                    }
                } else {
                    accum.success_after_failure_count += 1;
                }
            }
        } else {
            let pending = pending_failures
                .entry((actor_key, intent_key.clone()))
                .or_insert_with(|| PendingFailure {
                    intent_key: intent_key.clone(),
                    failed_actions: BTreeSet::new(),
                    agent_id: entry.agent_id.clone(),
                    first_seen: entry.timestamp.clone(),
                    last_seen: entry.timestamp.clone(),
                });
            pending.failed_actions.insert(entry.action.clone());
            pending.last_seen = entry.timestamp.clone();
            if entry.timestamp < pending.first_seen {
                pending.first_seen = entry.timestamp.clone();
            }
        }
    }

    for pending in pending_failures.into_values() {
        if let Some(candidate) = candidates.get_mut(&pending.intent_key) {
            candidate.abandonment_count += 1;
        }
        let abandonment = abandonments
            .entry(pending.intent_key.clone())
            .or_insert_with(|| AbandonmentAccum {
                intent_key: pending.intent_key.clone(),
                intent_title: candidates
                    .get(&pending.intent_key)
                    .map(|value| value.intent_title.clone())
                    .unwrap_or_else(|| "Investigate unmet intent".to_string()),
                failed_actions: BTreeSet::new(),
                sample_agents: BTreeSet::new(),
                abandonment_count: 0,
                first_seen: pending.first_seen.clone(),
                last_seen: pending.last_seen.clone(),
            });
        abandonment.abandonment_count += 1;
        abandonment
            .failed_actions
            .extend(pending.failed_actions.into_iter());
        abandonment.last_seen = pending.last_seen.clone();
        if pending.first_seen < abandonment.first_seen {
            abandonment.first_seen = pending.first_seen.clone();
        }
        if let Some(agent_id) = pending
            .agent_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            abandonment.sample_agents.insert(agent_id.to_string());
        }
    }

    let mut intent_candidates = candidates
        .into_values()
        .filter(|candidate| {
            candidate.failure_count > 0
                || candidate.workaround_count > 0
                || candidate.abandonment_count > 0
        })
        .map(finalize_intent_candidate)
        .collect::<Vec<_>>();
    intent_candidates.sort_by(|a, b| {
        score_intent_candidate(b)
            .cmp(&score_intent_candidate(a))
            .then_with(|| b.last_seen.cmp(&a.last_seen))
    });
    intent_candidates.truncate(12);

    let mut workaround_patterns = workarounds
        .into_values()
        .map(finalize_workaround_pattern)
        .collect::<Vec<_>>();
    workaround_patterns.sort_by(|a, b| {
        b.occurrences
            .cmp(&a.occurrences)
            .then_with(|| b.last_seen.cmp(&a.last_seen))
    });
    workaround_patterns.truncate(8);

    let mut abandonment_patterns = abandonments
        .into_values()
        .map(finalize_abandonment_pattern)
        .collect::<Vec<_>>();
    abandonment_patterns.sort_by(|a, b| {
        b.abandonment_count
            .cmp(&a.abandonment_count)
            .then_with(|| b.last_seen.cmp(&a.last_seen))
    });
    abandonment_patterns.truncate(8);

    let trajectory_samples = sorted_entries
        .iter()
        .rev()
        .take(20)
        .map(sample_from_entry)
        .collect::<Vec<_>>();

    tracing::Span::current().record("candidate_count", intent_candidates.len());

    IntentEvidenceSummary {
        intent_candidates,
        workaround_patterns,
        abandonment_patterns,
        trajectory_samples,
    }
}

fn finalize_intent_candidate(candidate: IntentCandidateAccum) -> IntentCandidate {
    let success_rate = if candidate.total_count == 0 {
        0.0
    } else {
        candidate.success_count as f64 / candidate.total_count as f64
    };
    let suggested_kind = if candidate.authz_denials > 0
        && candidate.authz_denials
            >= candidate
                .failure_count
                .saturating_sub(candidate.success_count)
    {
        "governance_gap".to_string()
    } else if candidate.workaround_count > 0 {
        "workaround".to_string()
    } else if candidate
        .failure_patterns
        .iter()
        .any(|pattern| matches!(pattern.as_str(), "EntitySetNotFound" | "ActionNotFound"))
    {
        "missing_capability".to_string()
    } else {
        "friction".to_string()
    };
    let status = if candidate.failure_count == 0 {
        "resolved"
    } else if candidate.workaround_count > 0 {
        "workaround"
    } else if candidate.success_count > 0 {
        "mixed"
    } else {
        "open"
    }
    .to_string();
    let hint_entity_type = candidate.entity_types.iter().next().cloned();
    let hint_action = candidate.attempted_actions.iter().next().cloned();
    let hint_intent = candidate.sample_intent.clone();
    let recommendation = match suggested_kind.as_str() {
        "governance_gap" => format!(
            "Align policy with the intended '{}' workflow and keep the scope limited to the minimum required principals/resources.",
            candidate.intent_title
        ),
        "workaround" => format!(
            "Promote the successful workaround into a first-class capability for '{}', so users stop relying on alternate action chains.",
            candidate.intent_title
        ),
        "friction" => format!(
            "Collapse the repeated multi-step flow behind '{}' into a simpler supported path.",
            candidate.intent_title
        ),
        _ => format!(
            "Add direct product/spec support for '{}'.",
            candidate.intent_title
        ),
    };
    let problem_statement = match suggested_kind.as_str() {
        "governance_gap" => format!(
            "The intended outcome '{}' is blocked by repeated authorization denials across the current workflow.",
            candidate.intent_statement
        ),
        "workaround" => format!(
            "Users and agents are trying to achieve '{}' and are only succeeding through alternate action paths rather than a direct capability.",
            candidate.intent_statement
        ),
        "friction" => format!(
            "The intended outcome '{}' is possible, but only after repeated retries or unnecessary extra steps.",
            candidate.intent_statement
        ),
        _ => format!(
            "The intended outcome '{}' is not directly supported by the current product/spec surface.",
            candidate.intent_statement
        ),
    };

    IntentCandidate {
        intent_key: candidate.intent_key.clone(),
        intent_title: candidate.intent_title.clone(),
        intent_statement: candidate.intent_statement,
        recommended_issue_title: candidate.recommended_issue_title,
        symptom_title: candidate.symptom_title,
        suggested_kind: suggested_kind.clone(),
        status,
        entity_types: candidate.entity_types.into_iter().collect(),
        attempted_actions: candidate.attempted_actions.iter().cloned().collect(),
        successful_actions: candidate.successful_actions.iter().cloned().collect(),
        failure_patterns: candidate.failure_patterns.iter().cloned().collect(),
        total_count: candidate.total_count,
        failure_count: candidate.failure_count,
        success_count: candidate.success_count,
        authz_denials: candidate.authz_denials,
        workaround_count: candidate.workaround_count,
        abandonment_count: candidate.abandonment_count,
        success_after_failure_count: candidate.success_after_failure_count,
        success_rate,
        first_seen: candidate.first_seen,
        last_seen: candidate.last_seen,
        sample_intent: candidate.sample_intent,
        sample_body: candidate.sample_body,
        sample_agents: candidate.sample_agents.iter().cloned().collect(),
        recommendation,
        problem_statement,
        logfire_query_hint: build_logfire_query_hint(
            &suggested_kind,
            hint_entity_type.as_deref(),
            hint_action.as_deref(),
            hint_intent.as_deref(),
        ),
        evidence_examples: candidate.evidence_examples,
    }
}

fn finalize_workaround_pattern(pattern: WorkaroundAccum) -> WorkaroundPattern {
    WorkaroundPattern {
        intent_key: pattern.intent_key.clone(),
        intent_title: pattern.intent_title.clone(),
        failed_actions: pattern.failed_actions.iter().cloned().collect(),
        successful_actions: pattern.successful_actions.iter().cloned().collect(),
        occurrences: pattern.occurrences,
        sample_agents: pattern.sample_agents.iter().cloned().collect(),
        last_seen: pattern.last_seen,
        recommendation: format!(
            "Inspect '{}' and graduate the successful alternate path into a supported single-step workflow.",
            pattern.intent_title
        ),
        logfire_query_hint: build_logfire_query_hint(
            "alternate_success_paths",
            None,
            pattern.failed_actions.iter().next().map(String::as_str),
            Some(pattern.intent_title.as_str()),
        ),
    }
}

fn finalize_abandonment_pattern(pattern: AbandonmentAccum) -> AbandonmentPattern {
    AbandonmentPattern {
        intent_key: pattern.intent_key.clone(),
        intent_title: pattern.intent_title.clone(),
        failed_actions: pattern.failed_actions.iter().cloned().collect(),
        abandonment_count: pattern.abandonment_count,
        sample_agents: pattern.sample_agents.iter().cloned().collect(),
        first_seen: pattern.first_seen,
        last_seen: pattern.last_seen,
        recommendation: format!(
            "Investigate why '{}' never reaches a successful outcome after the observed failed attempts.",
            pattern.intent_title
        ),
        logfire_query_hint: build_logfire_query_hint(
            "intent_abandonment",
            None,
            pattern.failed_actions.iter().next().map(String::as_str),
            Some(pattern.intent_title.as_str()),
        ),
    }
}

fn sample_from_entry(entry: &crate::state::TrajectoryEntry) -> TrajectorySample {
    TrajectorySample {
        timestamp: entry.timestamp.clone(),
        entity_type: entry.entity_type.clone(),
        action: entry.action.clone(),
        success: entry.success,
        error_pattern: (!entry.success).then(|| categorize_error(entry.error.as_deref())),
        error: entry.error.clone(),
        intent: entry.intent.clone(),
        agent_id: entry.agent_id.clone(),
        session_id: entry.session_id.clone(),
    }
}

fn score_intent_candidate(candidate: &IntentCandidate) -> u64 {
    candidate.failure_count.saturating_mul(4)
        + candidate.workaround_count.saturating_mul(5)
        + candidate.abandonment_count.saturating_mul(4)
        + candidate.authz_denials.saturating_mul(3)
        + candidate.success_after_failure_count.saturating_mul(2)
}

fn actor_intent_key(entry: &crate::state::TrajectoryEntry) -> String {
    let actor = entry
        .session_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            entry
                .agent_id
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "anonymous".to_string());
    format!("{actor}::{}", derive_intent_key(entry))
}

fn derive_intent_key(entry: &crate::state::TrajectoryEntry) -> String {
    if let Some(intent) = entry
        .intent
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        return normalize_for_key(intent);
    }

    if let Some(request_body) = entry.request_body.as_ref() {
        for key in ["intent", "goal", "objective", "Title", "title"] {
            if let Some(value) = request_body.get(key).and_then(serde_json::Value::as_str)
                && !value.trim().is_empty()
            {
                return normalize_for_key(value);
            }
        }
    }

    normalize_for_key(&derive_intent_statement(&entry.entity_type, &entry.action))
}

fn derive_intent_title(sample_intent: Option<&str>, entity_type: &str, action: &str) -> String {
    if let Some(intent) = sample_intent.filter(|value| !value.trim().is_empty()) {
        return title_case(intent);
    }

    let action_lower = action.to_ascii_lowercase();
    let entity = humanize_identifier(entity_type).to_ascii_lowercase();
    if action_lower.starts_with("generate") {
        return format!("Enable {entity} generation");
    }
    if action_lower.starts_with("create") {
        return format!("Enable {entity} creation");
    }
    if let Some(target) = action
        .strip_prefix("MoveTo")
        .or_else(|| action.strip_prefix("moveTo"))
    {
        return format!(
            "Allow {} to reach {}",
            humanize_identifier(entity_type).to_ascii_lowercase(),
            humanize_identifier(target).to_ascii_lowercase()
        );
    }

    format!(
        "Enable {} {} workflow",
        entity,
        humanize_identifier(action).to_ascii_lowercase()
    )
}

fn derive_issue_title(
    intent_title: &str,
    sample_intent: Option<&str>,
    entity_type: &str,
    action: &str,
) -> String {
    if !intent_title.trim().is_empty() {
        return title_case(intent_title);
    }
    if let Some(intent) = sample_intent.filter(|value| !value.trim().is_empty()) {
        return title_case(intent);
    }
    title_case(&derive_intent_statement(entity_type, action))
}

fn derive_intent_statement(entity_type: &str, action: &str) -> String {
    let action_lower = action.to_ascii_lowercase();
    let entity = humanize_identifier(entity_type).to_ascii_lowercase();
    if action_lower.starts_with("generate") {
        return format!("Generate {entity}");
    }
    if action_lower.starts_with("create") {
        return format!("Create {entity}");
    }
    if let Some(target) = action
        .strip_prefix("MoveTo")
        .or_else(|| action.strip_prefix("moveTo"))
    {
        return format!(
            "Move {} to {}",
            entity,
            humanize_identifier(target).to_ascii_lowercase()
        );
    }
    format!(
        "{} {}",
        humanize_identifier(action),
        humanize_identifier(entity_type).to_ascii_lowercase()
    )
}

fn derive_symptom_title(entry: &crate::state::TrajectoryEntry) -> String {
    if entry.success {
        return format!(
            "{} succeeded via {}",
            humanize_identifier(&entry.entity_type),
            humanize_identifier(&entry.action)
        );
    }

    let error_pattern = categorize_error(entry.error.as_deref());
    match error_pattern.as_str() {
        "AuthzDenied" => format!(
            "{} is denied while attempting {}",
            humanize_identifier(&entry.entity_type),
            humanize_identifier(&entry.action)
        ),
        "EntitySetNotFound" => format!(
            "{} is missing for {}",
            humanize_identifier(&entry.entity_type),
            humanize_identifier(&entry.action)
        ),
        _ => format!(
            "{} fails during {}",
            humanize_identifier(&entry.entity_type),
            humanize_identifier(&entry.action)
        ),
    }
}

fn build_logfire_query_hint(
    query_kind: &str,
    entity_type: Option<&str>,
    action: Option<&str>,
    intent_text: Option<&str>,
) -> serde_json::Value {
    let normalized_query_kind = match query_kind {
        "workaround" => "alternate_success_paths",
        "governance_gap" => "intent_failure_cluster",
        other => other,
    };
    let mut hint = serde_json::json!({
        "tool": "logfire_query",
        "query_kind": normalized_query_kind,
        "service_name": "temper-platform",
        "environment": "local",
        "limit": 25,
        "lookback_minutes": 240,
    });
    if let Some(entity_type) = entity_type.filter(|value| !value.trim().is_empty()) {
        hint["entity_type"] = serde_json::json!(entity_type);
    }
    if let Some(action) = action.filter(|value| !value.trim().is_empty()) {
        hint["action"] = serde_json::json!(action);
    }
    if let Some(intent_text) = intent_text.filter(|value| !value.trim().is_empty()) {
        hint["intent_text"] = serde_json::json!(intent_text);
    }
    hint
}

fn normalize_for_key(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn humanize_identifier(value: &str) -> String {
    let mut out = String::new();
    let mut previous_lowercase = false;
    for ch in value.chars() {
        if ch == '_' || ch == '-' {
            if !out.ends_with(' ') {
                out.push(' ');
            }
            previous_lowercase = false;
            continue;
        }
        if ch.is_ascii_uppercase() && previous_lowercase {
            out.push(' ');
        }
        out.push(ch.to_ascii_lowercase());
        previous_lowercase = ch.is_ascii_lowercase();
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn title_case(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            format!(
                "{}{}",
                first.to_ascii_uppercase(),
                chars.as_str().to_ascii_lowercase()
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn join_set(values: &BTreeSet<String>) -> String {
    values.iter().cloned().collect::<Vec<_>>().join(",")
}

/// Minimum number of platform-source trajectory failures before generating a FR-Record.
const FEATURE_REQUEST_THRESHOLD: u64 = 3;

/// Generate feature request records from platform-source trajectories.
///
/// Filters trajectory entries with `source == Some(Platform)`, groups by
/// `(action, error_pattern)`, and creates `FeatureRequestRecord`s for groups
/// that exceed the frequency threshold.
pub(crate) fn generate_feature_requests(
    entries: &[crate::state::TrajectoryEntry],
) -> Vec<FeatureRequestRecord> {
    if entries.is_empty() {
        return Vec::new();
    }

    // Group platform-source failures by (action, error_pattern).
    let mut groups: BTreeMap<(String, String), PlatformGapAccum> = BTreeMap::new();

    for entry in entries {
        // Only consider platform-source trajectories.
        if entry.source != Some(TrajectorySource::Platform) {
            continue;
        }
        if entry.success {
            continue;
        }

        let error_pattern = categorize_error(entry.error.as_deref());

        // AuthzDenied = governance decision, not a feature request.
        if error_pattern == "AuthzDenied" || entry.authz_denied == Some(true) {
            continue;
        }

        let key = (entry.action.clone(), error_pattern.clone());
        let accum = groups.entry(key).or_insert_with(|| PlatformGapAccum {
            action: entry.action.clone(),
            error_pattern,
            description: entry.error.clone().unwrap_or_default(),
            count: 0,
            timestamps: Vec::new(),
        });
        accum.count += 1;
        accum.timestamps.push(entry.timestamp.clone());
    }

    let mut feature_requests = Vec::new();

    for accum in groups.into_values() {
        if accum.count < FEATURE_REQUEST_THRESHOLD {
            continue;
        }

        let category = match accum.error_pattern.as_str() {
            "EntitySetNotFound" => PlatformGapCategory::MissingCapability,
            "ActionNotFound" => PlatformGapCategory::MissingMethod,
            _ => PlatformGapCategory::MissingCapability,
        };

        let description = format!(
            "Agents tried '{}' {} times — {}",
            accum.action, accum.count, accum.description,
        );

        let header = RecordHeader::new(RecordType::FeatureRequest, "insight-generator");
        feature_requests.push(FeatureRequestRecord {
            header,
            category,
            description,
            frequency: accum.count,
            trajectory_refs: accum.timestamps,
            disposition: FeatureRequestDisposition::Open,
            developer_notes: None,
        });
    }

    // Sort by frequency (highest first).
    feature_requests.sort_by_key(|b| std::cmp::Reverse(b.frequency));
    feature_requests
}

/// Accumulator for platform gap grouping.
struct PlatformGapAccum {
    action: String,
    error_pattern: String,
    description: String,
    count: u64,
    timestamps: Vec<String>,
}

/// Categorize an error string into a pattern name.
/// Generate unmet intent summaries from SQL-aggregated trajectory rows.
///
/// Accepts the output of [`ServerState::load_unmet_intent_rows_aggregated`]
/// instead of loading thousands of raw [`TrajectoryEntry`] rows.  The
/// `submitted_specs` map is keyed by entity_type and holds the latest
/// SubmitSpec timestamp for that type.
///
/// Multiple SQL rows that map to the same (entity_type, error_pattern) after
/// [`categorize_error`] are merged by summing counts and taking extreme timestamps.
#[instrument(skip_all, fields(failure_row_count = failures.len(), intent_count = tracing::field::Empty))]
pub(crate) fn generate_unmet_intents_from_aggregated(
    failures: &[temper_store_turso::UnmetIntentAggRow],
    submitted_specs: &std::collections::BTreeMap<String, String>,
) -> Vec<UnmetIntent> {
    // Merge rows whose raw errors collapse to the same category.
    let mut groups: BTreeMap<(String, String), UnmetIntentAccum> = BTreeMap::new();

    for row in failures {
        let error_pattern = categorize_error(row.error.as_deref());
        // AuthzDenied belongs in the Decisions view, not Unmet Intents.
        if error_pattern == "AuthzDenied" {
            continue;
        }
        let key = (row.entity_type.clone(), error_pattern.clone());
        let accum = groups.entry(key).or_insert_with(|| UnmetIntentAccum {
            entity_type: row.entity_type.clone(),
            action: row.action.clone(),
            error_pattern,
            count: 0,
            first_seen: row.first_seen.clone(),
            last_seen: row.last_seen.clone(),
            sample_body: None,
            sample_intent: None,
        });
        accum.count += row.count;
        if row.first_seen < accum.first_seen {
            accum.first_seen = row.first_seen.clone();
        }
        if row.last_seen > accum.last_seen {
            accum.last_seen = row.last_seen.clone();
        }
    }

    let intents: Vec<UnmetIntent> = groups
        .into_values()
        .map(|accum| {
            let resolved = submitted_specs.contains_key(&accum.entity_type);
            let resolved_by = submitted_specs.get(&accum.entity_type).cloned();
            let recommendation = if resolved {
                format!("Spec for '{}' has been submitted.", accum.entity_type)
            } else {
                match accum.error_pattern.as_str() {
                    "EntitySetNotFound" => {
                        format!("Consider creating '{}' entity type.", accum.entity_type)
                    }
                    _ => format!(
                        "Investigate failures for '{}' (pattern: {}).",
                        accum.entity_type, accum.error_pattern,
                    ),
                }
            };
            UnmetIntent {
                entity_type: accum.entity_type,
                action: accum.action,
                error_pattern: accum.error_pattern,
                failure_count: accum.count,
                first_seen: accum.first_seen,
                last_seen: accum.last_seen,
                status: if resolved {
                    "resolved".to_string()
                } else {
                    "open".to_string()
                },
                resolved_by,
                recommendation,
                sample_body: accum.sample_body,
                sample_intent: accum.sample_intent,
            }
        })
        .collect();
    tracing::Span::current().record("intent_count", intents.len());
    intents
}

fn categorize_error(error: Option<&str>) -> String {
    match error {
        Some(e) if e.contains("EntitySetNotFound") || e.contains("entity set not found") => {
            "EntitySetNotFound".to_string()
        }
        Some(e) if e.contains("Authorization denied") || e.contains("authorization denied") => {
            "AuthzDenied".to_string()
        }
        Some(e) if e.contains("ActionNotFound") || e.contains("unknown action") => {
            "ActionNotFound".to_string()
        }
        Some(e) if e.contains("guard") => "GuardRejected".to_string(),
        Some(_) => "Other".to_string(),
        None => "Unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TrajectoryEntry;

    fn entry(entity_type: &str, action: &str, success: bool) -> TrajectoryEntry {
        TrajectoryEntry {
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            tenant: "test".to_string(),
            entity_type: entity_type.to_string(),
            entity_id: "e1".to_string(),
            action: action.to_string(),
            success,
            from_status: None,
            to_status: None,
            error: None,
            agent_id: None,
            session_id: None,
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: None,
            spec_governed: None,
            agent_type: None,
            request_body: None,
            intent: None,
        }
    }

    fn failed_entry(entity_type: &str, action: &str, error: &str) -> TrajectoryEntry {
        TrajectoryEntry {
            error: Some(error.to_string()),
            ..entry(entity_type, action, false)
        }
    }

    fn authz_denied_entry(entity_type: &str, action: &str) -> TrajectoryEntry {
        TrajectoryEntry {
            authz_denied: Some(true),
            ..entry(entity_type, action, false)
        }
    }

    fn platform_failed_entry(entity_type: &str, action: &str, error: &str) -> TrajectoryEntry {
        TrajectoryEntry {
            source: Some(TrajectorySource::Platform),
            ..failed_entry(entity_type, action, error)
        }
    }

    fn failed_entry_with_intent(
        entity_type: &str,
        action: &str,
        error: &str,
        intent: &str,
        agent_id: &str,
        session_id: &str,
    ) -> TrajectoryEntry {
        TrajectoryEntry {
            error: Some(error.to_string()),
            intent: Some(intent.to_string()),
            agent_id: Some(agent_id.to_string()),
            session_id: Some(session_id.to_string()),
            ..entry(entity_type, action, false)
        }
    }

    fn success_entry_with_intent(
        entity_type: &str,
        action: &str,
        intent: &str,
        agent_id: &str,
        session_id: &str,
    ) -> TrajectoryEntry {
        TrajectoryEntry {
            intent: Some(intent.to_string()),
            agent_id: Some(agent_id.to_string()),
            session_id: Some(session_id.to_string()),
            ..entry(entity_type, action, true)
        }
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(generate_insights(&[]).is_empty());
        assert!(generate_unmet_intents(&[]).is_empty());
        assert!(generate_feature_requests(&[]).is_empty());
    }

    #[test]
    fn below_threshold_signals_skipped() {
        // Single entry (total < 2) should produce no insights.
        let entries = vec![entry("Ticket", "Create", true)];
        let insights = generate_insights(&entries);
        assert!(
            insights.is_empty(),
            "signals with total < 2 should be skipped"
        );
    }

    #[test]
    fn entity_set_not_found_open_unmet_intent() {
        let entries = vec![
            failed_entry("Invoice", "Create", "EntitySetNotFound: Invoice"),
            failed_entry("Invoice", "Create", "EntitySetNotFound: Invoice"),
        ];
        let insights = generate_insights(&entries);
        assert!(!insights.is_empty());
        assert!(insights[0].signal.intent.contains("not found"));
        assert!(insights[0].recommendation.contains("Consider creating"));
    }

    #[test]
    fn entity_set_not_found_resolved_by_submit_spec() {
        let entries = vec![
            failed_entry("Invoice", "Create", "EntitySetNotFound: Invoice"),
            failed_entry("Invoice", "Create", "EntitySetNotFound: Invoice"),
            entry("Invoice", "SubmitSpec", true),
        ];
        let insights = generate_insights(&entries);
        assert!(!insights.is_empty());
        let resolved = insights
            .iter()
            .find(|i| i.signal.intent.contains("Invoice"))
            .unwrap();
        assert!(resolved.signal.intent.contains("resolved"));
        assert!(resolved.recommendation.contains("submitted"));
    }

    #[test]
    fn authz_denial_above_threshold_generates_insight() {
        // > 30% authz denials should trigger an insight.
        let mut entries = Vec::new();
        for _ in 0..4 {
            entries.push(authz_denied_entry("Task", "Delete"));
        }
        entries.push(entry("Task", "Delete", true));
        // 4 denials out of 5 = 80% > 30%

        let insights = generate_insights(&entries);
        let denial_insight = insights.iter().find(|i| i.signal.intent.contains("denied"));
        assert!(
            denial_insight.is_some(),
            "should generate authz denial insight"
        );
        assert!(
            denial_insight
                .unwrap()
                .recommendation
                .contains("Cedar permit")
        );
    }

    #[test]
    fn authz_denial_below_threshold_no_special_insight() {
        // 1 denial out of 10 = 10% < 30% — no special authz insight.
        let mut entries = Vec::new();
        entries.push(authz_denied_entry("Task", "Delete"));
        for _ in 0..9 {
            entries.push(entry("Task", "Delete", true));
        }

        let insights = generate_insights(&entries);
        let denial_insight = insights.iter().find(|i| i.signal.intent.contains("denied"));
        assert!(
            denial_insight.is_none(),
            "should not generate authz denial insight below threshold"
        );
    }

    #[test]
    fn insights_sorted_by_priority_descending() {
        let mut entries = Vec::new();
        // High failure rate action.
        for _ in 0..20 {
            entries.push(failed_entry("Order", "Process", "guard rejected"));
        }
        // Low failure rate action.
        for _ in 0..2 {
            entries.push(entry("User", "Login", false));
        }

        let insights = generate_insights(&entries);
        for window in insights.windows(2) {
            assert!(
                window[0].priority_score >= window[1].priority_score,
                "insights should be sorted by priority descending"
            );
        }
    }

    #[test]
    fn feature_requests_empty_for_non_platform_source() {
        let entries = vec![
            failed_entry("Ticket", "Create", "EntitySetNotFound"),
            failed_entry("Ticket", "Create", "EntitySetNotFound"),
            failed_entry("Ticket", "Create", "EntitySetNotFound"),
        ];
        assert!(
            generate_feature_requests(&entries).is_empty(),
            "non-platform source should not generate FRs"
        );
    }

    #[test]
    fn feature_requests_below_threshold_skipped() {
        // 2 failures < FEATURE_REQUEST_THRESHOLD (3)
        let entries = vec![
            platform_failed_entry("Task", "Archive", "EntitySetNotFound"),
            platform_failed_entry("Task", "Archive", "EntitySetNotFound"),
        ];
        assert!(generate_feature_requests(&entries).is_empty());
    }

    #[test]
    fn feature_requests_above_threshold_generated() {
        let entries = vec![
            platform_failed_entry("Report", "Generate", "ActionNotFound: Generate"),
            platform_failed_entry("Report", "Generate", "ActionNotFound: Generate"),
            platform_failed_entry("Report", "Generate", "ActionNotFound: Generate"),
        ];
        let frs = generate_feature_requests(&entries);
        assert_eq!(frs.len(), 1);
        assert!(frs[0].description.contains("Generate"));
        assert_eq!(frs[0].frequency, 3);
    }

    #[test]
    fn unmet_intents_open_vs_resolved() {
        let entries = vec![
            failed_entry("Billing", "Charge", "EntitySetNotFound"),
            failed_entry("Billing", "Charge", "EntitySetNotFound"),
            entry("Billing", "SubmitSpec", true),
        ];
        let intents = generate_unmet_intents(&entries);
        assert!(!intents.is_empty());
        let billing = intents.iter().find(|i| i.entity_type == "Billing").unwrap();
        assert_eq!(billing.status, "resolved");
    }

    #[test]
    fn intent_evidence_prefers_explicit_intent_and_detects_workaround() {
        let entries = vec![
            failed_entry_with_intent(
                "Invoice",
                "GenerateInvoice",
                "EntitySetNotFound: Invoice",
                "Send an invoice to the customer",
                "agent-1",
                "session-1",
            ),
            success_entry_with_intent(
                "InvoiceDraft",
                "CreateDraft",
                "Send an invoice to the customer",
                "agent-1",
                "session-1",
            ),
        ];

        let evidence = generate_intent_evidence(&entries);
        assert_eq!(evidence.intent_candidates.len(), 1);
        assert_eq!(evidence.workaround_patterns.len(), 1);
        assert_eq!(
            evidence.intent_candidates[0].intent_title,
            "Send An Invoice To The Customer"
        );
        assert_eq!(evidence.intent_candidates[0].suggested_kind, "workaround");
        assert_eq!(evidence.intent_candidates[0].workaround_count, 1);
        assert_eq!(evidence.workaround_patterns[0].occurrences, 1);
    }

    #[test]
    fn intent_evidence_marks_abandonment_for_unrecovered_failures() {
        let entries = vec![
            failed_entry_with_intent(
                "Issue",
                "MoveToTodo",
                "Authorization denied",
                "Move issue into active work",
                "worker-1",
                "session-2",
            ),
            failed_entry_with_intent(
                "Issue",
                "MoveToTodo",
                "Authorization denied",
                "Move issue into active work",
                "worker-1",
                "session-2",
            ),
        ];

        let evidence = generate_intent_evidence(&entries);
        assert_eq!(evidence.intent_candidates.len(), 1);
        assert_eq!(evidence.abandonment_patterns.len(), 1);
        assert_eq!(evidence.intent_candidates[0].abandonment_count, 1);
        assert_eq!(
            evidence.intent_candidates[0].suggested_kind,
            "governance_gap"
        );
    }

    #[test]
    fn categorize_error_patterns() {
        assert_eq!(
            categorize_error(Some("EntitySetNotFound: X")),
            "EntitySetNotFound"
        );
        assert_eq!(
            categorize_error(Some("Authorization denied")),
            "AuthzDenied"
        );
        assert_eq!(
            categorize_error(Some("ActionNotFound: Y")),
            "ActionNotFound"
        );
        assert_eq!(categorize_error(Some("guard rejected")), "GuardRejected");
        assert_eq!(categorize_error(Some("something else")), "Other");
        assert_eq!(categorize_error(None), "Unknown");
    }
}
