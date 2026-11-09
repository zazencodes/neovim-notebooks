# Recovery performance

Run the ignored component profile in a release build:

```sh
CARGO_BUILD_JOBS=2 cargo test --release -p nbv-core --lib profile_large_output_save_and_recovery -- --ignored --nocapture
```

Set `CARGO_TARGET_DIR` as required by your checkout. The profile creates an isolated temporary
notebook and recovery directory, then removes them. It uses 1,000 code cells, roughly ten
source lines per ordinary cell, and one synthetic 8 MiB base64-like image output. It measures
initial capture/persistence, twenty source-only recovery updates, ten full notebook encodes,
and ten normal commits. Assertions check that source-only updates share the same immutable
output object and leave its persisted blob untouched. Timings have no pass/fail threshold.

A local macOS release run on 2026-11-09, during other project builds, produced:

| Component | Samples | Median | p95 |
|---|---:|---:|---:|
| First capture on the editor thread | 1 | 1.38 ms | — |
| First recovery encode/hash/fsync on the worker | 1 | 52.08 ms | — |
| Source-only capture on the editor thread | 20 | 1.84 ms | 17.11 ms |
| Source-only recovery encode/fsync on the worker | 20 | 10.42 ms | 37.67 ms |
| Full notebook JSON encode (`to_bytes`) | 10 | 4.76 ms | 91.14 ms |
| Full notebook commit, including disk read/hash, encode/hash and fsync | 10 | 74.13 ms | 144.97 ms |

These measurements separate component work; they do not measure end-to-end typing or claim
an end-to-end CPU improvement. Concurrent builds and filesystem contention affected the tails.
The image output was not cloned, encoded or rewritten during the twenty source-only updates.
Empty output arrays remain inline, avoiding a separate blob write/fsync for each empty cell.

Capture still runs synchronously on the editor thread, at most once per timer tick when
changes need a snapshot and the previous write has finished. It copies sources and small cell
fields, compares cached output values, and initially clones new or replaced output data.
Its cost grows with notebook size; large attachments and other cell metadata are also copied.
JSON encoding, content hashing, atomic replacement, fsync and output-blob collection run on
a blocking worker. Only one write can be in flight; further edits coalesce until the next
snapshot, and orderly unexpected shutdown drains the latest revision.

Normal `.ipynb` saves retain the existing synchronous commit and buffer-repair protocol.
Neovim still runs `BufWritePre` on modified cells, sends all cell buffers' text in
the commit request, and runs `BufWritePost` after success. Removing that repair payload would
change the existing attachment/resynchronization contract; recovery does not rely on its
removal. The table excludes RPC transfer, Neovim buffer reads and arbitrary plugin hooks, so
actual saves can take longer. Recovery snapshots neither request those buffers nor run hooks.
