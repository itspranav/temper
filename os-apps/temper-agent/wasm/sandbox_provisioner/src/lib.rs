//! Sandbox Provisioner — WASM module for provisioning Modal sandboxes.
//!
//! Creates a Modal sandbox and a TemperFS workspace + conversation file
//! for the agent. Returns sandbox connection details and TemperFS FKs.
//!
//! Build: `cargo build --target wasm32-unknown-unknown --release`

use temper_wasm_sdk::prelude::*;

/// Entry point.
#[unsafe(no_mangle)]
pub extern "C" fn run(_ctx_ptr: i32, _ctx_len: i32) -> i32 {
    let result = (|| -> Result<(), String> {
        let ctx = Context::from_host()?;
        ctx.log("info", "sandbox_provisioner: starting");

        let fields = ctx
            .entity_state
            .get("fields")
            .cloned()
            .unwrap_or(json!({}));

        let prompt = fields
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if prompt.is_empty() {
            return Err("agent not configured — prompt is empty".to_string());
        }

        // Step 1: Provision Modal sandbox
        let sandbox_result = provision_modal_sandbox(&ctx)?;
        ctx.log(
            "info",
            &format!(
                "sandbox_provisioner: sandbox ready at {}",
                sandbox_result.sandbox_url
            ),
        );

        // Step 2: Create TemperFS workspace for this agent
        let workspace_id = create_temper_workspace(&ctx)?;
        ctx.log(
            "info",
            &format!("sandbox_provisioner: workspace created: {workspace_id}"),
        );

        // Step 3: Create TemperFS file for conversation storage
        let conversation_file_id = create_conversation_file(&ctx, &workspace_id)?;
        ctx.log(
            "info",
            &format!("sandbox_provisioner: conversation file created: {conversation_file_id}"),
        );

        // Step 4: Initialize conversation with user prompt
        let initial_messages = json!([
            { "role": "user", "content": prompt }
        ]);
        write_conversation(&ctx, &conversation_file_id, &initial_messages)?;

        // Return all IDs to the state machine
        set_success_result(
            "SandboxReady",
            &json!({
                "sandbox_url": sandbox_result.sandbox_url,
                "sandbox_id": sandbox_result.sandbox_id,
                "workspace_id": workspace_id,
                "conversation_file_id": conversation_file_id,
            }),
        );

        Ok(())
    })();

    if let Err(e) = result {
        set_error_result(&e);
    }
    0
}

struct SandboxResult {
    sandbox_url: String,
    sandbox_id: String,
}

/// Provision a Modal sandbox via the Modal API.
fn provision_modal_sandbox(ctx: &Context) -> Result<SandboxResult, String> {
    // Read Modal API token from secrets
    let modal_token = ctx
        .get_secret("modal_api_token")
        .unwrap_or_default();

    if modal_token.is_empty() {
        // If no Modal token, check for a static sandbox_url in config
        // (for local development with a pre-provisioned sandbox)
        if let Some(static_url) = ctx.config.get("sandbox_url") {
            return Ok(SandboxResult {
                sandbox_url: static_url.clone(),
                sandbox_id: "static-sandbox".to_string(),
            });
        }
        return Err("missing modal_api_token secret and no static sandbox_url in config".to_string());
    }

    // Call Modal Sandbox API to create a new sandbox
    let body = json!({
        "image": ctx.config.get("sandbox_image")
            .cloned()
            .unwrap_or_else(|| "python:3.12-slim".to_string()),
        "timeout": 600,
        "cpu": 1.0,
        "memory": 512,
    });

    let headers = vec![
        ("authorization".to_string(), format!("Bearer {modal_token}")),
        ("content-type".to_string(), "application/json".to_string()),
    ];

    let modal_api_url = ctx
        .config
        .get("modal_api_url")
        .cloned()
        .unwrap_or_else(|| "https://api.modal.com/v1/sandboxes".to_string());

    let resp = ctx.http_call("POST", &modal_api_url, &headers, &body.to_string())?;

    if resp.status < 200 || resp.status >= 300 {
        return Err(format!(
            "Modal sandbox creation failed (HTTP {}): {}",
            resp.status,
            &resp.body[..resp.body.len().min(500)]
        ));
    }

    let parsed: Value = serde_json::from_str(&resp.body)
        .map_err(|e| format!("failed to parse Modal response: {e}"))?;

    let sandbox_id = parsed
        .get("sandbox_id")
        .or_else(|| parsed.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let sandbox_url = parsed
        .get("tunnel_url")
        .or_else(|| parsed.get("url"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if sandbox_url.is_empty() {
        return Err("Modal response missing sandbox URL".to_string());
    }

    Ok(SandboxResult {
        sandbox_url,
        sandbox_id,
    })
}

/// Create a TemperFS Workspace entity for this agent.
fn create_temper_workspace(ctx: &Context) -> Result<String, String> {
    let workspace_id = format!("ws-agent-{}", ctx.entity_id);
    let url = format!(
        "http://localhost:8080/api/tenants/{}/odata/Workspaces",
        ctx.tenant
    );

    let body = json!({
        "Id": workspace_id,
    });

    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
    ];

    let resp = ctx.http_call("POST", &url, &headers, &body.to_string())?;

    if resp.status >= 200 && resp.status < 300 {
        // Configure the workspace with a reasonable quota
        let configure_url = format!(
            "http://localhost:8080/api/tenants/{}/odata/Workspaces('{workspace_id}')/NS.Create",
            ctx.tenant
        );
        let configure_body = json!({
            "name": format!("Agent {}", ctx.entity_id),
            "quota_limit": 104857600, // 100 MB
        });
        let _ = ctx.http_call("POST", &configure_url, &headers, &configure_body.to_string());

        Ok(workspace_id)
    } else if resp.status == 409 {
        // Workspace already exists (idempotent)
        Ok(workspace_id)
    } else {
        Err(format!(
            "failed to create workspace (HTTP {}): {}",
            resp.status, resp.body
        ))
    }
}

/// Create a TemperFS File entity for conversation storage.
fn create_conversation_file(ctx: &Context, workspace_id: &str) -> Result<String, String> {
    let file_id = format!("conv-{}", ctx.entity_id);
    let url = format!(
        "http://localhost:8080/api/tenants/{}/odata/Files",
        ctx.tenant
    );

    let body = json!({
        "Id": file_id,
    });

    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
    ];

    let resp = ctx.http_call("POST", &url, &headers, &body.to_string())?;

    if resp.status >= 200 && resp.status < 300 {
        // Configure the file
        let create_url = format!(
            "http://localhost:8080/api/tenants/{}/odata/Files('{file_id}')/NS.Create",
            ctx.tenant
        );
        let create_body = json!({
            "name": "conversation.json",
            "path": format!("/{}/conversation.json", ctx.entity_id),
            "directory_id": "",
            "workspace_id": workspace_id,
            "mime_type": "application/json",
        });
        let _ = ctx.http_call("POST", &create_url, &headers, &create_body.to_string());

        Ok(file_id)
    } else if resp.status == 409 {
        Ok(file_id)
    } else {
        Err(format!(
            "failed to create conversation file (HTTP {}): {}",
            resp.status, resp.body
        ))
    }
}

/// Write conversation JSON to TemperFS.
fn write_conversation(ctx: &Context, file_id: &str, messages: &Value) -> Result<(), String> {
    let url = format!(
        "http://localhost:8080/api/tenants/{}/odata/Files('{file_id}')/$value",
        ctx.tenant
    );
    let body = serde_json::to_string(messages).unwrap_or_default();
    let headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
    ];

    let resp = ctx.http_call("PUT", &url, &headers, &body)?;

    if resp.status >= 200 && resp.status < 300 {
        Ok(())
    } else {
        Err(format!(
            "failed to write conversation (HTTP {}): {}",
            resp.status, resp.body
        ))
    }
}
