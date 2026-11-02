# Speed Demon 2026 — team BaldheadBill

Rust, zero crates, all four tracks of [Speed Demon 2026](https://speeddemon.dev).
Written during the 72-hour window (Oct 30 – Nov 2, 2026). Final board
result: **42.401 points overall**, from the best three tracks.

## Final board numbers (Nov 2, 2026)

| Track | K | Board time | Board memory | Lines | Points |
| --- | --- | --- | --- | --- | --- |
| A: Ingest | 12.477476 | 0.177 s | 64 MB | 503 | 54.806 |
| B: Index | 6.206545 | 1.409 s | 501 MB | 741 | 39.642 |
| C: Join | 5.031920 | 0.377 s | 275 MB | 634 | 35.087 |
| D: Traverse | 4.810890 | 2.062 s | 524 MB | 356 | 34.111 |

Best three (A, B, C) give the 42.401 overall; D is the dropped track.
Board ranks at the same check: overall 103, A 111, B 95, C 102, D 107 —
the board filled up over the sprint weekend, so ranks moved a long way
from the Oct 30 positions.

## Approaches

- **A: Ingest** — mmap input (Vec fallback off Unix), SWAR field parsing,
  1024-shard mutex group table keyed by (UTC hour, endpoint exact
  bytes), inline small-group latency storage. Bit-exact nearest-rank
  percentiles; error = 5xx only. See `track-a/PERF.md`.
- **B: Index** — wanted-terms-only index (only query terms get
  postings), parallel line-aligned doc scan with timestamp dedup,
  parallel BOOL chunks and parallel phrase second scan. ASCII-only
  byte tokenisation. See `track-b/PERF.md`.
- **C: Join** — parallel hostile-CSV scan split by quote-parity, same
  byte state machine per chunk, 64-way sharded name interning,
  per-thread partial groups merged, direct-array join on `region_id`,
  integer-cents money. See `track-c/PERF.md`.
- **D: Traverse** — CSR graph loaded once, bidirectional Dijkstra per
  query, parallel across queries via atomic work-stealing, per-thread
  distance stores with u16 generation stamps. See `track-d/PERF.md`.

Every track's output is byte-exact against the board's expected hash;
see each `PERF.md` for the hashes. No track uses any crate
(`dependency_count 0` in every `.speed-demon.toml`), and no code is
shared between track folders except by copying it in.

## What is in a track folder

| File | What it is |
| --- | --- |
| `Dockerfile` | Two-stage offline build from an allowlisted image pinned by digest |
| `Cargo.toml` | Package `track-x`, edition 2021, empty `[dependencies]` |
| `src/main.rs` | The whole solver, standard library only |
| `.speed-demon.toml` | Track, team BaldheadBill, language Rust, dependency count 0 |
| `PERF.md` | Optimisation log: approach, board runs, what each version changed |

## How your program runs

- It reads its inputs from `/data`, which is read-only, and writes
  exactly one file to `/out`, then exits 0. The file is `summary.txt`
  for A, `results.txt` for B, `aggregates.txt` for C and
  `distances.txt` for D.
- `TRACK` is set to `a`, `b`, `c` or `d`.
- There is no network, at build time or at run time.
- It runs as an unprivileged user on a read-only filesystem.
- It gets 12 hardware threads, 8 GB of memory with no swap.
- A run that fails correctness, the timeout or the sandbox scores
  nothing.

## Build and run one track

From inside a track folder:

```powershell
docker build -t speeddemon-x .
docker run --rm -v <data-dir>:/data:ro -v <out-dir>:/out speeddemon-x
```

Local Docker times are for iteration only and are not comparable to
board times; board numbers count.

## Licence

MIT (kept from the starter). This repo was private during the sprint
and made public at code freeze, as the rules require.
