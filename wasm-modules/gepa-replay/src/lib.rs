//! GEPA Replay WASM module.
//!
//! Replays OTS trajectory actions against a candidate IOA spec using
//! `host_evaluate_spec`. Tracks successes, guard rejections, unknown
//! actions, and invalid transitions. Returns aggregated replay results.
//!
//! Build: `cargo build -p gepa-replay-module --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        ctx.log("info", "gepa-replay: starting trajectory replay");

        // Read candidate IOA source from entity state fields (set by SelectCandidate params)
        let fields = ctx.entity_state.get("fields").unwrap_or(&ctx.entity_state);
        let ioa_source = fields
            .get("SpecSource")
            .and_then(Value::as_str)
            .or_else(|| ctx.trigger_params.get("SpecSource").and_then(Value::as_str))
            .ok_or("entity_state.fields missing 'SpecSource'")?;

        // Read trajectory actions from trigger params or entity state
        let actions_val = ctx.trigger_params
            .get("TrajectoryActions")
            .or_else(|| fields.get("TrajectoryActions"));
        // Parse if string, use directly if array
        let actions_parsed: Vec<Value>;
        let actions = match actions_val {
            Some(Value::Array(arr)) => arr,
            Some(Value::String(s)) => {
                actions_parsed = serde_json::from_str(s).unwrap_or_default();
                &actions_parsed
            }
            _ => return Err("trigger_params missing 'TrajectoryActions'".into()),
        };

        let initial_state = ctx.trigger_params
            .get("initial_state")
            .and_then(Value::as_str)
            .unwrap_or("Created");

        let mut current_state = initial_state.to_string();
        let mut actions_attempted: u32 = 0;
        let mut succeeded: u32 = 0;
        let mut guard_rejections: u32 = 0;
        let mut unknown_actions: u32 = 0;
        let mut errors: Vec<Value> = Vec::new();

        for action_val in actions {
            let action = action_val.get("action")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let params = action_val.get("params")
                .cloned()
                .unwrap_or(json!({}));
            let params_str = params.to_string();

            actions_attempted += 1;

            let result = ctx.evaluate_spec(
                ioa_source,
                &current_state,
                action,
                &params_str,
            )?;

            let success = result.get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            if success {
                succeeded += 1;
                if let Some(new_state) = result.get("new_state").and_then(Value::as_str) {
                    current_state = new_state.to_string();
                }
            } else {
                let error_msg = result.get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");

                // Classify the error
                if error_msg.contains("not defined") || error_msg.contains("unknown action") {
                    unknown_actions += 1;
                    errors.push(json!({
                        "action": action,
                        "from_state": current_state,
                        "error_kind": "unknown_action",
                        "message": error_msg,
                    }));
                } else if error_msg.contains("guard") {
                    guard_rejections += 1;
                    errors.push(json!({
                        "action": action,
                        "from_state": current_state,
                        "error_kind": "guard_rejection",
                        "message": error_msg,
                    }));
                } else {
                    errors.push(json!({
                        "action": action,
                        "from_state": current_state,
                        "error_kind": "invalid_transition",
                        "message": error_msg,
                    }));
                }
            }
        }

        let success_rate = if actions_attempted > 0 {
            succeeded as f64 / actions_attempted as f64
        } else {
            0.0
        };

        ctx.log("info", &format!(
            "gepa-replay: {succeeded}/{actions_attempted} succeeded (rate: {success_rate:.2})"
        ));

        Ok(json!({
            "replay_result": {
                "actions_attempted": actions_attempted,
                "succeeded": succeeded,
                "guard_rejections": guard_rejections,
                "unknown_actions": unknown_actions,
                "success_rate": success_rate,
                "errors": errors,
            }
        }))
    }
}
