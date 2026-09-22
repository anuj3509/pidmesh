use std::collections::{BTreeMap, HashSet};
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::converge::{Participant, WorktreeScan, base_divergent, classify};

const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS workspaces (
    id TEXT PRIMARY KEY,
    root TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS agents (
    id TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    pid INTEGER NOT NULL,
    parent_pid INTEGER,
    provider TEXT NOT NULL,
    capabilities_json TEXT NOT NULL DEFAULT '[]',
    started_at INTEGER NOT NULL,
    heartbeat_at INTEGER NOT NULL,
    stopped_at INTEGER,
    status TEXT NOT NULL DEFAULT 'running',
    checkout_path TEXT,
    git_branch TEXT
);

CREATE INDEX IF NOT EXISTS agents_workspace_status
ON agents(workspace_id, status, heartbeat_at DESC);

CREATE TABLE IF NOT EXISTS memories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    agent_id TEXT REFERENCES agents(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    key TEXT,
    content TEXT NOT NULL,
    importance REAL NOT NULL DEFAULT 0.5,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS memories_workspace_created
ON memories(workspace_id, created_at DESC);

CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
    content,
    key,
    kind,
    content='memories',
    content_rowid='id'
);

CREATE TRIGGER IF NOT EXISTS memories_after_insert AFTER INSERT ON memories BEGIN
    INSERT INTO memory_fts(rowid, content, key, kind)
    VALUES (new.id, new.content, coalesce(new.key, ''), new.kind);
END;

CREATE TRIGGER IF NOT EXISTS memories_after_delete AFTER DELETE ON memories BEGIN
    INSERT INTO memory_fts(memory_fts, rowid, content, key, kind)
    VALUES ('delete', old.id, old.content, coalesce(old.key, ''), old.kind);
END;

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    sender_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    recipient_id TEXT REFERENCES agents(id) ON DELETE CASCADE,
    body TEXT NOT NULL,
    correlation_id TEXT,
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS messages_workspace_created
ON messages(workspace_id, created_at DESC);

CREATE TABLE IF NOT EXISTS message_receipts (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    acknowledged_at INTEGER NOT NULL,
    PRIMARY KEY (message_id, agent_id)
);

CREATE TABLE IF NOT EXISTS claims (
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    task_key TEXT NOT NULL,
    detail TEXT,
    agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    claimed_at INTEGER NOT NULL,
    lease_expires_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, task_key)
);

CREATE INDEX IF NOT EXISTS claims_lease
ON claims(workspace_id, lease_expires_at);

CREATE TABLE IF NOT EXISTS resource_leases (
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    resource_kind TEXT NOT NULL,
    resource_key TEXT NOT NULL,
    agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    task_key TEXT,
    detail TEXT,
    acquired_at INTEGER NOT NULL,
    lease_expires_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, resource_kind, resource_key)
);

CREATE INDEX IF NOT EXISTS resource_leases_agent
ON resource_leases(workspace_id, agent_id);

CREATE INDEX IF NOT EXISTS resource_leases_expiry
ON resource_leases(workspace_id, lease_expires_at);

CREATE TABLE IF NOT EXISTS events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    agent_id TEXT REFERENCES agents(id) ON DELETE SET NULL,
    event_type TEXT NOT NULL,
    subject TEXT,
    data_json TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS events_workspace_sequence
ON events(workspace_id, sequence DESC);

CREATE TABLE IF NOT EXISTS footprints (
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    path TEXT NOT NULL,
    change_kind TEXT NOT NULL,
    digest TEXT,
    base_commit TEXT,
    branch TEXT,
    ranges TEXT,
    observed_at INTEGER NOT NULL,
    PRIMARY KEY (workspace_id, agent_id, path)
);

CREATE INDEX IF NOT EXISTS footprints_workspace_path
ON footprints(workspace_id, path);

CREATE INDEX IF NOT EXISTS footprints_agent
ON footprints(workspace_id, agent_id);
";

#[derive(Clone, Debug)]
pub struct MeshStore {
    path: PathBuf,
    connection: Arc<Mutex<Connection>>,
}

#[derive(Debug)]
struct AgentRow {
    workspace_id: String,
    started_at: i64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ResourceKey {
    kind: String,
    key: String,
}

struct WorkspaceContext {
    branch: Option<String>,
    checkout: String,
    root: String,
}

struct DashboardActivity {
    events: Vec<Value>,
    memories: Vec<Value>,
    messages: Vec<Value>,
    totals: (i64, i64, i64),
}

impl MeshStore {
    pub fn new(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            let created = !parent.exists();
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
            if created {
                restrict_directory(parent)?;
            }
        }
        let connection = Self::open_connection(&path)?;
        let store = Self {
            path,
            connection: Arc::new(Mutex::new(connection)),
        };
        store.initialize()?;
        store.restrict_permissions()?;
        Ok(store)
    }

    pub fn from_environment() -> Result<Self> {
        Self::new(default_database_path()?)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn register_agent(
        &self,
        name: &str,
        pid: u32,
        root: Option<&Path>,
        provider: &str,
        capabilities: &[String],
        agent_id: Option<&str>,
    ) -> Result<Value> {
        let workspace = workspace_context(root)?;
        let identifier = agent_id.map_or_else(
            || format!("{name}-{pid}-{}", &Uuid::new_v4().simple().to_string()[..8]),
            ToOwned::to_owned,
        );
        let now = now_ms()?;
        self.write(|transaction| {
            let workspace_id = Self::ensure_workspace(transaction, &workspace.root)?;
            transaction.execute(
                "INSERT INTO agents(
                    id, workspace_id, name, pid, parent_pid, provider,
                    capabilities_json, started_at, heartbeat_at, checkout_path, git_branch
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    identifier,
                    workspace_id,
                    name,
                    i64::from(pid),
                    parent_pid(),
                    provider,
                    serde_json::to_string(capabilities)?,
                    now,
                    now,
                    workspace.checkout,
                    workspace.branch
                ],
            )?;
            Self::event(
                transaction,
                &workspace_id,
                Some(&identifier),
                "agent.joined",
                Some(&identifier),
                &json!({}),
            )?;
            Ok(json!({
                "agent_id": identifier,
                "name": name,
                "pid": pid,
                "provider": provider,
                "checkout_path": workspace.checkout,
                "git_branch": workspace.branch,
                "workspace": workspace.root,
                "workspace_id": workspace_id
            }))
        })
    }

    pub fn heartbeat(&self, agent_id: &str) -> Result<bool> {
        let now = now_ms()?;
        self.write(|transaction| {
            let changed = transaction.execute(
                "UPDATE agents SET heartbeat_at = ?, status = 'running', stopped_at = NULL
                 WHERE id = ?",
                params![now, agent_id],
            )?;
            Ok(changed == 1)
        })
    }

    pub fn update_agent_pid(&self, agent_id: &str, pid: u32) -> Result<bool> {
        let now = now_ms()?;
        self.write(|transaction| {
            let changed = transaction.execute(
                "UPDATE agents SET pid = ?, heartbeat_at = ? WHERE id = ?",
                params![i64::from(pid), now, agent_id],
            )?;
            Ok(changed == 1)
        })
    }

    pub fn update_agent_checkout(
        &self,
        agent_id: &str,
        checkout_path: &Path,
        git_branch: Option<&str>,
    ) -> Result<bool> {
        let checkout_path = checkout_path
            .canonicalize()
            .unwrap_or_else(|_| checkout_path.to_path_buf());
        self.write(|transaction| {
            Ok(transaction.execute(
                "UPDATE agents SET checkout_path = ?, git_branch = ? WHERE id = ?",
                params![checkout_path.to_string_lossy(), git_branch, agent_id],
            )? == 1)
        })
    }

    pub fn stop_agent(&self, agent_id: &str) -> Result<bool> {
        let now = now_ms()?;
        self.write(|transaction| {
            let workspace_id: Option<String> = transaction
                .query_row(
                    "SELECT workspace_id FROM agents WHERE id = ?",
                    [agent_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(workspace_id) = workspace_id else {
                return Ok(false);
            };
            transaction.execute(
                "UPDATE agents SET status = 'stopped', stopped_at = ? WHERE id = ?",
                params![now, agent_id],
            )?;
            transaction.execute("DELETE FROM claims WHERE agent_id = ?", [agent_id])?;
            transaction.execute("DELETE FROM resource_leases WHERE agent_id = ?", [agent_id])?;
            Self::event(
                transaction,
                &workspace_id,
                Some(agent_id),
                "agent.stopped",
                Some(agent_id),
                &json!({}),
            )?;
            Ok(true)
        })
    }

    pub fn remember(
        &self,
        agent_id: &str,
        content: &str,
        kind: &str,
        key: Option<&str>,
        importance: f64,
    ) -> Result<Value> {
        if !(0.0..=1.0).contains(&importance) {
            bail!("importance must be between 0 and 1");
        }
        self.write(|transaction| {
            let agent = Self::agent_row(transaction, agent_id)?;
            transaction.execute(
                "INSERT INTO memories(
                    workspace_id, agent_id, kind, key, content, importance, created_at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![
                    agent.workspace_id,
                    agent_id,
                    kind,
                    key,
                    content,
                    importance,
                    now_ms()?
                ],
            )?;
            let memory_id = transaction.last_insert_rowid();
            Self::event(
                transaction,
                &agent.workspace_id,
                Some(agent_id),
                "memory.created",
                Some(&memory_id.to_string()),
                &json!({}),
            )?;
            Ok(json!({"memory_id": memory_id, "kind": kind, "key": key}))
        })
    }

    pub fn recall(&self, agent_id: &str, query: &str, limit: u32) -> Result<Value> {
        self.read(|connection| {
            let agent = Self::agent_row(connection, agent_id)?;
            let terms: Vec<_> = query
                .split(|character: char| {
                    !(character.is_alphanumeric() || matches!(character, '.' | '-' | '_'))
                })
                .filter(|term| !term.is_empty())
                .collect();
            let mut results = Vec::new();
            if terms.is_empty() {
                let mut statement = connection.prepare(
                    "SELECT m.id, m.agent_id, m.kind, m.key, m.content, m.importance,
                            m.created_at, a.name
                     FROM memories m LEFT JOIN agents a ON a.id = m.agent_id
                     WHERE m.workspace_id = ?
                     ORDER BY m.importance DESC, m.created_at DESC LIMIT ?",
                )?;
                let rows = statement.query_map(params![agent.workspace_id, limit], memory_json)?;
                for row in rows {
                    results.push(row?);
                }
            } else {
                let expression = terms
                    .iter()
                    .map(|term| format!("\"{term}\"*"))
                    .collect::<Vec<_>>()
                    .join(" AND ");
                let mut statement = connection.prepare(
                    "SELECT m.id, m.agent_id, m.kind, m.key, m.content, m.importance,
                            m.created_at, a.name
                     FROM memory_fts
                     JOIN memories m ON m.id = memory_fts.rowid
                     LEFT JOIN agents a ON a.id = m.agent_id
                     WHERE memory_fts MATCH ? AND m.workspace_id = ?
                     ORDER BY bm25(memory_fts), m.importance DESC, m.created_at DESC LIMIT ?",
                )?;
                let rows = statement
                    .query_map(params![expression, agent.workspace_id, limit], memory_json)?;
                for row in rows {
                    results.push(row?);
                }
            }
            Ok(Value::Array(results))
        })
    }

    pub fn send(
        &self,
        agent_id: &str,
        body: &str,
        recipient: &str,
        correlation_id: Option<&str>,
    ) -> Result<Value> {
        self.write(|transaction| {
            let sender = Self::agent_row(transaction, agent_id)?;
            let recipient_id = if recipient == "*" {
                None
            } else {
                transaction
                    .query_row(
                        "SELECT id FROM agents
                         WHERE workspace_id = ? AND (id = ? OR name = ?) AND status = 'running'
                         ORDER BY heartbeat_at DESC LIMIT 1",
                        params![sender.workspace_id, recipient, recipient],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .ok_or_else(|| anyhow!("active recipient not found: {recipient}"))?
                    .into()
            };
            transaction.execute(
                "INSERT INTO messages(
                    workspace_id, sender_id, recipient_id, body, correlation_id, created_at
                 ) VALUES (?, ?, ?, ?, ?, ?)",
                params![
                    sender.workspace_id,
                    agent_id,
                    recipient_id,
                    body,
                    correlation_id,
                    now_ms()?
                ],
            )?;
            let message_id = transaction.last_insert_rowid();
            let routed_to = recipient_id.as_deref().unwrap_or("*");
            Self::event(
                transaction,
                &sender.workspace_id,
                Some(agent_id),
                "message.sent",
                Some(&message_id.to_string()),
                &json!({"recipient": routed_to}),
            )?;
            Ok(json!({"message_id": message_id, "recipient": routed_to}))
        })
    }

    pub fn inbox(&self, agent_id: &str, unread_only: bool, limit: u32) -> Result<Value> {
        self.read(|connection| {
            let agent = Self::agent_row(connection, agent_id)?;
            let unread = if unread_only {
                "AND r.message_id IS NULL"
            } else {
                ""
            };
            let sql = format!(
                "SELECT m.id, m.body, m.correlation_id, m.created_at, m.recipient_id,
                        s.id, s.name, r.acknowledged_at
                 FROM messages m
                 JOIN agents s ON s.id = m.sender_id
                 LEFT JOIN message_receipts r
                   ON r.message_id = m.id AND r.agent_id = ?
                 WHERE m.workspace_id = ? AND m.sender_id != ?
                   AND (m.recipient_id = ? OR (m.recipient_id IS NULL AND m.created_at >= ?))
                   {unread}
                 ORDER BY m.created_at, m.id LIMIT ?"
            );
            let mut statement = connection.prepare(&sql)?;
            let rows = statement.query_map(
                params![
                    agent_id,
                    agent.workspace_id,
                    agent_id,
                    agent_id,
                    agent.started_at,
                    limit
                ],
                |row| {
                    Ok(json!({
                        "id": row.get::<_, i64>(0)?,
                        "body": row.get::<_, String>(1)?,
                        "correlation_id": row.get::<_, Option<String>>(2)?,
                        "created_at": row.get::<_, i64>(3)?,
                        "recipient_id": row.get::<_, Option<String>>(4)?,
                        "sender_id": row.get::<_, String>(5)?,
                        "sender_name": row.get::<_, String>(6)?,
                        "acknowledged_at": row.get::<_, Option<i64>>(7)?
                    }))
                },
            )?;
            let mut messages = Vec::new();
            for row in rows {
                messages.push(row?);
            }
            Ok(Value::Array(messages))
        })
    }

    pub fn acknowledge(&self, agent_id: &str, message_ids: &[i64]) -> Result<usize> {
        if message_ids.is_empty() {
            return Ok(0);
        }
        self.write(|transaction| {
            let agent = Self::agent_row(transaction, agent_id)?;
            let now = now_ms()?;
            let mut acknowledged = 0;
            for message_id in message_ids {
                let visible: bool = transaction.query_row(
                    "SELECT EXISTS(
                        SELECT 1 FROM messages
                        WHERE workspace_id = ? AND id = ?
                          AND (recipient_id = ? OR recipient_id IS NULL)
                     )",
                    params![agent.workspace_id, message_id, agent_id],
                    |row| row.get(0),
                )?;
                if visible {
                    transaction.execute(
                        "INSERT OR IGNORE INTO message_receipts(
                            message_id, agent_id, acknowledged_at
                         ) VALUES (?, ?, ?)",
                        params![message_id, agent_id, now],
                    )?;
                    acknowledged += 1;
                }
            }
            Ok(acknowledged)
        })
    }

    pub fn claim(
        &self,
        agent_id: &str,
        task_key: &str,
        lease_seconds: u64,
        detail: Option<&str>,
    ) -> Result<Value> {
        if lease_seconds == 0 {
            bail!("lease_seconds must be positive");
        }
        let now = now_ms()?;
        let lease_expires_at = now
            .checked_add(i64::try_from(lease_seconds)?.saturating_mul(1000))
            .ok_or_else(|| anyhow!("lease duration is too large"))?;
        self.write(|transaction| {
            let agent = Self::agent_row(transaction, agent_id)?;
            transaction.execute(
                "INSERT INTO claims(
                    workspace_id, task_key, detail, agent_id,
                    claimed_at, lease_expires_at, updated_at
                 ) VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(workspace_id, task_key) DO UPDATE SET
                    detail = excluded.detail,
                    agent_id = excluded.agent_id,
                    claimed_at = excluded.claimed_at,
                    lease_expires_at = excluded.lease_expires_at,
                    updated_at = excluded.updated_at
                 WHERE claims.lease_expires_at <= ? OR claims.agent_id = excluded.agent_id",
                params![
                    agent.workspace_id,
                    task_key,
                    detail,
                    agent_id,
                    now,
                    lease_expires_at,
                    now,
                    now
                ],
            )?;
            let claim = transaction.query_row(
                "SELECT c.task_key, c.detail, c.agent_id, c.claimed_at,
                        c.lease_expires_at, c.updated_at, a.name
                 FROM claims c JOIN agents a ON a.id = c.agent_id
                 WHERE c.workspace_id = ? AND c.task_key = ?",
                params![agent.workspace_id, task_key],
                |row| {
                    Ok(json!({
                        "task_key": row.get::<_, String>(0)?,
                        "detail": row.get::<_, Option<String>>(1)?,
                        "agent_id": row.get::<_, String>(2)?,
                        "claimed_at": row.get::<_, i64>(3)?,
                        "lease_expires_at": row.get::<_, i64>(4)?,
                        "updated_at": row.get::<_, i64>(5)?,
                        "agent_name": row.get::<_, String>(6)?
                    }))
                },
            )?;
            let acquired = claim["agent_id"] == agent_id;
            if acquired {
                Self::event(
                    transaction,
                    &agent.workspace_id,
                    Some(agent_id),
                    "task.claimed",
                    Some(task_key),
                    &json!({}),
                )?;
            }
            let mut claim = claim;
            claim["acquired"] = Value::Bool(acquired);
            Ok(claim)
        })
    }

    pub fn release(&self, agent_id: &str, task_key: &str) -> Result<bool> {
        self.write(|transaction| {
            let agent = Self::agent_row(transaction, agent_id)?;
            let changed = transaction.execute(
                "DELETE FROM claims WHERE workspace_id = ? AND task_key = ? AND agent_id = ?",
                params![agent.workspace_id, task_key, agent_id],
            )?;
            if changed == 1 {
                Self::event(
                    transaction,
                    &agent.workspace_id,
                    Some(agent_id),
                    "task.released",
                    Some(task_key),
                    &json!({}),
                )?;
            }
            Ok(changed == 1)
        })
    }

    pub fn reserve_resources(
        &self,
        agent_id: &str,
        resources: &[String],
        task_key: Option<&str>,
        detail: Option<&str>,
        lease_seconds: u64,
    ) -> Result<Value> {
        let resources = normalize_resources(resources)?;
        if lease_seconds == 0 || lease_seconds > 86_400 {
            bail!("lease_seconds must be between 1 and 86400");
        }
        let now = now_ms()?;
        let lease_expires_at = now
            .checked_add(i64::try_from(lease_seconds)?.saturating_mul(1000))
            .ok_or_else(|| anyhow!("lease duration is too large"))?;
        self.write(|transaction| {
            let agent = Self::agent_row(transaction, agent_id)?;
            transaction.execute(
                "DELETE FROM resource_leases
                 WHERE workspace_id = ? AND lease_expires_at <= ?",
                params![agent.workspace_id, now],
            )?;
            let mut conflicts = Vec::new();
            let mut seen = HashSet::new();
            for resource in &resources {
                for conflict in
                    resource_conflicts(transaction, &agent.workspace_id, agent_id, resource, now)?
                {
                    let identity = (
                        conflict["resource_kind"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                        conflict["resource_key"]
                            .as_str()
                            .unwrap_or_default()
                            .to_owned(),
                    );
                    if seen.insert(identity) {
                        conflicts.push(conflict);
                    }
                }
            }
            if !conflicts.is_empty() {
                return Ok(json!({"acquired": false, "conflicts": conflicts, "resources": []}));
            }
            let mut acquired = Vec::new();
            for resource in &resources {
                transaction.execute(
                    "INSERT INTO resource_leases(
                        workspace_id, resource_kind, resource_key, agent_id, task_key,
                        detail, acquired_at, lease_expires_at, updated_at
                     ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
                     ON CONFLICT(workspace_id, resource_kind, resource_key) DO UPDATE SET
                        agent_id = excluded.agent_id,
                        task_key = excluded.task_key,
                        detail = excluded.detail,
                        lease_expires_at = excluded.lease_expires_at,
                        updated_at = excluded.updated_at",
                    params![
                        agent.workspace_id,
                        resource.kind,
                        resource.key,
                        agent_id,
                        task_key,
                        detail,
                        now,
                        lease_expires_at,
                        now
                    ],
                )?;
                let subject = resource.display();
                Self::event(
                    transaction,
                    &agent.workspace_id,
                    Some(agent_id),
                    "resource.reserved",
                    Some(&subject),
                    &json!({"lease_expires_at": lease_expires_at, "task_key": task_key}),
                )?;
                acquired.push(subject);
            }
            Ok(json!({
                "acquired": true,
                "conflicts": [],
                "lease_expires_at": lease_expires_at,
                "resources": acquired
            }))
        })
    }

    pub fn release_resources(&self, agent_id: &str, resources: &[String]) -> Result<usize> {
        let resources = normalize_resources(resources)?;
        self.write(|transaction| {
            let agent = Self::agent_row(transaction, agent_id)?;
            let mut released = 0;
            for resource in &resources {
                let changed = transaction.execute(
                    "DELETE FROM resource_leases
                     WHERE workspace_id = ? AND resource_kind = ?
                       AND resource_key = ? AND agent_id = ?",
                    params![agent.workspace_id, resource.kind, resource.key, agent_id],
                )?;
                if changed == 1 {
                    let subject = resource.display();
                    Self::event(
                        transaction,
                        &agent.workspace_id,
                        Some(agent_id),
                        "resource.released",
                        Some(&subject),
                        &json!({}),
                    )?;
                    released += 1;
                }
            }
            Ok(released)
        })
    }

    pub fn resources(&self, agent_id: &str) -> Result<Value> {
        let now = now_ms()?;
        self.read(|connection| {
            let agent = Self::agent_row(connection, agent_id)?;
            workspace_resources(connection, &agent.workspace_id, now)
        })
    }

    /// Withdraw this agent's footprint entirely.
    ///
    /// Publishing an empty scan has the same effect, but an agent that is shutting down or whose
    /// checkout has been removed cannot produce a scan at all.
    pub fn release_footprint(&self, agent_id: &str) -> Result<Value> {
        self.write(|transaction| {
            let agent = Self::agent_row(transaction, agent_id)?;
            let before = collision_signatures(transaction, &agent.workspace_id)?;
            let released = transaction.execute(
                "DELETE FROM footprints WHERE workspace_id = ? AND agent_id = ?",
                params![agent.workspace_id, agent_id],
            )?;
            let after = collision_signatures(transaction, &agent.workspace_id)?;
            for path in before.keys() {
                if !after.contains_key(path) {
                    Self::event(
                        transaction,
                        &agent.workspace_id,
                        Some(agent_id),
                        "collision.cleared",
                        Some(path),
                        &json!({}),
                    )?;
                }
            }
            Ok(json!({"released": released}))
        })
    }

    /// Decide whether this agent can merge without breaking somebody else.
    ///
    /// Collision reporting says a conflict exists; this says what to do about it. The caller
    /// supplies the integration branch's current head, because resolving it is a git question.
    pub fn merge_readiness(&self, agent_id: &str, integration_head: &str) -> Result<Value> {
        let now = now_ms()?;
        self.read(|connection| {
            let agent = Self::agent_row(connection, agent_id)?;
            let base_commit: Option<String> = connection
                .query_row(
                    "SELECT base_commit FROM footprints
                     WHERE workspace_id = ? AND agent_id = ? LIMIT 1",
                    params![agent.workspace_id, agent_id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();

            let mut blockers = Vec::new();

            // A worktree cut from a commit the integration branch has moved past may still merge
            // cleanly and be wrong, because it was written against code that no longer exists.
            if let Some(base) = base_commit.as_deref()
                && base != integration_head
            {
                blockers.push(json!({
                    "code": "stale_base",
                    "detail": "the integration branch has advanced since this worktree was cut; \
                               rebase before merging",
                    "base_commit": base,
                    "integration_head": integration_head
                }));
            }

            // Only live peers block a merge. A stopped agent's preserved worktree is still worth
            // reporting as a collision, but it cannot be asked to rebase and must not deadlock
            // the queue.
            for (path, participants) in workspace_collisions(connection, &agent.workspace_id)? {
                if !participants
                    .iter()
                    .any(|participant| participant.agent_id == agent_id)
                {
                    continue;
                }
                let severity = classify(&participants);
                // Duplicated work is safe to merge, and edits to separable regions of one file
                // are what git three-way merges exist to resolve.
                if severity == "identical" || severity == "adjacent" {
                    continue;
                }
                let peers: Vec<Value> = participants
                    .iter()
                    .filter(|participant| {
                        participant.agent_id != agent_id && participant.status == "running"
                    })
                    .map(|participant| {
                        json!({"agent_id": participant.agent_id, "agent_name": participant.agent_name})
                    })
                    .collect();
                if peers.is_empty() {
                    continue;
                }
                blockers.push(json!({
                    "code": "contested_path",
                    "detail": "another live checkout holds different content on this path",
                    "path": path,
                    "severity": severity,
                    "peers": peers
                }));
            }

            // Whoever holds the lease is mid-merge; anyone else would be racing them.
            let lease: Option<(String, i64)> = connection
                .query_row(
                    "SELECT agent_id, lease_expires_at FROM claims
                     WHERE workspace_id = ? AND task_key = ? AND lease_expires_at > ?",
                    params![agent.workspace_id, INTEGRATION_TASK_KEY, now],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let holder = lease.as_ref().map(|(holder, _)| holder.clone());
            if let Some((holder, expires_at)) = lease.as_ref()
                && holder != agent_id
            {
                blockers.push(json!({
                    "code": "integration_held",
                    "detail": "another agent holds the integration lease and is merging now",
                    "held_by": holder,
                    "lease_expires_at": expires_at
                }));
            }

            Ok(json!({
                "agent_id": agent_id,
                "mergeable": blockers.is_empty(),
                "base_commit": base_commit,
                "integration_head": integration_head,
                "holds_integration_lease": holder.as_deref() == Some(agent_id),
                "blockers": blockers
            }))
        })
    }

    /// Live agents in this workspace that have a recorded checkout.
    ///
    /// A supervisor uses this to observe a fleet that never calls `sync` itself: every checkout is
    /// already known from registration, so no agent cooperation is required.
    pub fn live_checkouts(&self, root: Option<&Path>) -> Result<Vec<AgentCheckout>> {
        self.read(|connection| {
            let root = workspace_root(root)?;
            let workspace_id = connection
                .query_row("SELECT id FROM workspaces WHERE root = ?", [&root], |row| {
                    row.get::<_, String>(0)
                })
                .optional()?;
            let Some(workspace_id) = workspace_id else {
                return Ok(Vec::new());
            };
            let mut statement = connection.prepare(
                "SELECT id, name, pid, checkout_path FROM agents
                 WHERE workspace_id = ? AND status = 'running' AND checkout_path IS NOT NULL
                 ORDER BY heartbeat_at DESC",
            )?;
            let rows = statement
                .query_map([&workspace_id], |row| {
                    Ok((
                        AgentCheckout {
                            agent_id: row.get(0)?,
                            agent_name: row.get(1)?,
                            checkout_path: PathBuf::from(row.get::<_, String>(3)?),
                        },
                        row.get::<_, u32>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            // A session marked running whose PID is gone should not have its checkout scanned.
            Ok(rows
                .into_iter()
                .filter(|(_, pid)| process_is_alive(*pid))
                .map(|(checkout, _)| checkout)
                .collect())
        })
    }

    /// Replace this agent's observed footprint and report the collisions that result.
    ///
    /// The footprint is authoritative per agent: a scan that no longer touches a path releases it,
    /// so an agent that merges and cleans its worktree drops out of every collision automatically.
    pub fn publish_footprint(&self, agent_id: &str, scan: &WorktreeScan) -> Result<Value> {
        let now = now_ms()?;
        self.write(|transaction| {
            let agent = Self::agent_row(transaction, agent_id)?;
            let before = collision_signatures(transaction, &agent.workspace_id)?;
            transaction.execute(
                "DELETE FROM footprints WHERE workspace_id = ? AND agent_id = ?",
                params![agent.workspace_id, agent_id],
            )?;
            for entry in &scan.entries {
                transaction.execute(
                    "INSERT INTO footprints(
                        workspace_id, agent_id, path, change_kind, digest,
                        base_commit, branch, ranges, observed_at
                     ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                    params![
                        agent.workspace_id,
                        agent_id,
                        entry.path,
                        entry.change_kind.as_str(),
                        entry.digest,
                        scan.base_commit,
                        scan.branch,
                        entry.ranges,
                        now
                    ],
                )?;
            }
            let contested = workspace_collisions(transaction, &agent.workspace_id)?;
            let after = signatures_of(&contested);

            // Emitting only on transitions keeps `pidmesh wait` from spinning while a fleet
            // re-scans, while still waking every peer the moment a real overlap appears.
            for (path, signature) in &after {
                if before.get(path) != Some(signature) {
                    Self::event(
                        transaction,
                        &agent.workspace_id,
                        Some(agent_id),
                        "collision.detected",
                        Some(path),
                        &json!({"signature": signature}),
                    )?;
                }
            }
            for path in before.keys() {
                if !after.contains_key(path) {
                    Self::event(
                        transaction,
                        &agent.workspace_id,
                        Some(agent_id),
                        "collision.cleared",
                        Some(path),
                        &json!({}),
                    )?;
                }
            }

            Self::event(
                transaction,
                &agent.workspace_id,
                Some(agent_id),
                "footprint.published",
                scan.branch.as_deref(),
                &json!({
                    "paths": scan.entries.len(),
                    "base_ref": scan.base_ref,
                    "base_commit": scan.base_commit
                }),
            )?;

            Ok(json!({
                "agent_id": agent_id,
                "base_ref": scan.base_ref,
                "base_commit": scan.base_commit,
                "branch": scan.branch,
                "paths": scan.entries.len(),
                "collisions": collision_report(&contested, agent_id),
                "collision_count": contested.len()
            }))
        })
    }

    /// Report every contested path in the workspace without changing any footprint.
    pub fn collisions(&self, agent_id: &str) -> Result<Value> {
        self.read(|connection| {
            let agent = Self::agent_row(connection, agent_id)?;
            let contested = workspace_collisions(connection, &agent.workspace_id)?;
            Ok(json!({
                "agent_id": agent_id,
                "collisions": collision_report(&contested, agent_id),
                "collision_count": contested.len()
            }))
        })
    }

    pub fn status(&self, agent_id: Option<&str>, root: Option<&Path>) -> Result<Value> {
        self.read(|connection| {
            let workspace = if let Some(agent_id) = agent_id {
                connection
                    .query_row(
                        "SELECT w.id, w.root FROM workspaces w
                         JOIN agents a ON a.workspace_id = w.id WHERE a.id = ?",
                        [agent_id],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()?
                    .ok_or_else(|| anyhow!("agent not found: {agent_id}"))?
            } else {
                let root = workspace_root(root)?;
                let found = connection
                    .query_row(
                        "SELECT id, root FROM workspaces WHERE root = ?",
                        [&root],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()?;
                let Some(found) = found else {
                    return Ok(json!({
                        "workspace": root,
                        "agents": [],
                        "claims": [],
                        "resources": []
                    }));
                };
                found
            };
            let mut agents_statement = connection.prepare(
                "SELECT id, name, pid, parent_pid, provider, capabilities_json,
                        started_at, heartbeat_at, stopped_at, status,
                        checkout_path, git_branch
                 FROM agents WHERE workspace_id = ?
                 ORDER BY status = 'running' DESC, heartbeat_at DESC",
            )?;
            let current_time = now_ms()?;
            let agent_rows = agents_statement.query_map([&workspace.0], |row| {
                let pid = row.get::<_, u32>(2)?;
                let capabilities: String = row.get(5)?;
                let heartbeat_at = row.get::<_, i64>(7)?;
                Ok(json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "pid": pid,
                    "parent_pid": row.get::<_, Option<i64>>(3)?,
                    "provider": row.get::<_, String>(4)?,
                    "capabilities": serde_json::from_str::<Value>(&capabilities)
                        .unwrap_or_else(|_| json!([])),
                    "started_at": row.get::<_, i64>(6)?,
                    "heartbeat_at": heartbeat_at,
                    "stopped_at": row.get::<_, Option<i64>>(8)?,
                    "status": row.get::<_, String>(9)?,
                    "checkout_path": row.get::<_, Option<String>>(10)?,
                    "git_branch": row.get::<_, Option<String>>(11)?,
                    "pid_alive": process_is_alive(pid),
                    "heartbeat_age_ms": current_time.saturating_sub(heartbeat_at)
                }))
            })?;
            let mut agents = Vec::new();
            for row in agent_rows {
                agents.push(row?);
            }
            let mut claims_statement = connection.prepare(
                "SELECT c.task_key, c.detail, c.agent_id, c.claimed_at,
                        c.lease_expires_at, c.updated_at, a.name
                 FROM claims c JOIN agents a ON a.id = c.agent_id
                 WHERE c.workspace_id = ? ORDER BY c.task_key",
            )?;
            let claim_rows = claims_statement.query_map([&workspace.0], |row| {
                Ok(json!({
                    "task_key": row.get::<_, String>(0)?,
                    "detail": row.get::<_, Option<String>>(1)?,
                    "agent_id": row.get::<_, String>(2)?,
                    "claimed_at": row.get::<_, i64>(3)?,
                    "lease_expires_at": row.get::<_, i64>(4)?,
                    "updated_at": row.get::<_, i64>(5)?,
                    "agent_name": row.get::<_, String>(6)?
                }))
            })?;
            let mut claims = Vec::new();
            for row in claim_rows {
                claims.push(row?);
            }
            let resources = workspace_resources(connection, &workspace.0, current_time)?;
            Ok(json!({
                "workspace": workspace.1,
                "workspace_id": workspace.0,
                "agents": agents,
                "claims": claims,
                "resources": resources
            }))
        })
    }

    pub fn dashboard_snapshot(&self, root: &Path, limit: u32) -> Result<Value> {
        if !(1..=500).contains(&limit) {
            bail!("dashboard limit must be between 1 and 500");
        }
        let status = self.status(None, Some(root))?;
        let Some(workspace_id) = status["workspace_id"].as_str().map(ToOwned::to_owned) else {
            return Ok(json!({
                "agents": [],
                "claims": [],
                "events": [],
                "generated_at": now_ms()?,
                "memories": [],
                "messages": [],
                "resources": [],
                "stats": {
                    "claims": 0,
                    "events": 0,
                    "memories": 0,
                    "messages": 0,
                    "resources": 0,
                    "running_agents": 0,
                    "total_agents": 0
                },
                "workspace": status["workspace"]
            }));
        };
        let activity =
            self.read(|connection| dashboard_activity(connection, &workspace_id, limit))?;
        let agents = status["agents"].as_array().cloned().unwrap_or_default();
        let claims = status["claims"].as_array().cloned().unwrap_or_default();
        let resources = status["resources"].as_array().cloned().unwrap_or_default();
        let resource_count = resources.len();
        let running_agents = agents
            .iter()
            .filter(|agent| agent["status"] == "running")
            .count();
        Ok(json!({
            "agents": agents,
            "claims": claims,
            "events": activity.events,
            "generated_at": now_ms()?,
            "memories": activity.memories,
            "messages": activity.messages,
            "resources": resources,
            "stats": {
                "claims": claims.len(),
                "events": activity.totals.2,
                "memories": activity.totals.0,
                "messages": activity.totals.1,
                "resources": resource_count,
                "running_agents": running_agents,
                "total_agents": agents.len()
            },
            "workspace": status["workspace"],
            "workspace_id": workspace_id
        }))
    }

    pub fn events(&self, agent_id: &str, after: i64, limit: u32) -> Result<Value> {
        self.read(|connection| {
            let agent = Self::agent_row(connection, agent_id)?;
            let mut statement = connection.prepare(
                "SELECT e.sequence, e.agent_id, e.event_type, e.subject,
                        e.data_json, e.created_at, a.name
                 FROM events e LEFT JOIN agents a ON a.id = e.agent_id
                 WHERE e.workspace_id = ? AND e.sequence > ?
                 ORDER BY e.sequence LIMIT ?",
            )?;
            let rows = statement.query_map(params![agent.workspace_id, after, limit], |row| {
                let data: String = row.get(4)?;
                Ok(json!({
                    "sequence": row.get::<_, i64>(0)?,
                    "agent_id": row.get::<_, Option<String>>(1)?,
                    "event_type": row.get::<_, String>(2)?,
                    "subject": row.get::<_, Option<String>>(3)?,
                    "data": serde_json::from_str::<Value>(&data).unwrap_or_else(|_| json!({})),
                    "created_at": row.get::<_, i64>(5)?,
                    "agent_name": row.get::<_, Option<String>>(6)?
                }))
            })?;
            let mut events = Vec::new();
            for row in rows {
                events.push(row?);
            }
            Ok(Value::Array(events))
        })
    }

    pub fn wait_for_events(
        &self,
        agent_id: &str,
        after: i64,
        timeout: Duration,
        poll_interval: Duration,
        limit: u32,
    ) -> Result<Value> {
        if timeout > Duration::from_secs(60) {
            bail!("timeout must be at most 60 seconds");
        }
        if !(Duration::from_millis(10)..=Duration::from_secs(5)).contains(&poll_interval) {
            bail!("poll interval must be between 10 milliseconds and 5 seconds");
        }
        let deadline = Instant::now() + timeout;
        loop {
            let events = self.events(agent_id, after, limit)?;
            if events.as_array().is_some_and(|items| !items.is_empty()) {
                return Ok(events);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(json!([]));
            }
            thread::sleep(poll_interval.min(deadline.saturating_duration_since(now)));
        }
    }

    pub fn collect_stale(&self, stale_after: Duration) -> Result<Value> {
        let cutoff = now_ms()?.saturating_sub(i64::try_from(stale_after.as_millis())?);
        self.write(|transaction| {
            let mut statement = transaction.prepare(
                "SELECT id, pid, workspace_id FROM agents
                 WHERE status = 'running' AND heartbeat_at < ?",
            )?;
            let candidates = statement
                .query_map([cutoff], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            let now = now_ms()?;
            let dead: Vec<_> = candidates
                .into_iter()
                .filter(|(_, pid, _)| !process_is_alive(*pid))
                .collect();
            for (agent_id, _, workspace_id) in &dead {
                transaction.execute(
                    "UPDATE agents SET status = 'dead', stopped_at = ? WHERE id = ?",
                    params![now, agent_id],
                )?;
                transaction.execute("DELETE FROM claims WHERE agent_id = ?", [agent_id])?;
                transaction
                    .execute("DELETE FROM resource_leases WHERE agent_id = ?", [agent_id])?;
                Self::event(
                    transaction,
                    workspace_id,
                    Some(agent_id),
                    "agent.dead",
                    Some(agent_id),
                    &json!({}),
                )?;
            }
            // A footprint outlives its session on purpose: PidMesh preserves worktrees, so a
            // stopped or dead agent's uncommitted work still genuinely contests the path. Once
            // the checkout is gone that contention cannot be real.
            //
            // This sweeps every agent that is no longer running rather than only the ones this
            // pass just marked dead. An agent whose worktree is removed after it died would
            // otherwise never be revisited, because the liveness query above selects only
            // 'running' sessions, and its phantom collision would be permanent.
            let mut orphan_statement = transaction.prepare(
                "SELECT DISTINCT a.id, a.workspace_id, a.checkout_path
                 FROM footprints f JOIN agents a ON a.id = f.agent_id
                 WHERE a.status != 'running'",
            )?;
            let orphans = orphan_statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(orphan_statement);

            let mut abandoned_footprints = 0;
            let mut touched: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (agent_id, workspace_id, checkout) in orphans {
                if checkout.is_none_or(|path| !Path::new(&path).is_dir()) {
                    touched.entry(workspace_id).or_default().push(agent_id);
                }
            }
            for (workspace_id, agent_ids) in touched {
                // Removing a footprint can end a collision, and anything parked on `wait` has to
                // learn that the same way an explicit withdrawal announces it.
                let before = collision_signatures(transaction, &workspace_id)?;
                for agent_id in &agent_ids {
                    abandoned_footprints += transaction
                        .execute("DELETE FROM footprints WHERE agent_id = ?", [agent_id])?;
                }
                let after = collision_signatures(transaction, &workspace_id)?;
                for path in before.keys() {
                    if !after.contains_key(path) {
                        Self::event(
                            transaction,
                            &workspace_id,
                            None,
                            "collision.cleared",
                            Some(path),
                            &json!({"reason": "checkout removed"}),
                        )?;
                    }
                }
            }

            let expired =
                transaction.execute("DELETE FROM claims WHERE lease_expires_at <= ?", [now])?;
            let expired_resources = transaction.execute(
                "DELETE FROM resource_leases WHERE lease_expires_at <= ?",
                [now],
            )?;
            Ok(json!({
                "dead_agents": dead.len(),
                "expired_claims": expired,
                "expired_resources": expired_resources,
                "abandoned_footprints": abandoned_footprints
            }))
        })
    }

    fn initialize(&self) -> Result<()> {
        let mut delay = Duration::from_millis(10);
        let connection = self.lock_connection()?;
        for attempt in 0..8 {
            match connection.execute_batch(SCHEMA) {
                Ok(()) => {
                    add_column_if_missing(&connection, "agents", "checkout_path", "TEXT")?;
                    add_column_if_missing(&connection, "agents", "git_branch", "TEXT")?;
                    add_column_if_missing(&connection, "footprints", "ranges", "TEXT")?;
                    connection.pragma_update(None, "user_version", 4)?;
                    return Ok(());
                }
                Err(error) if is_busy(&error) && attempt < 7 => {
                    thread::sleep(delay);
                    delay *= 2;
                }
                Err(error) => return Err(error.into()),
            }
        }
        bail!("database initialization retries exhausted")
    }

    fn open_connection(path: &Path) -> Result<Connection> {
        let connection =
            Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
        connection.busy_timeout(Duration::from_secs(30))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(connection)
    }

    fn lock_connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| anyhow!("database connection lock is poisoned"))
    }

    fn read<T>(&self, operation: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let connection = self.lock_connection()?;
        operation(&connection)
    }

    fn write<T>(&self, mut operation: impl FnMut(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut delay = Duration::from_millis(10);
        let mut connection = self.lock_connection()?;
        for attempt in 0..8 {
            let transaction =
                match connection.transaction_with_behavior(TransactionBehavior::Immediate) {
                    Ok(transaction) => transaction,
                    Err(error) if is_busy(&error) && attempt < 7 => {
                        thread::sleep(delay);
                        delay *= 2;
                        continue;
                    }
                    Err(error) => return Err(error.into()),
                };
            let result = operation(&transaction)?;
            match transaction.commit() {
                Ok(()) => return Ok(result),
                Err(error) if is_busy(&error) && attempt < 7 => {
                    thread::sleep(delay);
                    delay *= 2;
                }
                Err(error) => return Err(error.into()),
            }
        }
        bail!("database write retries exhausted")
    }

    fn ensure_workspace(transaction: &Transaction<'_>, root: &str) -> Result<String> {
        let workspace_id = Uuid::new_v5(&Uuid::NAMESPACE_URL, format!("pidmesh:{root}").as_bytes())
            .simple()
            .to_string();
        transaction.execute(
            "INSERT OR IGNORE INTO workspaces(id, root, created_at) VALUES (?, ?, ?)",
            params![workspace_id, root, now_ms()?],
        )?;
        Ok(workspace_id)
    }

    fn agent_row(connection: &Connection, agent_id: &str) -> Result<AgentRow> {
        connection
            .query_row(
                "SELECT workspace_id, started_at FROM agents WHERE id = ?",
                [agent_id],
                |row| {
                    Ok(AgentRow {
                        workspace_id: row.get(0)?,
                        started_at: row.get(1)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| anyhow!("agent not found: {agent_id}"))
    }

    fn event(
        transaction: &Transaction<'_>,
        workspace_id: &str,
        agent_id: Option<&str>,
        event_type: &str,
        subject: Option<&str>,
        data: &Value,
    ) -> Result<()> {
        transaction.execute(
            "INSERT INTO events(
                workspace_id, agent_id, event_type, subject, data_json, created_at
             ) VALUES (?, ?, ?, ?, ?, ?)",
            params![
                workspace_id,
                agent_id,
                event_type,
                subject,
                serde_json::to_string(data)?,
                now_ms()?
            ],
        )?;
        Ok(())
    }

    #[cfg(unix)]
    fn restrict_permissions(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    #[cfg(not(unix))]
    fn restrict_permissions(&self) -> Result<()> {
        Ok(())
    }
}

pub fn default_database_path() -> Result<PathBuf> {
    if let Some(configured) = env::var_os("PIDMESH_DB") {
        return Ok(PathBuf::from(configured));
    }
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".pidmesh").join("pidmesh.db"))
}

pub fn workspace_root(root: Option<&Path>) -> Result<String> {
    Ok(workspace_context(root)?.root)
}

fn workspace_context(root: Option<&Path>) -> Result<WorkspaceContext> {
    let configured = root
        .map(Path::to_path_buf)
        .or_else(|| env::var_os("PIDMESH_WORKSPACE").map(PathBuf::from));
    let explicit = configured.is_some();
    let selected = configured.unwrap_or(env::current_dir()?);
    let absolute = if selected.is_absolute() {
        selected
    } else {
        env::current_dir()?.join(selected)
    };
    let normalized = absolute.canonicalize().unwrap_or(absolute);
    let git = git_context(&normalized);
    let checkout = git
        .as_ref()
        .map_or_else(|| normalized.clone(), |context| context.checkout.clone());
    let root = if explicit {
        normalized
    } else {
        git.as_ref()
            .map_or_else(|| normalized.clone(), |context| context.primary.clone())
    };
    Ok(WorkspaceContext {
        branch: git.and_then(|context| context.branch),
        checkout: checkout.to_string_lossy().into_owned(),
        root: root.to_string_lossy().into_owned(),
    })
}

struct GitContext {
    branch: Option<String>,
    checkout: PathBuf,
    primary: PathBuf,
}

fn git_context(directory: &Path) -> Option<GitContext> {
    let output = Command::new("git")
        .arg("-C")
        .arg(directory)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
            "--abbrev-ref",
            "HEAD",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let mut lines = text.lines();
    let checkout = PathBuf::from(lines.next()?).canonicalize().ok()?;
    let common = PathBuf::from(lines.next()?).canonicalize().ok()?;
    let branch = lines
        .next()
        .filter(|branch| *branch != "HEAD" && !branch.is_empty())
        .map(ToOwned::to_owned);
    let primary = if common.file_name().is_some_and(|name| name == ".git") {
        common.parent()?.to_path_buf()
    } else {
        checkout.clone()
    };
    Some(GitContext {
        branch,
        checkout,
        primary,
    })
}

/// Diagnose whether this workspace is actually one shared mesh.
///
/// An explicit `--workspace` or `PIDMESH_WORKSPACE` bypasses linked-worktree discovery, so every
/// checkout becomes its own mesh. Coordination then silently does nothing while every command
/// still succeeds, which is the worst failure mode this system has.
pub fn diagnose(database: &Path, root: Option<&Path>) -> Result<Value> {
    let store = MeshStore::new(database)?;
    let here = git_context(&env::current_dir()?);
    let resolved = workspace_root(root)?;
    let primary = here.as_ref().map(|context| context.primary.clone());

    let registered = store.read(|connection| {
        let mut statement = connection.prepare(
            "SELECT w.root, COUNT(a.id)
             FROM workspaces w LEFT JOIN agents a
               ON a.workspace_id = w.id AND a.status = 'running'
             GROUP BY w.root",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?;

    // Any two workspace roots that resolve to one repository are a split mesh.
    let mut siblings = Vec::new();
    if let Some(primary) = primary.as_ref() {
        for (root, live) in &registered {
            let path = Path::new(root);
            let shares_repository = git_context(path)
                .is_some_and(|context| context.primary == *primary)
                || path == primary;
            if shares_repository {
                siblings.push(json!({"workspace": root, "live_agents": live}));
            }
        }
    }

    let mut warnings = Vec::new();
    if siblings.len() > 1 {
        warnings.push(json!({
            "code": "split_mesh",
            "detail": format!(
                "{} workspace roots belong to one repository, so their agents cannot see each \
                 other. This happens when --workspace or PIDMESH_WORKSPACE is set explicitly \
                 inside a linked worktree; omit it and let discovery resolve the primary checkout.",
                siblings.len()
            ),
            "roots": siblings
        }));
    }
    if here.is_none() {
        warnings.push(json!({
            "code": "not_a_repository",
            "detail": "This directory is not inside a git repository, so linked-worktree \
                       discovery cannot group checkouts."
        }));
    }

    Ok(json!({
        "database": database,
        "resolved_workspace": resolved,
        "checkout": here.as_ref().map(|context| context.checkout.to_string_lossy()),
        "primary_repository": primary.as_ref().map(|path| path.to_string_lossy()),
        "branch": here.as_ref().and_then(|context| context.branch.clone()),
        "linked_worktree": here
            .as_ref()
            .is_some_and(|context| context.checkout != context.primary),
        "healthy": warnings.is_empty(),
        "warnings": warnings
    }))
}

#[cfg(unix)]
#[must_use]
pub fn process_is_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    i32::try_from(pid).is_ok_and(|pid| match kill(Pid::from_raw(pid), None) {
        Ok(()) | Err(Errno::EPERM) => true,
        Err(_) => false,
    })
}

#[cfg(not(unix))]
#[must_use]
pub const fn process_is_alive(pid: u32) -> bool {
    pid > 0
}

#[allow(clippy::unnecessary_wraps)]
fn parent_pid() -> Option<i64> {
    #[cfg(unix)]
    {
        Some(i64::from(nix::unistd::getppid().as_raw()))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

fn now_ms() -> Result<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?;
    i64::try_from(duration.as_millis()).context("timestamp exceeds i64")
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

impl ResourceKey {
    fn display(&self) -> String {
        format!("{}:{}", self.kind, self.key)
    }
}

fn normalize_resources(resources: &[String]) -> Result<Vec<ResourceKey>> {
    if resources.is_empty() || resources.len() > 64 {
        bail!("resources must contain between 1 and 64 entries");
    }
    let mut normalized = Vec::new();
    let mut seen = HashSet::new();
    for resource in resources {
        let (kind, key) = resource
            .split_once(':')
            .ok_or_else(|| anyhow!("resource must use kind:key syntax: {resource}"))?;
        if kind.is_empty()
            || kind.len() > 32
            || !kind.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
            })
        {
            bail!("resource kind must be lowercase alphanumeric, '-' or '_': {kind}");
        }
        let key = if kind == "path" {
            normalize_resource_path(key)?
        } else {
            let key = key.trim();
            if key.is_empty() || key.len() > 512 || key.chars().any(char::is_control) {
                bail!("resource key must contain between 1 and 512 printable bytes");
            }
            key.to_owned()
        };
        let resource = ResourceKey {
            kind: kind.to_owned(),
            key,
        };
        if seen.insert(resource.clone()) {
            normalized.push(resource);
        }
    }
    Ok(normalized)
}

fn normalize_resource_path(value: &str) -> Result<String> {
    if value.is_empty() || value.len() > 512 {
        bail!("path resource must contain between 1 and 512 bytes");
    }
    let mut parts = Vec::new();
    for component in Path::new(value).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| anyhow!("path resource must be valid UTF-8"))?;
                if part.chars().any(char::is_control) {
                    bail!("path resource must not contain control characters");
                }
                parts.push(part);
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                bail!("path resource must be relative and must not contain '..'");
            }
        }
    }
    if parts.is_empty() {
        bail!("path resource must identify a workspace-relative path");
    }
    Ok(parts.join("/"))
}

fn workspace_resources(connection: &Connection, workspace_id: &str, now: i64) -> Result<Value> {
    let mut statement = connection.prepare(
        "SELECT r.resource_kind, r.resource_key, r.agent_id, a.name, a.pid,
                r.task_key, r.detail, r.lease_expires_at
         FROM resource_leases r JOIN agents a ON a.id = r.agent_id
         WHERE r.workspace_id = ? AND r.lease_expires_at > ?
         ORDER BY r.resource_kind, r.resource_key",
    )?;
    let rows = statement.query_map(params![workspace_id, now], resource_lease_json)?;
    Ok(Value::Array(rows.collect::<rusqlite::Result<Vec<_>>>()?))
}

fn resource_conflicts(
    transaction: &Transaction<'_>,
    workspace_id: &str,
    agent_id: &str,
    resource: &ResourceKey,
    now: i64,
) -> Result<Vec<Value>> {
    let sql = if resource.kind == "path" {
        "SELECT r.resource_kind, r.resource_key, r.agent_id, a.name, a.pid,
                r.task_key, r.detail, r.lease_expires_at
         FROM resource_leases r JOIN agents a ON a.id = r.agent_id
         WHERE r.workspace_id = ? AND r.agent_id != ?
           AND r.lease_expires_at > ? AND r.resource_kind = ?
           AND (r.resource_key = ?
                OR instr(r.resource_key, ? || '/') = 1
                OR instr(?, r.resource_key || '/') = 1)"
    } else {
        "SELECT r.resource_kind, r.resource_key, r.agent_id, a.name, a.pid,
                r.task_key, r.detail, r.lease_expires_at
         FROM resource_leases r JOIN agents a ON a.id = r.agent_id
         WHERE r.workspace_id = ? AND r.agent_id != ?
           AND r.lease_expires_at > ? AND r.resource_kind = ?
           AND r.resource_key = ?"
    };
    let mut statement = transaction.prepare(sql)?;
    let rows = if resource.kind == "path" {
        statement.query_map(
            params![
                workspace_id,
                agent_id,
                now,
                resource.kind,
                resource.key,
                resource.key,
                resource.key
            ],
            resource_lease_json,
        )?
    } else {
        statement.query_map(
            params![workspace_id, agent_id, now, resource.kind, resource.key],
            resource_lease_json,
        )?
    };
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn resource_lease_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "resource_kind": row.get::<_, String>(0)?,
        "resource_key": row.get::<_, String>(1)?,
        "agent_id": row.get::<_, String>(2)?,
        "agent_name": row.get::<_, String>(3)?,
        "agent_pid": row.get::<_, u32>(4)?,
        "task_key": row.get::<_, Option<String>>(5)?,
        "detail": row.get::<_, Option<String>>(6)?,
        "lease_expires_at": row.get::<_, i64>(7)?
    }))
}

fn add_column_if_missing(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<()> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|existing| existing == column);
    drop(statement);
    if !exists {
        connection.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {column} {definition}"
        ))?;
    }
    Ok(())
}

/// A live agent session and the checkout it is working in.
/// Task key reserved for the workspace-wide integration lease.
///
/// Merges are serialized through the ordinary claim table rather than a new mechanism: it already
/// guarantees exactly one owner until expiry, survives a crashed holder, and is released with the
/// rest of an agent's state.
pub const INTEGRATION_TASK_KEY: &str = "pidmesh:integration";

#[derive(Clone, Debug)]
pub struct AgentCheckout {
    pub agent_id: String,
    pub agent_name: String,
    pub checkout_path: PathBuf,
}

/// Every path in the workspace changed in more than one checkout.
///
/// Contention is keyed on checkout rather than agent: a worker that runs both a CLI session and an
/// MCP session in one worktree is one editor of that path, not two.
fn workspace_collisions(
    connection: &Connection,
    workspace_id: &str,
) -> Result<Vec<(String, Vec<Participant>)>> {
    // One checkout gets one opinion per path. Several sessions can occupy a worktree and a
    // watcher publishes the same scan for each of them, so without collapsing them first a stale
    // footprint from a co-located session would argue with its own checkout and fabricate a
    // divergent collision. Deduplicating here rather than only in the contested-path test also
    // keeps the participant list and the contention test derived from the same rows.
    let mut statement = connection.prepare(
        "WITH ranked AS (
             SELECT f.path AS path, f.agent_id AS agent_id, a.name AS agent_name,
                    a.status AS status, f.change_kind AS change_kind, f.digest AS digest,
                    f.base_commit AS base_commit, f.branch AS branch, f.ranges AS ranges,
                    ROW_NUMBER() OVER (
                        PARTITION BY f.path, COALESCE(a.checkout_path, f.agent_id)
                        ORDER BY f.observed_at DESC, f.agent_id
                    ) AS position
             FROM footprints f
             JOIN agents a ON a.id = f.agent_id
             WHERE f.workspace_id = ?1
         )
         SELECT path, agent_id, agent_name, status, change_kind, digest, base_commit, branch,
                ranges
         FROM ranked
         WHERE position = 1
           AND path IN (
               SELECT path FROM ranked WHERE position = 1
               GROUP BY path HAVING COUNT(*) > 1
           )
         ORDER BY path, agent_id",
    )?;
    let rows = statement.query_map(params![workspace_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            Participant {
                agent_id: row.get(1)?,
                agent_name: row.get(2)?,
                status: row.get(3)?,
                change_kind: row.get(4)?,
                digest: row.get(5)?,
                base_commit: row.get(6)?,
                branch: row.get(7)?,
                ranges: row.get(8)?,
            },
        ))
    })?;
    let mut grouped: Vec<(String, Vec<Participant>)> = Vec::new();
    for row in rows {
        let (path, participant) = row?;
        match grouped.last_mut() {
            Some((last, participants)) if *last == path => participants.push(participant),
            _ => grouped.push((path, vec![participant])),
        }
    }
    Ok(grouped)
}

/// A stable fingerprint of one collision, used to emit events only on real transitions.
fn signatures_of(contested: &[(String, Vec<Participant>)]) -> BTreeMap<String, String> {
    contested
        .iter()
        .map(|(path, participants)| {
            let mut agents: Vec<&str> = participants
                .iter()
                .map(|participant| participant.agent_id.as_str())
                .collect();
            agents.sort_unstable();
            (
                path.clone(),
                format!("{}|{}", classify(participants), agents.join(",")),
            )
        })
        .collect()
}

fn collision_signatures(
    connection: &Connection,
    workspace_id: &str,
) -> Result<BTreeMap<String, String>> {
    Ok(signatures_of(&workspace_collisions(
        connection,
        workspace_id,
    )?))
}

fn collision_report(contested: &[(String, Vec<Participant>)], viewer: &str) -> Value {
    let entries: Vec<Value> = contested
        .iter()
        .map(|(path, participants)| {
            json!({
                "path": path,
                "severity": classify(participants),
                "base_divergent": base_divergent(participants),
                "involves_me": participants
                    .iter()
                    .any(|participant| participant.agent_id == viewer),
                "participants": participants
                    .iter()
                    .map(|participant| json!({
                        "agent_id": participant.agent_id,
                        "agent_name": participant.agent_name,
                        "status": participant.status,
                        "change_kind": participant.change_kind,
                        "digest": participant.digest,
                        "base_commit": participant.base_commit,
                        "branch": participant.branch,
                        "ranges": participant.ranges
                    }))
                    .collect::<Vec<Value>>()
            })
        })
        .collect();
    Value::Array(entries)
}

fn dashboard_activity(
    connection: &Connection,
    workspace_id: &str,
    limit: u32,
) -> Result<DashboardActivity> {
    let mut events_statement = connection.prepare(
        "SELECT e.sequence, e.agent_id, e.event_type, e.subject,
                e.data_json, e.created_at, a.name
         FROM events e LEFT JOIN agents a ON a.id = e.agent_id
         WHERE e.workspace_id = ? ORDER BY e.sequence DESC LIMIT ?",
    )?;
    let event_rows = events_statement.query_map(params![workspace_id, limit], |row| {
        let data: String = row.get(4)?;
        Ok(json!({
            "sequence": row.get::<_, i64>(0)?,
            "agent_id": row.get::<_, Option<String>>(1)?,
            "event_type": row.get::<_, String>(2)?,
            "subject": row.get::<_, Option<String>>(3)?,
            "data": serde_json::from_str::<Value>(&data).unwrap_or_else(|_| json!({})),
            "created_at": row.get::<_, i64>(5)?,
            "agent_name": row.get::<_, Option<String>>(6)?
        }))
    })?;
    let events = event_rows.collect::<rusqlite::Result<Vec<_>>>()?;

    let mut memories_statement = connection.prepare(
        "SELECT m.id, m.agent_id, m.kind, m.key, m.content, m.importance,
                m.created_at, a.name
         FROM memories m LEFT JOIN agents a ON a.id = m.agent_id
         WHERE m.workspace_id = ? ORDER BY m.created_at DESC, m.id DESC LIMIT ?",
    )?;
    let memory_rows = memories_statement.query_map(params![workspace_id, limit], memory_json)?;
    let memories = memory_rows.collect::<rusqlite::Result<Vec<_>>>()?;

    let mut messages_statement = connection.prepare(
        "SELECT m.id, m.body, m.correlation_id, m.created_at,
                sender.id, sender.name, recipient.id, recipient.name
         FROM messages m
         JOIN agents sender ON sender.id = m.sender_id
         LEFT JOIN agents recipient ON recipient.id = m.recipient_id
         WHERE m.workspace_id = ? ORDER BY m.created_at DESC, m.id DESC LIMIT ?",
    )?;
    let message_rows = messages_statement.query_map(params![workspace_id, limit], |row| {
        Ok(json!({
            "id": row.get::<_, i64>(0)?,
            "body": row.get::<_, String>(1)?,
            "correlation_id": row.get::<_, Option<String>>(2)?,
            "created_at": row.get::<_, i64>(3)?,
            "sender_id": row.get::<_, String>(4)?,
            "sender_name": row.get::<_, String>(5)?,
            "recipient_id": row.get::<_, Option<String>>(6)?,
            "recipient_name": row.get::<_, Option<String>>(7)?
        }))
    })?;
    let messages = message_rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let totals = connection.query_row(
        "SELECT
            (SELECT COUNT(*) FROM memories WHERE workspace_id = ?),
            (SELECT COUNT(*) FROM messages WHERE workspace_id = ?),
            (SELECT COUNT(*) FROM events WHERE workspace_id = ?)",
        params![workspace_id, workspace_id, workspace_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(DashboardActivity {
        events,
        memories,
        messages,
        totals,
    })
}

fn memory_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "agent_id": row.get::<_, Option<String>>(1)?,
        "kind": row.get::<_, String>(2)?,
        "key": row.get::<_, Option<String>>(3)?,
        "content": row.get::<_, String>(4)?,
        "importance": row.get::<_, f64>(5)?,
        "created_at": row.get::<_, i64>(6)?,
        "agent_name": row.get::<_, Option<String>>(7)?
    }))
}

#[cfg(unix)]
fn restrict_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_directory(_: &Path) -> Result<()> {
    Ok(())
}
