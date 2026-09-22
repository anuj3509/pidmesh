# PidMesh

Fast, local process-aware memory and coordination for concurrent AI agents—implemented in Rust.

[Website](https://pidmesh.patelapurva.com/) · [Protocol](docs/protocol.md) · [Releases](https://github.com/Apurva3509/pidmesh/releases)

Run Codex, Claude Code, Cursor, local models, and custom workers in separate terminals without
making them work blind. Every process gets a workspace-scoped identity, shared durable memory, an
inbox, a wakeable event stream, atomic task leases, and hierarchical resource reservations through
one private SQLite database.

No daemon. No Python runtime. No cloud account. No API key.

## Why this exists

Long-term memory systems retrieve old context. Agent message buses move text. Neither primitive
answers the operating-system questions that matter when many local agents work simultaneously:

- Which sessions and PIDs are alive?
- Which process owns a task right now?
- Which agent owns a path, port, or local service before another process collides with it?
- Can another worker safely take over after a crash?
- Did two agents start the same edit?
- What decisions and handoffs happened in order?

PidMesh is the coordination kernel for those questions. Each process keeps one native SQLite
connection, while WAL and short `BEGIN IMMEDIATE` transactions coordinate safely across processes.

## Performance

Measured on Apple Silicon with the same SQLite schema and workload:

| Operation | Python v0.2 | Rust v1.0 | Improvement |
| --- | ---: | ---: | ---: |
| CLI startup, median of 100 | 34.105 ms | 7.153 ms | 4.8× faster |
| Durable memory writes | 1,333/sec | 15,596/sec | 11.7× faster |
| FTS5 recalls over 2,000 memories | 607/sec | 934/sec | 1.5× faster |

Throughput values are medians of five runs with 2,000 committed writes and 500 ranked FTS5 recalls:

```bash
cargo run --release --example benchmark
```

## Install

From source:

```bash
cargo install --git https://github.com/Apurva3509/pidmesh --locked
```

Release archives contain both `pidmesh` and `pidmesh-mcp`:

```bash
gh release download --repo Apurva3509/pidmesh --pattern 'pidmesh-*'
```

## Open the local agent IDE

Launch a private browser dashboard for the current workspace:

```bash
pidmesh dashboard
```

PidMesh prints a one-time local URL containing the session token and binds only to `127.0.0.1`.
The Workspace view separates concurrent workstreams inside the current repository without guessing
project names from its contents. Create a bounded task, name its workstream, select an installed
Codex or Claude Code profile, reserve its allowed paths, and launch it in a generated branch and
isolated git worktree. Runs stay grouped by workstream in the sidebar; opening one brings its real
PTY, file tree, change review, and task brief into the main pane. The Operations view retains live
PIDs, claims, path/port/service ownership, shared memory, messages, and the event ledger. Use
`pidmesh dashboard --port 0` to select an available port automatically; `pidmesh ui` is an alias.

Launch profiles are resolved and allowlisted by the Rust server. The HTTP API cannot supply an
arbitrary command, executable, branch, worktree path, base ref, or environment. A run is limited to
the generated worktree, but path scope is enforced at the review gate rather than by an operating
system sandbox: PidMesh refuses approval while changes exist outside the reserved paths.

The primary workflow is:

1. Click **New task**, name the workstream, and choose Codex or Claude Code.
2. Describe the outcome and enter one allowed relative path per line.
3. Track concurrent workstreams in the workspace sidebar and open a run.
4. Work with the agent through the embedded terminal and inspect Files and Diff.
5. Stop the agent, resolve any scope violations, then approve and merge.

Managed terminal scrollback and run handles are currently retained in memory for the lifetime of the
dashboard. Worktrees are deliberately preserved after stop or failure so uncommitted agent work is
never deleted automatically.

## Five-minute demo

Register two live processes in the same repository:

```bash
pidmesh join --name planner --provider codex --pid $$
pidmesh join --name implementer --provider claude --pid $$
```

Set the returned IDs in their respective shells, then coordinate:

```bash
export PIDMESH_AGENT_ID=planner-1234-abcd1234
pidmesh remember "Use SQLite WAL; no daemon" --kind decision --key architecture
pidmesh send "Implement the claim transaction" --to implementer

export PIDMESH_AGENT_ID=implementer-5678-efgh5678
pidmesh inbox --ack
pidmesh claim store.claim --lease-seconds 900
pidmesh reserve path:src/store.rs port:4399 --task store.claim --lease-seconds 900
pidmesh recall "architecture SQLite"
```

Inspect or wait on the mesh from another terminal:

```bash
pidmesh status
pidmesh resources
pidmesh unreserve path:src/store.rs port:4399
pidmesh events --agent "$PIDMESH_AGENT_ID"
pidmesh wait --agent "$PIDMESH_AGENT_ID" --after 42 --timeout-seconds 30
pidmesh gc
```

Every command emits JSON for reliable agent consumption.

## Run a native agent swarm

Launch five independently addressable agent processes with one supervisor:

```bash
pidmesh swarm --workers 5 --name-prefix researcher --provider codex -- \
  codex exec "Claim one open task, complete it, and send a handoff"
```

Each child receives `PIDMESH_AGENT_ID`, `PIDMESH_AGENT_INDEX`, `PIDMESH_AGENT_NAME`,
`PIDMESH_SWARM_ID`, `PIDMESH_SWARM_SIZE`, `PIDMESH_DB`, and `PIDMESH_WORKSPACE`. Workers can use
the CLI or MCP server against the same mesh while remaining separate operating-system processes.
The supervisor heartbeats every live worker, observes exits independently, and marks sessions stopped
when they finish. `--fail-fast` terminates the remaining workers after the first failure. Ctrl-C,
SIGTERM, and SIGHUP request a graceful shutdown before forcing stragglers to exit.

Linked git worktrees automatically join the same logical project mesh. PidMesh still records each
agent's actual checkout path and branch, so a fleet launched by Superset, Intent, Orca, or a manual
`git worktree` workflow can coordinate through one kernel without losing branch-level visibility.

## MCP setup

The native MCP server uses the official Rust SDK and exposes eleven tools: status, remember, recall,
send, inbox, claim, release, resource reservation/release, event stream, and bounded event waiting.

Claude Code:

```bash
claude mcp add --scope user pidmesh -- pidmesh-mcp
```

Codex:

```toml
[mcp_servers.pidmesh]
command = "pidmesh-mcp"
env = { PIDMESH_AGENT_NAME = "codex", PIDMESH_PROVIDER = "codex" }
```

Each MCP process registers its real PID, pulses its heartbeat every five seconds, and releases its
claims and resource reservations during a clean shutdown. Linked git worktrees are detected
automatically. Set `PIDMESH_WORKSPACE` to override that project identity when the host does not start
the server from the project directory. `PIDMESH_DB` overrides the default
`~/.pidmesh/pidmesh.db`.

## Concurrency guarantees

- A persistent connection removes per-operation setup overhead within each process.
- WAL allows readers while other processes write.
- Bounded retries and `BEGIN IMMEDIATE` serialize competing mutations.
- A task claim has exactly one owner until expiry or explicit release.
- Multi-resource reservations are all-or-nothing.
- `path:src` conflicts with `path:src/store.rs`, while ports and services use exact ownership.
- Stopped and dead sessions release their claims and resources.
- Broadcast acknowledgements are independent for every agent.
- Linked git worktrees share one logical project while unrelated workspaces remain isolated.
- Bounded waits wake agents without a tight polling loop.

The test suite launches eight separate processes to verify write integrity and prove that task and
overlapping-path contention each have exactly one winner. It also tests linked worktree discovery,
schema upgrades, dashboard security, and a full MCP stdio handshake with native tool calls.

## Architecture

```text
Codex PID 4101 ─┐             one connection / process
Claude PID 4102 ├── CLI/MCP ───────────┐
Worker PID 4103 ┘                      ├── SQLite WAL
Browser IDE ── localhost/token ────────┤
            ├── PTY + attach ticket ───┤
            └── worktree/diff/review ──┘
                                      └── memory + inbox + claims + resources + events

pidmesh swarm ──┬── Worker PID 5101
                ├── Worker PID 5102
                └── Worker PID 5103
```

The Rust runtime reads databases created by the earlier Python releases without a migration. See
[docs/protocol.md](docs/protocol.md) for the storage and lifecycle contract.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --locked
cargo build --release --bins --locked
```

## License

MIT
