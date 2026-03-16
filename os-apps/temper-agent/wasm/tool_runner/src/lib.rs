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

        // Return tool results as the pending_tool_calls param
        // The HandleToolResults action will append these to conversation
        let results_json = serde_json::to_string(&tool_results).unwrap_or_default();
        set_success_result(
            "HandleToolResults",
            &json!({ "pending_tool_calls": results_json }),
        );

        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}

/// Execute a single tool call against the sandbox API.
fn execute_tool(
    ctx: &Context,
    sandbox_url: &str,
    workdir: &str,
    tool_name: &str,
    input: &Value,
) -> Result<String, String> {
    match tool_name {
        "read" => {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or("read: missing 'path' parameter")?;

            let full_path = resolve_path(workdir, path);
            let url = format!(
                "{sandbox_url}/v1/fs/file?path={}",
                url_encode(&full_path)
            );

            let resp = ctx.http_get(&url)?;
            if resp.status == 200 {
                Ok(resp.body)
            } else {
                Err(format!("read failed (HTTP {}): {}", resp.status, resp.body))
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
            let url = format!(
                "{sandbox_url}/v1/fs/file?path={}",
                url_encode(&full_path)
            );

            let headers = vec![
                ("content-type".to_string(), "text/plain".to_string()),
            ];
            let resp = ctx.http_call("PUT", &url, &headers, content)?;
            if resp.status >= 200 && resp.status < 300 {
                Ok(format!("File written: {full_path}"))
            } else {
                Err(format!(
                    "write failed (HTTP {}): {}",
                    resp.status, resp.body
                ))
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

            // Read current file
            let full_path = resolve_path(workdir, path);
            let read_url = format!(
                "{sandbox_url}/v1/fs/file?path={}",
                url_encode(&full_path)
            );
            let read_resp = ctx.http_get(&read_url)?;
            if read_resp.status != 200 {
                return Err(format!(
                    "edit: read failed (HTTP {}): {}",
                    read_resp.status, read_resp.body
                ));
            }

            // Apply edit
            let current = &read_resp.body;
            if !current.contains(old_string) {
                return Err(format!(
                    "edit: old_string not found in {full_path}"
                ));
            }
            let updated = current.replacen(old_string, new_string, 1);

            // Write updated file
            let headers = vec![
                ("content-type".to_string(), "text/plain".to_string()),
            ];
            let write_resp = ctx.http_call("PUT", &read_url, &headers, &updated)?;
            if write_resp.status >= 200 && write_resp.status < 300 {
                Ok(format!("File edited: {full_path}"))
            } else {
                Err(format!(
                    "edit: write failed (HTTP {}): {}",
                    write_resp.status, write_resp.body
                ))
            }
        }
        "bash" => {
            let command = input
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or("bash: missing 'command' parameter")?;

            let url = format!("{sandbox_url}/v1/processes/run");
            let body = serde_json::to_string(&json!({
                "command": command,
                "workdir": workdir,
            }))
            .unwrap_or_default();

            let headers = vec![
                ("content-type".to_string(), "application/json".to_string()),
            ];
            let resp = ctx.http_call("POST", &url, &headers, &body)?;

            if resp.status >= 200 && resp.status < 300 {
                // Parse process output
                if let Ok(parsed) = serde_json::from_str::<Value>(&resp.body) {
                    let stdout = parsed
                        .get("stdout")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let stderr = parsed
                        .get("stderr")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let exit_code = parsed
                        .get("exit_code")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(-1);

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
                Err(format!(
                    "bash failed (HTTP {}): {}",
                    resp.status, resp.body
                ))
            }
        }
        unknown => Err(format!("unknown tool: {unknown}")),
    }
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
