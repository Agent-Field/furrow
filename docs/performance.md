# furrow Performance Baselines

This document records reproducible measurements, not extrapolated product claims. The harness is `furrow-core/benches/engine.rs`; every sample runs in a fresh subprocess so CPU and peak RSS are isolated from other scenarios.

## Run It

```bash
cargo bench -p furrow-core --bench engine
FURROW_BENCH_ENFORCE=1 cargo bench -p furrow-core --bench engine
FURROW_BENCH_PROFILE=reference cargo bench -p furrow-core --bench engine
```

The default developer profile uses three samples, 5,000 files, 100 changed files, a 128 MiB generated stream, a 32 MiB warm dependency, 721 snapshots spread over six months, and 1,000 tree/ref lookups. A smaller enforced smoke profile is available for macOS and Linux. The `reference` profile uses five samples, 1,000,000 files, a 1 GiB stream, and 17,281 historical snapshots.

Peak RSS comes from `getrusage(RUSAGE_SELF)`. CPU is user plus system time consumed inside the measured operation. Fixture construction, Git initialization, and baseline capture are outside wall/CPU timing but remain included in the subprocess peak-RSS measurement.

## Baseline: 2026-07-09

Hardware: Apple arm64 `Mac15,10`, APFS, macOS 26.5.1. Rust release profile uses thin LTO, one codegen unit, and `opt-level=3`.

| Scenario | Dataset | Median | p95 | CPU p95 | Peak RSS | Minimum rate |
|---|---:|---:|---:|---:|---:|---:|
| Chunk + BLAKE3 | 128 MiB | 240.9 ms | 244.8 ms | 239.3 ms | 7.8 MiB | 522.9 MiB/s |
| Paged tree diff | 5k entries, one change, 1k runs | 1.371 s | 1.378 s | 1.347 s | 9.7 MiB | 1.38 ms/run |
| Reverse ref lookup | 721 refs, 20 returned, 1k runs | 176.8 ms | 188.8 ms | 181.7 ms | 6.4 MiB | 0.189 ms/read |
| Cold seal | 5k files | 967.1 ms | 1.065 s | 778.1 ms | 12.8 MiB | 4,693 files/s |
| Delta seal | 100 of 5k files | 15.3 ms | 16.1 ms | 2.1 ms | 13.1 MiB | 6,202 files/s |
| Full-state fork | 5k files + 32 MiB warm cache | 727.7 ms | 751.4 ms | 621.7 ms | 18.3 MiB | 6,693 entries/s |
| Five warm universes | 5 × (5k files + shared 32 MiB cache) | 1.242 s | 1.270 s | 837.9 ms | 18.6 MiB | 3.94 universes/s |
| Conflict radar | 5 forks × 100 overlapping paths | 543.6 ms | 548.1 ms | 524.1 ms | 18.0 MiB | 912 dirty memberships/s |
| Retention + exact GC | 721 snapshots / six months | 78.0 ms | 87.8 ms | 60.2 ms | 9.1 MiB | 8,216 snapshots/s |

The universe row covers five verified CoW materializations plus five concurrent process launches through the macOS sibling-directory driver. It measures startup cost, not page-cache sharing.

The radar row is a cold family-index reconciliation from five independently sealed heads. Its p95 no-change refresh was 52.5 ms. Dirty scopes and conflict membership stay in SQLite; the process never constructs a repository-sized cross-fork map.

The fork's p95 inner atomic hierarchy clone was 133 ms. The remaining time is bounded metadata/cache proof, path-index backup, and durable publication.

## Optimization Evidence

The same 20,000-file + 32 MiB fork scenario was measured while optimizing the macOS path:

| Implementation | Wall | Inner clone | Peak RSS |
|---|---:|---:|---:|
| Per-file clone + full destination recapture | 8.822 s | 3.572 s | 33.8 MiB |
| Atomic recursive clone + serial proof | 2.323 s | 425 ms | 18.5 MiB |

This is a 3.8x wall-time improvement and a 45% peak-RSS reduction. The fast path uses one atomic recursive `clonefile`, repairs directory metadata, rejects hard-link/special-file cases to the fidelity-preserving fallback, proves every indexed path against capture metadata and cached blob identity, rejects extra/missing paths, and then reuses the already-durable base DAG. Policy-excluded trees use the conservative verifier.

## Gates And Gaps

`FURROW_BENCH_ENFORCE=1` currently requires at least 50 MiB/s chunk throughput, a 2 s delta seal ceiling, at most 100 ms per paged-tree diff, at most 5 ms per indexed ref read, tier-aware fork and multi-universe ceilings, a 10 s conflict-radar ceiling, a 10 s GC ceiling, and at most 512 MiB peak RSS per isolated scenario.

Still unproven and therefore not claimed as achieved:

- The full 1M-file reference profile on both declared reference machines.
- Idle watcher `<1% CPU / <=150 MiB RSS` across 10 repos and 2M entries.
- The complete Btrfs/ZFS/overlayfs/APFS/XFS platform matrix.
- Ten-universe physical-memory/page-cache sharing. The harness measures startup wall time, CPU, and process RSS, but the `~1.0x` shared-page-cache goal remains unclaimed until resident pages are measured on Linux CoW filesystems.
- Generic flat-directory Merkle diff is proportional to root page count; agent delta sealing avoids that path through the disk-backed changed-path index.
- Fork verification remains O(entries) without a true immutable filesystem snapshot, even when hierarchy creation is atomic.

## Warm SSH Session Smoke Baseline: 2026-07-10

The CLI integration fixture runs two independent repositories and object stores
through the real framed SSH-helper protocol, using a local wrapper in place of
network transport. A peer publishes a small changed snapshot while the other
side is blocked on the persistent HEAD subscription. The measured publish start
through notification, pull, and workspace materialization was **255 ms** on the
development Mac. The same test asserts one SSH helper process across multiple
reconciliation cycles, a measured notification phase, and
`reused_connection=true` on the second operation.

This is a local protocol regression baseline, not a WAN result. Run the same
test under latency/loss shaping before claiming the 100 ms RTT contract:

```bash
cargo test --locked -j 2 -p furrow-cli --test cli persistent_ssh_helper_syncs_independent_stores_over_framed_stdio -- --nocapture
```

## Shared-store and Git-aware capture contracts

`furrow-cli/tests/shared_store.rs` measures actual data-directory bytes for four
independent timelines capturing a 31 MiB tree, verifies that adding one changed
file adds only its chunks plus metadata, and stresses concurrent daemon writers
and one-shot CLI processes against the same store. It compares the capture
policy against real Git's tracked and ignored file lists, and exercises shared
export/import, purge and GC. Each test names its S1–S10 contract items.

The `gitignore` directive uses in-process matchers and an identity-cached Git
index. Sparse directory entries resolve Git tree names on demand without
expanding unrelated sparse trees or listing ignored directories. Full capture
prunes ignored directories and follows indexed tracked
exceptions directly, without listing unrelated children. Delta capture checks
only named paths and their ancestor rule chains. Omission reports contain
collapsed names and a remaining-group count, with no recursive accounting.

### Default-on wave round 1: 2026-10-22

Four caller-named timelines capturing 31 distinct MiB of files occupied
33,521,664 allocated bytes versus 32,899,072 for one timeline (1.0189×).
Replacing one 1 MiB file added 1,150,976 allocated bytes. Round 1's broad
benchmark comparison ran separately under disk contention and showed large
regressions. Round 2 replaces that comparison with interleaved base/head runs.
The old exact-count report required ignored-directory enumeration; G4 replaces
that report so pruning can avoid enumeration entirely.

### Default-on wave round 2: interleaved comparison

Linux/ext4 shared host, Rust 1.97.1 release builds. Historical engine 7660d4f
versus this change, 10,000 source files, 100 changed files, 32 MiB warm cache,
128 MiB chunk stream, 721 historical snapshots, 1,000 lookups and five universes.
Each scenario starts with five samples per binary in strict base/head
alternation, with fixture setup excluded. Cold seal, delta seal and fork then
receive ten additional alternating pairs, a fixed extension selected because
the earlier run showed wide disk-latency variation. All samples are retained;
the main table pools all 15 for these three scenarios and keeps five for the
others. The initial five-pair results appear separately below. The unchanged-tree scenario adds the identical
2.1-second fixture-aging harness to the historical bench only. Engine sources
in the historical archive are unchanged. Spread below is minimum–maximum.

| Scenario | Samples/version | Base median ms (range) | Head median ms (range) | Head/base | G5 |
|---|---:|---:|---:|---:|---|
| chunk | 5 | 225.678 (222.240–256.404) | 222.865 (220.754–224.323) | 0.9875× | — |
| tree-diff | 5 | 20986.334 (20759.286–21100.655) | 20781.684 (20523.972–21073.710) | 0.9902× | — |
| refs | 5 | 112.420 (110.867–114.219) | 115.193 (111.188–120.101) | 1.0247× | — |
| cold-seal | 15 | 2327.899 (1620.307–4364.582) | 2250.488 (1319.080–4169.045) | 0.9667× | PASS |
| delta-seal | 15 | 94.510 (66.407–208.081) | 96.715 (62.345–195.779) | 1.0233× | PASS |
| fork | 15 | 3166.522 (2929.513–16077.012) | 3006.921 (2903.549–15909.477) | 0.9496× | PASS |
| universes | 5 | 19814.143 (15207.448–46526.283) | 17072.004 (13465.753–47566.766) | 0.8616× | — |
| radar | 5 | 2348.073 (1199.761–2907.287) | 1566.393 (1173.590–2033.463) | 0.6671× | — |
| gc | 5 | 454.601 (273.663–720.462) | 399.121 (319.865–897.781) | 0.8780× | — |
| unchanged-seal | 5 | 548.297 (532.282–568.242) | 596.056 (521.663–1854.538) | 1.0871× | — |

Initial five-pair critical results (same final source):

| Scenario | Base median ms | Head median ms | Head/base |
|---|---:|---:|---:|
| cold-seal | 2811.488 | 3242.160 | 1.1532× |
| delta-seal | 197.134 | 139.719 | 0.7088× |
| fork | 5539.454 | 5537.018 | 0.9996× |

CPU medians for the pooled critical samples, for diagnosis only:

| Scenario | Base CPU ms | Head CPU ms |
|---|---:|---:|
| cold-seal | 717.351 | 718.231 |
| delta-seal | 11.096 | 11.413 |
| fork | 2212.883 | 2181.149 |

Policy-specific measurements use nine alternating samples per variant on the
same 10,000-source-file fixture. G2 compares 20,000 ignored files with that
directory deleted; G3 compares 100-path delta seals with the directive off/on.

- G2 deleted: median 4232.569 ms, range 2657.193–9672.180 ms; CPU median 788.993 ms.
- G2 ignored: median 4814.895 ms, range 2594.448–34785.349 ms; CPU median 783.799 ms.
- G2 ratio: 1.1376×; limit 1.2×: PASS.
- G3 off: median 380.455 ms, range 227.420–646.782 ms; CPU median 11.546 ms.
- G3 on: median 571.018 ms, range 259.388–2306.962 ms; CPU median 12.407 ms.
- G3 ratio: 1.5009×; limit 1.1×: FAIL.

G3 fixed follow-up: the initial nine pairs had a wide spread and exceeded the
limit, so 32 additional alternating pairs were selected before collecting them,
after the other benchmarks and tests. All 41 pairs are pooled below; none of
the initial samples are discarded. This is the final G3 latency result.

- off: 41 samples, median 232.694 ms, range 90.100–646.782 ms; CPU median 10.944 ms.
- on: 41 samples, median 259.388 ms, range 111.020–2306.962 ms; CPU median 11.668 ms.
- Final G3 ratio: 1.1147×; limit 1.1×: FAIL.

Strace of cold capture, a source-file delta and an ignored-file delta with
20,000 ignored dist/ files found zero openat/getdents64 operations under dist/
and no Git subprocesses. Full traces, sample JSON, commands and gate results
are retained in the default-on handoff lane f1r2.

A separate instrumented base/head comparison counted operations only between
the benchmark's timed endpoints. Sync counts are identical: cold seal has
26 fsync and two fdatasync calls, delta seal two and two, and fork 34 and four.
fcntl counts match; cold seal and fork add one uncontended startup flock.
No lock retries occurred. In the instrumented delta pair, base/head CPU was
45.935/47.371 ms while sync calls blocked 303.581/827.567 ms; one head fsync
blocked 423.792 ms. These diagnostic timings are excluded from the tables.
Host I/O pressure during the diagnostic run was 41.92% some / 38.31% full,
and reached 52.68% / 50.38% during the policy timing run. This establishes
large variation in required durability waits, without extra sync operations.
It does not turn a failed latency gate into a pass. Final gate decisions use
the pooled critical medians and the 41-pair G3 result, with the initial results
retained above. A controlled-host run would distinguish a small engine cost
from storage contention more precisely without weakening durability.

Four timelines use 33,521,664 allocated bytes versus 32,899,072 for one,
or 1.018924× (G6 limit 1.15×). Changing one MiB added 1,142,784 bytes.
The full six-case syscall checks also cover ordinary and packed sparse-index
tracked exceptions: only the tracked child is opened and the ignored directory
is never enumerated. Full cold/delta pruning therefore passes independently
of the wall-time ratios.

Final unresolved latency bounds: G3 directive delta.

A separate directive off/on delta trace measured the same fixture after the
benchmark run. These instrumented timings are diagnostic, outside the medians:

- off: 2 fsync, 2 fdatasync; 187.414 ms in sync calls, 46.982 ms CPU, 288.162 ms wall.
- on: 2 fsync, 2 fdatasync; 221.961 ms in sync calls, 47.634 ms CPU, 325.970 ms wall.

## Merged shared-store and Windows validation (2026-10-23)

F3 measures the merged shared-store/Windows source against 7660d4f, using the same explicit 10,000-file fixture profile as the preceding F1 round. Samples alternate base then merged throughout one sitting. Cold capture, delta capture and fork have fifteen samples per version; every other scenario has five. All raw samples are retained in the default-on handoff lane f3. No lane build, test or native execution overlapped these timings.

| Scenario | n/version | Base median ms (min–max) | Merged median ms (min–max) | Ratio |
|---|---:|---:|---:|---:|
| chunk | 5 | 225.904 (223.756–227.413) | 224.582 (222.909–236.409) | 0.9941× |
| tree-diff | 5 | 21460.004 (21198.197–22775.674) | 21901.562 (21049.863–22508.391) | 1.0206× |
| refs | 5 | 118.284 (113.425–130.702) | 114.696 (113.909–118.443) | 0.9697× |
| cold-seal | 15 | 2361.009 (1761.149–6904.542) | 2080.354 (1631.436–3126.213) | 0.8811× |
| delta-seal | 15 | 145.997 (24.262–329.643) | 133.649 (22.232–537.385) | 0.9154× |
| fork | 15 | 9698.674 (4977.692–21966.423) | 9466.246 (4969.603–41428.317) | 0.9760× |
| universes | 5 | 40272.372 (28694.177–55040.569) | 32405.814 (22805.917–41078.142) | 0.8047× |
| radar | 5 | 862.780 (662.902–1225.138) | 1395.714 (877.952–1546.254) | 1.6177× |
| gc | 5 | 86.296 (57.729–109.303) | 100.361 (66.821–193.077) | 1.1630× |
| unchanged-seal | 5 | 476.596 (420.888–1492.634) | 721.583 (394.051–4192.695) | 1.5140× |

The G5 +10% bounds apply to cold capture, delta capture and fork.

- cold-seal: PASS, 0.881129× versus 1.1×.
- delta-seal: PASS, 0.915425× versus 1.1×.
- fork: PASS, 0.976035× versus 1.1×.

Policy-specific comparisons use fifteen alternating pairs per case on the same merged executable. G2 compares 20,000 ignored files with the ignored directory deleted. G3 compares a 100-path delta seal with the directive off/on.

| Gate | Variant | Median ms | Min ms | Max ms | CPU median ms |
|---|---|---:|---:|---:|---:|
| G2 | deleted | 2905.156 | 1763.360 | 5697.997 | 814.441 |
| G2 | ignored | 2915.295 | 2610.144 | 6188.896 | 807.202 |
| G3 | off | 27.024 | 18.273 | 88.167 | 10.649 |
| G3 | on | 34.872 | 16.302 | 182.022 | 11.462 |

- G2: PASS, 1.003490× versus 1.2×.
- G3: FAIL, 1.290425× versus 1.1×.

A separate instrumented G3 pair counts operations strictly between the benchmark’s getrusage endpoints. It is diagnostic and excluded from the medians:

- off: 181.400 ms wall, 46.798 ms CPU, 65.875 ms in sync calls; fsync=2, fdatasync=2, flock=30, fcntl=92, openat=146, statx=430, read=232, pread64=189.
- on: 263.230 ms wall, 55.411 ms CPU, 119.013 ms in sync calls; fsync=2, fdatasync=2, flock=30, fcntl=92, openat=153, statx=551, read=240, pread64=191.

The other slower wall-time medians also have equal or lower uninstrumented CPU medians:

- radar: base/merged CPU 290.128/289.923 ms (0.999293×).
- gc: base/merged CPU 42.826/42.613 ms (0.995026×).
- unchanged-seal: base/merged CPU 134.834/130.422 ms (0.967278×).

Separate instrumented sync/lock diagnostics for those cases (excluded from medians):

| Case | Version | Wall ms | CPU ms | Sync wait ms | fsync | fdatasync | flock | fcntl |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| radar | base | 3669.119 | 1462.881 | 60.795 | 22 | 0 | 8340 | 8578 |
| radar | head | 3707.703 | 1485.160 | 75.779 | 22 | 0 | 8345 | 8578 |
| gc | base | 2193.807 | 710.982 | 174.674 | 5 | 1 | 10 | 14701 |
| gc | head | 2114.146 | 703.920 | 96.844 | 5 | 1 | 10 | 14718 |
| unchanged-seal | base | 2492.518 | 674.451 | 1058.004 | 7 | 2 | 8 | 104 |
| unchanged-seal | head | 2140.043 | 685.649 | 681.881 | 7 | 2 | 8 | 104 |

Four timelines use 33,521,664 allocated bytes versus 32,899,072 for one (1.018924×, limit 1.15×). Changing one MiB adds 1,150,976 allocated bytes. Six syscall cases confirm zero ignored-directory enumeration and zero ignored-content opens, including ordinary and packed sparse-index tracked exceptions.

Functional gates pass: Linux 268 tests and Windows 249 tests, with one existing ignored vector generator on each platform. Every Windows executable runs in one native request (final-native-all-01, 20 executables). Both release builds, fmt, both clippy gates and ARM64 check pass. Native Windows-to-Linux fixture return, old v0.1.0/v0.2.0 writers and 53 local relay vectors pass.

Unmet measured bounds: G3. These failed wall-time bounds are retained as failures. The shared host has variable filesystem waits; CPU medians and syscall diagnostics are supporting evidence, not substitutes for the required wall-time limits. No durability sync, assertion, ceiling or sample is removed to obtain a pass.

## Default-on F4: ordered capture and retained sessions (2026-10-23)

Base is `618bbcb`; head is the edit-only F4 handoff. Linux/ext4 on the shared eight-core host, release builds with the pinned Rust toolchain. `measure.py` in the F4 lane directory runs seven transport pairs and five base/head pairs per existing scenario in strict A B A B order, followed by a fixed five additional pairs for each scenario whose first-five head/base median exceeds 1.10× (the rule was declared before the full matrix completed). Every raw row in `measurements.jsonl` records before/after `/proc/pressure/io`, load, and artifact hashes. `measurement-source.json` binds the binaries to source digests. Fixture setup is excluded from timed operations; subprocess RSS includes setup. All samples are retained.

The host was loaded: I/O `some avg10` ranged from 13.67% to 69.80%. The Linux workspace suite also ran during part of the measurement interval. These are loaded-host observations; quiet-machine acceptance remains unproven.

Linux head SHA-256: `9d465d8af37054a668a0bb2e79bf1ad257c5adfcae547c136c8528422c746c05`. Benchmark SHA-256: `8da8d5c6368fc65909b3159d9e051ab3864dc4b0d16cebb4934f13352190fa52`.

### Transport and ignore comparison

The transport fixture has 10,000 unique files across ten directories, a real Git repository, an aged initial stat cache, five warmups, and one rewritten file per request. Spawned CLI samples connect to the persistent daemon via `FURROW_SOCKET`. Second-timeline samples include attachment and the first full capture of the same unchanged folder. The original transport phase retained seven head-only second-timeline observations; a separate five-pair A/B phase closes that comparison gap and is shown separately below.

| Operation | Base p50 / p95 ms | Head p50 / p95 ms | Head range ms |
|---|---:|---:|---:|
| Daemon delta, no gitignore | 261.555 / 408.733 | 3.914 / 7.950 | 3.644–7.950 |
| Daemon delta, gitignore | 165.393 / 2408.649 | 3.634 / 3.707 | 3.540–3.707 |
| Spawned CLI delta | 285.211 / 560.426 | 5.258 / 9.263 | 4.798–9.263 |
| Second timeline, original head-only phase | — | 1253.188 / 3039.216 | 555.293–3039.216 |
| Second timeline, five matched A/B pairs | 543.812 / 672.213 | 139.854 / 190.440 | 137.508–190.440 |

The ignore/no-ignore median ratio is 0.929× (P5 limit 1.1×). Warm daemon and spawned-CLI numeric limits pass in this run. The matched second-timeline phase meets the <=300 ms numeric limit for all five head samples. The original head-only phase under heavier load exceeded it; both phases are retained. Quiet-machine acceptance is not claimed.

### Existing scenarios

10,000 files, 100 changed files for the existing delta scenario, 128 MiB chunk stream, 32 MiB warm cache, 721 historical snapshots, 1,000 lookups and five universes. Existing scenarios retain synchronous durability. The comparison below uses medians; <=1.10× is the specified regression limit.

| Scenario | N per binary | Base p50 / p95 ms | Head p50 / p95 ms | Head/base p50 | Base / head p50 CPU ms |
|---|---:|---:|---:|---:|---:|
| chunk | 5 | 225.966 / 228.943 | 224.681 / 231.164 | 0.994× | 225.961 / 224.682 |
| tree-diff | 5 | 20657.844 / 21001.158 | 21093.929 / 21231.769 | 1.021× | 20654.616 / 21093.546 |
| refs | 5 | 117.331 / 118.339 | 117.366 / 120.946 | 1.000× | 117.310 / 117.362 |
| cold-seal | 10 | 2582.941 / 6743.858 | 2286.697 / 6374.228 | 0.885× | 748.224 / 702.050 |
| delta-seal | 5 | 151.591 / 852.022 | 110.329 / 278.221 | 0.728× | 10.943 / 12.578 |
| unchanged-seal | 5 | 782.947 / 1089.723 | 476.536 / 744.997 | 0.609× | 144.823 / 132.826 |
| fork | 5 | 7298.878 / 15344.312 | 6931.215 / 13341.313 | 0.950× | 2320.901 / 2197.445 |
| universes | 10 | 19134.323 / 57735.127 | 24605.836 / 50566.322 | 1.286× | 9394.855 / 9240.962 |
| radar | 10 | 574.709 / 875.516 | 690.499 / 1076.744 | 1.201× | 289.749 / 275.466 |
| gc | 5 | 88.456 / 97.246 | 69.618 / 93.436 | 0.787× | 42.036 / 39.721 |

| Scenario | Base wall range ms | Head wall range ms |
|---|---:|---:|
| chunk | 225.260–228.943 | 222.927–231.164 |
| tree-diff | 20558.195–21001.158 | 20818.066–21231.769 |
| refs | 112.790–118.339 | 111.516–120.946 |
| cold-seal | 1772.760–6743.858 | 1542.737–6374.228 |
| delta-seal | 104.381–852.022 | 62.006–278.221 |
| unchanged-seal | 384.505–1089.723 | 266.579–744.997 |
| fork | 3177.775–15344.312 | 3401.509–13341.313 |
| universes | 14283.330–57735.127 | 16623.265–50566.322 |
| radar | 400.540–875.516 | 348.351–1076.744 |
| gc | 61.552–97.246 | 65.694–93.436 |

The +10% comparison is not established: `universes` 1.286×, `radar` 1.201×. Contention is recorded per sample; it is not used to discard observations or declare this gate green.

### New engine scenarios

`capture-delta` times one written capture; `capture-plan-delta` includes planning and capture. Both move explicit durability outside the timer. `second-timeline` includes attaching the second timeline and its first unchanged capture. Fixture aging prevents racy-stat rehashes.

| Scenario | N | p50 / p95 wall ms | p50 CPU ms | Wall range ms |
|---|---:|---:|---:|---:|
| capture-delta | 5 | 6.419 / 6.558 | 3.676 | 6.064–6.558 |
| capture-plan-delta | 5 | 7.361 / 59.381 | 4.490 | 7.323–59.381 |
| second-timeline | 5 | 1107.882 / 1418.582 | 132.078 | 887.315–1418.582 |

### Retained store resources

Five already-warm requests per binary were traced separately on a 50-file fixture. Flush-thread pack opens are counted separately from capture reader/append opens; explicit or periodic durability still needs storage handles. Timing includes strace overhead.

| Version | Requests | Total ms | Catalog opens | Path-index opens | Pack read / append opens | Flush pack opens | fsync calls |
|---|---:|---:|---:|---:|---:|---:|---:|
| A | 5 | 283.385 | 0 | 10 | 10 / 20 | 0 | 10 |
| B | 5 | 87.810 | 0 | 0 | 0 / 0 | 0 | 0 |

### Original five pairs for extended scenarios

Every original and extension observation remains in the pooled table above. There is one fixed extension, with no further sampling based on its outcome.

| Scenario | Initial base p50 ms | Initial head p50 ms | Initial head/base |
|---|---:|---:|---:|
| cold-seal | 3257.310 | 3676.479 | 1.129× |
| universes | 18495.929 | 22544.204 | 1.219× |
| radar | 575.855 | 726.226 | 1.261× |

The lane also retains `trace_sessions.py` and its raw syscall traces for the before/after store-open proof, crash-injection and torn-record test logs, native Windows summaries, and compatibility runs using the actual v0.1.0 and earlier v0.2.0 writers. See the lane NOTES for exact commands and gate outcomes.
