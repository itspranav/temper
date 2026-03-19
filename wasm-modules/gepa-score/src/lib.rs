//! GEPA Score WASM module.
//!
//! Computes multi-objective scores from replay results. Produces
//! success_rate, guard_pass_rate, and coverage metrics, plus a
//! weighted sum for single-value comparison.
//!
//! Build: `cargo build -p gepa-score-module --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        ctx.log("info", "gepa-score: computing objective scores");

        // Read replay result from trigger params (passed by RecordVerificationPass callback)
        let replay = ctx.trigger_params
            .get("replay_result")
            .or_else(|| ctx.trigger_params.get("result"))
            .unwrap_or(&ctx.trigger_params);

        let actions_attempted = replay.get("actions_attempted")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let succeeded = replay.get("succeeded")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let guard_rejections = replay.get("guard_rejections")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let unknown_actions = replay.get("unknown_actions")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        let mut scores = json!({});

        if actions_attempted > 0 {
            // Success rate: fraction of attempted actions that succeeded
            let success_rate = succeeded as f64 / actions_attempted as f64;
            scores["success_rate"] = json!(success_rate);

            // Guard pass rate: 1.0 - (guard rejections / attempted)
            let guard_pass_rate = 1.0 - (guard_rejections as f64 / actions_attempted as f64);
            scores["guard_pass_rate"] = json!(guard_pass_rate);
        }

        // Coverage: fraction of unique actions that are known
        let total_unique = succeeded + guard_rejections + unknown_actions;
        if total_unique > 0 {
            let coverage = 1.0 - (unknown_actions as f64 / total_unique as f64);
            scores["coverage"] = json!(coverage);
        }

        // Read scoring weights from entity state (or use defaults)
        let weights = ctx.entity_state.get("scoring_weights").cloned().unwrap_or(json!({
            "success_rate": 1.0,
            "coverage": 0.8,
            "guard_pass_rate": 0.6,
        }));

        // Compute weighted sum
        let mut total = 0.0_f64;
        let mut weight_sum = 0.0_f64;

        if let Some(weights_obj) = weights.as_object() {
            for (objective, weight_val) in weights_obj {
                let weight = weight_val.as_f64().unwrap_or(0.0);
                if let Some(score) = scores.get(objective).and_then(Value::as_f64) {
                    total += score * weight;
                    weight_sum += weight;
                }
            }
        }

        let weighted_sum = if weight_sum > 0.0 { total / weight_sum } else { 0.0 };
        scores["weighted_sum"] = json!(weighted_sum);

        ctx.log("info", &format!("gepa-score: weighted_sum={weighted_sum:.3}"));

        Ok(json!({
            "scores": scores,
        }))
    }
}
