# agit — Document 2: Distributed Runtime, Developer Experience, and Commercial Model

**Status:** Draft 0.1 — companion to `DISTRIBUTED_AGENT_WORKSPACE_SPEC.md` (Document 1, draft 0.3)
**Audience:** Product, architecture, infrastructure, and business reviewers
**Scope boundary:** Document 1 owns the local engine and its semantics — snapshots, rewind, fork, merge, sync, retention, fidelity, security of the store. This document owns everything *around* it: ephemeral runners, network architecture, inter-agent coordination primitives, cold-start engineering, the open-source/commercial split, unit economics, and DX/UX contracts. Where the two documents touch (classes, snapshot IDs, transport encryption, phases), Document 1 is authoritative and this document cites it.
**Last updated:** 2026-07-09

---

## 1. Governing Principles

1. **Vendor neutrality is absolute.** Nothing specific to Claude Code, Codex, Cursor, or any agent product enters agit code. Integration surfaces are generic: filesystem semantics, CLI, hooks (any command can call `agit snap`), and MCP (an open protocol). Agent-specific glue lives in user config or contrib examples, never in core.
2. **OSS is never artificially slow.** Every algorithm ships open: hydration planner, prefetch profiles, dedup, memoization, coordination primitives. The commercial tier is faster only through *physics* (compute colocated with state), *shared infrastructure* (registry pool, prewarmed sandboxes), and *absence of setup* — never through withheld or degraded code paths.
3. **All connections are outbound.** No participant — laptop, desktop, runner — ever requires an inbound port, NAT traversal, VPN, or Tailscale. Optional direct paths (LAN, tailnet) are auto-detected accelerations; correctness never depends on connectivity luck.
4. **Capability is free; adjacency and absence-of-setup are paid.** The paywall sits exactly on "I could do this myself in ~30 minutes of setup and upkeep, or pay a small amount and not."
5. **The state model does the coordinating.** Multi-agent behavior emerges from versioned filesystem primitives (forks, claims, a coordination class), not from an orchestrator or message bus.

---

## 2. IP-6: Ephemeral Runners (`agit run`)

### 2.1 What it does

```text
agit run --cloud 3 "claude -p 'fix issue #42'"      # hosted (paid)
agit run --host ssh://my-hetzner-box "cargo test"    # BYO compute (OSS)
agit run --driver docker "npx codemod ..."           # local container (OSS)
```

Lifecycle, identical across drivers:

1. **Seal + push delta.** The current workspace seals (forced boundary, Doc 1 §8.1); only private chunks the rendezvous lacks are uploaded — typically KBs–MBs, since dependency bytes resolve via pool or regeneration and history is deduped.
2. **Provision N sandboxes.** Hosted: prewarmed micro-VMs colocated with the managed store. BYO: any Linux host reachable outbound (SSH, Docker socket, or a cloud driver with the user's API key).
3. **Lazy attach.** Each sandbox mounts a fork of the snapshot via FUSE (or NFS-loopback) — *server-side Linux only*, where lazy mounts are cheap and controlled. An agent that reads 40 files and edits 6 transfers kilobytes. Dev machines never require FUSE (Doc 1's materialization rules stand).
4. **Run + seal continuously.** The command executes; the runner's engine seals to its own fork timeline; logs and progress stream back live.
5. **Runners die; snapshots survive.** Results appear locally as sibling forks: `agit forks` → diff → `agit try`/merge one, discard the rest. Sandboxes are destroyed; nothing durable lives on a runner.

### 2.2 Driver interface (OSS)

A small trait: `provision(n, image) / attach(snapshot, fork) / exec(cmd, streams) / destroy()`. Shipped drivers: `ssh`, `docker`, `local`. Cloud drivers (Fly, Hetzner, EC2) are pluggable and community-extensible. The hosted service implements the same interface — the paid product is a driver plus infrastructure, not a fork of the product.

### 2.3 Secrets and keys on runners

- A runner executing code necessarily sees plaintext of what it mounts. Therefore: **session-scoped keys**, granted per run, living only inside the sandbox, dying with it.
- **`config-secret` class is never shipped to runners by default** (Doc 1 §5.9). Specific secrets are allowlisted per-path in `.agitpolicy` (e.g., a test API key). The default answer to "you run my code with my `.env` on your machines?" is "no — unless you name the exact file."
- Runner-sealed snapshots are signed by the session key; local `agit forks` shows which universe came from which runner.

### 2.4 Cold start, decomposed

| Layer | Mechanism | OSS | Hosted |
|---|---|---|---|
| Source + config bytes | delta from rendezvous, priority hydration | seconds | seconds |
| Dependencies | planner picks: registry pool / regenerate (`npm ci`, `cargo fetch` — the registries' own CDNs) / fetch from bucket | regeneration-dominated, ~1–3 min | pool bytes incl. postinstall output + build caches, seconds |
| Sandbox boot | user's machine or container | already running / container start | prewarmed micro-VM, ~hundreds of ms |
| Transfer path | user's bucket region ↔ runner | internet RTTs | intra-region, GB/s |
| Prefetch | learned access profiles shipped with the snapshot (OSS algorithm) | ✔ | ✔ + warm CDN cache |

**Targets:** OSS BYO cold start ≤ 3 min (dominated by dependency regeneration — honest, not crippled). Hosted: **command-to-agent-working < 10 s.** The gap is physics and shared infrastructure; both paths run identical open code.

---

## 3. Network Architecture

### 3.1 Rendezvous model

```text
 laptop ──outbound HTTPS──►   rendezvous   ◄──outbound HTTPS── runner / desktop
                        data plane: bucket (S3/R2/B2 API) or managed store
                        control plane: relay (outbound WebSocket both sides —
                        logs, progress, cancellation, lease grants)
```

- **Data plane:** chunks move through the rendezvous — the user's own bucket (OSS) or the managed store (hosted). Plain object-storage HTTPS: works from hotel Wi-Fi, corporate NAT, CI.
- **Control plane:** both endpoints hold outbound WebSockets to a relay that pairs them; that is how a terminal receives live logs from a sandbox neither side can dial. (Same pattern as CI runners and tunnel services — boring, proven.)
- **Self-hosted control:** for pure-OSS `--host ssh://...` runs, the SSH channel *is* the control plane; no relay involved. The relay is only needed when neither side can reach the other — which is exactly the hosted case.
- **Auto-detected accelerations:** mDNS on shared LANs and existing tailnets are used for direct device↔device transfer when present (Doc 1 §8.5 device-CDN); the rendezvous path always remains as fallback. Tailscale is a silent speed boost, never a prerequisite.

### 3.2 DX tiers (simplest first)

| Tier | Setup | Cost | What works |
|---|---|---|---|
| Hosted | `agit login` — once | subscription + per-minute runners | everything, zero config |
| BYO bucket | `agit remote add r2://…` — paste one URL + key | free; cents of storage | sync, teleport, backup, BYO runners against your bucket |
| BYO compute | `agit run --host ssh://box` or a cloud API key | free; your hardware | runners without any hosted component |
| Local only | install binary | free | rewind, fork, try, bisect, shrink, merge, MCP |

Every tier above "local only" degrades gracefully to the tier below it when offline.

---

## 4. IP-7: Inter-Agent Coordination Primitives

Agents coordinate **through versioned state, not APIs** — filesystem-level, so anything that can read and write files (or speak MCP) can participate. All primitives are OSS.

- **`agit claim <glob>`** — advisory path leases recorded in the DAG. Agent A claims `src/auth/**`; agent B's overlapping claim is refused with A's identity and claim time. Work partitioning with zero orchestrator. Claims expire with the fork or by TTL; they are advisory (the engine never blocks writes — it makes contention *visible*, and merge classification uses claim history as a signal).
- **`coord` content class** — a designated directory (default `.agit/coord/`) replicated *eagerly* between sibling forks — not waiting for quiescence seals. Agents leave task lists, notes, partial results; it is a versioned blackboard with the same history, rewind, and attribution as everything else.
- **`agit watch-fork <name>`** — subscribe to a sibling fork's seals (CLI stream and MCP notification): "agent B just sealed changes to the file you are editing."
- **MCP surface:** `claim`, `release`, `coord-read/write`, `watch-fork`, `fork-diff` — the complete multi-agent toolkit exposed through one open protocol, with zero knowledge of which agent product is calling.

**Colocation acceleration (hosted, by physics):** sibling forks on the same runner host share one local CAS, so coord-class propagation and watch-fork latency drop from bucket-round-trip to milliseconds. Same primitives, faster adjacency — consistent with Principle 2.

---

## 5. Open-Source / Commercial Split

### 5.1 The split

| | Open source (capability) | Commercial (adjacency + absence-of-setup) |
|---|---|---|
| Engine: rewind, fork, `try`, bisect, `shrink`, merge, timeline | ✔ all | — |
| Hydration planner, prefetch profiles, memoization, dedup | ✔ algorithms open | warm shared caches |
| Multi-machine sync | ✔ self-hosted (bucket / SSH) | zero-config sync + relay, no bucket setup |
| Backup | ✔ to your own bucket, E2E | managed encrypted backup, one login |
| Runners | ✔ BYO (SSH/Docker/cloud drivers) | colocated prewarmed sandboxes, per-minute, <10 s cold start |
| Registry pool | planner falls back to regeneration (registries' own CDNs) | pool CDN: exact bytes incl. postinstall + build caches |
| Coordination primitives | ✔ all | millisecond propagation via colocation |
| MCP server, hooks | ✔ | — |

### 5.2 Friction audit — does OSS still go viral?

The viral surface — `shrink` freeing 50 GB, `try` making any risky command reversible, bisect answering "what broke it", fork enabling parallel agents on one laptop — carries **zero setup**: one binary, no account, offline. Those spread on their own; every rescue and every disk-space screenshot is distribution.

The frictions that remain in OSS (create a bucket once, ~10 min; provision and babysit runner hosts, ongoing) gate precisely the features where paying a small amount is genuinely easier than DIY. That is the correct paywall placement: friction removal, not capability — the Tailscale/Syncthing-era lesson applied deliberately.

### 5.3 Unit economics (hosted, indie ~$12/mo + metered runner minutes)

- **Sync/backup storage:** deduped, delta-based ciphertext; a heavy user holds ~10–20 GB → COGS ≈ $0.15–0.30/mo on zero-egress storage. ~97% gross margin on the subscription core.
- **Registry pool:** one shared copy of each public package version serves every customer; CDN cache-hit-dominated. **Cost per user falls as users grow** — inverse of typical infra scaling.
- **Runners:** cost-plus per-minute over commodity micro-VMs; subscription includes N free minutes (habit formation), margin in metered overage. Main idle-cost risk is the prewarm pool → demand-based sizing (§7, R5).
- **Relay:** WebSocket pairing; near-zero.
- **Free tier COGS: zero** — it runs on the user's hardware and the user's buckets.

---

## 6. DX / UX Contracts

1. **One-command onboarding per tier:** `agit login` (hosted) or `agit remote add <url>` (BYO) unlock their whole tier; no further configuration is required for defaults to be safe and useful.
2. **Cost is disclosed before it is incurred:** `agit run` prints sandbox count, estimated minutes (from prior runs when available), and hydration plan (pool/regenerate/fetch split) before provisioning; `agit teleport` prints estimated upload bytes. No surprise bills, no surprise waits (extends Doc 1 Invariant 11 to money and minutes).
3. **Live, attributable output:** every runner streams logs prefixed by fork name; `agit forks` is the single pane for what every universe is doing and what it costs.
4. **Failure is legible:** a dead runner leaves its last sealed snapshot and a terminal status (`completed | failed(exit) | lost(timeout)`); partial work is never silently discarded — it is a sibling fork like any other.
5. **Offline never breaks local:** losing connectivity mid-run means results arrive when connectivity returns; local features are untouched (Doc 1 Invariant 13).
6. **Agent-facing surfaces are explicit:** runners and agents address snapshots by ID, destructive operations require explicit flags (Doc 1 §9.3), and MCP tools mirror CLI semantics one-to-one — no agent-only behavior.

---

## 7. Risks Specific to This Document

| # | Risk | Resolution | Residual |
|---|---|---|---|
| R1 | Runner sees plaintext (inherent to executing code) | Session-scoped keys dying with the sandbox; `config-secret` never shipped unless path-allowlisted; signed runner seals (§2.3) | Users must understand allowlisting = disclosure to the sandbox; stated at the prompt |
| R2 | Free/cheap runner minutes attract abuse (mining, spam) | Runner egress restricted to registries + rendezvous by default; minute caps on new accounts; workload heuristics | Arms race; standard for compute products |
| R3 | Relay becomes an availability choke point | Relay is control-plane only (data goes bucket-direct); SSH tier bypasses it entirely; stateless relay scales horizontally | Hosted live-logs degrade during relay outage; runs complete regardless |
| R4 | Pool coverage gaps (ecosystems, private mirrors) | Planner's regeneration fallback is a first-class path, not an error; coverage sequencing npm → PyPI → crates → Go (Doc 1 §19.1) | Uncovered ecosystems get OSS-grade cold start on hosted too — disclosed in the plan output |
| R5 | Prewarm pool idle cost inverts runner margins | Demand-based pool sizing; micro-VM boot is fast enough (~sub-second) that the pool can run shallow; include-minutes tuned to observed utilization | Cold-start target degrades to ~30 s under pool misses at demand spikes |
| R6 | BYO cloud drivers rot (API churn) | Drivers are a thin OSS trait with community ownership; `ssh` and `docker` (stable interfaces) are the maintained reference paths | Exotic drivers may lag; core never depends on them |
| R7 | Coordination primitives race (claim conflicts across partitions) | Claims are advisory + DAG-recorded: conflicts are *detected and visible*, never load-bearing for correctness; merge uses claim history as a signal (§4) | Two offline agents can still both claim a path; surfaced at sync, resolved at merge — consistent with Doc 1 sibling semantics |
| R8 | Hosted tier perceived as the "real" product, OSS as bait | Principle 2 enforced in CI: no code path may branch on tier for performance; benchmarks publish OSS-vs-hosted numbers with the physics explanation | Perception risk persists; countered by transparency, not code |

---

## 8. Delivery Mapping (against Document 1 §16)

| Doc 1 phase | This document adds |
|---|---|
| Phase 0–1 (POC, engine) | Nothing — this document intentionally has no Phase-0/1 footprint; the Codex worker's current scope is unaffected |
| Phase 2 (fork, MCP, follow-only sync) | Coordination primitives v0 (`claim`, `coord` class, `watch-fork`) — they are local/DAG features and strengthen the parallel-agent demo |
| Phase 3 (merge) | Claim-history as a merge classification signal |
| Phase 4 (full sync, teleport, hosted plane) | Runner driver interface + `ssh`/`docker` drivers (OSS); relay; hosted runners + prewarm pool + session keys; pool CDN; pricing launch |
| Phase 5 (Windows, team tier) | Colocated multi-runner CAS sharing; team runner quotas |

---

## 9. Open Questions

1. Default runner image contents (language runtimes preinstalled vs. hydrated per-project via the planner) — image weight vs. cold-start variance.
2. Whether `agit run` without `--cloud`/`--host` should default to the `docker` driver locally (safest demo) or refuse (least surprise).
3. Included-minutes quantity in the base subscription vs. pure metering — habit formation vs. margin predictability.
4. Claim TTL defaults and whether claims should optionally *block* MCP-mediated writes (strict mode) despite the advisory-only engine stance.
5. Relay protocol: reuse an existing open tunnel protocol vs. minimal bespoke WebSocket framing (leaning bespoke-minimal; scope discipline).
6. Whether prefetch access-profiles are shared to the pool anonymously (better cold starts for everyone) or remain strictly per-account (simpler privacy story) — default: per-account.
