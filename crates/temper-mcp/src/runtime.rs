//! MCP runtime context and stdio server loop.

use anyhow::{Result, bail};
use monty::MontyObject;
use temper_ots::{
    DecisionType, MessageRole, OTSChoice, OTSConsequence, OTSContext, OTSDecision, OTSMessage,
    OTSMessageContent, OTSMetadata, OutcomeType, TrajectoryBuilder,
};
use temper_runtime::scheduler::sim_now;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::McpConfig;
use super::protocol::dispatch_json_line;

/// Client identity received from the MCP `initialize` handshake.
#[derive(Clone, Debug, Default)]
pub(crate) struct ClientInfo {
    /// MCP client name (e.g. `"claude-code"`).
    pub(crate) name: Option<String>,
    /// MCP client version string.
    pub(crate) version: Option<String>,
}

/// Response from `POST /api/identity/resolve`.
///
/// Only the fields needed by the MCP runtime are declared; extra fields
/// from the server response are silently ignored by serde.
#[derive(serde::Deserialize)]
struct ResolvedIdentityResponse {
    agent_instance_id: String,
    agent_type_name: String,
}

/// Thin-client runtime context for the MCP server.
///
/// Connects to an already-running Temper server via `--port` (local) or
/// `--url` (remote). Does not spawn servers, parse local specs, or manage
/// any infrastructure.
///
/// Stores a [`PersistentSandbox`] so that variables and heap state persist
/// across `execute` tool calls within a single MCP session.
pub(crate) struct RuntimeContext {
    pub(crate) base_url: String,
    pub(crate) http: reqwest::Client,
    pub(crate) agent_id: Option<String>,
    pub(crate) agent_type: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) identity_tenant: String,
    sandbox: temper_sandbox::runner::PersistentSandbox,
    /// OTS trajectory builder for capturing agent execution traces.
    pub(crate) trajectory: Option<TrajectoryBuilder>,
}

impl RuntimeContext {
    pub(super) fn from_config(config: &McpConfig) -> Result<Self> {
        let base_url = match (&config.temper_url, config.temper_port) {
            (Some(url), _) => url.trim_end_matches('/').to_string(),
            (None, Some(port)) => format!("http://127.0.0.1:{port}"),
            (None, None) => bail!(
                "Either --url or --port is required. \
                 Use --port <n> for a local server or --url <url> for a remote server."
            ),
        };
        Ok(Self {
            base_url,
            http: reqwest::Client::new(),
            agent_id: config.agent_id.clone(),
            agent_type: config.agent_type.clone(),
            session_id: config.session_id.clone(),
            api_key: config
                .api_key
                .clone()
                .or_else(|| std::env::var("TEMPER_API_KEY").ok()), // determinism-ok: startup config
            identity_tenant: std::env::var("TEMPER_TENANT")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "default".to_string()), // determinism-ok: startup config
            sandbox: temper_sandbox::runner::PersistentSandbox::new(&[("temper", "Temper", 1)]),
            trajectory: None,
        })
    }

    /// Apply MCP `clientInfo` from the `initialize` handshake.
    ///
    /// If `api_key` is set, resolves the credential against the platform's
    /// identity registry to get a platform-assigned agent ID and verified
    /// agent type. Returns an error if credential resolution fails — there
    /// is no fallback to self-declared identity.
    ///
    /// If no `api_key` is set (local dev mode), identity fields remain as
    /// configured (or `None`).
    ///
    /// See ADR-0033: Platform-Assigned Agent Identity.
    pub(crate) async fn apply_client_info(&mut self, info: ClientInfo) -> Result<()> {
        tracing::info!(
            client_name = info.name.as_deref().unwrap_or("unknown"),
            client_version = info.version.as_deref().unwrap_or("unknown"),
            "MCP client connected"
        );
        if let Some(ref api_key) = self.api_key {
            match self.resolve_credential(api_key).await {
                Some(resolved) => {
                    self.agent_id = Some(resolved.agent_instance_id);
                    self.agent_type = Some(resolved.agent_type_name);
                    return Ok(());
                }
                None => {
                    // Credential resolution failed — no fallback to legacy derivation.
                    // Log the error but don't bail: the global API key may have a
                    // bootstrap-registered credential that hasn't been created yet
                    // (server still starting). Identity will be "operator" via the
                    // server-side bearer auth fallback.
                    tracing::warn!(
                        "Credential resolution failed for TEMPER_API_KEY. \
                         Agent will use server-assigned operator identity. \
                         Ensure an AgentCredential is registered for this key."
                    );
                }
            }
        }

        Ok(())
    }

    /// Resolve a bearer token against the platform's identity endpoint.
    async fn resolve_credential(&self, token: &str) -> Option<ResolvedIdentityResponse> {
        let url = format!("{}/api/identity/resolve", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("X-Tenant-Id", &self.identity_tenant)
            .json(&serde_json::json!({
                "bearer_token": token,
                "tenant": self.identity_tenant,
            }))
            .send()
            .await
            .ok()?;

        if !resp.status().is_success() {
            return None;
        }

        resp.json::<ResolvedIdentityResponse>().await.ok()
    }

    /// Initialize OTS trajectory capture after the MCP handshake completes.
    pub(crate) fn init_trajectory(&mut self) {
        let now = sim_now(); // determinism-ok: sim_now is DST-safe
        let agent_id = self.agent_id.as_deref().unwrap_or("unknown");
        let metadata = OTSMetadata::new("mcp-session", agent_id, OutcomeType::Success, now);

        let context = OTSContext::new();

        self.trajectory = Some(TrajectoryBuilder::new(metadata, context));
    }

    /// Record an execute tool call as an OTS turn with a decision.
    pub(crate) fn record_execute_turn(&mut self, code: &str, result: &Result<String>) {
        let Some(ref mut builder) = self.trajectory else {
            return;
        };

        let now = sim_now(); // determinism-ok: sim_now is DST-safe
        builder.start_turn(now);

        // User message: the Python code submitted
        builder.add_message(OTSMessage::new(
            MessageRole::User,
            OTSMessageContent::text(code),
            now,
        ));

        // Decision: the execution outcome
        let (outcome_str, consequence) = match result {
            Ok(text) => {
                // Assistant message: the execution result
                builder.add_message(OTSMessage::new(
                    MessageRole::Assistant,
                    OTSMessageContent::text(text),
                    now,
                ));
                ("success", OTSConsequence::success())
            }
            Err(e) => {
                builder.add_message(OTSMessage::new(
                    MessageRole::Assistant,
                    OTSMessageContent::text(&e.to_string()),
                    now,
                ));
                (
                    "failure",
                    OTSConsequence::failure().with_error_type(e.to_string()),
                )
            }
        };

        let decision = OTSDecision::new(
            DecisionType::ToolSelection,
            OTSChoice::new(format!("execute: {}", &code[..code.len().min(100)])),
            consequence,
        );
        builder.add_decision(decision);

        builder.end_turn(now);

        tracing::debug!(outcome = outcome_str, "ots.trajectory.turn_recorded");
    }

    /// Finalize and POST the trajectory to the server.
    pub(crate) async fn finalize_trajectory(&mut self) {
        let Some(builder) = self.trajectory.take() else {
            return;
        };

        let trajectory = builder.build();
        let json = match serde_json::to_string(&trajectory) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "ots.trajectory.serialize_failed");
                return;
            }
        };

        let url = format!("{}/api/ots/trajectories", self.base_url);
        let mut request = self.http.post(&url).body(json).header("Content-Type", "application/json");

        if let Some(ref agent_id) = self.agent_id {
            request = request.header("X-Agent-Id", agent_id);
        }
        if let Some(ref session_id) = self.session_id {
            request = request.header("X-Session-Id", session_id);
        }
        if let Some(ref api_key) = self.api_key {
            request = request.header("Authorization", format!("Bearer {api_key}"));
        }

        match request.send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!("ots.trajectory.uploaded");
            }
            Ok(resp) => {
                tracing::warn!(
                    status = resp.status().as_u16(),
                    "ots.trajectory.upload_failed"
                );
            }
            Err(e) => {
                tracing::warn!(error = %e, "ots.trajectory.upload_failed");
            }
        }
    }

    pub(crate) async fn run_execute(&mut self, code: &str) -> Result<String> {
        let http = self.http.clone();
        let base_url = self.base_url.clone();
        let agent_id = self.agent_id.clone();
        let agent_type = self.agent_type.clone();
        let session_id = self.session_id.clone();
        let api_key = self.api_key.clone();

        self.sandbox
            .execute(
                code,
                |function_name: String,
                 args: Vec<MontyObject>,
                 kwargs: Vec<(MontyObject, MontyObject)>| {
                    let http = http.clone();
                    let base_url = base_url.clone();
                    let agent_id = agent_id.clone();
                    let agent_type = agent_type.clone();
                    let session_id = session_id.clone();
                    let api_key = api_key.clone();
                    async move {
                        if !kwargs.is_empty() {
                            return Err(format!(
                                "temper.{function_name} does not support keyword arguments"
                            ));
                        }

                        // Strip self arg
                        let args = if args.is_empty() {
                            &args[..]
                        } else {
                            &args[1..]
                        };

                        // Extract tenant from args[0]
                        let tenant = temper_sandbox::helpers::expect_string_arg(
                            args,
                            0,
                            "tenant",
                            &function_name,
                        )?;
                        let remaining = if args.len() > 1 { &args[1..] } else { &[] };

                        let ctx = temper_sandbox::dispatch::DispatchContext {
                            http: &http,
                            base_url: &base_url,
                            tenant: &tenant,
                            agent_id: agent_id.as_deref(),
                            agent_type: agent_type.as_deref(),
                            session_id: session_id.as_deref(),
                            entity_set_resolver: None,
                            binary_path: None,
                            api_key: api_key.as_deref(),
                        };
                        temper_sandbox::dispatch::dispatch_temper_method(
                            &ctx,
                            &function_name,
                            remaining,
                            &kwargs,
                        )
                        .await
                    }
                },
            )
            .await
    }
}

/// Run the MCP server on stdio with JSON-RPC over newline-delimited JSON.
pub async fn run_stdio_server(config: McpConfig) -> Result<()> {
    let mut ctx = RuntimeContext::from_config(&config)?;
    let stdin = BufReader::new(io::stdin());
    let mut lines = stdin.lines();
    let mut stdout = io::stdout();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(response) = dispatch_json_line(&mut ctx, line).await {
            let encoded = serde_json::to_string(&response)?;
            stdout.write_all(encoded.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }
    }

    // Finalize and upload OTS trajectory on session close.
    ctx.finalize_trajectory().await;

    Ok(())
}
