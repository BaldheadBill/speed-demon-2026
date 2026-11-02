# Track A: Ingest — performance log

Team BaldheadBill, Rust, zero crates. Every number below is the SpeedDemon
board's displayed result at the recorded commit, except where marked
computed from K (points = 50 × log10(K), which reproduces the board's
displayed points exactly for the champion run). Board run IDs were not
recorded during the sprint, so none are invented here; commits are given
instead and are verifiable in git history.

## Approach (champion)

Zero-crate Rust. The input is memory-mapped on Unix (plain `Vec` read
fallback on non-Unix) instead of being read into a buffer. Parsing is
SWAR: newlines and field separators are found with word-at-a-time scans,
not per-byte loops. Groups are keyed by (UTC hour, endpoint exact bytes,
no normalisation) in a 1024-shard mutex table; small groups store their
latencies inline. Method is ignored; error = 5xx only (499 is not an
error); error_ppm = floor(errors × 1,000,000 / count); percentiles are
nearest-rank, rank `(p*n+99)//100`, duplicates kept. Output is
`summary.txt`, sorted by hour ascending then endpoint unsigned-byte
ascending. Thread count comes from a `THREADS` environment variable.

## Runs (board, Falkenstein scoring box)

| When (EDT, Oct 30, 2026) | Version | Board time | Board memory | Lines | K | Points |
| --- | --- | --- | --- | --- | --- | --- |
| reference | Reference, unchanged | 32.05 s | 228 MB | — | — | 0 |
| ~13:10 | Parallel, whole file read into memory | 0.319 s | 309 MB | 177 | 7.941 | 44.994 |
| ~13:30 | Streaming ranges, per-thread buffered line reader | 0.259 s | 73 MB | 204 | 11.519 | 53.071 |
| evening | Merged final: 1024-shard mutex table, inline small-group latency storage, SWAR parse, hour-mixed shard index | 0.181 s | 67 MB | 468 | 12.350 | 54.583 (computed) |
| evening | Hot-cache variant of merged final | 0.170 s | 68 MB | 539 | 12.427 | 54.719 (computed) |
| evening | **Champion: mmap variant** — merged final + mmap input + env vars, no hot cache, no hour-mixed shard index | 0.177 s | 64 MB | 503 | 12.477476 | 54.806 |

## What each version changed, and why

- **Parallel:** proved the pipeline end to end — Dockerfile, base images,
  and I/O paths all correct on the scoring box.
- **Streaming ranges:** stopped holding the whole 239 MB input in memory.
  Memory 309 MB → 73 MB, the biggest single K gain so far.
- **Merged final:** sharded table removed lock contention, inline
  small-group storage cut allocation, SWAR parsing cut parse cost.
- **Hot-cache variant:** beat merged final slightly, but the gain was
  within box variance and it added lines.
- **Mmap champion (kept):** mmap pages did not blow up the board memory
  number (64 MB vs 68 MB hot-cache), settling the memory question, and
  it beat the hot-cache champion on K with fewer lines.

A DEAD-Q/W53-style compression stage was evaluated and rejected: the
output must be bit-exact percentiles, latencies are only ~12 MB inside
67 MB, time is the binding axis, and compression adds CPU.

## Correctness

Byte-exact against the board's expected summary on every version.
Expected `summary.txt` SHA-256:
`495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3`.

## Reproduction

From inside `track-a/`:

```powershell
docker build -t speeddemon-a .
docker run --rm -v <data-dir>:/data:ro -v <out-dir>:/out speeddemon-a
```

Local Docker times are for iteration only and are not comparable to
board times; board numbers count.
