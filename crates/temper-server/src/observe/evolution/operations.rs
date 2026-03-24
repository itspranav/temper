use std::collections::BTreeMap;
use std::convert::Infallible;

use axum::extract::{Json as ExtractJson, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Json;
use axum::response::sse::{Event, KeepAlive, Sse};
use serde::{Deserialize, Serialize};
use temper_evolution::records::{ImpactAssessment, SolutionOption};
use temper_evolution::{
    AnalysisRecord, Complexity, FeatureRequestDisposition, InsightCategory, InsightRecord,
    InsightSignal, ObservationClass, ObservationRecord, ProblemRecord, RecordHeader, RecordType,
    Severity, SolutionRisk, Trend,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;
use tracing::instrument;

use super::insight_generator;
use crate::authz::require_observe_auth;
use crate::odata::extract_tenant;
use crate::request_context::{AgentContext, extract_agent_context};
use crate::sentinel;
use crate::state::{DispatchExtOptions, ServerState};

/// Persist an evolution record to Turso and return whether persistence succeeded.
async fn persist_evolution_record(
    state: &ServerState,
    record_id: &str,
    record_type: &str,
    status: &str,
    created_by: &str,
    derived_from: Option<&str>,
    data_json: &str,
) -> Result<(), String> {
    let Some(turso) = state.platform_persistent_store() else {
        tracing::debug!(
            record_id,
            record_type,
            status,
            created_by,
            "evolution.store.unavailable"
        );
        return Ok(());
    };
    turso
        .insert_evolution_record(
            record_id,
            record_type,
            status,
            created_by,
            derived_from,
            data_json,
        )
        .await
        .map_err(|e| {
            tracing::warn!(
                record_id,
                record_type,
                status,
                created_by,
                error = %e,
                "evolution.store.write"
            );
            e.to_string()
        })?;
    tracing::info!(
        record_id,
        record_type,
        status,
        created_by,
        derived_from,
        "evolution.store.write"
    );
    Ok(())
}

/// Create an entity in the temper-system tenant, logging a warning on failure.
async fn create_system_entity(
    state: &ServerState,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    params: serde_json::Value,
) {
    let system_tenant = TenantId::new("temper-system");
    if let Err(e) = state
        .dispatch_tenant_action(
            &system_tenant,
            entity_type,
            entity_id,
            action,
            params,
            &AgentContext::system(),
        )
        .await
    {
        tracing::warn!(error = %e, entity_type, entity_id, "failed to create system entity");
    }
}

/// Persist sentinel alerts to Turso and create Observation entities.
async fn persist_alerts(
    state: &ServerState,
    alerts: &[sentinel::SentinelAlert],
) -> Result<Vec<serde_json::Value>, StatusCode> {
    let mut results = Vec::new();
    for alert in alerts {
        tracing::warn!(
            rule = %alert.rule_name,
            record_id = %alert.record.header.id,
            source = %alert.record.source,
            classification = ?alert.record.classification,
            observed_value = ?alert.record.observed_value,
            threshold = ?alert.record.threshold_value,
            "evolution.sentinel"
        );
        let data_json = serde_json::to_string(&alert.record).unwrap_or_default();
        if let Err(e) = persist_evolution_record(
            state,
            &alert.record.header.id,
            "Observation",
            &format!("{:?}", alert.record.header.status),
            &alert.record.header.created_by,
            alert.record.header.derived_from.as_deref(),
            &data_json,
        )
        .await
        {
            tracing::warn!(
                record_id = %alert.record.header.id,
                error = %e,
                "evolution.store.write"
            );
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }

        let obs_id = format!("OBS-{}", sim_uuid());
        create_system_entity(
            state,
            "Observation",
            &obs_id,
            "CreateObservation",
            serde_json::json!({
                "source": alert.record.source,
                "classification": format!("{:?}", alert.record.classification),
                "evidence_query": alert.record.evidence_query,
                "context": serde_json::to_string(&alert.record.context).unwrap_or_default(),
                "tenant": "temper-system",
                "legacy_record_id": alert.record.header.id,
            }),
        )
        .await;

        results.push(serde_json::json!({
            "rule": alert.rule_name,
            "record_id": alert.record.header.id,
            "entity_id": obs_id,
            "source": alert.record.source,
            "classification": alert.record.classification,
            "threshold": alert.record.threshold_value,
            "observed": alert.record.observed_value,
        }));
    }
    Ok(results)
}

/// Persist generated insights to Turso and create Insight entities.
async fn persist_insights(
    state: &ServerState,
    insights: &[temper_evolution::InsightRecord],
) -> Vec<serde_json::Value> {
    let mut results = Vec::new();
    for insight in insights {
        tracing::info!(
            record_id = %insight.header.id,
            category = ?insight.category,
            intent = %insight.signal.intent,
            volume = insight.signal.volume,
            success_rate = insight.signal.success_rate,
            priority_score = insight.priority_score,
            "evolution.insight"
        );
        let data_json = serde_json::to_string(insight).unwrap_or_default();
        if let Err(e) = persist_evolution_record(
            state,
            &insight.header.id,
            "Insight",
            &format!("{:?}", insight.header.status),
            &insight.header.created_by,
            insight.header.derived_from.as_deref(),
            &data_json,
        )
        .await
        {
            tracing::warn!(record_id = %insight.header.id, error = %e, "evolution.store.write");
        }

        let insight_id = format!("INS-{}", sim_uuid());
        create_system_entity(
            state,
            "Insight",
            &insight_id,
            "CreateInsight",
            serde_json::json!({
                "observation_id": "",
                "category": format!("{:?}", insight.category),
                "signal": insight.signal.intent,
                "recommendation": insight.recommendation,
                "priority_score": format!("{:.4}", insight.priority_score),
                "legacy_record_id": insight.header.id,
            }),
        )
        .await;

        results.push(serde_json::json!({
            "record_id": insight.header.id,
            "entity_id": insight_id,
            "category": format!("{:?}", insight.category),
            "intent": insight.signal.intent,
            "priority_score": insight.priority_score,
            "recommendation": insight.recommendation,
        }));
    }
    results
}

#[derive(Debug, Deserialize)]
pub(crate) struct EvolutionAnalyzeRequest {
    pub reason: Option<String>,
    pub source: Option<String>,
    pub trigger_context: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct EvolutionMaterializeRequest {
    pub intent_discovery_id: String,
    pub analysis_json: String,
    pub signal_summary_json: String,
    pub tenant: Option<String>,
    pub reason: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AgentAnalysisPayload {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    findings: Vec<AgentFinding>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct AgentFinding {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    symptom_title: String,
    #[serde(default)]
    intent_title: String,
    #[serde(default)]
    recommended_issue_title: String,
    #[serde(default)]
    intent: String,
    #[serde(default)]
    recommendation: String,
    #[serde(default)]
    priority_score: f64,
    #[serde(default)]
    volume: u64,
    #[serde(default)]
    success_rate: f64,
    #[serde(default)]
    trend: String,
    #[serde(default)]
    requires_spec_change: bool,
    #[serde(default)]
    problem_statement: String,
    #[serde(default)]
    root_cause: String,
    #[serde(default)]
    spec_diff: String,
    #[serde(default)]
    acceptance_criteria: Vec<String>,
    #[serde(default)]
    dedupe_key: String,
    #[serde(default)]
    evidence: serde_json::Value,
}

async fn spawn_intent_discovery(
    state: &ServerState,
    tenant: &TenantId,
    reason: &str,
    source: &str,
    trigger_context: serde_json::Value,
    agent_ctx: &AgentContext,
    await_integration: bool,
) -> Result<(String, crate::entity_actor::EntityResponse), String> {
    let discovery_id = format!("intent-discovery-{}", sim_uuid());
    let response = state
        .dispatch_tenant_action_ext(
            tenant,
            "IntentDiscovery",
            &discovery_id,
            "Trigger",
            serde_json::json!({
                "reason": reason,
                "source": source,
                "trigger_context_json": trigger_context.to_string(),
            }),
            DispatchExtOptions {
                agent_ctx,
                await_integration,
            },
        )
        .await?;
    Ok((discovery_id, response))
}

fn next_system_entity_id(prefix: &str) -> String {
    format!("{prefix}-{}", sim_uuid())
}

fn trend_from_str(value: &str) -> Trend {
    match value.trim().to_ascii_lowercase().as_str() {
        "declining" => Trend::Declining,
        "stable" => Trend::Stable,
        _ => Trend::Growing,
    }
}

fn severity_from_score(score: f64) -> Severity {
    if score >= 0.85 {
        Severity::Critical
    } else if score >= 0.65 {
        Severity::High
    } else if score >= 0.40 {
        Severity::Medium
    } else {
        Severity::Low
    }
}

fn solution_risk_from_score(score: f64) -> SolutionRisk {
    if score >= 0.85 {
        SolutionRisk::High
    } else if score >= 0.65 {
        SolutionRisk::Medium
    } else if score >= 0.35 {
        SolutionRisk::Low
    } else {
        SolutionRisk::None
    }
}

fn complexity_from_finding(finding: &AgentFinding) -> Complexity {
    match finding.kind.trim().to_ascii_lowercase().as_str() {
        "friction" => Complexity::Low,
        "governance_gap" => Complexity::Low,
        "workaround" => Complexity::Medium,
        _ => Complexity::Medium,
    }
}

fn observation_class_for_finding(finding: &AgentFinding) -> ObservationClass {
    match finding.kind.trim().to_ascii_lowercase().as_str() {
        "governance_gap" => ObservationClass::AuthzDenied,
        _ => ObservationClass::Trajectory,
    }
}

fn insight_category_for_finding(finding: &AgentFinding) -> InsightCategory {
    match finding.kind.trim().to_ascii_lowercase().as_str() {
        "friction" => InsightCategory::Friction,
        "workaround" => InsightCategory::Workaround,
        "governance_gap" => InsightCategory::PlatformGap,
        _ => InsightCategory::UnmetIntent,
    }
}

fn issue_priority_level(score: f64) -> i64 {
    if score >= 0.85 {
        1
    } else if score >= 0.65 {
        2
    } else if score >= 0.40 {
        3
    } else {
        4
    }
}

fn preferred_title(candidates: &[&str], fallback: &str) -> String {
    candidates
        .iter()
        .find_map(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
        .unwrap_or_else(|| fallback.to_string())
}

fn finding_symptom_title(finding: &AgentFinding) -> String {
    preferred_title(
        &[
            &finding.symptom_title,
            &finding.title,
            &finding.problem_statement,
        ],
        "Observed workflow symptom",
    )
}

fn finding_intent_title(finding: &AgentFinding) -> String {
    preferred_title(
        &[&finding.intent_title, &finding.intent, &finding.title],
        "Enable unmet intent",
    )
}

fn finding_issue_title(finding: &AgentFinding) -> String {
    preferred_title(
        &[
            &finding.recommended_issue_title,
            &finding.intent_title,
            &finding.title,
            &finding.intent,
            &finding.symptom_title,
        ],
        "Investigate unmet intent",
    )
}

fn default_acceptance_criteria(finding: &AgentFinding) -> Vec<String> {
    if !finding.acceptance_criteria.is_empty() {
        return finding.acceptance_criteria.clone();
    }
    let issue_title = finding_issue_title(finding);
    vec![
        format!(
            "Agents can complete '{}' without the current failure mode.",
            issue_title
        ),
        "Observe metrics show improved completion for the affected workflow.".to_string(),
    ]
}

fn build_issue_description(summary: &str, finding: &AgentFinding, record_ids: &[String]) -> String {
    let acceptance_criteria = default_acceptance_criteria(finding)
        .into_iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Summary:\n{summary}\n\nIntent Title:\n{}\n\nObserved Symptom:\n{}\n\nIntent:\n{}\n\nRecommendation:\n{}\n\nProblem Statement:\n{}\n\nRoot Cause:\n{}\n\nSpec Diff:\n{}\n\nAcceptance Criteria:\n{}\n\nEvidence:\n{}\n\nEvolution Records:\n{}",
        finding_intent_title(finding),
        finding_symptom_title(finding),
        if finding.intent.is_empty() {
            "No explicit intent supplied."
        } else {
            finding.intent.as_str()
        },
        finding.recommendation,
        if finding.problem_statement.is_empty() {
            "No formal problem statement supplied."
        } else {
            finding.problem_statement.as_str()
        },
        if finding.root_cause.is_empty() {
            "No root cause supplied."
        } else {
            finding.root_cause.as_str()
        },
        if finding.spec_diff.is_empty() {
            "No spec diff supplied."
        } else {
            finding.spec_diff.as_str()
        },
        acceptance_criteria,
        serde_json::to_string_pretty(&finding.evidence).unwrap_or_else(|_| "{}".to_string()),
        record_ids.join(", ")
    )
}

async fn create_issue_for_finding(
    state: &ServerState,
    tenant: &TenantId,
    summary: &str,
    finding: &AgentFinding,
    record_ids: &[String],
) -> Result<String, String> {
    let issue_id = sim_uuid().to_string();
    let now = sim_now().to_rfc3339();
    let description = build_issue_description(summary, finding, record_ids);
    let acceptance_criteria = default_acceptance_criteria(finding).join("\n");
    let issue_title = finding_issue_title(finding);

    state
        .get_or_create_tenant_entity(
            tenant,
            "Issue",
            &issue_id,
            serde_json::json!({
                "Id": issue_id.clone(),
                "Title": issue_title,
                "Description": description,
                "AcceptanceCriteria": acceptance_criteria,
                "Priority": issue_priority_level(finding.priority_score),
                "CreatedAt": now,
                "UpdatedAt": now,
            }),
        )
        .await?;

    let system_ctx = AgentContext::system();
    let _ = state
        .dispatch_tenant_action(
            tenant,
            "Issue",
            &issue_id,
            "SetPriority",
            serde_json::json!({ "level": issue_priority_level(finding.priority_score) }),
            &system_ctx,
        )
        .await;
    let _ = state
        .dispatch_tenant_action(
            tenant,
            "Issue",
            &issue_id,
            "MoveToTriage",
            serde_json::json!({}),
            &system_ctx,
        )
        .await;
    let _ = state
        .dispatch_tenant_action(
            tenant,
            "Issue",
            &issue_id,
            "MoveToTodo",
            serde_json::json!({}),
            &system_ctx,
        )
        .await;

    Ok(issue_id)
}

/// POST /api/evolution/sentinel/check -- trigger sentinel rule evaluation.
///
/// Evaluates all default sentinel rules against current server state.
/// Any triggered rules generate O-Records and store them in the RecordStore.
/// Returns a list of alerts (may be empty if all is healthy).
#[instrument(skip_all, fields(
    otel.name = "POST /api/evolution/sentinel/check",
    trajectory_count = tracing::field::Empty,
    alerts_count = tracing::field::Empty,
    insights_count = tracing::field::Empty,
))]
pub(crate) async fn handle_sentinel_check(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "run_sentinel", "Evolution")?;
    let trajectory_entries = state.load_trajectory_entries(1_000).await;
    tracing::Span::current().record("trajectory_count", trajectory_entries.len());
    tracing::info!(
        trajectory_count = trajectory_entries.len(),
        "evolution.sentinel"
    );

    let rules = sentinel::default_rules();
    let alerts = sentinel::check_rules(&rules, &state, &trajectory_entries);
    tracing::Span::current().record("alerts_count", alerts.len());
    if alerts.is_empty() {
        tracing::info!(rule_count = rules.len(), "evolution.sentinel");
    } else {
        tracing::warn!(
            rule_count = rules.len(),
            alerts_count = alerts.len(),
            "evolution.sentinel"
        );
    }
    let results = persist_alerts(&state, &alerts).await?;
    let analysis_tenant =
        extract_tenant(&headers, &state).unwrap_or_else(|_| TenantId::new("temper-system"));
    let mut discovery_results = Vec::new();
    for alert in &alerts {
        let trigger_context = serde_json::json!({
            "rule_name": alert.rule_name.clone(),
            "observation_record_id": alert.record.header.id.clone(),
            "source": alert.record.source.clone(),
            "classification": format!("{:?}", alert.record.classification),
            "evidence_query": alert.record.evidence_query.clone(),
        });
        match spawn_intent_discovery(
            &state,
            &analysis_tenant,
            &format!("sentinel:{}", alert.rule_name),
            "automated",
            trigger_context,
            &AgentContext::system(),
            false,
        )
        .await
        {
            Ok((entity_id, _)) => discovery_results.push(serde_json::json!({
                "entity_id": entity_id,
                "reason": format!("sentinel:{}", alert.rule_name),
            })),
            Err(e) => {
                tracing::warn!(error = %e, rule = %alert.rule_name, "failed to create IntentDiscovery from sentinel")
            }
        }
    }

    let insights = insight_generator::generate_insights(&trajectory_entries);
    tracing::Span::current().record("insights_count", insights.len());
    tracing::info!(insights_count = insights.len(), "evolution.insight");
    let insight_results = persist_insights(&state, &insights).await;

    // Notify Observe UI that evolution data changed.
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::EvolutionRecords);
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::EvolutionInsights);
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::UnmetIntents);
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::FeatureRequests);

    Ok(Json(serde_json::json!({
        "alerts_count": alerts.len(),
        "alerts": results,
        "intent_discoveries": discovery_results,
        "insights_count": insights.len(),
        "insights": insight_results,
    })))
}

/// GET /observe/evolution/unmet-intents -- grouped unmet intents from trajectories.
///
/// Uses a SQL GROUP BY aggregation instead of loading raw trajectory rows to
/// avoid the OOM-causing bulk-load anti-pattern (previously 10,000 rows on
/// every 15-second Observe UI poll).
#[instrument(skip_all, fields(
    otel.name = "GET /observe/evolution/unmet-intents",
    open_count = tracing::field::Empty,
    resolved_count = tracing::field::Empty,
    total_intents = tracing::field::Empty,
))]
pub(crate) async fn handle_unmet_intents(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "read_evolution", "Evolution")?;
    let (failure_rows, submitted_specs) = state.load_unmet_intent_rows_aggregated().await;
    let intents =
        insight_generator::generate_unmet_intents_from_aggregated(&failure_rows, &submitted_specs);
    let open_count = intents.iter().filter(|i| i.status == "open").count();
    let resolved_count = intents.iter().filter(|i| i.status == "resolved").count();
    tracing::Span::current().record("open_count", open_count);
    tracing::Span::current().record("resolved_count", resolved_count);
    tracing::Span::current().record("total_intents", intents.len());
    if open_count > 0 {
        tracing::warn!(
            open_count,
            resolved_count,
            total = intents.len(),
            "unmet_intent"
        );
    } else {
        tracing::info!(
            open_count,
            resolved_count,
            total = intents.len(),
            "unmet_intent"
        );
    }

    // Per-intent detail at debug level to avoid OTEL span spam on every poll.
    for intent in &intents {
        tracing::debug!(
            entity_type = %intent.entity_type,
            action = %intent.action,
            error_pattern = %intent.error_pattern,
            failure_count = intent.failure_count,
            first_seen = %intent.first_seen,
            last_seen = %intent.last_seen,
            recommendation = %intent.recommendation,
            "unmet_intent.detail"
        );
    }

    Ok(Json(serde_json::json!({
        "intents": intents,
        "open_count": open_count,
        "resolved_count": resolved_count,
    })))
}

/// GET /observe/evolution/intent-evidence -- richer unmet-intent evidence from raw trajectories.
///
/// This endpoint is intentionally distinct from `/unmet-intents`. It uses a
/// bounded raw trajectory read so higher-level analysis can reason about
/// explicit caller intent, workaround sequences, and abandonment patterns
/// without changing the cheaper aggregated UI contract.
#[instrument(skip_all, fields(otel.name = "GET /observe/evolution/intent-evidence"))]
pub(crate) async fn handle_intent_evidence(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "read_evolution", "Evolution")?;
    let trajectory_entries = state.load_trajectory_entries(2_000).await;
    let evidence = insight_generator::generate_intent_evidence(&trajectory_entries);
    Ok(Json(serde_json::to_value(evidence).unwrap_or_else(|_| {
        serde_json::json!({
            "intent_candidates": [],
            "workaround_patterns": [],
            "abandonment_patterns": [],
            "trajectory_samples": [],
        })
    })))
}

/// GET /observe/evolution/feature-requests -- list feature request records from Turso.
///
/// Supports optional `disposition` query parameter to filter by status.
#[instrument(skip_all, fields(otel.name = "GET /observe/evolution/feature-requests"))]
pub(crate) async fn handle_feature_requests(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Query(params): Query<BTreeMap<String, String>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "read_evolution", "Evolution")?;
    let disposition_filter = params.get("disposition").map(|d| d.as_str());

    // Load trajectory entries for feature request generation.
    let trajectory_entries = state.load_trajectory_entries(1_000).await;

    // Query Turso directly (single source of truth).
    let system_tenant = TenantId::new("temper-system");
    if let Some(turso) = state.platform_persistent_store() {
        // First, generate and upsert fresh feature requests from trajectory data.
        let generated = insight_generator::generate_feature_requests(&trajectory_entries);
        for fr in &generated {
            let refs_json =
                serde_json::to_string(&fr.trajectory_refs).unwrap_or_else(|_| "[]".to_string());
            let disp_str = match fr.disposition {
                FeatureRequestDisposition::Open => "Open",
                FeatureRequestDisposition::Acknowledged => "Acknowledged",
                FeatureRequestDisposition::Planned => "Planned",
                FeatureRequestDisposition::WontFix => "WontFix",
                FeatureRequestDisposition::Resolved => "Resolved",
            };
            if let Err(e) = turso
                .upsert_feature_request(
                    &fr.header.id,
                    &format!("{:?}", fr.category),
                    &fr.description,
                    fr.frequency as i64,
                    &refs_json,
                    disp_str,
                    fr.developer_notes.as_deref(),
                )
                .await
            {
                tracing::warn!(error = %e, "failed to upsert feature request to Turso");
            }

            // Also create FeatureRequest entity in temper-system tenant.
            let fr_id = format!("FR-{}", sim_uuid());
            let fr_params = serde_json::json!({
                "category": format!("{:?}", fr.category),
                "description": fr.description,
                "frequency": format!("{}", fr.frequency),
                "developer_notes": fr.developer_notes.clone().unwrap_or_default(),
                "legacy_record_id": fr.header.id,
            });
            if let Err(e) = state
                .dispatch_tenant_action(
                    &system_tenant,
                    "FeatureRequest",
                    &fr_id,
                    "CreateFeatureRequest",
                    fr_params,
                    &AgentContext::system(),
                )
                .await
            {
                tracing::warn!(error = %e, "failed to create FeatureRequest entity");
            }
        }

        // Then read back from Turso with filter.
        match turso.list_feature_requests(disposition_filter).await {
            Ok(rows) => {
                let items: Vec<serde_json::Value> = rows
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "id": r.id,
                            "category": r.category,
                            "description": r.description,
                            "frequency": r.frequency,
                            "trajectory_refs": serde_json::from_str::<serde_json::Value>(&r.trajectory_refs).unwrap_or_default(),
                            "disposition": r.disposition,
                            "developer_notes": r.developer_notes,
                            "created_at": r.created_at,
                        })
                    })
                    .collect();
                let total = items.len();
                return Ok(Json(
                    serde_json::json!({ "feature_requests": items, "total": total }),
                ));
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to query feature requests from Turso");
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
        }
    }

    // No persistent store configured — return empty.
    Ok(Json(
        serde_json::json!({ "feature_requests": [], "total": 0 }),
    ))
}

/// PATCH /observe/evolution/feature-requests/:id -- update disposition + notes in Turso.
///
/// Admin principals bypass Cedar; other principals require "manage_feature_requests"
/// on "FeatureRequest".
#[instrument(skip_all, fields(otel.name = "PATCH /observe/evolution/feature-requests/{id}"))]
pub(crate) async fn handle_update_feature_request(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Cedar authorization: admin/system bypass, others need manage_feature_requests.
    require_observe_auth(
        &state,
        &headers,
        "manage_feature_requests",
        "FeatureRequest",
    )?;

    let disposition = body.get("disposition").and_then(|v| v.as_str());
    let notes = body.get("developer_notes").and_then(|v| v.as_str());

    // Validate disposition if provided.
    if let Some(d) = disposition {
        match d.to_lowercase().as_str() {
            "open" | "acknowledged" | "planned" | "wontfix" | "wont_fix" | "resolved" => {}
            _ => {
                tracing::warn!(disposition = %d, "invalid disposition value");
                return Err(StatusCode::BAD_REQUEST);
            }
        }
    }

    let Some(turso) = state.platform_persistent_store() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    turso
        .update_feature_request(&id, disposition.unwrap_or(""), notes)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to update feature request");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::FeatureRequests);

    Ok(Json(serde_json::json!({
        "id": id,
        "updated": true,
    })))
}

/// POST /api/evolution/analyze -- create and run one IntentDiscovery cycle.
#[instrument(skip_all, fields(otel.name = "POST /api/evolution/analyze"))]
pub(crate) async fn handle_evolution_analyze(
    State(state): State<ServerState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "run_sentinel", "Evolution")?;
    let tenant = extract_tenant(&headers, &state).map_err(|_| StatusCode::BAD_REQUEST)?;
    let payload = if body.is_empty() {
        EvolutionAnalyzeRequest {
            reason: None,
            source: None,
            trigger_context: None,
        }
    } else {
        serde_json::from_slice::<EvolutionAnalyzeRequest>(&body)
            .map_err(|_| StatusCode::BAD_REQUEST)?
    };
    let agent_ctx = extract_agent_context(&headers);
    let reason = payload.reason.unwrap_or_else(|| "manual".to_string());
    let source = payload.source.unwrap_or_else(|| "developer".to_string());
    let trigger_context = payload
        .trigger_context
        .unwrap_or_else(|| serde_json::json!({}));

    let (entity_id, response) = spawn_intent_discovery(
        &state,
        &tenant,
        &reason,
        &source,
        trigger_context,
        &agent_ctx,
        true,
    )
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, tenant = %tenant, "failed to run IntentDiscovery");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    Ok(Json(serde_json::json!({
        "tenant": tenant.as_str(),
        "entity_id": entity_id,
        "success": response.success,
        "status": response.state.status,
        "error": response.error,
        "fields": response.state.fields,
    })))
}

/// POST /api/evolution/materialize -- persist O/P/A/I records and PM issues.
#[instrument(skip_all, fields(otel.name = "POST /api/evolution/materialize"))]
pub(crate) async fn handle_evolution_materialize(
    State(state): State<ServerState>,
    headers: HeaderMap,
    ExtractJson(payload): ExtractJson<EvolutionMaterializeRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_observe_auth(&state, &headers, "run_sentinel", "Evolution")?;
    let tenant = extract_tenant(&headers, &state).map_err(|_| StatusCode::BAD_REQUEST)?;
    let analysis = serde_json::from_str::<AgentAnalysisPayload>(&payload.analysis_json)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let signal_summary = serde_json::from_str::<serde_json::Value>(&payload.signal_summary_json)
        .unwrap_or_else(|_| serde_json::json!({}));
    let system_tenant = TenantId::new("temper-system");
    let summary = if analysis.summary.is_empty() {
        "IntentDiscovery produced structured findings.".to_string()
    } else {
        analysis.summary.clone()
    };

    let mut record_ids = Vec::<String>::new();
    let mut issue_ids = Vec::<String>::new();
    let mut findings_report = Vec::<serde_json::Value>::new();

    for finding in &analysis.findings {
        let mut finding_record_ids = Vec::<String>::new();
        let mut observation_entity_id = String::new();
        let mut derived_from_record_id: Option<String> = None;

        if finding.requires_spec_change {
            let observation = ObservationRecord {
                header: RecordHeader::new(RecordType::Observation, "intent-discovery"),
                source: format!(
                    "intent-discovery:{}",
                    if finding.kind.is_empty() {
                        "analysis"
                    } else {
                        finding.kind.as_str()
                    }
                ),
                classification: observation_class_for_finding(finding),
                evidence_query: format!(
                    "intent discovery {} -> symptom={} intent={}",
                    payload.intent_discovery_id,
                    finding_symptom_title(finding),
                    finding_intent_title(finding)
                ),
                threshold_field: None,
                threshold_value: None,
                observed_value: Some(finding.volume as f64),
                context: serde_json::json!({
                    "tenant": tenant.as_str(),
                    "reason": payload.reason,
                    "source": payload.source,
                    "signal_summary": signal_summary.clone(),
                    "finding": finding,
                }),
            };
            let observation_json = serde_json::to_string(&observation)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            persist_evolution_record(
                &state,
                &observation.header.id,
                "Observation",
                &format!("{:?}", observation.header.status),
                &observation.header.created_by,
                observation.header.derived_from.as_deref(),
                &observation_json,
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            finding_record_ids.push(observation.header.id.clone());
            record_ids.push(observation.header.id.clone());

            observation_entity_id = next_system_entity_id("OBS");
            create_system_entity(
                &state,
                "Observation",
                &observation_entity_id,
                "CreateObservation",
                serde_json::json!({
                    "source": observation.source,
                    "classification": format!("{:?}", observation.classification),
                    "evidence_query": observation.evidence_query,
                    "context": serde_json::to_string(&observation.context).unwrap_or_default(),
                    "tenant": tenant.as_str(),
                    "legacy_record_id": observation.header.id,
                }),
            )
            .await;

            let problem = ProblemRecord {
                header: RecordHeader::new(RecordType::Problem, "intent-discovery")
                    .derived_from(&observation.header.id),
                problem_statement: if finding.problem_statement.is_empty() {
                    format!(
                        "{} blocks intended workflow completion.",
                        finding_intent_title(finding)
                    )
                } else {
                    finding.problem_statement.clone()
                },
                invariants: default_acceptance_criteria(finding),
                constraints: if finding.dedupe_key.is_empty() {
                    Vec::new()
                } else {
                    vec![format!("dedupe_key={}", finding.dedupe_key)]
                },
                impact: ImpactAssessment {
                    affected_users: Some(finding.volume),
                    severity: severity_from_score(finding.priority_score),
                    trend: trend_from_str(&finding.trend),
                },
            };
            let problem_json =
                serde_json::to_string(&problem).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            persist_evolution_record(
                &state,
                &problem.header.id,
                "Problem",
                &format!("{:?}", problem.header.status),
                &problem.header.created_by,
                problem.header.derived_from.as_deref(),
                &problem_json,
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            finding_record_ids.push(problem.header.id.clone());
            record_ids.push(problem.header.id.clone());

            let problem_entity_id = next_system_entity_id("PRB");
            state
                .dispatch_tenant_action(
                    &system_tenant,
                    "Problem",
                    &problem_entity_id,
                    "CreateProblem",
                    serde_json::json!({
                        "observation_id": observation_entity_id,
                        "problem_statement": problem.problem_statement,
                        "severity": problem.impact.severity.to_string(),
                        "invariants": serde_json::to_string(&problem.invariants).unwrap_or_default(),
                    }),
                    &AgentContext::system(),
                )
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            state
                .dispatch_tenant_action(
                    &system_tenant,
                    "Problem",
                    &problem_entity_id,
                    "MarkReviewed",
                    serde_json::json!({}),
                    &AgentContext::system(),
                )
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            let analysis_record = AnalysisRecord {
                header: RecordHeader::new(RecordType::Analysis, "intent-discovery")
                    .derived_from(&problem.header.id),
                root_cause: if finding.root_cause.is_empty() {
                    "IntentDiscovery inferred a missing platform capability.".to_string()
                } else {
                    finding.root_cause.clone()
                },
                options: vec![SolutionOption {
                    description: finding.recommendation.clone(),
                    spec_diff: if finding.spec_diff.is_empty() {
                        "No explicit spec diff supplied.".to_string()
                    } else {
                        finding.spec_diff.clone()
                    },
                    tla_impact: "NONE".to_string(),
                    risk: solution_risk_from_score(finding.priority_score),
                    complexity: complexity_from_finding(finding),
                }],
                recommendation: Some(0),
            };
            let analysis_record_json = serde_json::to_string(&analysis_record)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            persist_evolution_record(
                &state,
                &analysis_record.header.id,
                "Analysis",
                &format!("{:?}", analysis_record.header.status),
                &analysis_record.header.created_by,
                analysis_record.header.derived_from.as_deref(),
                &analysis_record_json,
            )
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            finding_record_ids.push(analysis_record.header.id.clone());
            record_ids.push(analysis_record.header.id.clone());
            derived_from_record_id = Some(analysis_record.header.id.clone());

            let analysis_entity_id = next_system_entity_id("ANL");
            state
                .dispatch_tenant_action(
                    &system_tenant,
                    "Analysis",
                    &analysis_entity_id,
                    "CreateAnalysis",
                    serde_json::json!({
                        "problem_id": problem_entity_id,
                        "root_cause": analysis_record.root_cause,
                        "options": serde_json::to_string(&analysis_record.options).unwrap_or_default(),
                        "recommendation": analysis_record.recommendation.unwrap_or_default().to_string(),
                    }),
                    &AgentContext::system(),
                )
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        }

        let mut insight_header = RecordHeader::new(RecordType::Insight, "intent-discovery");
        if let Some(parent) = derived_from_record_id.as_ref() {
            insight_header = insight_header.derived_from(parent.clone());
        }
        let insight = InsightRecord {
            header: insight_header,
            category: insight_category_for_finding(finding),
            signal: InsightSignal {
                intent: if finding.intent.is_empty() {
                    finding_intent_title(finding)
                } else {
                    finding.intent.clone()
                },
                volume: finding.volume,
                success_rate: finding.success_rate,
                trend: trend_from_str(&finding.trend),
                growth_rate: None,
            },
            recommendation: finding.recommendation.clone(),
            priority_score: finding.priority_score,
        };
        let insight_json =
            serde_json::to_string(&insight).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        persist_evolution_record(
            &state,
            &insight.header.id,
            "Insight",
            &format!("{:?}", insight.header.status),
            &insight.header.created_by,
            insight.header.derived_from.as_deref(),
            &insight_json,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        finding_record_ids.push(insight.header.id.clone());
        record_ids.push(insight.header.id.clone());

        create_system_entity(
            &state,
            "Insight",
            &next_system_entity_id("INS"),
            "CreateInsight",
            serde_json::json!({
                "observation_id": observation_entity_id,
                "category": format!("{:?}", insight.category),
                "signal": insight.signal.intent,
                "recommendation": insight.recommendation,
                "priority_score": format!("{:.4}", insight.priority_score),
                "legacy_record_id": insight.header.id,
            }),
        )
        .await;

        let issue_id =
            create_issue_for_finding(&state, &tenant, &summary, finding, &finding_record_ids)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        issue_ids.push(issue_id.clone());
        findings_report.push(serde_json::json!({
            "title": finding_issue_title(finding),
            "intent_title": finding_intent_title(finding),
            "symptom_title": finding_symptom_title(finding),
            "kind": finding.kind.clone(),
            "record_ids": finding_record_ids,
            "issue_id": issue_id,
        }));
    }

    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::EvolutionRecords);
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::EvolutionInsights);
    let _ = state
        .observe_refresh_tx
        .send(crate::state::ObserveRefreshHint::Entities);

    Ok(Json(serde_json::json!({
        "intent_discovery_id": payload.intent_discovery_id,
        "tenant": payload.tenant.unwrap_or_else(|| tenant.as_str().to_string()),
        "records_created_count": record_ids.len(),
        "issues_created_count": issue_ids.len(),
        "record_ids": record_ids,
        "issue_ids": issue_ids,
        "findings": findings_report,
    })))
}

/// GET /observe/evolution/stream -- SSE for real-time evolution events.
///
/// Streams new evolution records and insights as they are generated.
/// Uses the same broadcast channel pattern as the pending decision stream.
#[instrument(skip_all, fields(otel.name = "GET /observe/evolution/stream"))]
pub(crate) async fn handle_evolution_stream(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, StatusCode> {
    require_observe_auth(&state, &headers, "read_evolution", "EvolutionStream")?;
    // Subscribe to pending decision broadcasts (which include authz denials
    // that create evolution records). A dedicated evolution broadcast channel
    // could be added later for O/P/A/D/I records specifically.
    let rx = state.pending_decision_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| match result {
        Ok(pd) => Some(Ok(Event::default()
            .event("evolution_event")
            .json_data(serde_json::json!({
                "type": "new_decision",
                "decision_id": pd.id,
                "action": pd.action,
                "resource_type": pd.resource_type,
                "status": "pending",
            }))
            .unwrap_or_else(|_| Event::default().data("{}")))),
        Err(_) => None,
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_title_prefers_intent_shaped_fields() {
        let finding = AgentFinding {
            title: "Invoice entity type not implemented".to_string(),
            symptom_title: "GenerateInvoice hits EntitySetNotFound on Invoice".to_string(),
            intent_title: "Enable invoice generation workflow".to_string(),
            recommended_issue_title: "Enable invoice generation workflow".to_string(),
            intent: "Generate invoices for customers".to_string(),
            ..AgentFinding::default()
        };

        assert_eq!(
            finding_issue_title(&finding),
            "Enable invoice generation workflow"
        );
        assert_eq!(
            finding_symptom_title(&finding),
            "GenerateInvoice hits EntitySetNotFound on Invoice"
        );
        assert_eq!(
            finding_intent_title(&finding),
            "Enable invoice generation workflow"
        );
    }
}
