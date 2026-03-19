//! GEPA Reflective Dataset WASM module.
//!
//! Converts replay traces into reflective triplets
//! `(input, output, feedback, score)` for mutation.

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        ctx.log("info", "gepa-reflective: building reflective dataset");

        let fields = ctx.entity_state.get("fields").unwrap_or(&ctx.entity_state);
        let skill_name = fields
            .get("SkillName")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let entity_type = fields
            .get("TargetEntityType")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        let replay_json = read_json_value(
            ctx.trigger_params
                .get("ReplayResultJson")
                .or_else(|| fields.get("ReplayResultJson"))
                .or_else(|| ctx.trigger_params.get("replay_result"))
                .or_else(|| fields.get("replay_result")),
        );
        let replay = replay_json.unwrap_or_else(|| json!({}));

        let verification_feedback = read_string_list(
            ctx.trigger_params
                .get("VerificationErrors")
                .or_else(|| fields.get("VerificationErrors")),
        );

        let action_results = replay
            .get("action_results")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut triplets: Vec<Value> = Vec::new();
        for (idx, action_result) in action_results.iter().enumerate() {
            let action = action_result
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let from_state = action_result
                .get("from_state")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let to_state = action_result
                .get("to_state")
                .and_then(Value::as_str)
                .unwrap_or(from_state);
            let success = action_result
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let error_kind = action_result
                .get("error_kind")
                .and_then(Value::as_str)
                .unwrap_or("");
            let error = action_result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("");

            let score = if success { 1.0 } else { 0.0 };
            let feedback = if success {
                format!("Action '{action}' succeeded from state '{from_state}' to '{to_state}'.")
            } else if error_kind == "unknown_action" {
                format!(
                    "Action '{action}' is undefined from '{from_state}'. Add or expose this action in the spec."
                )
            } else if error_kind == "guard_rejection" {
                format!(
                    "Action '{action}' was rejected by guards in '{from_state}': {error}. Revisit guards/preconditions."
                )
            } else {
                format!(
                    "Action '{action}' failed from '{from_state}': {error}. Validate transition topology and target states."
                )
            };

            triplets.push(json!({
                "input": format!("state={from_state}, action={action}, params={}", action_result.get("params").cloned().unwrap_or(json!({}))),
                "output": format!("to_state={to_state}, success={success}"),
                "feedback": feedback,
                "score": score,
                "trajectory_id": fields.get("CandidateId").and_then(Value::as_str).unwrap_or("candidate"),
                "turn_id": idx,
                "entity_type": entity_type,
                "action": action,
            }));
        }

        // Oldest failures first: sort by score ascending, then turn index.
        triplets.sort_by(|a, b| {
            let a_score = a.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            let b_score = b.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            let a_turn = a.get("turn_id").and_then(Value::as_u64).unwrap_or(0);
            let b_turn = b.get("turn_id").and_then(Value::as_u64).unwrap_or(0);
            a_score
                .partial_cmp(&b_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a_turn.cmp(&b_turn))
        });

        let failure_count = triplets
            .iter()
            .filter(|t| t.get("score").and_then(Value::as_f64).unwrap_or(0.0) < 0.5)
            .count();
        let success_count = triplets.len().saturating_sub(failure_count);

        let dataset = json!({
            "skill_name": skill_name,
            "entity_type": entity_type,
            "triplets": triplets,
            "verification_feedback": verification_feedback,
            "failure_count": failure_count,
            "success_count": success_count,
        });

        ctx.log(
            "info",
            &format!(
                "gepa-reflective: built {} triplets ({failure_count} failures, {success_count} successes)",
                dataset
                    .get("triplets")
                    .and_then(Value::as_array)
                    .map(|a| a.len())
                    .unwrap_or(0)
            ),
        );

        Ok(json!({
            "DatasetJson": dataset.to_string(),
            "reflective_dataset": dataset,
        }))
    }
}

fn read_json_value(value: Option<&Value>) -> Option<Value> {
    match value {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).ok(),
        Some(v) => Some(v.clone()),
        None => None,
    }
}

fn read_string_list(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}
