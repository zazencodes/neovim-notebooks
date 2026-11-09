# Process CPU and memory profiles

`scripts/profile-notebook.py` measures the running application alongside its embedded
Neovim, Python kernel and other observed descendants (including language servers).
It supplements component benchmarks with repeatable process CPU/RSS observations.
It uses Python's standard library, `ps`, tmux, Neovim and a Python with `ipykernel` and
`IPython`. No matplotlib or profiler package is required. Use a release binary.

```sh
python3 scripts/profile-notebook.py \
  --binary /absolute/path/to/target/release/nvb \
  --nvim /absolute/path/to/nvim \
  --python /absolute/path/to/python \
  --config clean --seconds 10 --output /tmp/nvb-clean.json
```

Repeat with `--config user --output /tmp/nvb-user.json` to keep your actual Neovim
configuration and plugins. Both output paths must be new `.json` files. Compare matching
binary hashes, durations and environments; repeat runs to see variability. There are no
pass/fail performance thresholds. User configuration runs can start their usual plugins,
LSPs, background services or write plugin state, just as opening a notebook normally does.

Each invocation creates its own temporary tmux socket, one application pane, synthetic
notebooks and a temporary kernelspec pointing at exactly `--python`. It never operates on
existing tmux servers, agent panes or user notebooks. Each workload starts a fresh app and
kernel. `clean` adds `--clean` and isolates all four XDG directories. `user` inherits XDG
paths and `NVIM_APPNAME`, retaining the real config and plugin environment. The harness
always uses a canonical temporary notebook directory and private Jupyter runtime/data search path.
Observed app descendants are stopped and temporary artifacts removed at completion/error.
The report preserves already collected samples if a workload fails.

Five workloads run in order, for `--seconds` each (minimum 2):

- **idle:** 121 code cells, kernel ready, no input.
- **scroll:** the same notebook, continuous `ctrl-e`/`ctrl-y` line scrolling at up to 10
  actions per second, reversing every two seconds.
- **stream:** a finite Python loop prints 50 flushed, approximately 100-byte lines per
  second. The bounded loop lasts slightly longer than the sampling window.
- **plots:** a finite loop replaces one display ID with a new 320×180 RGB PNG about ten
  times per second. Python generates and compresses the PNG with `struct`/`zlib`, then
  `IPython.display` sends it. This measures display replacement without matplotlib.
- **edit-save:** insert lowercase comments and `:w` repeatedly while preserving an
  approximately 5 MB saved stream output. Saves wait for file-state changes; the final
  notebook is checked for edited source and the unchanged output hash.

Startup and state transitions use bounded predicates: visible NAV/EDIT/kernel-idle state,
observed kernel process, kernel-created start/completion markers and saved-file changes.
Polling delays pace observations; no fixed delay is treated as proof of readiness.

The JSON includes raw cumulative `ps` CPU/RSS samples with PID/start-time identity,
command lines, actual timestamps, sampling-command duration, inputs, versions and binary
SHA-256. Each workload summarizes CPU deltas and maximum sampled RSS per process and per
role. Role RSS peaks sum simultaneously observed processes; they are not sums of each
process's independent peak. CPU percentage is `100 × cumulative CPU delta / measured wall
seconds`, where one logical core is 100%. Other descendants remain individually listed.

`ps` CPU-time precision varies by OS; short windows are noisy. Startup, unsampled exit tails
and processes living entirely between samples are excluded. RSS includes shared resident
pages and is neither unique memory nor allocated bytes. Sampling targets 0.5-second
intervals; actual intervals include `ps` and input overhead. Harness/tmux-server CPU is
excluded, but host contention and instrumentation still affect the app. Detached tmux uses
a fixed 140×40 `xterm-256color` pane, so these are not real-terminal graphics measurements.
Kernel readiness gates the window; asynchronous plugin/LSP initialization may overlap it.
Processes launched through an existing external service may not descend from the app and
cannot be attributed or cleaned by this tool. Keep raw samples when reporting comparisons.
