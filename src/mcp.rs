use std::env;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::converge::scan_worktree;
use crate::store::MeshStore;

#[derive(Clone)]
pub struct PidMeshMcp {
    store: MeshStore,
    agent_id: String,
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RememberParams {
    content: String,
    #[serde(default = "default_kind")]
    kind: String,
    key: Option<String>,
    #[serde(default = "default_importance")]
    importance: f64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RecallParams {
    query: String,
    #[serde(default = "default_recall_limit")]
    limit: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SendParams {
    message: String,
    #[serde(default = "broadcast")]
    recipient: String,
    correlation_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct InboxParams {
    #[serde(default)]
    acknowledge: bool,
    #[serde(default = "default_inbox_limit")]
    limit: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SyncParams {
    /// Integration branch to measure against; defaults to main, then master.
    base: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ClaimParams {
    task: String,
    #[serde(default = "default_lease")]
    lease_seconds: u64,
    detail: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReleaseParams {
    task: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReserveResourcesParams {
    resources: Vec<String>,
    task: Option<String>,
    detail: Option<String>,
    #[serde(default = "default_lease")]
    lease_seconds: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ReleaseResourcesParams {
    resources: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct EventParams {
    #[serde(default)]
    after_sequence: i64,
    #[serde(default = "default_event_limit")]
    limit: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WaitParams {
    #[serde(default)]
    after_sequence: i64,
    #[serde(default = "default_wait")]
    timeout_seconds: f64,
    #[serde(default = "default_event_limit")]
    limit: u32,
}

#[tool_router]
impl PidMeshMcp {
    pub fn from_environment() -> Result<Self> {
        let store = MeshStore::from_environment()?;
        let name = env::var("PIDMESH_AGENT_NAME").unwrap_or_else(|_| "mcp-agent".to_owned());
        let provider = env::var("PIDMESH_PROVIDER").unwrap_or_else(|_| "mcp".to_owned());
        let workspace = env::var_os("PIDMESH_WORKSPACE").map(PathBuf::from);
        let capabilities = [
            "memory",
            "messaging",
            "claims",
            "resources",
            "events",
            "wait",
            "convergence",
        ]
        .map(str::to_owned);
        let managed_agent = env::var("PIDMESH_AGENT_ID").ok();
        let agent_id = if let Some(agent_id) = managed_agent {
            if !store.heartbeat(&agent_id)? {
                store.register_agent(
                    &name,
                    std::process::id(),
                    workspace.as_deref(),
                    &provider,
                    &capabilities,
                    Some(&agent_id),
                )?;
            }
            agent_id
        } else {
            store
                .register_agent(
                    &name,
                    std::process::id(),
                    workspace.as_deref(),
                    &provider,
                    &capabilities,
                    None,
                )?
                .get("agent_id")
                .and_then(Value::as_str)
                .context("registration did not return an agent id")?
                .to_owned()
        };
        Ok(Self {
            store,
            agent_id,
            tool_router: Self::tool_router(),
        })
    }

    #[must_use]
    pub fn store(&self) -> MeshStore {
        self.store.clone()
    }

    #[must_use]
    pub fn agent_id(&self) -> String {
        self.agent_id.clone()
    }

    #[tool(description = "List local agents, process liveness, task claims, and resource leases")]
    fn mesh_status(&self) -> Result<CallToolResult, McpError> {
        let result = self
            .store
            .heartbeat(&self.agent_id)
            .and_then(|_| self.store.status(Some(&self.agent_id), None));
        tool_result(result)
    }

    #[tool(description = "Append a durable memory visible to agents in this workspace")]
    fn remember(
        &self,
        Parameters(parameters): Parameters<RememberParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(self.store.remember(
            &self.agent_id,
            &parameters.content,
            &parameters.kind,
            parameters.key.as_deref(),
            parameters.importance,
        ))
    }

    #[tool(description = "Search durable memories written by any agent in this workspace")]
    fn recall(
        &self,
        Parameters(parameters): Parameters<RecallParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(
            self.store
                .recall(&self.agent_id, &parameters.query, parameters.limit),
        )
    }

    #[tool(description = "Send a message to an active agent by name/id or broadcast with '*'")]
    fn send(
        &self,
        Parameters(parameters): Parameters<SendParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(self.store.send(
            &self.agent_id,
            &parameters.message,
            &parameters.recipient,
            parameters.correlation_id.as_deref(),
        ))
    }

    #[tool(description = "Read unread direct and broadcast messages for this agent")]
    fn inbox(
        &self,
        Parameters(parameters): Parameters<InboxParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = self
            .store
            .inbox(&self.agent_id, true, parameters.limit)
            .and_then(|messages| {
                let acknowledged = if parameters.acknowledge {
                    let ids = messages
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|message| message["id"].as_i64())
                        .collect::<Vec<_>>();
                    self.store.acknowledge(&self.agent_id, &ids)?
                } else {
                    0
                };
                Ok(json!({"messages": messages, "acknowledged": acknowledged}))
            });
        tool_result(result)
    }

    #[tool(description = "Atomically acquire or renew a task lease")]
    fn claim(
        &self,
        Parameters(parameters): Parameters<ClaimParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(self.store.claim(
            &self.agent_id,
            &parameters.task,
            parameters.lease_seconds,
            parameters.detail.as_deref(),
        ))
    }

    #[tool(description = "Release a task lease owned by this agent")]
    fn release(
        &self,
        Parameters(parameters): Parameters<ReleaseParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(
            self.store
                .release(&self.agent_id, &parameters.task)
                .map(|released| json!({"released": released})),
        )
    }

    #[tool(description = "Atomically reserve paths or other resources before using them")]
    fn reserve_resources(
        &self,
        Parameters(parameters): Parameters<ReserveResourcesParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(self.store.reserve_resources(
            &self.agent_id,
            &parameters.resources,
            parameters.task.as_deref(),
            parameters.detail.as_deref(),
            parameters.lease_seconds,
        ))
    }

    #[tool(description = "Release resource leases owned by this agent")]
    fn release_resources(
        &self,
        Parameters(parameters): Parameters<ReleaseResourcesParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(
            self.store
                .release_resources(&self.agent_id, &parameters.resources)
                .map(|released| json!({"released": released})),
        )
    }

    #[tool(description = "Read ordered coordination events after a sequence number")]
    fn event_stream(
        &self,
        Parameters(parameters): Parameters<EventParams>,
    ) -> Result<CallToolResult, McpError> {
        tool_result(
            self.store
                .events(&self.agent_id, parameters.after_sequence, parameters.limit),
        )
    }

    #[tool(description = "Wait up to 60 seconds for the next coordination event")]
    async fn wait_for_events(
        &self,
        Parameters(parameters): Parameters<WaitParams>,
    ) -> Result<CallToolResult, McpError> {
        let store = self.store.clone();
        let agent_id = self.agent_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            if !parameters.timeout_seconds.is_finite()
                || parameters.timeout_seconds.is_sign_negative()
            {
                anyhow::bail!("timeout_seconds must be a finite non-negative number");
            }
            store.wait_for_events(
                &agent_id,
                parameters.after_sequence,
                Duration::from_secs_f64(parameters.timeout_seconds),
                Duration::from_millis(100),
                parameters.limit,
            )
        })
        .await
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
        tool_result(result)
    }

    #[tool(
        description = "Publish what this checkout actually changed and report paths contested by other agents"
    )]
    fn sync_footprint(
        &self,
        Parameters(parameters): Parameters<SyncParams>,
    ) -> Result<CallToolResult, McpError> {
        let result = checkout_directory()
            .and_then(|directory| scan_worktree(&directory, parameters.base.as_deref()))
            .and_then(|scan| self.store.publish_footprint(&self.agent_id, &scan));
        tool_result(result)
    }

    #[tool(description = "Report paths contested by more than one agent's observed footprint")]
    fn collisions(&self) -> Result<CallToolResult, McpError> {
        tool_result(self.store.collisions(&self.agent_id))
    }
}

#[tool_handler(
    name = "PidMesh",
    version = "1.5.0",
    instructions = "Check status and inbox, claim a task, reserve intended paths before editing, and record decisions as memories. Call sync_footprint after each round of edits so other agents can see what this checkout actually changed, and read collisions before continuing work on a contested path."
)]
impl ServerHandler for PidMeshMcp {}

fn tool_result(result: Result<Value>) -> Result<CallToolResult, McpError> {
    Ok(match result {
        Ok(value) => CallToolResult::structured(value),
        Err(error) => CallToolResult::structured_error(json!({"error": format!("{error:#}")})),
    })
}

/// The checkout this server observes: the configured workspace, else the process directory.
fn checkout_directory() -> Result<PathBuf> {
    match env::var_os("PIDMESH_WORKSPACE") {
        Some(workspace) => Ok(PathBuf::from(workspace)),
        None => Ok(env::current_dir()?),
    }
}

fn default_kind() -> String {
    "note".to_owned()
}

const fn default_importance() -> f64 {
    0.5
}

const fn default_recall_limit() -> u32 {
    10
}

const fn default_inbox_limit() -> u32 {
    50
}

const fn default_event_limit() -> u32 {
    100
}

const fn default_lease() -> u64 {
    300
}

const fn default_wait() -> f64 {
    30.0
}

fn broadcast() -> String {
    "*".to_owned()
}
