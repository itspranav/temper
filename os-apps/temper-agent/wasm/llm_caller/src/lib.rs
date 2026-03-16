//! LLM Caller — WASM module for calling the Anthropic Messages API.
//!
//! Reads conversation from TemperFS, appends the LLM response, writes it back,
//! and returns a dynamic callback action based on the LLM's response:
//! - `ProcessToolCalls` if the response contains tool_use blocks
//! - `RecordResult` if the response is an end_turn
//! - `Fail` if the turn budget is exceeded
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;

/// Entry point — NOT using `temper_module!` because we need dynamic callback actions.
#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        ctx.log("info", "llm_caller: starting");

        // Read entity state
        let fields = ctx
            .entity_state
            .get("fields")
            .cloned()
            .unwrap_or(json!({}));

        // Check turn budget
        let turn_count = fields
            .get("turn_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let max_turns = fields
            .get("max_turns")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(20);

        if turn_count >= max_turns {
            set_success_result(
                "Fail",
                &json!({ "error_message": format!("turn budget exhausted ({turn_count}/{max_turns})") }),
            );
            return Ok(());
        }

        // Read configuration
        let model = fields
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("claude-sonnet-4-20250514");
        let provider = fields
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("anthropic");
        let tools_enabled = fields
            .get("tools_enabled")
            .and_then(|v| v.as_str())
            .unwrap_or("read,write,edit,bash");
        let system_prompt = fields
            .get("system_prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let prompt = fields
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let sandbox_url = fields
            .get("sandbox_url")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let workdir = fields
            .get("workdir")
            .and_then(|v| v.as_str())
            .unwrap_or("/workspace");

        // Get API key from integration config (already resolved from {secret:anthropic_api_key})
        let api_key = ctx
            .config
            .get("api_key")
            .cloned()
            .unwrap_or_default();

        if api_key.is_empty() {
            return Err("missing api_key in integration config".to_string());
        }

        // Read conversation from TemperFS
        let conversation_file_id = fields
            .get("conversation_file_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let mut messages: Vec<Value> = if conversation_file_id.is_empty() {
            // First turn — initialize with user prompt
            vec![json!({ "role": "user", "content": prompt })]
        } else {
            // Read existing conversation from TemperFS
            let file_url = format!(
                "http://localhost:8080/api/tenants/{}/odata/Files('{conversation_file_id}')/$value",
                ctx.tenant
            );
            match ctx.http_get(&file_url) {
                Ok(resp) if resp.status == 200 => {
                    serde_json::from_str(&resp.body)
                        .unwrap_or_else(|_| vec![json!({ "role": "user", "content": prompt })])
                }
                _ => vec![json!({ "role": "user", "content": prompt })],
            }
        };

        // Build tool definitions based on tools_enabled
        let tools = build_tool_definitions(tools_enabled, sandbox_url, workdir);

        // Call LLM API
        let response = match provider {
            "anthropic" => {
                call_anthropic(&ctx, &api_key, model, system_prompt, &messages, &tools)?
            }
            other => return Err(format!("unsupported LLM provider: {other}")),
        };

        ctx.log(
            "info",
            &format!("llm_caller: got response, stop_reason={}", response.stop_reason),
        );

        // Append assistant response to conversation
        messages.push(json!({
            "role": "assistant",
            "content": response.content,
        }));

        // Write updated conversation back to TemperFS
        if !conversation_file_id.is_empty() {
            let file_url = format!(
                "http://localhost:8080/api/tenants/{}/odata/Files('{conversation_file_id}')/$value",
                ctx.tenant
            );
            let conv_json = serde_json::to_string(&messages).unwrap_or_default();
            let headers = vec![
                ("content-type".to_string(), "application/json".to_string()),
            ];
            let _ = ctx.http_call("PUT", &file_url, &headers, &conv_json);
        }

        // Route based on stop_reason
        match response.stop_reason.as_str() {
            "tool_use" => {
                // Extract tool_use blocks
                let tool_calls: Vec<Value> = response
                    .content
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .filter(|block| {
                        block.get("type").and_then(|v| v.as_str()) == Some("tool_use")
                    })
                    .cloned()
                    .collect();

                let tool_calls_json = serde_json::to_string(&tool_calls).unwrap_or_default();
                set_success_result(
                    "ProcessToolCalls",
                    &json!({ "pending_tool_calls": tool_calls_json }),
                );
            }
            "end_turn" | "stop" => {
                // Extract text result
                let result_text = response
                    .content
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .filter_map(|block| {
                        if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                            block.get("text").and_then(|v| v.as_str()).map(String::from)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");

                set_success_result("RecordResult", &json!({ "result": result_text }));
            }
            other => {
                set_success_result(
                    "Fail",
                    &json!({ "error_message": format!("unexpected stop_reason: {other}") }),
                );
            }
        }

        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}

/// Parsed LLM response.
struct LlmResponse {
    content: Value,
    stop_reason: String,
}

/// Call Anthropic Messages API.
fn call_anthropic(
    ctx: &Context,
    api_key: &str,
    model: &str,
    system_prompt: &str,
    messages: &[Value],
    tools: &[Value],
) -> Result<LlmResponse, String> {
    let mut body = json!({
        "model": model,
        "max_tokens": 4096,
        "messages": messages,
    });

    if !system_prompt.is_empty() {
        body["system"] = json!(system_prompt);
    }

    if !tools.is_empty() {
        body["tools"] = json!(tools);
    }

    let body_str = serde_json::to_string(&body).map_err(|e| format!("JSON serialize error: {e}"))?;

    let headers = vec![
        ("x-api-key".to_string(), api_key.to_string()),
        ("anthropic-version".to_string(), "2023-06-01".to_string()),
        ("content-type".to_string(), "application/json".to_string()),
    ];

    let resp = ctx.http_call(
        "POST",
        "https://api.anthropic.com/v1/messages",
        &headers,
        &body_str,
    )?;

    if resp.status != 200 {
        return Err(format!(
            "Anthropic API returned {}: {}",
            resp.status,
            &resp.body[..resp.body.len().min(500)]
        ));
    }

    let parsed: Value =
        serde_json::from_str(&resp.body).map_err(|e| format!("failed to parse LLM response: {e}"))?;

    let stop_reason = parsed
        .get("stop_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("end_turn")
        .to_string();

    let content = parsed.get("content").cloned().unwrap_or(json!([]));

    Ok(LlmResponse {
        content,
        stop_reason,
    })
}

/// Build tool definitions for the LLM based on enabled tools.
fn build_tool_definitions(tools_enabled: &str, sandbox_url: &str, workdir: &str) -> Vec<Value> {
    let enabled: Vec<&str> = tools_enabled.split(',').map(str::trim).collect();
    let mut tools = Vec::new();

    if enabled.contains(&"read") {
        tools.push(json!({
            "name": "read",
            "description": format!("Read a file from the sandbox at {sandbox_url}. Working directory: {workdir}"),
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to read" }
                },
                "required": ["path"]
            }
        }));
    }

    if enabled.contains(&"write") {
        tools.push(json!({
            "name": "write",
            "description": format!("Write a file to the sandbox at {sandbox_url}. Working directory: {workdir}"),
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to write" },
                    "content": { "type": "string", "description": "File content" }
                },
                "required": ["path", "content"]
            }
        }));
    }

    if enabled.contains(&"edit") {
        tools.push(json!({
            "name": "edit",
            "description": format!("Edit a file in the sandbox at {sandbox_url} using search-and-replace. Working directory: {workdir}"),
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path to edit" },
                    "old_string": { "type": "string", "description": "Text to find" },
                    "new_string": { "type": "string", "description": "Text to replace with" }
                },
                "required": ["path", "old_string", "new_string"]
            }
        }));
    }

    if enabled.contains(&"bash") {
        tools.push(json!({
            "name": "bash",
            "description": format!("Run a bash command in the sandbox at {sandbox_url}. Working directory: {workdir}"),
            "input_schema": {
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Bash command to execute" }
                },
                "required": ["command"]
            }
        }));
    }

    tools
}
