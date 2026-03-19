//! GEPA mutation proposer WASM module driven by TemperAgent entities.
//!
//! This module replaces direct local-CLI adapters in the evolution pipeline.
//! It orchestrates a `TemperAgent` run through Temper's own entity actions:
//! create -> configure -> provision -> poll -> extract mutation JSON.

use temper_wasm_sdk::prelude::*;

temper_module! {
    fn run(ctx: Context) -> Result<Value> {
        ctx.log("info", "gepa-proposer-agent: starting TemperAgent-driven mutation proposal");

        let fields = ctx.entity_state.get("fields").unwrap_or(&ctx.entity_state);
        let dataset_json = read_dataset_json(&ctx, fields)?;
        let spec_source = fields
            .get("SpecSource")
            .and_then(Value::as_str)
            .or_else(|| ctx.trigger_params.get("SpecSource").and_then(Value::as_str))
            .ok_or("missing SpecSource in EvolutionRun state/trigger params")?;

        let skill_name = fields
            .get("SkillName")
            .and_then(Value::as_str)
            .unwrap_or("unknown-skill");
        let entity_type = fields
            .get("TargetEntityType")
            .and_then(Value::as_str)
            .unwrap_or("unknown-entity");
        let evo_id = fields
            .get("Id")
            .and_then(Value::as_str)
            .unwrap_or("evolution-run");
        let candidate_id = fields
            .get("CandidateId")
            .and_then(Value::as_str)
            .or_else(|| ctx.trigger_params.get("CandidateId").and_then(Value::as_str))
            .unwrap_or("candidate");
        let attempt = fields
            .get("mutation_attempts")
            .and_then(Value::as_i64)
            .or_else(|| {
                fields
                    .get("mutation_attempts")
                    .and_then(Value::as_str)
                    .and_then(|s| s.parse::<i64>().ok())
            })
            .unwrap_or(0);

        let base_url = ctx
            .config
            .get("temper_api_url")
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:3000".to_string());
        let sandbox_url = ctx
            .config
            .get("sandbox_url")
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:9999".to_string());
        let model = ctx
            .config
            .get("model")
            .cloned()
            .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());
        let provider = ctx
            .config
            .get("provider")
            .cloned()
            .unwrap_or_else(|| "anthropic".to_string());
        let max_turns = ctx
            .config
            .get("max_turns")
            .cloned()
            .unwrap_or_else(|| "10".to_string());
        let workdir = ctx
            .config
            .get("workdir")
            .cloned()
            .unwrap_or_else(|| "/tmp/workspace".to_string());
        let tools_enabled = ctx
            .config
            .get("tools_enabled")
            .cloned()
            .unwrap_or_else(|| "read,write,edit,bash".to_string());
        let poll_attempts = ctx
            .config
            .get("poll_attempts")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(240);
        let poll_sleep_ms = ctx
            .config
            .get("poll_sleep_ms")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(250);

        let max_agent_retries = ctx
            .config
            .get("max_agent_retries")
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(3)
            .max(1);

        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("X-Tenant-Id".to_string(), ctx.tenant.clone()),
            // Drive TemperAgent via Cedar-governed agent identity.
            ("x-temper-principal-kind".to_string(), "agent".to_string()),
            (
                "x-temper-principal-id".to_string(),
                "gepa-proposer-agent".to_string(),
            ),
            ("x-temper-agent-type".to_string(), "supervisor".to_string()),
        ];

        let system_prompt = ctx
            .config
            .get("system_prompt")
            .cloned()
            .unwrap_or_else(default_system_prompt);
        let base_user_message = build_user_message(skill_name, entity_type, spec_source, &dataset_json);
        let mut last_error = String::new();

        for agent_retry in 0..max_agent_retries {
            let agent_id = build_agent_id(evo_id, candidate_id, attempt, agent_retry);
            let create_url = format!("{base_url}/tdata/TemperAgents");
            let create_resp = post_json(
                &ctx,
                &create_url,
                &headers,
                json!({
                    "TemperAgentId": agent_id,
                }),
            )?;
            let created_agent_id = extract_entity_id(&create_resp).unwrap_or_else(|| {
                create_resp
                    .get("fields")
                    .and_then(|f| f.get("Id"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown-agent")
                    .to_string()
            });

            let user_message = if agent_retry == 0 {
                base_user_message.clone()
            } else {
                format!(
                    "{base_user_message}\n\nIMPORTANT: previous attempt returned empty/invalid payload. \
Return valid compact JSON in one line with non-empty MutatedSpecSource and MutationSummary."
                )
            };

            let cfg_url = format!(
                "{base_url}/tdata/TemperAgents('{created_agent_id}')/Temper.Agent.TemperAgent.Configure"
            );
            let _ = post_json(
                &ctx,
                &cfg_url,
                &headers,
                json!({
                    "system_prompt": system_prompt,
                    "user_message": user_message,
                    "model": model,
                    "provider": provider,
                    "max_turns": max_turns,
                    "tools_enabled": tools_enabled,
                    "workdir": workdir,
                    "sandbox_url": sandbox_url,
                }),
            )?;

            let provision_url = format!(
                "{base_url}/tdata/TemperAgents('{created_agent_id}')/Temper.Agent.TemperAgent.Provision"
            );
            let _ = post_json(&ctx, &provision_url, &headers, json!({}))?;

            let mut attempt_finished = false;
            for poll in 0..poll_attempts {
                if poll > 0 && poll_sleep_ms > 0 {
                    let _ = sleep_tick(&ctx, &sandbox_url, &workdir, poll_sleep_ms);
                }
                let get_url = format!("{base_url}/tdata/TemperAgents('{created_agent_id}')");
                let entity = get_json(&ctx, &get_url, &headers)?;
                let status = entity
                    .get("status")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        entity
                            .get("fields")
                            .and_then(|f| f.get("Status"))
                            .and_then(Value::as_str)
                    })
                    .unwrap_or("Unknown");

                match status {
                    "Completed" => {
                        let result_text = entity
                            .get("fields")
                            .and_then(|f| f.get("result"))
                            .and_then(Value::as_str)
                            .or_else(|| {
                                entity
                                    .get("fields")
                                    .and_then(|f| f.get("Result"))
                                    .and_then(Value::as_str)
                            })
                            .unwrap_or_default();

                        match extract_mutation_payload(result_text) {
                            Ok((mutated_spec, summary)) => {
                                return Ok(json!({
                                    "MutatedSpecSource": mutated_spec,
                                    "MutationSummary": summary,
                                    "ProposerType": "temper_agent",
                                    "ProposerAgentId": created_agent_id,
                                }));
                            }
                            Err(err) => {
                                last_error = format!(
                                    "TemperAgent completed with invalid payload on retry {agent_retry}: {err}"
                                );
                                ctx.log("warn", &last_error);
                                attempt_finished = true;
                                break;
                            }
                        }
                    }
                    "Failed" | "Cancelled" => {
                        let err = entity
                            .get("fields")
                            .and_then(|f| f.get("error_message"))
                            .and_then(Value::as_str)
                            .or_else(|| {
                                entity
                                    .get("fields")
                                    .and_then(|f| f.get("ErrorMessage"))
                                    .and_then(Value::as_str)
                            })
                            .unwrap_or("TemperAgent run failed");
                        last_error = format!("TemperAgent {status} on retry {agent_retry}: {err}");
                        ctx.log("warn", &last_error);
                        attempt_finished = true;
                        break;
                    }
                    _ => {}
                }
            }

            if !attempt_finished {
                last_error = format!(
                    "Timed out waiting for TemperAgent completion after {poll_attempts} polls on retry {agent_retry}"
                );
                ctx.log("warn", &last_error);
            }
        }

        if last_error.is_empty() {
            Err("GEPA proposer failed without explicit error".to_string())
        } else {
            Err(last_error)
        }
    }
}

fn read_dataset_json(ctx: &Context, fields: &Value) -> Result<String, String> {
    if let Some(s) = ctx
        .trigger_params
        .get("DatasetJson")
        .and_then(Value::as_str)
    {
        return Ok(s.to_string());
    }
    if let Some(v) = ctx.trigger_params.get("reflective_dataset") {
        return Ok(v.to_string());
    }
    if let Some(s) = fields.get("DatasetJson").and_then(Value::as_str) {
        return Ok(s.to_string());
    }
    if let Some(v) = fields.get("reflective_dataset") {
        return Ok(v.to_string());
    }
    Err("missing DatasetJson in trigger/state".to_string())
}

fn post_json(
    ctx: &Context,
    url: &str,
    headers: &[(String, String)],
    body: Value,
) -> Result<Value, String> {
    let resp = ctx.http_call("POST", url, headers, &body.to_string())?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "POST {url} failed: HTTP {} body={}",
            resp.status, resp.body
        ));
    }
    parse_json_body(&resp.body)
}

fn get_json(ctx: &Context, url: &str, headers: &[(String, String)]) -> Result<Value, String> {
    let resp = ctx.http_call("GET", url, headers, "")?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "GET {url} failed: HTTP {} body={}",
            resp.status, resp.body
        ));
    }
    parse_json_body(&resp.body)
}

fn parse_json_body(body: &str) -> Result<Value, String> {
    if body.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str::<Value>(body)
        .map_err(|e| format!("failed to parse HTTP JSON body: {e}; body={body}"))
}

fn extract_entity_id(value: &Value) -> Option<String> {
    value
        .get("entity_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            value
                .get("fields")
                .and_then(|f| f.get("Id"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn default_system_prompt() -> String {
    "You are the GEPA evolution agent operating inside TemperAgent. \
Return only compact JSON with keys MutatedSpecSource and MutationSummary. \
Do not include markdown fences. Do not ask for permissions. \
Do not edit files; reason over the provided spec text."
        .to_string()
}

fn build_user_message(
    skill_name: &str,
    entity_type: &str,
    spec_source: &str,
    dataset_json: &str,
) -> String {
    format!(
        "Target skill: {skill_name}\n\
Target entity: {entity_type}\n\n\
Current IOA spec:\n{spec_source}\n\n\
Reflective dataset JSON:\n{dataset_json}\n\n\
Task:\n\
1) Read workflow-level triplets. Each triplet has:\n\
   - input: goal + reasoning chain\n\
   - output: what happened\n\
   - feedback: specific fix suggestion\n\
   - score: 1.0 success, 0.5 partial, 0.0 failed\n\
   - preserve: true means this working pattern must not regress\n\
2) Propose the minimal IOA mutation that improves workflow completion while preserving successful patterns.\n\
3) Triplets with preserve=true MUST remain valid after mutation.\n\
4) For failed/partial workflows, apply the feedback suggestion exactly where possible.\n\
5) Check patterns.missing_capabilities and add missing [[action]] sections or transitions as needed.\n\
6) Keep schema/invariants coherent and avoid unrelated changes.\n\
Output strict JSON only:\n\
{{\"MutatedSpecSource\":\"...full spec...\",\"MutationSummary\":\"...\"}}"
    )
}

fn sanitize_id(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    if out.is_empty() {
        "id".to_string()
    } else {
        out.chars().take(48).collect()
    }
}

fn build_agent_id(
    evo_id: &str,
    candidate_id: &str,
    mutation_attempt: i64,
    agent_retry: usize,
) -> String {
    let base = format!(
        "evo-{}-{}-a{}-r{}",
        sanitize_id(evo_id),
        sanitize_id(candidate_id),
        mutation_attempt,
        agent_retry
    );
    if base.len() <= 96 {
        return base;
    }
    base.chars().take(96).collect()
}

fn extract_mutation_payload(result_text: &str) -> Result<(String, String), String> {
    if result_text.trim().is_empty() {
        return Err("TemperAgent completed with empty result".to_string());
    }

    if let Ok(parsed) = serde_json::from_str::<Value>(result_text) {
        if let Some(found) = extract_from_json_value(&parsed) {
            return Ok(found);
        }
    }

    for block in extract_markdown_code_blocks(result_text) {
        if let Ok(parsed) = serde_json::from_str::<Value>(&block)
            && let Some(found) = extract_from_json_value(&parsed)
        {
            return Ok(found);
        }
    }

    Err("TemperAgent result missing MutatedSpecSource JSON payload".to_string())
}

fn extract_from_json_value(v: &Value) -> Option<(String, String)> {
    let spec = find_first_key(
        v,
        &[
            "MutatedSpecSource",
            "mutated_spec_source",
            "SpecSource",
            "spec_source",
            "new_spec",
        ],
    )?
    .as_str()?
    .to_string();

    let summary = find_first_key(
        v,
        &[
            "MutationSummary",
            "mutation_summary",
            "summary",
            "rationale",
            "change_summary",
        ],
    )
    .and_then(|s| s.as_str().map(str::to_string))
    .unwrap_or_else(|| "Mutation proposed by TemperAgent".to_string());

    Some((spec, summary))
}

fn find_first_key(root: &Value, keys: &[&str]) -> Option<Value> {
    for key in keys {
        if let Some(value) = find_key_recursive(root, key) {
            return Some(value);
        }
    }
    None
}

fn find_key_recursive(value: &Value, key: &str) -> Option<Value> {
    match value {
        Value::Object(map) => {
            if let Some(found) = map.get(key) {
                return Some(found.clone());
            }
            for nested in map.values() {
                if let Some(found) = find_key_recursive(nested, key) {
                    return Some(found);
                }
            }
            None
        }
        Value::Array(arr) => {
            for nested in arr {
                if let Some(found) = find_key_recursive(nested, key) {
                    return Some(found);
                }
            }
            None
        }
        _ => None,
    }
}

fn extract_markdown_code_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut cursor = 0usize;
    let bytes = text.as_bytes();

    while let Some(start_rel) = text[cursor..].find("```") {
        let fence_start = cursor + start_rel;
        let mut line_end = fence_start + 3;
        while line_end < bytes.len() && bytes[line_end] != b'\n' {
            line_end += 1;
        }
        if line_end >= bytes.len() {
            break;
        }
        let content_start = line_end + 1;
        let Some(end_rel) = text[content_start..].find("```") else {
            break;
        };
        let content_end = content_start + end_rel;
        blocks.push(text[content_start..content_end].trim().to_string());
        cursor = content_end + 3;
    }

    blocks
}

fn sleep_tick(
    ctx: &Context,
    sandbox_url: &str,
    workdir: &str,
    sleep_ms: u64,
) -> Result<(), String> {
    let secs = sleep_ms as f64 / 1000.0;
    let cmd = format!("sleep {secs:.3}");
    let url = format!("{sandbox_url}/v1/processes/run");
    let headers = vec![("Content-Type".to_string(), "application/json".to_string())];
    let body = json!({
        "command": cmd,
        "workdir": workdir,
    });

    let resp = ctx.http_call("POST", &url, &headers, &body.to_string())?;
    if !(200..300).contains(&resp.status) {
        return Err(format!(
            "sandbox sleep tick failed: HTTP {} body={}",
            resp.status, resp.body
        ));
    }
    Ok(())
}
