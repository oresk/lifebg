# lifebg

Conway's Game of Life as a Wayland layer-shell wallpaper, written to cost the
compositor as little as possible.

The premise: an animated wallpaper costs the compositor roughly
`frames_per_second × damaged_area`, and an animated GIF forces both to their
worst case — 30–60 fps, whole screen, every frame. A Game of Life grid is
mostly unchanged from one generation to the next, so this runs a few generations
per *second* (or one every few seconds) and tells the compositor to repaint only
the cells that actually flipped.

![lifebg running on a 4K desktop](screenshot.png)

Shown: `lifebg --spawn shapes --interval 1` on a 3840×2160 desktop, one
generation per second.

## Build and run

```sh
cargo build --release
./target/release/lifebg --spawn shapes --interval 1
```

Nothing is installed and nothing is added to your session startup. Run it, and
`pkill -x lifebg` to stop it.

## Options

```
    -c, --cell N        grid cell size in logical pixels [12]
    -g, --gap N         gap between cells in pixels [1]
    -f, --fps F         generations per second [1]; see also --interval
        --interval S    seconds between generations [1]; the inverse of --fps,
                        mutually exclusive with it
    -d, --density F     initial live fraction 0..1 [0.28]
        --spawn MODE    what to drop on the grid: noise | shapes | mixed [mixed]
    -s, --seed N        PRNG seed [time]
        --base HEX      dead cell colour [0x0b0e13]
        --alive HEX     live cell colour [0x3d7f6e]
        --seed-interval N
                        drop fresh soup every N generations, 0 = never [24]
        --layer L       background | bottom [background]
    -o, --monitor NAME  only draw on this output name
        --list-outputs  print detected outputs and exit
        --list-patterns print the named patterns --spawn shapes can drop, and exit
        --outline N     empty margin in display pixels around the screen edges;
                        the grid starts inside it so cells stay whole [0]
        --damage MODE   how much to damage per frame: auto | full [auto]
        --stats         print tick rate and damage-rect counts every 2s
    -h, --help          print this help
    -V, --version       print the version and exit
```

## The mode that matters: `--spawn shapes`

`--spawn noise` fills the screen with uniform random cells. It evolves, but
after a generation or two it reads as tinted static, and — more importantly —
dense noise changes most of the grid every generation, so there is nothing to
clip the damage to and every frame becomes a full-surface damage anyway.

`--spawn shapes` starts on an empty grid and drops recognisable Life patterns:
gliders, LWSS, blinkers, toads, beacons, pulsars, and occasionally a Gosper
glider gun. This is both the better-looking mode and the only one where the
whole low-cost design actually pays off. `--list-patterns` prints the set.

## Measured damage per frame

Measured with `--stats`, which reports the damaged area as a fraction of the
surface. `--damage full` damages the entire surface every tick, so it is 100% by
definition and serves as the baseline. Same seed, 2560×1404:

| config | full-damage ticks | rects/tick | damaged | vs full |
|---|---|---|---|---|
| `--spawn shapes -c 14 -f 1` | 0/7 | 164 | 1.2% | **91×** |
| `--spawn shapes -c 20 -f 1` | 0/6 | 106 | 1.3% | 77× |
| `--spawn shapes -c 12 -f 4` | 0/24 | 248 | 1.2% | 91× |
| `--spawn mixed -c 14 -f 1` | 2/7 | 1550 | 59.7% | 1.7× |
| `--spawn noise -c 14 -f 1` | 1/6 | 1962 | 40.9% | 2.4× |

This is damaged *area*, which is the right proxy for compositor repaint cost and
the number that can actually be measured reliably. It is **not** an end-to-end
GPU utilisation figure — that has never been measured, and `TODO.md` explains
what a valid measurement would require and why it could not be done on a normal
desktop.

## How the damage decision works

`DamageBuilder` merges changed cells into rectangles: contiguous cells in a row
collapse into a run, and a run in the next row with identical geometry extends
the rectangle vertically. It then hands the list to the compositor, unless:

- the merged area reaches `DAMAGE_MAX_PCT` (55%) of the surface, or
- the list is longer than one rect per `DAMAGE_MAX_RECTS_DIVISOR` (16) cells.

Both bounds are **relative**. An absolute rect-count limit means something
different at every resolution and cell size; the original default of 256
silently forced near-constant full-surface damage, which is how a working
optimisation ended up looking useless.

`--damage full` skips the merging entirely, for A/B comparison.

## Notes for whoever edits this next

**Fractional scaling.** On a fractional-scaled output, Hyprland does not accept
a layer surface sized with an integer `wl_surface.buffer_scale`. The buffer must
be `logical_size × fractional_scale`, the logical size must be declared through
`wp_viewport`, and a `wl_shm` buffer whose `size` is not page-aligned is
rejected. Get any of this wrong and the compositor kills the client with a bare
`EPIPE` and no readable protocol error. This is documented at the top of
`src/app.rs` because it cost a whole debugging session.

**Damage coordinates are logical, not buffer coordinates.** The viewport does the
scaling. Handing it buffer-space rectangles makes the compositor repaint the
wrong regions and leave stale cells behind.

**Hotplug.** `sync_monitors` runs every loop iteration: new outputs get a
surface, and surfaces whose output disappeared are dropped. One output going
away does not take the others down.

## Layout

| file | |
|---|---|
| `src/main.rs` | CLI, event loop, stats |
| `src/app.rs` | Wayland: layer shell, shm buffers, damage, viewport/fractional scale |
| `src/sim.rs` | Game of Life, pattern seeding (`#[cfg(test)]` covers the patterns) |
| `src/paint.rs` | palette and damage-rect merging (`#[cfg(test)]` covers the merging) |

`cargo test` runs 15 tests.
