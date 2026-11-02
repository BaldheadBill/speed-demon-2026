# Track D: Traverse — performance log

Team BaldheadBill, Rust, zero crates. Every board number below is the
SpeedDemon board's displayed result at the recorded commit. Board run IDs
were not recorded during the sprint, so none are invented here; commits
are given instead and are verifiable in git history.

## Approach (champion, Step 4)

Zero-crate Rust. The graph (5M edges, 1,740,512 nodes, weights uint32
including 0 and 4294967295, parallel edges, exact duplicates, self-loops,
isolated nodes) is loaded once into CSR form. Queries are answered with
bidirectional Dijkstra, run in parallel across queries through atomic
work-stealing; each thread keeps its own distance store with u16
generation stamps, so no clearing pass is needed between queries.
Thread count comes from a `THREADS` environment variable. Per-source
caching was evaluated and rejected: 1979 distinct sources over 2000
queries gives at most ~1% reuse. A* was rejected: the graph has no
coordinates. Output is `distances.txt`, `-1` where unreachable
(`s != t` can still be distance 0 via zero-weight paths).

## Runs (board, Falkenstein scoring box)

| When (EDT, Oct 30, 2026) | Version | Board time | Board memory | Lines | K | Points |
| --- | --- | --- | --- | --- | --- | --- |
| reference | Reference, unchanged | 72.69 s | 893 MB | — | — | 0 |
| evening | **Champion: Step 4, parallel queries** (first board score) | 2.117 s | 524 MB | 356 | 4.747 | 33.82 (computed) |
| Oct 30, re-run | Step 4, board re-run (same commit, box variance) | 2.062 s | 524 MB | 356 | 4.810890 | 34.111 |

A later variant (exact-size reads, freeing the graph text buffer before
the CSR build, no trailing newline, +7 lines) was hash-verified locally
(4.314 s vs 4.398 s local) but deliberately not pushed: the expected
gain was a fraction of a point on memory against a small lines cost, and
after the Track C v2 result this track became the dropped track, so
pushing it had no upside to the overall.

## Correctness

Byte-exact against the board's expected distances on every version.
Expected `distances.txt` SHA-256:
`a23dbdf0a3e49cc8901310a9ee1dc6a9a569a85297b6da75c2a7002cb2ed277e`.

## Reproduction

From inside `track-d/`:

```powershell
docker build -t speeddemon-d .
docker run --rm -v <data-dir>:/data:ro -v <out-dir>:/out speeddemon-d
```

Local Docker times are for iteration only and are not comparable to
board times; board numbers count.
