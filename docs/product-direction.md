# PidMesh product direction

## The honest category view

PidMesh is not the first multi-agent workspace. The category already includes:

- [Shire](https://www.agents-shire.sh/), which provides persistent agent teams, shared files, direct messaging, reusable skills, schedules, and sleep/resume.
- [Intent](https://www.augmentcode.com/blog/intent-a-workspace-for-agent-orchestration), which combines isolated git worktrees, a living specification, coordinator/implementor/verifier roles, diffs, browsers, and git workflow.
- [Superset](https://docs.superset.sh/overview), which provides local-first worktree isolation, terminals, diffs, ports, pull requests, automations, and agent orchestration.
- [Orca](https://www.onorca.dev/), which combines worktrees, agent terminals, a browser, editor, tasks, notes, diff review, and mobile supervision.

Shared memory, chat, a task board, or parallel terminals are therefore not sufficient differentiation.

## The problem PidMesh owns

When five or more independent agent processes run locally, the missing layer is a shared coordination kernel underneath their chosen terminals and workspaces:

- Which operating-system processes are actually alive?
- Which logical project does each checkout and git worktree belong to?
- Which agent owns a task, path, port, or local service right now?
- Which ownership can be recovered safely after a crash?
- Which agent attempted conflicting work, and what context led to the handoff?
- Can Codex, Claude Code, Cursor, local models, and custom workers coordinate without adopting one vendor's agent runtime?

PidMesh answers these questions through a local Rust binary, a private SQLite WAL database, and a vendor-neutral CLI/MCP protocol.

## Positioning

PidMesh is the local, vendor-neutral IDE and coordination kernel for agent fleets.

Its workspace now owns the thin end-to-end operator loop: create a scoped task, launch an allowlisted provider in an isolated worktree, attach to its terminal, inspect files and diffs, enforce scope at review, and approve or reject the merge. The Rust kernel remains usable independently through CLI and MCP, so agents launched by Superset, Intent, Orca, terminals, or custom harnesses can still join the same mesh.

## Product principles

1. **Kernel beneath the canvas.** Every visual control must map to a real process, worktree, lease, or review invariant.
2. **Any harness.** The protocol must work from CLI and MCP without requiring one model provider or terminal app.
3. **One project across worktrees.** Linked git worktrees share one mesh automatically while retaining their own checkout and branch identity.
4. **Cooperative collision prevention.** Agents reserve hierarchical paths and exact resources before acting; reservations are atomic and tied to live PIDs.
5. **Local by default.** Operational data and controls remain on loopback and never require a cloud account.
6. **Observable decisions.** Ownership, contention, messages, and memory changes produce an ordered event trail.
7. **One operator workspace.** Launch, terminal, files, diffs, attention, approvals, and fleet state belong in one interface.

## Version 1.3: Worktree-aware collision guard

The first differentiated release adds:

- automatic logical-project discovery across linked git worktrees;
- per-agent checkout and branch visibility;
- atomic resource reservations for paths, ports, and local services;
- hierarchical path conflicts such as `path:src` versus `path:src/store.rs`;
- all-or-nothing acquisition for multi-resource work;
- lease expiry and automatic cleanup when the owning PID stops or dies;
- collision and expiring-lease visibility in the local cockpit;
- CLI and MCP operations so existing coding agents can use the guard before editing.

Resource reservations are cooperative coordination, not an operating-system sandbox. Agents must call the protocol before modifying a resource.

## Version 1.4: Local agent IDE

The IDE release adds:

- generated branches and isolated worktrees for managed runs;
- server-side allowlisted Codex and Claude Code launch profiles;
- real pseudo-terminals with bounded reconnectable scrollback and one-use WebSocket attach tickets;
- a task-oriented workspace with run navigation, attention states, Files, Diff, Terminal, and Inspector panes;
- explicit user-named workstreams that group local runs in the workspace sidebar;
- canonicalized read-only file browsing contained inside the selected checkout;
- review-time detection of changes outside the reserved path scope;
- commit and no-fast-forward merge approval gated on process state, scope validity, primary-checkout cleanliness, and the expected base branch;
- preservation of dirty or failed worktrees instead of automatic destructive cleanup;
- an Operations view for the original fleet, ownership, memory, message, and event surfaces.

The dashboard accepts an explicit workstream, task, prompt, provider ID, and relative scopes. It never infers a workstream from repository vocabulary and never accepts arbitrary commands, executable paths, environment variables, worktree locations, branch names, or base refs. Terminal output is bounded to 1 MiB per run, input frames are bounded, and the dashboard allows at most eight simultaneous managed sessions.

Path scope is currently a merge-gate invariant, not a filesystem sandbox. Agents work inside an isolated git checkout, but a tool capable of accessing absolute paths can still reach the wider host. PidMesh identifies and blocks out-of-scope repository changes before approval; stronger operating-system containment remains future work.

## Workspace information architecture

The local workspace answers five questions in order:

1. **What needs attention?** Failed runs, review-ready changes, scope violations, collisions, and stale processes.
2. **What is the agent doing?** Live PTY, PID, provider, prompt, branch, worktree, and exit state.
3. **What changed?** Worktree file browser, changed-file list, and bounded unified diff.
4. **Can it merge safely?** Process stopped, scope valid, base checkout clean, and base branch unchanged.
5. **What does the fleet know?** Shared ownership, memory, messages, and the causal event timeline.

Workspace is the default IDE with a terminal-first main pane and workstream-grouped runs. Operations remains the fleet-level diagnostic view.

## Roadmap after 1.4

### 1.5: Durable runs and causal handoffs

- Persistent run metadata and optional local terminal transcripts across dashboard restarts.
- Structured handoff bundles linking task, resources, memories, messages, branch, and verification evidence.
- Attention states such as blocked, review requested, and human decision required.
- Event replay that shows what an agent knew when it made a decision.

### 1.6: Resource-aware scheduler

- CPU, memory, GPU, and token-budget admission control.
- Queueing and backpressure instead of blindly starting more agents.
- Recovery policies for crashed or rate-limited workers.

### 2.0: Native desktop workspace

- Native packaging around the same local Rust kernel.
- Multi-repository switching, notifications, previews, and richer editor integration.
- Repository and fleet switching without merging operational databases.
- Optional encrypted remote observation, disabled by default.
- Stronger process containment and explicit network/tool policies.

## Explicit non-goals

- Copying Superset's source-available desktop application.
- Pretending an agent's private context is shared unless the agent writes it to PidMesh.
- Generating large volumes of artificial pull requests for profile activity.
- Exposing arbitrary local command execution from the browser dashboard.
- Automatically deleting dirty worktrees after a failed or stopped run.
