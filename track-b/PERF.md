# Track B: Index — performance log

Team BaldheadBill, Rust, zero crates. Every board number below is the
SpeedDemon board's displayed result at the recorded commit. Board run IDs
were not recorded during the sprint, so none are invented here; commits
are given instead and are verifiable in git history.

## Approach (champion, solver v2)

Zero-crate Rust. Only terms that appear in the queries are indexed
(wanted-terms-only): the doc scan builds postings just for query terms,
which is what keeps memory near the 501 MB board figure instead of the
reference's 2.29 GB. The scan is parallel over line-aligned doc ranges
with timestamp dedup, BOOL chunks run in parallel, and PHRASE queries get
a parallel second scan. Query terms are lowercased at intern time.
Tokenisation is byte-based ASCII only: tokens are maximal `[a-z0-9]+`
runs, every byte ≥ 0x80 is a separator, no stemming, no stopwords, no
Unicode folding (`007` ≠ `7`). BOOL is left-associative AND/OR/NOT;
PHRASE matches exact token sequences. Output is `results.txt`: per query,
in query order, count, sum of matching ids (u64), and the ten smallest
ids ascending (`0 0` for no match). The parser strips the `BOOL ` /
`PHRASE ` type prefix on each query line and keys its duplicate cache on
the full raw line.

## Runs (board, Falkenstein scoring box)

| When (EDT, Oct 30, 2026) | Version | Board time | Board memory | Lines | K | Points |
| --- | --- | --- | --- | --- | --- | --- |
| reference | Reference, unchanged | 56.35 s | 2,200 MB | — | — | 0 |
| — | Solver v1, single-threaded (local only, never board-scored) | 8.33 s local | — | 666 | — | — |
| evening | **Champion: solver v2, parallel** (first board score) | 1.415 s | 503 MB | 741 | 6.190689 | 39.587 |
| Nov 2 | Solver v2, board re-run (same commit, box variance) | 1.409 s | 501 MB | 741 | 6.206545 | 39.642 |

Solver v1's local run: scan 5.89 s, BOOL 0.59 s, PHRASE 1.67 s,
22,240,530 postings. The v2 changes that mattered were scan parallelism
and lowercasing query terms at intern. A later memory-focused variant
(exact-size reads, Bloom pre-filter, CSR phrase assignments) was
hash-verified locally but deliberately not pushed: its memory gain was
about cancelled by added lines, and pushing risked the banked score.

## Correctness

Byte-exact against the board's expected results on every version.
Expected `results.txt` SHA-256:
`f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927`.

## Reproduction

From inside `track-b/`:

```powershell
docker build -t speeddemon-b .
docker run --rm -v <data-dir>:/data:ro -v <out-dir>:/out speeddemon-b
```

Local Docker times are for iteration only and are not comparable to
board times; board numbers count.
