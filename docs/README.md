# furrow Documentation

Start with the repository [README](../README.md) for installation, daily use,
and two-machine sync.

## Performance

[Performance](performance.md) — reproducible benchmark methodology, current
measurements, and regression gates.

[Journal chunking](journal-chunks.md) — filename selection, append amplification,
profile comparisons, compatibility, and the two-step rollout.

## Exchange

[Reading one file of a relayed snapshot](path-read.md) — `want --path
[--range]`, `peek` and `read`: why reading a remote chat fetches only its
transcript's spine and chunks, never the project folder.

## Repository Notes

- UI dependency notices remain at
  [furrow-core/ui/THIRD_PARTY_NOTICES.md](../furrow-core/ui/THIRD_PARTY_NOTICES.md) beside the embedded
  assets they describe.
- Runnable workflows live in [`demo/`](../demo/); benchmark code lives in
  [`furrow-core/benches/`](../furrow-core/benches/).
