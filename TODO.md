# TODO

## Measure end-to-end GPU cost (not yet done)

Everything this program claims to save is currently backed only by **damaged
area**, which is a proxy. The proxy is measured, but nobody has ever confirmed
that less damaged area turns into less GPU work on real hardware.

**Why it was never measured.** Every attempt on a normal desktop produced
nonsense: static wallpaper reading 7% then 68%, 16 gen/s cheaper than 4 gen/s,
a config measuring `-5%` against a baseline it should have been above. The
reason is that `gpu_busy_percent` and `radeontop` report **whole-GPU**
utilisation, which on a working desktop is dominated by:

- the compositor re-compositing transparent windows over the wallpaper, so the
  cost of one wallpaper frame depends on the window stack above it
- browsers, video playback, any GPU-accelerated app in the background
- whatever else happened to be animating during the sample window

Interleaving A/B/A/B against a static baseline cancels some of the drift and is
what produced the damage table, but it cannot cancel a browser decoding video
next door. The conclusion reached was that the number is not obtainable in a
normal session, and pretending otherwise produced a lot of confident nonsense.

**What a valid measurement needs.**

1. An empty workspace, no transparent or blurred windows over the wallpaper.
2. Nothing else touching the GPU — no browser, no video, no compositing of
   animated surfaces.
3. Long sample windows (30s+) and several alternating rounds, reporting the
   paired delta against a static baseline rather than absolute levels.
4. Compare at least: static image, `--damage full`, `--damage auto`, and an
   animated GIF via `awww` as the thing this is meant to beat.

**What is already in place to support it.**

- `--damage full` forces whole-surface damage, so the merging can be A/B'd
  directly.
- `--stats` prints damaged area as a percentage of the surface every 2s, which
  is deterministic and needs no GPU at all.
- `--seed N` makes runs identical, so two configurations see the same pattern.
- `--interval` / `--fps` let the frame rate be pinned exactly.

**Also worth recording if the measurement is ever done:** whether the relative
damage bounds in `src/app.rs` (`DAMAGE_MAX_PCT`, `DAMAGE_MAX_RECTS_DIVISOR`)
are actually optimal, or merely plausible. They were chosen from reasoning about
region-building cost, never tuned against hardware.

## Other open items

- Default `--spawn` is `mixed`, which measures as the worst of the three modes
  (59.7% damaged vs 1.2% for `shapes`). Should probably become `shapes`.
- One report of "pixels get broken after a couple of ticks" was never
  reproduced. Most likely a stale process holding an old binary across a
  rebuild, but it was never confirmed either way.
