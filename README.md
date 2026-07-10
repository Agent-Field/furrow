# agit

**Undo everything your coding agent or terminal changed, including files Git never knew existed.**

Git protects commits. Agent checkpoints usually protect edits made through one agent's editing tools. `agit` protects the complete working state between commits: dirty tracked files, untracked notes, ignored `.env` files, dependencies, generated output, SQLite data, symlinks, executable bits, extended attributes, and Git's own mutable state.

`agit` is local-first and open source. Snapshot bytes stay on your machine.

## Working Demo

```bash
./demo/agent-disaster.sh
./demo/risky-command.sh
./demo/shrink-cache.sh
./demo/parallel-agent-forks.sh
./demo/two-machine-sync.sh
```

The demo creates a real Git repository containing:

- A tracked application with dirty changes
- An ignored `.env`
- Untracked developer notes
- An ignored dependency tree and build output
- A SQLite development database

It then snapshots the workspace, runs `git clean -fdx`, damages the tracked application, previews the impact, and rewinds everything. Independent shell and SQLite checks verify the result.

The parallel-fork demo creates two independent full-state workspaces from one dirty repository, including a 32 MiB ignored dependency cache and local configuration. Two simulated agents modify them concurrently while checks prove the source remains untouched.

## Install

Rust 1.83 or newer is supported.

```bash
cargo install --path .
```

## Daily Use

```bash
cd my-project
agit watch

# Create a meaningful restore point before a risky agent task.
agit snap -m "before dependency upgrade"

# Run any command with automatic before/after restore points; no prior setup required.
agit try -m "dependency upgrade" -- npm install framework@latest

# Preview and reversibly remove recognized dependency/build caches.
agit shrink
agit shrink --yes

# Browse protected states.
agit timeline

# Preview and recover one ignored secret without touching newer work.
agit rewind <snapshot> --paths .env --dry-run
agit rewind <snapshot> --paths .env

# Restore the complete workspace.
agit rewind <snapshot>

# Prefer the logically consistent SQLite image captured with the snapshot.
agit rewind <snapshot> --sqlite-consistent

# Give an agent or risky command an isolated copy of the complete dirty state.
agit run auth-refactor -- claude

# Inspect active parallel workspaces and their actual clone/copy cost.
agit forks

# Review exactly what a fork added, modified, or deleted.
agit diff auth-refactor

# Make overlapping agent intent visible without blocking filesystem writes.
AGIT_AGENT_ID=auth-agent agit claim 'src/auth/**' --ttl-seconds 3600
agit claims

# Share a versioned blackboard value immediately with sibling forks.
AGIT_AGENT_ID=auth-agent agit coord write tasks/auth.md --value 'refresh tokens in progress'
agit coord list

# Converge only after the result passes the project's real verification command.
agit merge auth-refactor --check "cargo test --all"

# Remove the completed workspace and detach its independent timeline.
agit fork-rm auth-refactor

# Claims also expire automatically with their TTL or fork lifecycle.
AGIT_AGENT_ID=auth-agent agit release 'src/auth/**'

# Reclaim bytes no retained workspace can reach.
agit gc --dry-run
agit gc

# Pair two machines through an encrypted directory remote you control.
agit pair /mnt/private/agit-sync --name my-project
agit sync --push

# On the second machine, use the printed pairing key once.
agit pair /mnt/private/agit-sync --name my-project --key <pairing-key>
agit sync --pull --bootstrap

# Later transfers are encrypted, deduplicated deltas.
agit sync --pull
```

For a direct laptop-to-desktop path with no central data plane, install `agit` on both machines and use existing public-key SSH access:

```bash
# Laptop: publish ciphertext directly into the desktop's agit helper.
agit pair ssh://developer@desktop.local --name my-project
agit sync --push

# Desktop: pair to the same local account/namespace using the laptop's key.
agit pair ssh://localhost --name my-project --key <pairing-key>
agit sync --pull --bootstrap
```

SSH sync keeps one `BatchMode` connection open, batches up to 1,024 opaque object have-checks, and holds the remote writer lock until the authenticated HEAD is durable. The receiving machine can pull the stored state later after the sender disconnects.

`agit shrink` recognizes common JavaScript, Python, frontend, and Rust dependency/build caches. Preview is read-only; `--yes` first seals a complete restore point. Its result separates workspace bytes removed from protected-store bytes added and reports the net, because a never-before-captured cache cannot be both locally recoverable and immediately free its full physical size. Use repeated `--path <relative-path>` options for project-specific regenerable directories; Git and agit internals are always refused.

## Agent Integration

`agit mcp` is a local stdio MCP server. Bind it to one watched repository in any MCP-compatible coding agent:

```json
{
  "mcpServers": {
    "agit": {
      "command": "agit",
      "args": ["--repo", "/absolute/path/to/project", "mcp"]
    }
  }
}
```

The server exposes status, timeline, snapshots, fork inspection/creation, merge planning, and rewind. Rewind planning and application are separate tools; application requires a full 64-character snapshot ID repeated as an explicit confirmation. MCP merge is planning-only because verification commands execute shell code; apply verified merges through the CLI.

Every actual rewind first publishes a complete `pre_rewind` snapshot. Rewinding is therefore itself rewindable.

## What Works Today

- Complete immutable snapshots of tracked, untracked, and ignored state
- Streaming content-defined chunking with bounded memory
- Cross-repository content deduplication in an external per-user store
- Hash-verified, framed append-only packs
- Fsynced, hash-chained authoritative snapshot publication log
- Catalog reconstruction after deleting the SQLite index
- Recovery from truncated pack tails
- Human and JSON timelines
- Full and path-scoped dry-run/rewind
- Automatic recovery when `git clean -fdx` removes `.agit/`
- Symlink, executable mode, mtime, and extended-attribute restoration
- Raw filesystem-exact SQLite bytes plus an auxiliary SQLite-consistent backup
- Rewind path traversal and symlink-parent escape protection
- macOS and Linux builds
- Native APFS clonefile and Linux FICLONE warm workspace forks
- Pre-materialization fork disclosure from a streaming disk-backed index: entries, logical bytes, native-CoW projection, and worst-case copied bytes
- Streaming-copy fallback with disclosed physical copy cost
- Independent fork timelines, full-state consistency verification, and command launch
- Exact base-to-head fork inspection with path-level add/modify/delete reporting
- Explicit fork cleanup with safe timeline detachment
- Transactional advisory path claims shared across sibling forks
- Claim/release snapshots with owner, TTL, and conflict attribution in the DAG
- Eager `.agit/coord/` blackboard propagation with offline reconciliation and deletion tombstones
- Streaming cache discovery with honest logical/physical/net `shrink` accounting and exact undo
- Three-way full-state merge with explicit conflicts and scratch-fork verification
- Crash-safe exact reachability GC with shared-chunk preservation
- 64 KiB paged Merkle directories and disk-backed delta path indexing
- Checkpointed pack startup and O(1) reference-log head/append
- Authenticated XChaCha20-Poly1305 sync with opaque remote object names
- Delta-only directory-remote push/pull across independent local stores
- Persistent direct SSH helper transport with batched opaque have-checks
- Reversible first-machine bootstrap and proven fast-forward materialization
- Mandatory single-writer leases with stale-head and rollback rejection
- Durable sibling preservation when machines edit concurrently or offline
- MCP 2025-11-25 stdio server with bounded framing and negotiated lifecycle
- Agent-safe snapshot, timeline, diff, fork, claims, merge-plan, and confirmed rewind tools

The current implementation covers the recovery engine, continuous protection, warm forks, the process wrapper, exact merge planning with verification gating, exact reachability GC, MCP, and follow-only multi-machine sync over directories or persistent SSH. S3/WebDAV adapters, richer class-directed merge strategies, and provenance-accelerated teleport remain subsequent milestones from [the system specification](DISTRIBUTED_AGENT_WORKSPACE_SPEC.md).

On a local APFS benchmark with one flat directory containing 50,000 files, a one-file watcher delta sealed in 0.165-0.170 seconds. The path index is disk-backed; the process does not retain a repository-sized in-memory file map.

## Safety Model

- A snapshot ID becomes visible only after all referenced objects and the publication record are durable.
- SQLite is an advisory index. Packs plus per-workspace refs logs recover the timeline.
- Every chunk is verified against its BLAKE3 identity before restoration.
- Writes are materialized through same-directory temporary files.
- Unsafe `..`, embedded separators, NULs, and symlink-parent traversal are rejected.
- A failed rewind attempts to restore the automatically captured pre-rewind snapshot.
- Noninteractive rewind requires an explicit snapshot ID and `--yes`.

## Data Location

The content store never lives inside the watched repository:

- macOS: `~/Library/Application Support/dev.agit.agit/store-v1`
- Linux: `${XDG_DATA_HOME:-~/.local/share}/agit/store-v1`
- Tests and isolated runs: set `AGIT_DATA_DIR`

The repository's `.agit/workspace-id` is only a pointer. If it is deleted, `agit` rediscovers the repository from the external store.

## Development

```bash
cargo fmt --check
cargo test --all
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
```

The black-box suite uses independent temporary Git repositories and isolated stores. It verifies ignored secret recovery, reversible full rewind, metadata fidelity, SQLite logical recovery, index reconstruction, truncated-pack recovery, path-escape rejection, continuous watching, interrupted-rewind recovery, and independent warm forks.

## License

Apache-2.0
