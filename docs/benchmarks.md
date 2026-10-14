# WinSearch Benchmarks

Measured, not estimated. Every number below was produced by running the
benchmarks in this repository on the machine described and pasting the real
output. Re-run them with:

```sh
cargo run --release -p ws-index --example bench 50000     # index + search
cargo run --release -p ws-index --example bench 200000
target\release\winsearchd.exe --bench-crawl 20000         # crawl pipeline
```

- Date: 2026-10-13
- Machine: AMD64 Family 26 (Zen 5), 24 logical CPUs, Windows
- Build: `cargo build --release` (LTO thin, codegen-units 1)

## Search query latency (interactive path)

500 queries (10 realistic queries × 50 runs) against a synthetic but
realistically named index (report-2024-03-….pdf, meeting notes ….txt,
IMG_….png, mixed depths and extensions).

| Index size | p50 | p95 | p99 | max |
|---|---|---|---|---|
| 50,000 entries | **0.87 ms** | 3.8 ms | 4.0 ms | 4.7 ms |
| 200,000 entries | **3.15 ms** | 11.0 ms | 13.6 ms | 16.0 ms |

Per-source median at 200k entries (all index-driven; no table scans):

| Source | Median |
|---|---|
| name prefix (range seek on name index) | 0.04 ms |
| substring via trigram FTS index | 0.14 ms |
| word/token FTS match | 0.99 ms |

Earlier revisions of this pipeline were measured at p50 5.9 ms / p95 12.9 ms
(50k) and p50 26 ms for the prefix source alone (200k). The two fixes that
moved the numbers: dropping `ORDER BY rank` from the FTS candidate query
(5.7 ms → 0.3 ms median) and replacing a `LIKE '%…%'` table scan with a
trigram index plus a `>= / <` prefix range seek (2.3 ms → 0.006 ms median).

## Index write throughput

| Operation | 50k entries | 200k entries |
|---|---|---|
| Bulk insert (new entries) | ~50,000 entries/s | ~49,600 entries/s |
| Re-upsert, nothing changed (rescan) | ~241,000 entries/s | ~160,000 entries/s |
| Re-upsert, 10% modified | ~145,000 entries/s | ~70,000 entries/s |

Index size: 25.5 MiB at 50k entries, 101.5 MiB at 200k entries (~0.5 MiB per
1,000 entries — names + two FTS indexes + identity metadata).

## Crawl pipeline (scan → identity → upsert)

`winsearchd --bench-crawl` creates a real tree on disk (20,000 files in 100
directories) and runs the production path: directory enumeration, exclusion
policy, per-file identity lookup (`FILE_READ_ATTRIBUTES` open only — no
content reads, no cloud hydration), and batched index upserts.

| Phase | Result |
|---|---|
| Full crawl, 20,100 entries | 2.46 s (**8,140 files/s**) |
| Incremental rescan (all unchanged) | 1.55 s (**12,911 files/s**) |

Notes:

- This is a single-threaded measurement of the per-directory pipeline. The
  daemon runs 4 bounded crawl workers, so wall-clock throughput on a real
  tree scales with I/O concurrency (the queue is per-directory).
- ~640 bytes of index per file on disk.

## Envelope and interpretation

- Typing in the overlay debounces at 120 ms; a query costs well under a frame
  budget even at 200k entries, so the interactive path stays responsive while
  indexing runs in the background.
- Query cost is bounded by design: each candidate source is capped at 500
  rows, so worst-case latency does not depend on how many files match.
- The remaining p95 gap at 200k comes from two-token substring phrases (e.g.
  `presentation final`) with thousands of matches in the trigram index;
  measured at ~11 ms worst case.
- Idle cost: indexing threads block on the work queue (500 ms condvar wait)
  and watchers block in `ReadDirectoryChangesW`; the idle process does no
  polling.

## Targets (Phase 7 power/performance items)

- Idle CPU/GPU ≈ 0: already true by construction (blocking waits only); a
  resource-behavior test is still on the Phase 7 checklist.
- Power-aware indexing policies (AC / battery saver / quiet hours): Phase 7.
