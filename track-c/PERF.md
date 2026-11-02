# Track C: Join — performance log

Team BaldheadBill, Rust, zero crates. Every board number below is the
SpeedDemon board's displayed result at the recorded commit. Board run IDs
were not recorded during the sprint, so none are invented here; commits
are given instead and are verifiable in git history.

## Approach (champion, solver v2)

Zero-crate Rust. A single-threaded byte state machine in v1 became a
parallel scan in v2: the input is split into chunks at record boundaries
found by quote-parity, each thread runs the same byte state machine over
its chunk, names are interned through a 64-way sharded table, and
per-thread partial groups are merged at the end. The core parser is
unchanged between v1 and v2 (`scan_field`, delimiter handling,
integer-cents parsing, money formatting). Join is a direct array lookup
on `region_id`; money is integer cents throughout (printed with exactly
two decimals, `0.00` never `-0.00`); `name`/`note` distinct counts and
byte counts use exact bytes after unescaping. Parsing follows the spec
exactly: BOM skipped only at file start or immediately after a record
terminator, quoted fields with `""` → one `"`, commas/LF/CR inside quotes
literal, unquoted fields run to `,`/CR/LF with no trimming, empty
unquoted and empty quoted (`""`) are the same empty value.

## Runs (board, Falkenstein scoring box)

| When (EDT) | Version | Board time | Board memory | Lines | K | Points |
| --- | --- | --- | --- | --- | --- | --- |
| reference | Reference, unchanged | 14.44 s | 367 MB | — | — | 0 |
| Oct 30, ~22:15 | Solver v1, single-threaded | 1.272 s | 244 MB | 445 | 2.957 | 23.54 (computed) |
| Nov 2 | **Champion: solver v2, parallel** | 0.377 s | 275 MB | 634 | 5.031920 | 35.087 |

The v2 parallelisation was worth 2.2× locally (5.445 s → 2.461 s) and
3.4× on the board. The extra memory (244 MB → 275 MB) is the per-thread
partial groups; the time axis dominates, so the trade wins.

## Correctness

Byte-exact against the board's expected aggregates on every version.
Expected `aggregates.txt` SHA-256:
`d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536`.

## Reproduction

From inside `track-c/`:

```powershell
docker build -t speeddemon-c .
docker run --rm -v <data-dir>:/data:ro -v <out-dir>:/out speeddemon-c
```

Local Docker times are for iteration only and are not comparable to
board times; board numbers count.
