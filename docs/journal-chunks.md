# Journal chunking change notes

The journal profile is built but **off by default**. Only
`FURROW_JOURNAL_CHUNKS=1` enables it; an unset variable or any other value keeps
0.3.6 chunking for every file, including journals. Capture and onboarding
estimation share this switch. When enabled, they select journals solely by filename: a
case-insensitive `.jsonl` or `.ndjson` suffix. Parent directory names, contents,
file size, local history, and caches do not select the profile. For example,
`transcript.JSONL` and `.cell/worker.ndjson` qualify; `report.json` and
`transcript.jsonl.bak` do not.

Journal chunks have a 32 KiB minimum, a 64 KiB gear hash target, and a 128 KiB
maximum. After the minimum, either a gear hash boundary or a newline closes the
chunk. EOF closes the remaining bytes. The target is a hash parameter, not a
claim that the mean chunk is exactly 64 KiB: newlines usually close chunks
earlier. Closing complete records reduces the tail exported on the next save
without creating a chunk for every short line. Records larger than the maximum
are split even without a newline. No JSON parsing is required.

Other files retain the 0.3.6 gear table, boundary rules, and 64/256/1024 KiB
minimum/target/maximum. `ChunkStream::new` retains that default, including for
the separate local zstd archive. `estimate` uses the same path selector and
stream constructor as snapshot capture.

## Measured profile choice

These measurements use the unchanged 0.3.6 release build from `ed7dfc2` as the
baseline. Each run appends the same 50 synthetic JSONL messages of about
5–40 KiB, totaling 1,182,178 bytes (23,643.56 bytes/save). The seeded run begins
with 8,388,877 bytes of the same kind of messages and publishes that initial
snapshot before the counted saves. All newly exported frame bytes count,
including sealed chunks, the complete Blob chunk list, trees, snapshots,
encryption overhead, and frame headers. Each export is marked published before
the next save.

| Profile, min/target/max KiB | Empty mean bytes/save | Empty amplification | Seeded mean bytes/save | Seeded amplification |
| --- | ---: | ---: | ---: | ---: |
| 0.3.6: 64/256/1024 | 241,670.34 | 10.2214× | 238,948.78 | 10.1063× |
| Gear only: 4/16/64 | 40,564.42 | 1.7157× | 101,077.84 | 4.2751× |
| Gear only: 8/32/128 | 55,016.02 | 2.3269× | 81,506.18 | 3.4473× |
| Gear only: 16/64/256 | 89,918.58 | 3.8031× | 84,822.98 | 3.5876× |
| **Gear + newline: 32/64/128** | **36,138.86** | **1.5285×** | **59,518.82** | **2.5173×** |

The chosen profile is the only measured candidate below 3× in both runs.
Small size-only profiles reduce the tail but re-export too many Blob references
for a mature journal. Newline boundaries reduce repeated tail bytes while the
32 KiB minimum limits the chunk list. These are measurements of this workload,
not a universal amplification bound for arbitrary data or larger journals;
the Blob still lists every chunk and grows with the file.

## Compatibility and two-step rollout

Blob, ChunkRef, snapshot, and exchange wire formats retain their existing
versions. Every new journal chunk is at most 128 KiB, below the existing 1 MiB
reader limit. Restored files are verified against the expected Blob's recorded
chunk lengths, chunk IDs and total length without reading stored chunk objects.
With the switch off, capture, export, sync, merge, fork and GC retain their
0.3.6 behavior. Profile-independent post-restore verification is the only
behavior change on that path.

Roll out in two steps:

1. Ship this release to every computer with the switch off. It teaches readers
   profile-independent verification. The cache namespace and capture generation
   remain unchanged, avoiding a forced full repository walk.
2. A later release or the embedding program can set `FURROW_JOURNAL_CHUNKS=1`
   once no computer older than this release remains. Original 0.3.5/0.3.6 readers
   can materialize the new chunks, but their post-restore verification still
   re-chunks with their own profile and can reject the correct bytes. Enabling
   the profile before those readers are upgraded is therefore unsupported.

An unchanged journal keeps its old Blob after the switch is enabled. The
ordinary stat cache handles settled files. When a journal must be read, capture
checks an equal-length cached Blob, or its baseline entry when no equal-length
cache candidate exists, against the working bytes. It checks the recorded
chunk lengths and IDs and restores any missing chunks from the verified
working bytes before reusing the Blob. Estimation applies the same layout
check and counts missing chunks without storing them. Changed bytes use the
selected profile; fresh captures of those bytes retain deterministic boundaries.
The separate local zstd archive continues to use its default chunker.

With the switch on, two computers that independently write identical new bytes
to the same journal using different profiles keep different Blob IDs. Sync and
merge continue to compare those IDs, so that representation difference can
appear as a change or divergence. This is rare when one computer holds a chat
at a time. A future release may add semantic comparison if this matters.

The original forced upgrade export measured 8,445,821 bytes for the
8,388,877-byte fixture. That forced re-chunking has been removed. The fix-round
measurements below replace that upgrade-cost guidance: upgrading an unchanged
journal exports only snapshot metadata, while the first append with the switch
on can export the full journal under the new profile, plus Blob/tree/snapshot
metadata. A history-retaining relay keeps the old frames too.

With the switch on, the fix round 2 J1 rerun measured **36,139.24 B/save
(1.5285×)** from empty and **59,517.54 B/save (2.5173×)** from the seeded
journal, confirming the chosen profile did not regress. The same unchanged
0.3.6 executable and fixtures were retained as the baseline above.

For the 8,388,877-byte journal published with 0.3.6, the first upgraded,
unchanged capture with the switch on exported **702 bytes: one snapshot
object and no journal objects** (170 header bytes + 532 sealed payload bytes).
The next unchanged capture exported 696 bytes, also only a snapshot. The first
22,554-byte append then exported **8,468,375 bytes in two frames / 179 objects**
(18,179 header bytes + 8,450,196 sealed payload bytes) for the now
8,411,431-byte journal. This is the deferred one-time conversion cost, about
8.08 MiB. Including that conversion, the 50 upgraded saves average 9.6226×;
J1's fresh-profile runs above exclude conversion of a legacy representation.
