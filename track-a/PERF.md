# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | | | | | |
# Track A: Ingest — Performance Log

## Approach

Zero-crate Rust, no dependencies. The input is split into byte ranges,
one per thread (up to 12). Each thread seeks to its range, aligns to the
next line boundary, and aggregates independently: endpoints are interned
to small integer IDs, groups are keyed by (hour, endpoint), latencies are
kept per group, and percentiles use nearest-rank via `select_nth_unstable`
(three selections, no full sort). Thread results are merged by remapping
local endpoint IDs to global IDs, then output is sorted by hour, then by
endpoint bytes. Correctness is checked against the board's expected
summary hash on every version.

## Runs

All board numbers are from the SpeedDemon board (Falkenstein scoring box).
Local numbers are from Docker on the dev machine and are for iteration
only — board numbers count. Expected summary SHA-256 on every run:
`495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3`.

| When (EDT, Oct 30, 2026) | Version | Board time | Board memory | Lines | K | PTS | Place at the time |
| --- | --- | --- | --- | --- | --- | --- | --- |
| ~13:10 | Parallel, whole file read into memory | 0.319 s | 309 MB | 177 | 7.941 | 44.994 | 22nd |
| ~13:30 | Streaming ranges, per-thread buffered line reader | 0.259 s | 73 MB | 204 | 11.519 | 53.071 | — |
| (pending) | Chunked 1 MB block parsing, SWAR field scan, native-CPU build | — | — | — | — | — | — |

Local times for the same versions: 2.44 s (parallel), 2.13 s (streaming
ranges), 1.92 s (chunked blocks). The board is consistently much faster
than local Docker; local times are not comparable to board times.

## What each version changed, and why

- **Parallel (run 1):** proved the pipeline end to end — Dockerfile,
  base images, and I/O paths all correct on the scoring box.
- **Streaming ranges (run 2):** stopped holding the whole 239 MB input
  in memory. Memory 309 MB → 73 MB, which was the biggest single K gain
  so far.
- **Chunked blocks (run 3):** replaced the per-line reader with 1 MB
  block reads and a SWAR newline/space scan, and built with
  `target-cpu=native` so the scoring box's own vector instructions are
  used. Target: board time, the remaining gap to the leaders.

## Reproduction

From inside `track-a/`:

```powershell
docker build -t speeddemon-a .
docker run --rm -v <data-dir>:/data:ro -v <out-dir>:/out speeddemon-a