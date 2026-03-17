//! Sandbox Provisioner — WASM module for provisioning sandboxes.
//!
//! Provisions a sandbox (static URL from config, or Modal API) and returns
//! the sandbox connection details. Conversation is stored inline in entity
//! state (Phase 0); TemperFS integration deferred to Phase 1.
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

        // Provision sandbox
        let sandbox_result = provision_sandbox(&ctx)?;
        ctx.log(
            "info",
            &format!(
                "sandbox_provisioner: sandbox ready at {}",
                sandbox_result.sandbox_url
            ),
        );

        // Return sandbox details to the state machine
        set_success_result(
            "SandboxReady",
            &json!({
                "sandbox_url": sandbox_result.sandbox_url,
                "sandbox_id": sandbox_result.sandbox_id,
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

/// Provision a sandbox. Priority order:
/// 1. sandbox_url from entity state (set via Configure action)
/// 2. sandbox_url from integration config
/// 3. Modal API broker
fn provision_sandbox(ctx: &Context) -> Result<SandboxResult, String> {
    let fields = ctx
        .entity_state
        .get("fields")
        .cloned()
        .unwrap_or(json!({}));

    // Priority 1: sandbox_url from entity state (set at Configure time).
    let static_url = fields
        .get("sandbox_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| ctx.config.get("sandbox_url").cloned())
        .or_else(|| {
            ctx.trigger_params
                .get("sandbox_url")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });
    if let Some(url) = static_url {
        ctx.log("info", &format!("sandbox_provisioner: using static sandbox_url: {url}"));
        return Ok(SandboxResult {
            sandbox_url: url,
            sandbox_id: "static-sandbox".to_string(),
        });
    }

    // Priority 2: Modal API (requires modal_api_token secret).
    // Note: Modal uses a Python SDK, not a REST API. This path requires
    // a pre-deployed broker endpoint that wraps the SDK.
    let modal_token = ctx
        .get_secret("modal_api_token")
        .unwrap_or_default();

    if modal_token.is_empty() {
        return Err("no sandbox_url in config and no modal_api_token secret".to_string());
    }

    let modal_broker_url = ctx
        .config
        .get("modal_broker_url")
        .cloned()
        .unwrap_or_else(|| "http://localhost:8888/create-sandbox".to_string());

    let headers = vec![
        ("authorization".to_string(), format!("Bearer {modal_token}")),
        ("content-type".to_string(), "application/json".to_string()),
    ];

    let body = json!({
        "timeout": 600,
    });

    let resp = ctx.http_call("POST", &modal_broker_url, &headers, &body.to_string())?;

    if resp.status < 200 || resp.status >= 300 {
        return Err(format!(
            "sandbox broker failed (HTTP {}): {}",
            resp.status,
            &resp.body[..resp.body.len().min(500)]
        ));
    }

    let parsed: Value = serde_json::from_str(&resp.body)
        .map_err(|e| format!("failed to parse broker response: {e}"))?;

    let sandbox_id = parsed
        .get("sandbox_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();

    let sandbox_url = parsed
        .get("sandbox_url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if sandbox_url.is_empty() {
        return Err("broker response missing sandbox_url".to_string());
    }

    Ok(SandboxResult {
        sandbox_url,
        sandbox_id,
    })
}
