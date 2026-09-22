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

## Detect collisions nobody declared

Resource reservations describe what an agent *intends* to touch. They only work when every agent
remembers to call `reserve` before editing, and they cannot see the scope an agent discovered
halfway through its task. The convergence guard closes that gap by observing what each checkout
**actually** changed:

```bash
pidmesh sync          # observe this worktree and publish its footprint
pidmesh collisions    # every path contested by more than one agent
```

`sync` is pure observation. It never writes to the checkout, needs no cooperation beyond being
pointed at a directory, and counts both committed and uncommitted work, including untracked files.
A footprint is authoritative per agent, so an agent that merges and cleans its worktree drops out of
every collision on its next scan.

Each contested path is classified by what would actually happen on merge:

| Severity | Meaning |
| --- | --- |
| `identical` | Every checkout reached the same outcome — byte-identical content, or all of them deleting the path: duplicated effort, safe to merge. |
| `adjacent` | The same path, but each checkout rewrote separable regions of it. Git three-way merges these cleanly, so they do not block. |
| `divergent` | The same path, with edits to overlapping regions. This is the case that silently overwrites work. |
| `delete_edit` | One agent removed a path another is still editing. Git merges this without complaint in several common orderings. |

Every collision also reports `base_divergent`. Two agents can edit different files and still break
each other when one cuts its worktree from a base the other has already moved past, which is how a
clean merge still produces broken behaviour.

### Catch the conflict that shares no file

Two agents can edit entirely different files and still break each other: one withdraws an exported
name, the other writes code that calls it. Git merges both without complaint and the result does
not build. No amount of path comparison can see this.

A scan also records the exported names a checkout **withdrew** — declared on a removed line and
never added back — and the identifiers its changed files reference. A withdrawn name that another
live checkout still uses is reported as a symbol break and blocks the merge of whoever is removing
it:

```bash
pidmesh collisions   # includes symbol_breaks alongside contested paths
pidmesh mergeable    # blocks on removed_export_in_use
```

Export detection is a deliberately narrow heuristic covering the common declaration forms of Rust,
TypeScript, JavaScript, Python and Go. It returns nothing when unsure, because a missed warning is
cheaper than a false alarm, and a name that is deleted and re-added — an edited signature — is not
a withdrawal.

### Gate the merge, not just the edit

Collision reporting says a conflict exists. Merge ordering says what to do about it:

```bash
pidmesh mergeable           # can this checkout merge without breaking anyone?
pidmesh integrate           # take the workspace-wide lease, then merge
pidmesh integrate --release
```

`mergeable` exits non-zero and names every blocker:

| Blocker | Meaning |
| --- | --- |
| `stale_base` | The integration branch advanced since this worktree was cut. The diff may still apply cleanly and be wrong, because it was written against code that no longer exists. Rebase. |
| `contested_path` | Another **live** checkout rewrote an overlapping region of a path this one changed. Judged per pair, so the blocker names exactly which peers conflict. |
| `integration_held` | Another agent holds the integration lease and is merging right now. |
| `removed_export_in_use` | This checkout withdraws an exported name another live checkout still references. |

Duplicated work never blocks: if two checkouts hold byte-identical content, merging either is safe.
Neither do `adjacent` edits, which is what keeps a shared router or module index from blocking the
whole fleet.
Neither does a stopped peer — its preserved worktree is still reported as a collision, but it
cannot be asked to rebase and must not deadlock the queue behind it.

The integration lease is an ordinary task claim under a reserved key, so it already has exactly one
owner until expiry, survives a crashed holder, and is released with the rest of an agent's state.

### Check the mesh is actually shared

One repository must resolve to one mesh or nothing coordinates. An explicit `--workspace` or
`PIDMESH_WORKSPACE` inside a linked worktree bypasses discovery and silently gives every checkout
its own mesh, while every command still reports success:

```bash
pidmesh doctor
```

It names the resolved workspace, the primary repository behind the current checkout, and exits
non-zero when two workspace roots belong to one repository, listing the orphaned roots.

### Observe a fleet that never calls sync

`sync` still has to be invoked by somebody. A watcher removes even that requirement: registration
already recorded every checkout, so one supervisor can observe the whole fleet without any agent
cooperating.

```bash
pidmesh watch                      # sweep every live checkout every 5 seconds
pidmesh watch --once               # a single pass, for a hook or CI step
pidmesh watch --interval-seconds 15
```

Each sweep scans every distinct live checkout once, publishes a footprint for every session in it,
and reports the resulting collisions. A checkout that has been removed is skipped rather than
failing the sweep, and a session whose PID is gone is not scanned at all.

`pidmesh unsync` withdraws a footprint explicitly, for an agent that is shutting down and can no
longer produce a scan. Garbage collection keeps a dead agent's footprint while its checkout still
exists, because PidMesh preserves worktrees and that uncommitted work genuinely still contests the
path, and removes it once the checkout is gone.

Detection is a mesh event, not a return value. A new or changed collision appends
`collision.detected`, and withdrawing from a contested path appends `collision.cleared`, so peers
already parked on `pidmesh wait` wake the moment an overlap appears:

```bash
pidmesh wait --agent "$PIDMESH_AGENT_ID" --after 0 --timeout-seconds 30
```

Events fire only on transitions. A fleet that re-scans on a loop produces no event churn while
nothing changes.

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

The native MCP server uses the official Rust SDK and exposes seventeen tools: status, remember, recall,
send, inbox, claim, release, resource reservation/release, event stream, bounded event waiting,
footprint sync/release, collision reporting, merge readiness, and integration lease acquire/release.

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
- Observed footprints detect overlapping edits that no agent reserved.
- Collision events fire on transitions only, so steady-state re-scanning is free.
- A watcher can observe every checkout without any agent participating.
- Edits to separable regions of one file are distinguished from edits that overwrite each other.
- Withdrawing an export a live checkout still calls is caught even when no file is shared.
- A stale base blocks a merge even when the diff would apply cleanly.
- The integration lease admits one merge at a time and has exactly one owner until expiry.

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
