//! GEPA Reflective Dataset WASM module.
//!
//! Converts OTS trajectory data into (input, output, feedback) triplets
//! for LLM mutation guidance. Also incorporates verification failure
//! messages from previous mutation attempts.
//!
//! Build: `cargo build -p gepa-reflective-module --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        ctx.log("info", "gepa-reflective: building reflective dataset");

        // Read trajectories from trigger params
        let trajectories = ctx.trigger_params
            .get("trajectories")
            .and_then(Value::as_array)
            .ok_or("trigger_params missing 'trajectories' array")?;

        // Read skill/entity context
        let skill_name = ctx.entity_state
            .get("skill_name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let entity_type = ctx.entity_state
            .get("target_entity_type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        // Read previous verification errors (if any)
        let verification_feedback: Vec<String> = ctx.entity_state
            .get("verification_errors")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();

        let mut triplets: Vec<Value> = Vec::new();

        for trajectory in trajectories {
            let trajectory_id = trajectory.get("trajectory_id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");

            let turns = match trajectory.get("turns").and_then(Value::as_array) {
                Some(t) => t,
                None => continue,
            };

            for (turn_idx, turn) in turns.iter().enumerate() {
                // Extract decision from turn
                let decisions = match turn.get("decisions").and_then(Value::as_array) {
                    Some(d) => d,
                    None => continue,
                };

                for decision in decisions {
                    let action = decision.get("action")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let outcome = decision.get("outcome")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    let reasoning = decision.get("reasoning")
                        .and_then(Value::as_str)
                        .unwrap_or("");

                    // Compute score: success=1.0, partial=0.5, failure=0.0
                    let score = match outcome {
                        "success" => 1.0,
                        "partial_success" => 0.5,
                        _ => 0.0,
                    };

                    // Build feedback based on outcome
                    let feedback = if score < 0.5 {
                        let error = decision.get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("action failed");
                        format!("Action '{action}' failed: {error}. Consider adding or modifying this action in the spec.")
                    } else {
                        format!("Action '{action}' succeeded.")
                    };

                    triplets.push(json!({
                        "input": reasoning,
                        "output": format!("{action} → {outcome}"),
                        "feedback": feedback,
                        "score": score,
                        "trajectory_id": trajectory_id,
                        "turn_id": turn_idx,
                        "entity_type": entity_type,
                        "action": action,
                    }));
                }
            }
        }

        // Sort by score (worst first — focus LLM on failures)
        triplets.sort_by(|a, b| {
            let a_score = a.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            let b_score = b.get("score").and_then(Value::as_f64).unwrap_or(0.0);
            a_score.partial_cmp(&b_score).unwrap_or(std::cmp::Ordering::Equal)
        });

        let failure_count = triplets.iter()
            .filter(|t| t.get("score").and_then(Value::as_f64).unwrap_or(0.0) < 0.5)
            .count();
        let success_count = triplets.len() - failure_count;

        ctx.log("info", &format!(
            "gepa-reflective: {failure_count} failures, {success_count} successes from {} trajectories",
            trajectories.len()
        ));

        Ok(json!({
            "reflective_dataset": {
                "skill_name": skill_name,
                "entity_type": entity_type,
                "triplets": triplets,
                "verification_feedback": verification_feedback,
                "failure_count": failure_count,
                "success_count": success_count,
            }
        }))
    }
}
