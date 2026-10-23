//! lifebg — Conway's Game of Life as a Wayland layer-shell wallpaper.
//!
//! The whole reason this exists: an animated wallpaper costs the compositor
//! `frames_per_second x damaged_area`, and a GIF forces both to their worst
//! case (60 fps, whole screen). A Game of Life grid is mostly unchanged from
//! one generation to the next, so we can run it at a handful of generations per
//! second and hand Hyprland a short list of dirty rectangles instead.

mod app;
mod paint;
mod sim;

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use wayland_client::{Connection, EventQueue};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::Layer;

use crate::app::{App, Options};
use crate::paint::{Palette, Rect};
use crate::sim::{SimCfg, Spawn, PATTERNS};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: i32) {
    STOP.store(true, Ordering::SeqCst);
}

struct Config {
    cell: usize,
    gap: usize,
    outline: usize,
    full_damage: bool,
    fps: f64,
    density: f64,
    spawn: Spawn,
    seed: u64,
    base: u32,
    alive: u32,
    seed_interval: u64,
    layer: Layer,
    monitor: Option<String>,
    stats: bool,
    list_outputs: bool,
    list_patterns: bool,
    version: bool,
    help: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            cell: 12,
            gap: 1,
            outline: 0,
            full_damage: false,
            fps: 1.0,
            density: 0.28,
            spawn: Spawn::Mixed,
            seed: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1),
            base: 0xff0b_0e13,
            alive: 0xff3d_7f6e,
            seed_interval: 24,
            layer: Layer::Background,
            monitor: None,
            stats: false,
            list_outputs: false,
            list_patterns: false,
            version: false,
            help: false,
        }
    }
}

fn parse_hex(s: &str) -> Result<u32, String> {
    let s = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if s.len() != 6 && s.len() != 8 {
        return Err(format!("expected RRGGBB or AARRGGBB, got '{s}'"));
    }
    let v = u32::from_str_radix(s, 16).map_err(|e| format!("bad colour '{s}': {e}"))?;
    Ok(if s.len() == 6 { v | 0xff00_0000 } else { v })
}

struct Stats {
    on: bool,
    ticks: usize,
    full: usize,
    rects: usize,
    /// Damaged surface pixels accumulated over the window, and the surface area
    /// needed to express that as a percentage. This is the number that actually
    /// predicts compositor cost — rect count is only the region-building tax.
    area: u64,
    area_total: u64,
    since: Instant,
}

impl Stats {
    fn on(on: bool) -> Stats {
        Stats { on, ticks: 0, full: 0, rects: 0, area: 0, area_total: 0, since: Instant::now() }
    }

    fn tick(&mut self, rects: Option<&[Rect]>, surface: u64) {
        if !self.on {
            return;
        }
        self.ticks += 1;
        self.area_total += surface;
        match rects {
            Some(list) => {
                self.rects += list.len();
                self.area += list.iter().map(|r| r.w.max(0) as u64 * r.h.max(0) as u64).sum::<u64>();
            }
            None => {
                self.full += 1;
                self.area += surface;
            }
        }
        let secs = self.since.elapsed().as_secs_f64();
        if secs >= 2.0 {
            let partial = self.ticks - self.full;
            let pct = if self.area_total > 0 { 100.0 * self.area as f64 / self.area_total as f64 } else { 0.0 };
            eprintln!(
                "stats: {:.1} ticks/s, full-damage {}/{} ticks, avg rects/tick {:.0}, damaged {:.1}% of surface",
                self.ticks as f64 / secs,
                self.full,
                self.ticks,
                if partial > 0 { self.rects as f64 / partial as f64 } else { 0.0 },
                pct
            );
            self.ticks = 0;
            self.full = 0;
            self.rects = 0;
            self.area = 0;
            self.area_total = 0;
            self.since = Instant::now();
        }
    }
}

fn opts() -> getopts::Options {
    let mut o = getopts::Options::new();
    o.optopt("c", "cell", "grid cell size in logical pixels [12]", "N")
        .optopt("g", "gap", "gap between cells in pixels [1]", "N")
        .optopt("f", "fps", "generations per second [1]; see also --interval", "F")
        .optopt("", "interval", "seconds between generations [1]; the inverse of --fps, mutually exclusive with it", "S")
        .optopt("d", "density", "initial live fraction 0..1 [0.28]", "F")
        .optopt("", "spawn", "what to drop on the grid: noise | shapes | mixed [mixed]", "MODE")
        .optopt("s", "seed", "PRNG seed [time]", "N")
        .optopt("", "base", "dead cell colour [0x0b0e13]", "HEX")
        .optopt("", "alive", "live cell colour [0x3d7f6e]", "HEX")
        .optopt("", "seed-interval", "drop fresh soup every N generations, 0 = never [24]", "N")
        .optopt("", "layer", "background | bottom [background]", "L")
        .optopt("o", "monitor", "only draw on this output name", "NAME")
        .optflag("", "list-outputs", "print detected outputs and exit")
        .optflag("", "list-patterns", "print the named patterns --spawn shapes can drop, and exit")
        .optopt("", "outline", "empty margin in display pixels around the screen edges; the grid starts inside it so cells stay whole [0]", "N")
        .optopt("", "damage", "how much to damage per frame: auto | full [auto]", "MODE")
        .optflag("", "stats", "print tick rate and damage-rect counts every 2s")
        .optflag("h", "help", "print this help")
        .optflag("V", "version", "print the version and exit");
    o
}

fn parse_args() -> Result<Config, String> {
    let o = opts();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let matches = o.parse(&args).map_err(|e| e.to_string())?;
    if matches.opt_present("h") {
        return Ok(Config { help: true, ..Default::default() });
    }

    let mut c = Config::default();
    if let Some(v) = matches.opt_str("c") {
        c.cell = v.parse().map_err(|_| format!("bad --cell '{v}'"))?;
    }
    if let Some(v) = matches.opt_str("g") {
        c.gap = v.parse().map_err(|_| format!("bad --gap '{v}'"))?;
    }
    let fps_opt = matches.opt_str("f");
    let interval_opt = matches.opt_str("interval");
    match (fps_opt.as_deref(), interval_opt.as_deref()) {
        (Some(_), Some(_)) => return Err("--fps and --interval are mutually exclusive; pick one".into()),
        (Some(v), None) => c.fps = v.parse().map_err(|_| format!("bad --fps '{v}'"))?,
        (None, Some(v)) => {
            let secs: f64 = v.parse().map_err(|_| format!("bad --interval '{v}'"))?;
            if !(0.008..=3600.0).contains(&secs) {
                return Err("--interval must be between 0.008 and 3600 seconds".into());
            }
            c.fps = 1.0 / secs;
        }
        (None, None) => {}
    }
    if let Some(v) = matches.opt_str("d") {
        c.density = v.parse().map_err(|_| format!("bad --density '{v}'"))?;
    }
    if let Some(v) = matches.opt_str("spawn") {
        c.spawn = match v.to_ascii_lowercase().as_str() {
            "noise" => Spawn::Noise,
            "shapes" => Spawn::Shapes,
            "mixed" => Spawn::Mixed,
            other => return Err(format!("unknown --spawn '{other}' (noise|shapes|mixed)")),
        };
    }
    if let Some(v) = matches.opt_str("s") {
        c.seed = v.parse().map_err(|_| format!("bad --seed '{v}'"))?;
    }
    if let Some(v) = matches.opt_str("base") {
        c.base = parse_hex(&v)?;
    }
    if let Some(v) = matches.opt_str("alive") {
        c.alive = parse_hex(&v)?;
    }
    if let Some(v) = matches.opt_str("seed-interval") {
        c.seed_interval = v.parse().map_err(|_| format!("bad --seed-interval '{v}'"))?;
    }
    if let Some(v) = matches.opt_str("outline") {
        c.outline = v.parse().map_err(|_| format!("bad --outline '{v}'"))?;
    }
    if let Some(v) = matches.opt_str("damage") {
        c.full_damage = match v.to_ascii_lowercase().as_str() {
            "auto" => false,
            "full" => true,
            other => return Err(format!("unknown --damage '{other}' (auto|full)")),
        };
    }
    if let Some(v) = matches.opt_str("layer") {
        c.layer = match v.to_ascii_lowercase().as_str() {
            "background" => Layer::Background,
            "bottom" => Layer::Bottom,
            other => return Err(format!("unknown --layer '{other}' (background|bottom)")),
        };
    }
    c.monitor = matches.opt_str("monitor");
    c.stats = matches.opt_present("stats");
    c.list_outputs = matches.opt_present("list-outputs");
    c.list_patterns = matches.opt_present("list-patterns");
    c.version = matches.opt_present("version");

    if c.cell < 2 {
        return Err("--cell must be at least 2".into());
    }
    if c.gap >= c.cell {
        return Err("--gap must be smaller than --cell".into());
    }
    if !(1.0 / 3600.0..=120.0).contains(&c.fps) {
        return Err("generation rate must be between 1 per hour and 120 per second".into());
    }
    if !(0.01..=0.9).contains(&c.density) {
        return Err("--density must be between 0.01 and 0.9".into());
    }
    Ok(c)
}

fn usage() {
    println!(
        "lifebg — Conway's Game of Life as a Wayland layer-shell wallpaper\n\
         \n\
         Cost model: the compositor pays for frames_per_second x damaged_area.\n\
         A GIF forces both to their worst case. This runs a few generations per\n\
         second and damages only the cells that actually changed.\n\
         \n\
         Options:"
    );
    print!("{}", opts().usage("lifebg [OPTIONS]"));
}

/// Wait up to `timeout` for the compositor to say something.
///
/// `EventQueue::blocking_dispatch` waits forever, which is wrong twice over:
/// a signal that sets `STOP` arrives while we are parked inside it and is not
/// noticed until the compositor happens to talk, and a compositor that stops
/// sending frame callbacks — a display in DPMS sleep is the obvious case for a
/// wallpaper — would stall the loop indefinitely.
fn wait_for_compositor(eq: &mut EventQueue<App>, timeout: Duration) {
    // Must go out before we wait, or the compositor never receives the request
    // we are waiting for a reply to. `blocking_dispatch` does this for you;
    // `prepare_read` does not.
    let _ = eq.flush();
    if let Some(guard) = eq.prepare_read() {
        let mut pfd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd over a fd borrowed from `guard`, which outlives the
        // call. The return value is ignored: a timeout or an interrupt both just
        // fall through, and `read` reports WouldBlock in that case.
        unsafe { libc::poll(&mut pfd, 1, timeout.as_millis().max(1) as i32) };
        // This is what actually takes the bytes off the socket. Dropping the
        // guard without reading leaves everything unread, and
        // `dispatch_pending` does not read by itself. A genuine failure here
        // shows up on the very next `dispatch_pending`, so it is not swallowed.
        let _ = guard.read();
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lifebg: {e}");
            std::process::exit(2);
        }
    };
    if cfg.help {
        usage();
        return Ok(());
    }

    if cfg.version {
        println!("lifebg {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if cfg.list_patterns {
        for p in PATTERNS {
            println!("{:<11} {:>2}x{:<2} {:>2} cells  weight {}", p.name, p.w, p.h, p.cells.len(), p.weight);
        }
        return Ok(());
    }

    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }

    let conn = Connection::connect_to_env()?;
    let mut eq: EventQueue<App> = conn.new_event_queue();
    let qh = eq.handle();

    let mut app = App::new(
        SimCfg { density: cfg.density, seed_interval: cfg.seed_interval, spawn: cfg.spawn, ..SimCfg::default() },
        Palette { base: cfg.base, alive: cfg.alive },
        Options { cell: cfg.cell, gap: cfg.gap, outline: cfg.outline, full_damage: cfg.full_damage, layer: cfg.layer },
        cfg.seed,
        cfg.monitor.clone(),
    );

    let _registry = conn.display().get_registry(&qh, ());

    // Initial roundtrip: globals, then each output's name/done. Bounded, so a
    // compositor that never finishes announcing outputs produces an error
    // instead of a hang.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !app.ready() || !app.outputs_ready() {
        wait_for_compositor(&mut eq, Duration::from_millis(200));
        eq.dispatch_pending(&mut app)?;
        if Instant::now() > deadline {
            break;
        }
    }

    if cfg.list_outputs {
        for name in app.output_names() {
            println!("{name}");
        }
        return Ok(());
    }

    app.sync_monitors(&qh)?;
    if app.monitors.is_empty() {
        return Err("no matching output".into());
    }
    let names: Vec<String> = app.monitors.iter().map(|m| m.name.clone()).collect();
    let rate = if cfg.fps < 1.0 {
        format!("{:.1}s per gen", 1.0 / cfg.fps)
    } else {
        format!("{:.1} gen per s", cfg.fps)
    };
    eprintln!("lifebg: {} on {} @ {}, cell {}px", env!("CARGO_PKG_VERSION"), names.join("+"), rate, cfg.cell);

    let interval = Duration::from_secs_f64(1.0 / cfg.fps);
    let mut stats = Stats::on(cfg.stats);
    let (comp, shm) = app.handles().ok_or("compositor is missing wl_compositor / wl_shm")?;
    let pal = app.palette;

    while app.running && !STOP.load(Ordering::SeqCst) {
        if let Err(e) = eq.dispatch_pending(&mut app) {
            eprintln!("lifebg: compositor: {e}");
            break;
        }
        eq.flush()?;

        // Pick up outputs that appeared or disappeared since last time.
        app.sync_monitors(&qh)?;

        let now = Instant::now();
        let mut wake = now + Duration::from_millis(250);
        let mut waiting = app.monitors.is_empty();

        for m in app.monitors.iter_mut() {
            if !m.configured {
                waiting = true;
                continue;
            }
            if m.frame_armed && !m.frame_is_stale() {
                waiting = true;
                continue;
            }
            m.frame_armed = false;

            if let Some((w, h)) = m.take_pending_size() {
                m.resize(w, h, &comp, &shm, &pal, &qh)?;
            }
            if m.free_now() {
                if now >= m.next_tick {
                    let rects = m.tick(&pal);
                    stats.tick(rects.as_deref(), m.surface_area());
                    m.present(rects, &qh);
                    m.next_tick = now + interval;
                }
                if m.next_tick < wake {
                    wake = m.next_tick;
                }
            } else {
                // Compositor is holding both buffers; retry very soon.
                wake = wake.min(now + Duration::from_millis(4));
            }
        }

        // Push everything this iteration queued out to the compositor before we
        // block, so it starts compositing (and releases buffers) right away.
        if let Err(e) = eq.flush() {
            eprintln!("lifebg: flush: {e}");
            break;
        }

        if waiting && app.monitors.iter().all(|m| m.frame_armed || !m.configured) {
            // Nothing to do until the compositor talks to us, but only wait so
            // long: a signal or a silent compositor must still be noticed.
            wait_for_compositor(&mut eq, Duration::from_millis(250));
            eq.dispatch_pending(&mut app)?;
        } else {
            let left = wake.saturating_duration_since(Instant::now());
            if !left.is_zero() {
                std::thread::sleep(left);
            }
        }
    }

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        let msg = e.to_string();
        if msg.contains("No such file or directory") || msg.contains("EACCES") {
            eprintln!("lifebg: cannot reach the Wayland compositor (is WAYLAND_DISPLAY set?)");
        } else {
            eprintln!("lifebg: {msg}");
        }
        std::process::exit(1);
    }
}
