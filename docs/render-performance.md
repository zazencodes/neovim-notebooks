# Rendering and image-cache checks

Run the release scenarios from the worktree, with an isolated Cargo target directory:

```sh
CARGO_TARGET_DIR="$PWD/.agents/builds/render" CARGO_BUILD_JOBS=2 cargo test -p nbv-tui --release --lib scenario -- --ignored --nocapture --test-threads=1
```

The cached-navigation scenario uses the isolated Neovim harness; `.tools/nvim-macos-arm64`
(or `NBV_NVIM`) must be available. The other component scenarios need no terminal or kernel.
The real terminal-input flood regression lives in `crates/nbv-cli/tests/tmux.rs` and uses the
normal tmux/Neovim/ipykernel test prerequisites.

A November 9, 2026 release run on the development Mac measured:

| Scenario | Reference work | Revised work |
|---|---:|---:|
| 2,000 viewport queries across 100,000 giant cells | Full block scan: 101.85 ms | Binary-search visible range: 110.92 µs |
| 200 navigation turns across 20,000 cells | Rebuild current geometry: 296.32 ms | Cached pan/scroll/select/align: 18.25 µs |
| Layout measurements for 200 800×800 PNGs | Full raster decode: 87.78 ms | Header-only measurement: 5.09 ms |

These are component measurements from one run, with construction excluded where noted in
the tests. The navigation reference rebuilds the current geometry; it does not recreate an
older application binary. Other workers were active on the machine. These timings do not
measure end-to-end frame latency or terminal memory.

The repeated-plot scenario encoded/rendered 200 replacement 800×800 Kitty plots in 373.86 ms.
Its peak retained protocol byte charge was 15,335,424 bytes. It checked that each of the 200
owned image IDs received an explicit deletion command. A separate raster scenario retained
2,560,000 decoded bytes while replacing 200 plots. Deterministic tests also exercise LRU
pressure, shared images, oversized rasters, retained offscreen plots and unchanged oversized
visible plots. These are allocation-accounting and generated-command checks, not RSS or
pixel measurements on a physical terminal.

## Cache and terminal resource lifetimes

- Notebook JSON retains complete image data. Output views hold compressed image bytes and
  dimensions; they never retain decoded raster Arcs.
- Decoded rasters use a 32 MiB LRU budget. An oversized raster is decoded transiently to
  construct its visible protocol; an unchanged protocol avoids another decode on redraw.
- Terminal encodings use a conservative byte charge because upstream representations are
  opaque: 128 bytes per resized pixel plus 1,024 bytes per terminal cell. The offscreen LRU
  shares a 64 MiB allowance with active entries. Visible protocols remain pinned even when
  their charge exceeds that allowance. Their count and geometry are bounded by the viewport;
  oversized entries are evicted once they leave it. This prevents every-frame reuploads.
- Invalidating text/status views retains image ownership until their replacements register
  their hashes. Unchanged plots survive the rebuild. Replacing/clearing the last owner
  releases derived data; identical images shared by another cell remain live.
- Kitty protocols use owned image IDs. Eviction, output removal and normal shutdown send
  `a=d,d=I,i=<owned-id>,q=2`, wrapped in tmux passthrough when needed. Deletions of offscreen
  entries flush after drawing, so a static screen does not leave an eviction pending.
  Ordinary focus changes preserve the protocol's already-transmitted state.

[Kitty's deletion specification](https://sw.kovidgoyal.net/kitty/graphics-protocol/#deleting-images)
allows terminals to retain data referenced elsewhere, including scrollback. Actual terminal
RSS is therefore outside the client cache bound. A disconnected tmux client's old terminal
cannot receive cleanup commands from a newly attached client. Sixel and iTerm2 image output
provide no per-image resource deletion through the upstream API used here; their terminal
scrollback/resource retention remains terminal-managed. Halfblocks retain ordinary cells.
Physical-terminal clipping and memory checks remain manual, as described in the README.

## Integration hooks

`App::invalidate_layout` invalidates measured notebook geometry; use it for output
collapse/expand as well as source, output, measured-height, structure and width changes.
`invalidate_view` schedules a replacement while preserving image ownership until rebuild;
`discard_view` immediately retires output ownership for an explicit clear. `clear_views`
retires every view and derived image resource for reload/resize/structural resets.

`outputs::Block::Image` carries `data: Arc<[u8]>` (compressed bytes), rather than a decoded
`DynamicImage`. `Images::decode`, `touch` and `retain` own raster lifetime. The private
`image_cache::Protocols` module owns encoded data, LRU stamps and Kitty IDs. Shown/in-flight
layouts are `Arc<Layout>` so pure navigation shares geometry without copying every cell.
