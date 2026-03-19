//! GEPA Replay WASM module.
//!
//! Replays trajectory actions against a candidate IOA spec using
//! `host_evaluate_spec`. Emits detailed action-level traces used by
//! reflective mutation and per-objective Pareto support updates.

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        ctx.log("info", "gepa-replay: starting trajectory replay");

        let fields = ctx.entity_state.get("fields").unwrap_or(&ctx.entity_state);
        let ioa_source = fields
            .get("SpecSource")
            .and_then(Value::as_str)
            .or_else(|| ctx.trigger_params.get("SpecSource").and_then(Value::as_str))
            .ok_or("entity_state.fields missing 'SpecSource'")?;

        let actions_val = ctx.trigger_params
            .get("TrajectoryActions")
            .or_else(|| fields.get("TrajectoryActions"));

        let parsed_actions: Vec<Value>;
        let actions = match actions_val {
            Some(Value::Array(arr)) => arr,
            Some(Value::String(raw)) => {
                parsed_actions = serde_json::from_str(raw).unwrap_or_default();
                &parsed_actions
            }
            _ => return Err("trigger_params missing 'TrajectoryActions'".into()),
        };

        let initial_state = ctx
            .trigger_params
            .get("InitialState")
            .and_then(Value::as_str)
            .or_else(|| ctx.trigger_params.get("initial_state").and_then(Value::as_str))
            .unwrap_or("Created");

        let mut current_state = initial_state.to_string();
        let mut actions_attempted: u32 = 0;
        let mut succeeded: u32 = 0;
        let mut guard_rejections: u32 = 0;
        let mut unknown_actions: u32 = 0;
        let mut invalid_transitions: u32 = 0;
        let mut errors: Vec<Value> = Vec::new();
        let mut action_results: Vec<Value> = Vec::new();
        let mut per_action = serde_json::Map::<String, Value>::new();

        for action_val in actions {
            let action = action_val
                .get("action")
                .and_then(Value::as_str)
                .or_else(|| action_val.get("Action").and_then(Value::as_str))
                .unwrap_or("unknown");
            let params = action_val
                .get("params")
                .cloned()
                .or_else(|| action_val.get("Params").cloned())
                .unwrap_or(json!({}));
            let params_str = params.to_string();
            let from_state = current_state.clone();

            actions_attempted += 1;

            let result = ctx.evaluate_spec(ioa_source, &current_state, action, &params_str)?;
            let success = result
                .get("success")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            let error_message = result
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let error_message_lower = error_message.to_ascii_lowercase();
            let error_kind = if error_message_lower.contains("unknown action")
                || error_message_lower.contains("not defined")
            {
                "unknown_action"
            } else if error_message_lower.contains("guard") {
                "guard_rejection"
            } else if error_message.is_empty() {
                "none"
            } else {
                "invalid_transition"
            };

            let to_state = if success {
                result
                    .get("new_state")
                    .and_then(Value::as_str)
                    .unwrap_or(&from_state)
                    .to_string()
            } else {
                from_state.clone()
            };

            if success {
                succeeded += 1;
                current_state = to_state.clone();
            } else {
                match error_kind {
                    "unknown_action" => unknown_actions += 1,
                    "guard_rejection" => guard_rejections += 1,
                    _ => invalid_transitions += 1,
                }

                errors.push(json!({
                    "action": action,
                    "from_state": from_state,
                    "error_kind": error_kind,
                    "message": if error_message.is_empty() { "spec evaluation failed" } else { &error_message },
                }));
            }

            let stats_entry = per_action
                .entry(action.to_string())
                .or_insert_with(|| json!({
                    "attempted": 0_u64,
                    "succeeded": 0_u64,
                    "guard_rejections": 0_u64,
                    "unknown_actions": 0_u64,
                    "invalid_transitions": 0_u64,
                }));
            if let Some(obj) = stats_entry.as_object_mut() {
                let attempted = obj.get("attempted").and_then(Value::as_u64).unwrap_or(0);
                obj.insert("attempted".into(), json!(attempted + 1));
                if success {
                    let succ = obj.get("succeeded").and_then(Value::as_u64).unwrap_or(0);
                    obj.insert("succeeded".into(), json!(succ + 1));
                } else {
                    match error_kind {
                        "guard_rejection" => {
                            let n = obj
                                .get("guard_rejections")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            obj.insert("guard_rejections".into(), json!(n + 1));
                        }
                        "unknown_action" => {
                            let n = obj
                                .get("unknown_actions")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            obj.insert("unknown_actions".into(), json!(n + 1));
                        }
                        _ => {
                            let n = obj
                                .get("invalid_transitions")
                                .and_then(Value::as_u64)
                                .unwrap_or(0);
                            obj.insert("invalid_transitions".into(), json!(n + 1));
                        }
                    }
                }
            }

            action_results.push(json!({
                "action": action,
                "params": params,
                "from_state": from_state,
                "to_state": to_state,
                "success": success,
                "error_kind": if success { Value::Null } else { json!(error_kind) },
                "error": if error_message.is_empty() { Value::Null } else { json!(error_message) },
            }));
        }

        let success_rate = if actions_attempted > 0 {
            succeeded as f64 / actions_attempted as f64
        } else {
            0.0
        };

        let replay_result = json!({
            "actions_attempted": actions_attempted,
            "succeeded": succeeded,
            "guard_rejections": guard_rejections,
            "unknown_actions": unknown_actions,
            "invalid_transitions": invalid_transitions,
            "success_rate": success_rate,
            "errors": errors,
            "action_results": action_results,
            "per_action": Value::Object(per_action),
        });

        ctx.log(
            "info",
            &format!(
                "gepa-replay: {succeeded}/{actions_attempted} succeeded (rate: {success_rate:.2})"
            ),
        );

        Ok(json!({
            "ReplayResultJson": replay_result.to_string(),
            "replay_result": replay_result,
        }))
    }
}
