//! GEPA Pareto WASM module.
//!
//! Updates the Pareto frontier by checking if a new candidate is
//! dominated by any existing member. If non-dominated, adds it and
//! removes any members it dominates.
//!
//! Build: `cargo build -p gepa-pareto-module --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        ctx.log("info", "gepa-pareto: updating Pareto frontier");

        // Read current frontier from entity state
        let frontier = ctx.entity_state
            .get("pareto_frontier")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        // Read new candidate from trigger params
        let candidate = ctx.trigger_params
            .get("candidate")
            .ok_or("trigger_params missing 'candidate'")?;

        let candidate_id = candidate.get("id")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let candidate_scores = candidate.get("scores")
            .and_then(Value::as_object)
            .ok_or("candidate missing 'scores'")?;

        // Check if candidate is dominated by any frontier member
        let mut is_dominated = false;
        let mut dominated_members: Vec<String> = Vec::new();

        for member in &frontier {
            let member_id = member.get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let member_scores = match member.get("scores").and_then(Value::as_object) {
                Some(s) => s,
                None => continue,
            };

            // Check if member dominates candidate
            if dominates(member_scores, candidate_scores) {
                is_dominated = true;
                break;
            }

            // Check if candidate dominates member
            if dominates(candidate_scores, member_scores) {
                dominated_members.push(member_id.to_string());
            }
        }

        if is_dominated {
            ctx.log("info", &format!(
                "gepa-pareto: candidate {candidate_id} is dominated, not added"
            ));
            return Ok(json!({
                "added": false,
                "frontier_size": frontier.len(),
                "removed": [],
            }));
        }

        // Build new frontier: remove dominated, add candidate
        let mut new_frontier: Vec<Value> = frontier.into_iter()
            .filter(|m| {
                let mid = m.get("id").and_then(Value::as_str).unwrap_or("");
                !dominated_members.contains(&mid.to_string())
            })
            .collect();

        new_frontier.push(candidate.clone());

        ctx.log("info", &format!(
            "gepa-pareto: added {candidate_id}, removed {} dominated, frontier size: {}",
            dominated_members.len(),
            new_frontier.len()
        ));

        Ok(json!({
            "added": true,
            "frontier": new_frontier,
            "frontier_size": new_frontier.len(),
            "removed": dominated_members,
        }))
    }
}

/// Check if `a` Pareto-dominates `b`: a >= b on all objectives, a > b on at least one.
fn dominates(
    a: &serde_json::Map<String, Value>,
    b: &serde_json::Map<String, Value>,
) -> bool {
    let mut dominated_at_least_one = false;

    // Collect all objectives from both
    let mut all_objectives: Vec<&String> = a.keys().collect();
    for k in b.keys() {
        if !all_objectives.contains(&k) {
            all_objectives.push(k);
        }
    }

    for obj in &all_objectives {
        let a_val = a.get(*obj).and_then(Value::as_f64).unwrap_or(0.0);
        let b_val = b.get(*obj).and_then(Value::as_f64).unwrap_or(0.0);

        if a_val < b_val {
            return false; // a is worse on this objective
        }
        if a_val > b_val {
            dominated_at_least_one = true;
        }
    }

    dominated_at_least_one
}
