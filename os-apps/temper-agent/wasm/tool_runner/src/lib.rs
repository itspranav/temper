//! Tool Runner — WASM module for executing tool calls in a sandbox.
//!
//! Reads pending_tool_calls from trigger params, executes each tool via
//! HTTP calls to the sandbox API, and returns tool results as callback params.
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;

/// Entry point.
#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        ctx.log("info", "tool_runner: starting");

        let fields = ctx
            .entity_state
            .get("fields")
            .cloned()
            .unwrap_or(json!({}));

        let sandbox_url = fields
            .get("sandbox_url")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if sandbox_url.is_empty() {
            return Err("sandbox_url is empty — cannot execute tools".to_string());
        }

        let workdir = fields
            .get("workdir")
            .and_then(|v| v.as_str())
            .unwrap_or("/workspace");

        // Read pending tool calls from trigger params
        let tool_calls_json = ctx
            .trigger_params
            .get("pending_tool_calls")
            .and_then(|v| v.as_str())
            .unwrap_or("[]");

        let tool_calls: Vec<Value> = serde_json::from_str(tool_calls_json)
            .map_err(|e| format!("failed to parse pending_tool_calls: {e}"))?;

        ctx.log(
            "info",
            &format!("tool_runner: executing {} tool calls", tool_calls.len()),
        );

        // Execute each tool call and collect results
        let mut tool_results: Vec<Value> = Vec::new();

        for call in &tool_calls {
            let tool_id = call
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let tool_name = call
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let input = call
                .get("input")
                .cloned()
                .unwrap_or(json!({}));

            ctx.log("info", &format!("tool_runner: executing tool '{tool_name}' id={tool_id}"));

            let result = execute_tool(&ctx, sandbox_url, workdir, tool_name, &input);

            let (content, is_error) = match result {
                Ok(output) => (output, false),
                Err(e) => (format!("Error: {e}"), true),
            };

            tool_results.push(json!({
                "type": "tool_result",
                "tool_use_id": tool_id,
                "content": content,
                "is_error": is_error,
            }));
        }

        // TemperFS conversation storage
        let conversation_file_id = fields
            .get("conversation_file_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        // Temper API URL: read from integration config, default to localhost
        let temper_api_url = ctx
            .config
            .get("temper_api_url")
            .cloned()
            .unwrap_or_else(|| "http://localhost:3210".to_string());
        let tenant = &ctx.tenant;

        // Read current conversation and append tool results
        let mut messages: Vec<Value> = if !conversation_file_id.is_empty() {
            read_conversation_from_temperfs(&ctx, &temper_api_url, tenant, conversation_file_id)?
        } else {
            let conversation_json = fields
                .get("conversation")
                .and_then(|v| v.as_str())
                .unwrap_or("[]");
            serde_json::from_str(conversation_json).unwrap_or_default()
        };

        // Append tool results as a user message (Anthropic API format)
        messages.push(json!({
            "role": "user",
            "content": tool_results,
        }));

        // Write back to TemperFS or pass inline
        let updated_conversation = serde_json::to_string(&messages).unwrap_or_default();
        if !conversation_file_id.is_empty() {
            let body = format!("{{\"messages\":{updated_conversation}}}");
            let url = format!("{temper_api_url}/tdata/Files('{conversation_file_id}')/$value");
            let headers = vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("x-tenant-id".to_string(), tenant.to_string()),
                ("x-temper-principal-kind".to_string(), "system".to_string()),
            ];
            match ctx.http_call("PUT", &url, &headers, &body) {
                Ok(resp) if resp.status >= 200 && resp.status < 300 => {
                    ctx.log("info", &format!("tool_runner: wrote conversation to TemperFS ({} bytes)", body.len()));
                }
                Ok(resp) => {
                    return Err(format!("TemperFS conversation write failed (HTTP {}): {}", resp.status, &resp.body[..resp.body.len().min(200)]));
                }
                Err(e) => {
                    return Err(format!("TemperFS conversation write failed: {e}"));
                }
            }
        }

        let results_json = serde_json::to_string(&tool_results).unwrap_or_default();
        let mut params = json!({
            "pending_tool_calls": results_json,
        });
        if conversation_file_id.is_empty() {
            params["conversation"] = json!(updated_conversation);
        }
        set_success_result("HandleToolResults", &params);

        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}

/// Detect whether the sandbox is E2B (envd daemon) based on the URL.
fn is_e2b_sandbox(sandbox_url: &str) -> bool {
    sandbox_url.contains("e2b.app") || sandbox_url.contains("e2b.dev")
}

/// Execute a single tool call against the sandbox API.
/// Supports both local sandbox API (/v1/fs/file, /v1/processes/run)
/// and E2B envd API (/files, Connect protocol for processes).
fn execute_tool(
    ctx: &Context,
    sandbox_url: &str,
    workdir: &str,
    tool_name: &str,
    input: &Value,
) -> Result<String, String> {
    let e2b = is_e2b_sandbox(sandbox_url);
    match tool_name {
        "read" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or("read: missing 'path' parameter")?;

            let full_path = resolve_path(workdir, path);
            if e2b {
                read_file_e2b(ctx, sandbox_url, &full_path)
            } else {
                read_file_local(ctx, sandbox_url, &full_path)
            }
        }
        "write" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or("write: missing 'path' parameter")?;
            let content = input
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or("write: missing 'content' parameter")?;

            let full_path = resolve_path(workdir, path);
            if e2b {
                write_file_e2b(ctx, sandbox_url, &full_path, content)
            } else {
                write_file_local(ctx, sandbox_url, &full_path, content)
            }
        }
        "edit" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or("edit: missing 'path' parameter")?;
            let old_string = input
                .get("old_string")
                .and_then(|v| v.as_str())
                .ok_or("edit: missing 'old_string' parameter")?;
            let new_string = input
                .get("new_string")
                .and_then(|v| v.as_str())
                .ok_or("edit: missing 'new_string' parameter")?;

            let full_path = resolve_path(workdir, path);
            // Read current file
            let current = if e2b {
                read_file_e2b(ctx, sandbox_url, &full_path)?
            } else {
                read_file_local(ctx, sandbox_url, &full_path)?
            };

            if !current.contains(old_string) {
                return Err(format!("edit: old_string not found in {full_path}"));
            }
            let updated = current.replacen(old_string, new_string, 1);

            // Write updated file
            if e2b {
                write_file_e2b(ctx, sandbox_url, &full_path, &updated)?;
            } else {
                write_file_local(ctx, sandbox_url, &full_path, &updated)?;
            }
            Ok(format!("File edited: {full_path}"))
        }
        "bash" => {
            let command = input
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or("bash: missing 'command' parameter")?;

            if e2b {
                run_bash_local(ctx, sandbox_url, command, workdir)
                    .map_err(|e| format!("E2B bash not yet supported via Connect protocol — \
                        process execution requires host_grpc_call or custom template. \
                        Underlying error: {e}"))
            } else {
                run_bash_local(ctx, sandbox_url, command, workdir)
            }
        }
        unknown => Err(format!("unknown tool: {unknown}")),
    }
}

// --- Local sandbox API (our custom HTTP server) ---

/// Read file via local sandbox API.
fn read_file_local(ctx: &Context, sandbox_url: &str, full_path: &str) -> Result<String, String> {
    let url = format!("{sandbox_url}/v1/fs/file?path={}", url_encode(full_path));
    let resp = ctx.http_get(&url)?;
    if resp.status == 200 {
        Ok(resp.body)
    } else {
        Err(format!("read failed (HTTP {}): {}", resp.status, resp.body))
    }
}

/// Write file via local sandbox API.
fn write_file_local(
    ctx: &Context,
    sandbox_url: &str,
    full_path: &str,
    content: &str,
) -> Result<String, String> {
    let url = format!("{sandbox_url}/v1/fs/file?path={}", url_encode(full_path));
    let headers = vec![("content-type".to_string(), "text/plain".to_string())];
    let resp = ctx.http_call("PUT", &url, &headers, content)?;
    if resp.status >= 200 && resp.status < 300 {
        Ok(format!("File written: {full_path}"))
    } else {
        Err(format!("write failed (HTTP {}): {}", resp.status, resp.body))
    }
}

/// Run bash command via local sandbox API.
fn run_bash_local(
    ctx: &Context,
    sandbox_url: &str,
    command: &str,
    workdir: &str,
) -> Result<String, String> {
    let url = format!("{sandbox_url}/v1/processes/run");
    let body = serde_json::to_string(&json!({
        "command": command,
        "workdir": workdir,
    }))
    .unwrap_or_default();

    let headers = vec![("content-type".to_string(), "application/json".to_string())];
    let resp = ctx.http_call("POST", &url, &headers, &body)?;

    if resp.status >= 200 && resp.status < 300 {
        if let Ok(parsed) = serde_json::from_str::<Value>(&resp.body) {
            let stdout = parsed.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
            let stderr = parsed.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
            let exit_code = parsed.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1);

            let mut output = String::new();
            if !stdout.is_empty() {
                output.push_str(stdout);
            }
            if !stderr.is_empty() {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str("STDERR: ");
                output.push_str(stderr);
            }
            if exit_code != 0 {
                output.push_str(&format!("\n(exit code: {exit_code})"));
            }
            Ok(output)
        } else {
            Ok(resp.body)
        }
    } else {
        Err(format!("bash failed (HTTP {}): {}", resp.status, resp.body))
    }
}

// --- E2B envd API (plain HTTP for files, port 49983) ---

/// Read file via E2B envd HTTP API: GET /files?path=...
fn read_file_e2b(ctx: &Context, sandbox_url: &str, full_path: &str) -> Result<String, String> {
    let url = format!("{sandbox_url}/files?path={}", url_encode(full_path));
    let resp = ctx.http_get(&url)?;
    if resp.status == 200 {
        Ok(resp.body)
    } else {
        Err(format!("E2B read failed (HTTP {}): {}", resp.status, resp.body))
    }
}

/// Write file via E2B envd HTTP API: POST /files?path=<full_path> with multipart file.
/// The E2B envd expects `path` as a query parameter (full file path) and the file
/// content as a multipart form-data upload with field name "file".
fn write_file_e2b(
    ctx: &Context,
    sandbox_url: &str,
    full_path: &str,
    content: &str,
) -> Result<String, String> {
    let url = format!(
        "{sandbox_url}/files?path={}",
        url_encode(full_path)
    );
    let boundary = "----TemperWasmBoundary7MA4YWxkTrZu0gW";
    let body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{full_path}\"\r\nContent-Type: application/octet-stream\r\n\r\n{content}\r\n--{boundary}--\r\n"
    );

    let headers = vec![(
        "content-type".to_string(),
        format!("multipart/form-data; boundary={boundary}"),
    )];
    let resp = ctx.http_call("POST", &url, &headers, &body)?;
    if resp.status >= 200 && resp.status < 300 {
        Ok(format!("File written: {full_path}"))
    } else {
        Err(format!("E2B write failed (HTTP {}): {}", resp.status, resp.body))
    }
}

/// Read conversation from TemperFS File entity.
fn read_conversation_from_temperfs(
    ctx: &Context,
    temper_api_url: &str,
    tenant: &str,
    file_id: &str,
) -> Result<Vec<Value>, String> {
    let url = format!("{temper_api_url}/tdata/Files('{file_id}')/$value");
    let headers = vec![
        ("x-tenant-id".to_string(), tenant.to_string()),
        ("x-temper-principal-kind".to_string(), "system".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];

    let resp = ctx
        .http_call("GET", &url, &headers, "")
        .map_err(|e| format!("TemperFS conversation read failed: {e}"))?;

    if resp.status != 200 {
        return Err(format!(
            "TemperFS conversation read failed (HTTP {}): {}",
            resp.status,
            &resp.body[..resp.body.len().min(200)]
        ));
    }

    let parsed: Value = serde_json::from_str(&resp.body)
        .map_err(|e| format!("TemperFS conversation parse failed: {e}"))?;

    Ok(parsed
        .get("messages")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default())
}

/// Resolve a path relative to the working directory.
fn resolve_path(workdir: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("{}/{}", workdir.trim_end_matches('/'), path)
    }
}

/// Minimal URL encoding for path parameters.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' => {
                out.push(b as char);
            }
            _ => {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}
