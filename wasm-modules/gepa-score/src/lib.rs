//! GEPA Score WASM module.
//!
//! Computes multi-objective scores from replay results and emits a normalized
//! score payload that downstream Pareto update can consume directly.

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        ctx.log("info", "gepa-score: computing objective scores");

        let fields = ctx.entity_state.get("fields").unwrap_or(&ctx.entity_state);
        let replay = read_replay_result(&ctx, fields);

        let actions_attempted = replay
            .get("actions_attempted")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let succeeded = replay
            .get("succeeded")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let guard_rejections = replay
            .get("guard_rejections")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let unknown_actions = replay
            .get("unknown_actions")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let invalid_transitions = replay
            .get("invalid_transitions")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        let mut scores = serde_json::Map::<String, Value>::new();
        if actions_attempted > 0 {
            let success_rate = succeeded as f64 / actions_attempted as f64;
            let guard_pass_rate = 1.0 - (guard_rejections as f64 / actions_attempted as f64);
            let transition_validity = 1.0 - (invalid_transitions as f64 / actions_attempted as f64);

            scores.insert("success_rate".into(), json!(success_rate));
            scores.insert("guard_pass_rate".into(), json!(guard_pass_rate));
            scores.insert("transition_validity".into(), json!(transition_validity));
        } else {
            scores.insert("success_rate".into(), json!(0.0));
            scores.insert("guard_pass_rate".into(), json!(0.0));
            scores.insert("transition_validity".into(), json!(0.0));
        }

        let coverage = if actions_attempted > 0 {
            1.0 - (unknown_actions as f64 / actions_attempted as f64)
        } else {
            0.0
        };
        scores.insert("coverage".into(), json!(coverage));

        let weights = fields
            .get("ScoringWeights")
            .or_else(|| fields.get("scoring_weights"))
            .cloned()
            .unwrap_or(json!({
                "success_rate": 1.0,
                "coverage": 0.8,
                "guard_pass_rate": 0.6,
                "transition_validity": 0.5,
            }));

        let mut weighted_sum = 0.0_f64;
        let mut total_weight = 0.0_f64;
        if let Some(weight_obj) = weights.as_object() {
            for (objective, weight_val) in weight_obj {
                let weight = weight_val.as_f64().unwrap_or(0.0);
                let score = scores.get(objective).and_then(Value::as_f64).unwrap_or(0.0);
                weighted_sum += score * weight;
                total_weight += weight;
            }
        }
        if total_weight > 0.0 {
            weighted_sum /= total_weight;
        }

        let threshold = fields
            .get("AcceptanceThreshold")
            .or_else(|| fields.get("acceptance_threshold"))
            .and_then(Value::as_f64)
            .unwrap_or(0.60);
        let is_acceptable = weighted_sum >= threshold && actions_attempted > 0;

        scores.insert("weighted_sum".into(), json!(weighted_sum));
        scores.insert("is_acceptable".into(), json!(is_acceptable));

        let candidate_id = fields
            .get("CandidateId")
            .and_then(Value::as_str)
            .or_else(|| ctx.trigger_params.get("CandidateId").and_then(Value::as_str))
            .unwrap_or("candidate-unknown");

        let score_payload = json!({
            "id": candidate_id,
            "scores": Value::Object(scores.clone()),
            "actions_attempted": actions_attempted,
            "succeeded": succeeded,
            "replay_signature": replay.get("ReplaySignature").cloned().unwrap_or(Value::Null),
        });

        ctx.log(
            "info",
            &format!(
                "gepa-score: candidate={candidate_id}, weighted_sum={weighted_sum:.3}, acceptable={is_acceptable}"
            ),
        );

        Ok(json!({
            "ScoresJson": score_payload.to_string(),
            "scores": Value::Object(scores),
            "candidate": score_payload,
        }))
    }
}

fn read_replay_result(ctx: &Context, fields: &Value) -> Value {
    if let Some(replay) = ctx.trigger_params.get("replay_result") {
        return replay.clone();
    }

    if let Some(val) = ctx.trigger_params.get("ReplayResultJson") {
        return parse_or_clone_json_value(val);
    }
    if let Some(val) = fields.get("ReplayResultJson") {
        return parse_or_clone_json_value(val);
    }
    if let Some(replay) = fields.get("replay_result") {
        return replay.clone();
    }
    json!({})
}

fn parse_or_clone_json_value(v: &Value) -> Value {
    match v {
        Value::String(raw) => serde_json::from_str::<Value>(raw).unwrap_or_else(|_| json!({})),
        _ => v.clone(),
    }
}
