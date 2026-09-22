# Coordination protocol

PidMesh scopes all state to a logical project path. With no explicit override, linked git worktrees
resolve through their common git directory and share the primary repository root as that identity.
Each agent also records its actual checkout path and branch. `PIDMESH_WORKSPACE` or an explicit CLI
workspace bypasses discovery. Agent sessions get immutable IDs containing their human-readable name,
operating-system PID, and a random suffix. Names are convenient routing aliases; IDs are the stable
address.

Each Rust process holds one configured SQLite connection for its lifetime. Clones inside that process
share the connection through a mutex; independent PIDs coordinate through SQLite WAL and immediate
write transactions. The schema remains compatible with databases created by Python v0.1 and v0.2.

## Session lifecycle

1. A process joins with its PID, provider, and capabilities.
2. Activity refreshes its heartbeat.
3. A clean exit marks the session stopped and releases its claims and resource reservations.
4. Garbage collection marks a stale session dead only when its heartbeat is old and its PID no
   longer exists.

The native swarm supervisor registers every child as a separate session, updates its registration
with the real child PID, and heartbeats only workers that have not exited. A supervisor interruption
sends a termination request to every live child, waits for the configured grace period, then forces
remaining processes to exit and releases their claims and resource reservations.

The PID check avoids declaring a quiet but live local process dead. Leases still expire independently,
so abandoned work can be recovered even before garbage collection runs.

## Memory

Memories are append-only records with a kind, optional key, importance, author, and timestamp. FTS5
provides local lexical retrieval without model downloads or external calls. Later memories do not
silently rewrite an earlier agent's record.

## Messaging

Messages are either direct to an immutable agent ID or broadcast to the workspace. A name resolves to
the most recently active matching session. Broadcasts are visible only to sessions that existed when
the message was sent, and acknowledgements are stored per recipient.

## Claims

Claims use `(workspace, task key)` as their unique identity. Acquisition is one SQLite transaction:
the insert succeeds when the key is free, while takeover succeeds only after expiration. The current
owner can renew a lease. Another agent receives the existing owner instead of a false success.

## Resource reservations

Resources use a namespaced identity such as `path:src/store.rs`, `port:4399`, or
`service:test-postgres`. A reservation request may include up to 64 resources and is acquired
all-or-nothing in one immediate SQLite transaction.

Path ownership is hierarchical. `path:src` conflicts with `path:src/store.rs`, but not with
`path:src2`. Paths are normalized lexically relative to the logical project, and absolute paths or
parent traversal are rejected. Other resource kinds conflict only on an exact normalized key.

The current owner can renew a reservation. Another agent receives the conflicting owners and expiry
times without acquiring any part of its requested set. Expired rows do not appear in reads and may be
taken over immediately. Clean exits, dead-process collection, and garbage collection release or
expire reservations.

Resource reservations are cooperative collision prevention, not filesystem or network enforcement.
Participating agents must reserve intended resources before modifying or binding them. Symlink aliases
that point to the same target can still appear as distinct lexical paths.

## Footprints and collisions

A reservation is a declaration of intent. A footprint is an observation of fact: the set of paths a
checkout has actually changed relative to an integration base, derived entirely from git and
requiring no cooperation from the agent beyond pointing the scan at a directory.

A scan resolves the integration base (`main`, then `master`, unless one is named), takes the merge
base against `HEAD`, and unions committed changes since that base with uncommitted changes including
untracked files. Uncommitted state wins where both describe the same path.

The filesystem, not the git status letter, decides whether a path still exists: git reports a
staged file that has since been deleted as added, and a path can vanish between the status call and
the scan, so a reported path that is absent is recorded as deleted.

Each surviving path is fingerprinted from its bytes so identical content is distinguishable from
divergent content. The digest is mesh-internal and is not a git object id; it only ever has to
answer whether two checkouts hold the same content. Anything without comparable content carries no
digest — a deletion, a directory, a submodule, a nested repository, a symlink, an unreadable or
oversized file, or a path whose name is not valid UTF-8 — and a missing digest is treated as
divergent, which is the conservative direction. One unreadable path never fails a scan.

Publishing a footprint replaces every row previously recorded for that agent in one immediate
transaction. The footprint is therefore authoritative: withdrawing a change removes the agent from
that path's contention on its next scan, and an agent that merges and cleans its checkout leaves
every collision automatically.

Contention is keyed on checkout rather than agent, in the participant list as well as in the test
for whether a path is contested. A worker that runs both a CLI session and an MCP session against
one worktree is a single editor of that path, and only its most recent observation counts;
otherwise a stale footprint from a co-located session would argue with its own checkout. A path
changed in more than one checkout is classified by merge outcome rather than by lock
ownership. `identical` means every participant reached the same outcome: the same content hash, or every
checkout deleting the path. `delete_edit` means at least one participant removed the path while
another still edits it, and outranks content comparison. Everything else is `divergent`. Collisions separately report whether the participants
cut their checkouts from different base commits, because a stale base is how a textually clean merge
still produces incorrect behaviour.

Collisions are fingerprinted by severity and participant set. A fingerprint that appears or changes
appends `collision.detected`; one that disappears appends `collision.cleared`. Unchanged state
appends neither, so agents polling the mesh in a loop generate no event churn while waking
immediately on a real overlap.

Footprints survive an agent being marked stopped, because PidMesh deliberately preserves worktrees
and their uncommitted work. Collision reports carry each participant's session status so a reader
can tell live contention from abandoned contention. Garbage collection removes a dead agent's
footprint only once its checkout no longer exists on disk, at which point the contention it
described cannot be real. An agent may also withdraw its footprint explicitly, which is the only
option available to a session that is shutting down and can no longer produce a scan.

Because every checkout is recorded at registration, a supervisor can publish footprints on behalf
of a fleet that never calls the protocol itself. Observation therefore requires no agent
participation at all, unlike a reservation, which is the property that makes convergence hold for
agents launched by an arbitrary harness. A sweep scans each distinct checkout once regardless of
how many sessions occupy it, skips checkouts that have been removed, and ignores sessions whose
process is gone.

## Merge ordering

A collision report states that two checkouts disagree. Merge readiness decides whether one of them
may land. The caller supplies the integration branch's current head, because resolving a ref is a
git question rather than a mesh one, and the mesh compares it against the commit the worktree was
cut from.

Three conditions block a merge. A stale base means the integration branch advanced after this
worktree was cut, so the diff may apply cleanly and still be wrong. A contested path means another
live checkout holds different content on a path this one changed. A held integration lease means
another agent is merging at this moment.

Two things deliberately do not block. Byte-identical content is duplicated effort, so merging
either copy is safe. A peer that is no longer running does not block either: its preserved worktree
is still reported as a collision, but it cannot be asked to rebase, and treating it as a blocker
would deadlock every later merge behind an agent that has already exited.

The integration lease is an ordinary claim under a reserved task key rather than a new mechanism.
It therefore inherits exactly-one-owner semantics, lease expiry so a crashed holder cannot wedge
the queue permanently, and release through the existing agent lifecycle.

## Event stream

Every coordination mutation appends an event with a monotonically increasing sequence. Consumers can
checkpoint a sequence and request only later events, which makes polling deterministic and cheap.
The bounded wait operation long-polls this sequence for up to 60 seconds, allowing an agent to sleep
until another session creates a memory, message, claim, resource reservation, handoff, or lifecycle
event.
