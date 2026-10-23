//! Wayland side: one opaque layer-shell surface per output, double-buffered
//! `wl_shm` backing store, and per-cell damage on commit.
//!
//! Sizing note, because it cost a whole debugging session: on a fractional-scaled
//! output Hyprland does not accept a layer surface sized with an integer
//! `wl_surface.buffer_scale`. The buffer has to be `logical_size * fractional_scale`
//! and the surface has to declare its logical size through `wp_viewport` — which
//! is precisely what awww does. Doing it any other way gets the client killed
//! with a bare EPIPE and no protocol error you can read.

use std::ffi::CString;
use std::io;
use std::ops::Range;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::time::Instant;

use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_callback::WlCallback;
use wayland_client::protocol::wl_compositor::{Event as CompositorEvent, WlCompositor};
use wayland_client::protocol::wl_output::{Event as OutputEvent, WlOutput};
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry::{Event as RegistryEvent, WlRegistry};
use wayland_client::protocol::wl_shm::{Event as ShmEvent, Format, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{Event as FractionalEvent, WpFractionalScaleV1},
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{
    Anchor, Event as LayerEvent, ZwlrLayerSurfaceV1,
};

use crate::paint::{self, DamageBuilder, Palette, Rect};
use crate::sim::{Life, SimCfg};

/// If the merged damage covers this fraction of the surface, clipping to it
/// saves the compositor so little that one whole-surface damage is the better
/// deal. Relative, so it means the same thing at any resolution or cell size.
const DAMAGE_MAX_PCT: usize = 55;

/// Merging is only worth doing if it yields at most one rectangle per this many
/// cells. Past that the changes are too scattered for clipping to pay, and the
/// region-building tax outweighs the area saved. Relative for the same reason
/// as `DAMAGE_MAX_PCT`: an absolute rect count means something different at
/// every resolution and cell size, which is how the original default of 256
/// silently forced near-constant full-surface damage.
const DAMAGE_MAX_RECTS_DIVISOR: usize = 16;

/// Absolute ceiling on the rectangle list regardless of grid size, so a huge
/// display cannot produce an absurd region.
const DAMAGE_RECT_HARD_CAP: usize = 4096;

/// A shared-memory mapping used as a `wl_buffer` backing store.
struct Mapped {
    ptr: NonNull<u8>,
    len: usize,
}

impl Mapped {
    fn map(fd: RawFd, len: usize) -> io::Result<Mapped> {
        // SAFETY: standard memfd + mmap of `len` bytes, kept alive by `self`.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mapped { ptr: NonNull::new(p as *mut u8).expect("mmap returned null"), len })
    }

    fn pixels(&mut self) -> &mut [u32] {
        // SAFETY: the mapping is `len` bytes and `len` is always a multiple of 4.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr() as *mut u32, self.len / 4) }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are exactly what was handed to mmap.
        unsafe {
            libc::munmap(self.ptr.as_ptr() as *mut _, self.len);
        }
    }
}

struct Buffer {
    _pool: WlShmPool,
    buffer: WlBuffer,
    /// First `u32` of this buffer inside the monitor's mapping. The buffers
    /// tile the mapping evenly, so the slot length is implied.
    base: usize,
    busy: bool,
}

/// Per-object user data for `wl_buffer`, so `release` tells us which slot freed
/// up on which monitor.
#[derive(Clone, Copy, Debug)]
struct BufKey {
    monitor: usize,
    slot: usize,
}

struct OutputInfo {
    output: WlOutput,
    /// The `wl_registry` global name, so `global_removed` can find us again.
    global: u32,
    name: String,
    done: bool,
    scale: i32,
    /// The output was destroyed (hot-unplug, or the compositor dropped it).
    gone: bool,
    /// How many surfaces we have created for this output. A compositor that
    /// keeps closing them must not be able to put us in a respawn spin.
    spawns: u32,
}

/// Give up on an output after this many surfaces closed unexpectedly.
const MAX_SPAWNS_PER_OUTPUT: u32 = 5;

fn output_name(info: &OutputInfo) -> String {
    if info.name.is_empty() {
        format!("output@{}", info.output.id().protocol_id())
    } else {
        info.name.clone()
    }
}

pub struct Monitor {
    pub id: usize,
    pub name: String,
    /// Index into `App::outputs`, so hotplug can tell which output we belong to.
    out_idx: usize,
    // Kept alive on purpose: these must outlive every buffer we attach, and
    // dropping the proxies would tear the surface down.
    _output: WlOutput,
    _layer: ZwlrLayerSurfaceV1,
    _regions: Vec<WlRegion>,
    _fractional: Option<WpFractionalScaleV1>,
    surface: WlSurface,
    viewport: Option<WpViewport>,
    width: u32,
    height: u32,
    /// Buffer dimensions in device pixels: `width * fscale`.
    bw: usize,
    bh: usize,
    fscale: f64,
    cell: usize,
    gap: usize,
    /// Cell pitch in buffer pixels.
    cell_buf: usize,
    /// Gap in buffer pixels.
    gap_buf: usize,
    /// Empty margin around the screen edges, in logical pixels.
    outline: usize,
    full_damage: bool,
    /// Grid origin in buffer pixels. The outline moves the origin instead of
    /// cutting cells, so every cell stays whole.
    origin_x: usize,
    origin_y: usize,
    gw: usize,
    gh: usize,
    simcfg: SimCfg,
    seed: u64,
    sim: Life,
    prev: Vec<bool>,
    /// One memfd + one `wl_shm_pool` per monitor, carved into buffers by the
    /// compositor's bump allocator. We map the whole pool and slice it at the
    /// same offsets the compositor uses.
    _shm_fd: Option<OwnedFd>,
    shm: Option<Mapped>,
    buffers: Vec<Buffer>,
    next_buf: usize,
    pub configured: bool,
    pending_size: Option<(u32, u32)>,
    pub frame_armed: bool,
    frame_since: Instant,
    pub next_tick: Instant,
    force_full: bool,
}

/// How the grid is drawn, as distinct from how the simulation runs.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub cell: usize,
    pub gap: usize,
    pub outline: usize,
    /// Skip the damage-rect merging entirely and damage the whole surface every
    /// frame. Exists so the merging can be A/B'd against the dumb version.
    pub full_damage: bool,
    pub layer: Layer,
}

/// Everything the event loop owns.
pub struct App {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    layer_shell: Option<ZwlrLayerShellV1>,
    viewporter: Option<WpViewporter>,
    fractional: Option<WpFractionalScaleManagerV1>,
    outputs: Vec<OutputInfo>,
    pub monitors: Vec<Monitor>,
    pub running: bool,
    next_id: usize,
    pub palette: Palette,
    simcfg: SimCfg,
    opts: Options,
    pub seed: u64,
    pub monitor_filter: Option<String>,
}

impl App {
    pub fn new(simcfg: SimCfg, palette: Palette, opts: Options, seed: u64, monitor_filter: Option<String>) -> App {
        App {
            compositor: None,
            shm: None,
            layer_shell: None,
            viewporter: None,
            fractional: None,
            outputs: Vec::new(),
            monitors: Vec::new(),
            running: true,
            next_id: 0,
            palette,
            simcfg,
            opts,
            seed,
            monitor_filter,
        }
    }

    pub fn ready(&self) -> bool {
        self.compositor.is_some() && self.shm.is_some() && self.layer_shell.is_some()
    }

    pub fn outputs_ready(&self) -> bool {
        !self.outputs.is_empty() && self.outputs.iter().all(|o| o.done)
    }

    pub fn output_names(&self) -> Vec<String> {
        self.outputs.iter().map(|o| o.name.clone()).collect()
    }

    /// The globals the draw loop needs, taken out of the optionals so the loop
    /// does not have to reach into `App` while borrowing a monitor.
    pub fn handles(&self) -> Option<(WlCompositor, WlShm)> {
        Some((self.compositor.as_ref()?.clone(), self.shm.as_ref()?.clone()))
    }

    /// Reconcile our layer surfaces with the outputs the compositor actually
    /// has: give newly announced outputs a surface, drop the surfaces of
    /// outputs that went away. Called every loop iteration, so hotplug and
    /// hot-unplug both work without a restart.
    pub fn sync_monitors(&mut self, qh: &QueueHandle<App>) -> io::Result<()> {
        let comp = self.compositor.clone().ok_or_else(|| io::Error::other("compositor has no wl_compositor"))?;
        let shell = self.layer_shell.clone().ok_or_else(|| io::Error::other("compositor has no wlr-layer-shell"))?;
        let viewporter = self.viewporter.clone();
        let fractional = self.fractional.clone();

        // Outputs that need a surface: announced, still present, not already
        // covered, and not one we have already given up on.
        let mut fresh: Vec<usize> = Vec::new();
        for (idx, info) in self.outputs.iter().enumerate() {
            if !info.done || info.gone || info.spawns >= MAX_SPAWNS_PER_OUTPUT {
                continue;
            }
            if self.monitors.iter().any(|m| m.out_idx == idx) {
                continue;
            }
            if self.monitor_filter.as_ref().is_some_and(|f| *f != output_name(info)) {
                continue;
            }
            fresh.push(idx);
        }

        for &idx in fresh.iter() {
            self.outputs[idx].spawns += 1;
            let info = &self.outputs[idx];
            let name = output_name(info);
            let id = self.next_id;
            self.next_id += 1;

            let surface = comp.create_surface(qh, ());
            let viewport = viewporter.as_ref().map(|v| v.get_viewport(&surface, qh, ()));
            let frac = fractional.as_ref().map(|f| f.get_fractional_scale(&surface, qh, id));

            let layer = shell.get_layer_surface(&surface, Some(&info.output), self.opts.layer, "lifebg".to_string(), qh, id);
            layer.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Left | Anchor::Right);
            layer.set_exclusive_zone(0);
            layer.set_margin(0, 0, 0, 0);
            layer.set_size(0, 0);
            surface.commit();

            let seed = self.seed.wrapping_add(id as u64 * 0x9E37_79B9_7F4A_7C15);
            let has_viewport = viewport.is_some();
            self.monitors.push(Monitor {
                id,
                out_idx: idx,
                name: name.clone(),
                _output: info.output.clone(),
                _layer: layer,
                _regions: Vec::new(),
                _fractional: frac,
                surface,
                viewport,
                // Without a viewport we cannot declare a logical size, so the
                // buffer has to be the logical size and the scale is 1.
                fscale: if has_viewport { info.scale.max(1) as f64 } else { 1.0 },
                width: 0,
                height: 0,
                bw: 0,
                bh: 0,
                cell: self.opts.cell,
                gap: self.opts.gap,
                cell_buf: self.opts.cell,
                gap_buf: self.opts.gap,
                outline: self.opts.outline,
                full_damage: self.opts.full_damage,
                origin_x: 0,
                origin_y: 0,
                gw: 0,
                gh: 0,
                simcfg: self.simcfg,
                seed,
                sim: Life::new(8, 8, self.simcfg, seed),
                prev: Vec::new(),
                _shm_fd: None,
                shm: None,
                buffers: Vec::new(),
                next_buf: 0,
                configured: false,
                pending_size: None,
                frame_armed: false,
                frame_since: Instant::now(),
                next_tick: Instant::now(),
                force_full: true,
            });
            eprintln!("lifebg: drawing on {name}");
        }

        // Outputs that went away: dropping the Monitor drops its proxies, which
        // destroys the surface. Only stop the app once nothing is left to draw
        // on, so unplugging one screen keeps the others alive.
        let gone: Vec<usize> = self.outputs.iter().enumerate().filter(|(_, o)| o.gone).map(|(i, _)| i).collect();
        let before = self.monitors.len();
        self.monitors.retain(|m| !gone.contains(&m.out_idx));
        if self.monitors.len() != before {
            eprintln!("lifebg: {} output(s) went away", before - self.monitors.len());
        }

        if self.monitors.is_empty() && !self.outputs.is_empty() {
            // Nothing to draw on: either every output went away or the filter
            // matched nothing. The caller decides whether that is fatal.
            self.running = false;
        }
        Ok(())
    }

    pub fn find_monitor_mut(&mut self, id: usize) -> Option<&mut Monitor> {
        self.monitors.iter_mut().find(|m| m.id == id)
    }
}

impl Monitor {
    /// Adopt a new surface size: resize the grid, reallocate buffers, repaint.
    pub fn resize(&mut self, w: u32, h: u32, comp: &WlCompositor, shm: &WlShm, pal: &Palette, qh: &QueueHandle<App>) -> io::Result<()> {
        self.width = w;
        self.height = h;
        let f = self.fscale.max(0.5);
        self.bw = ((w as f64) * f).round().max(1.0) as usize;
        self.bh = ((h as f64) * f).round().max(1.0) as usize;
        self.cell_buf = ((self.cell as f64) * f).round().max(2.0) as usize;
        self.gap_buf = ((self.gap as f64) * f).round() as usize;

        // Empty margin around the edges, measured in display pixels. The grid
        // origin moves inside the margin rather than the margin cutting cells,
        // so every cell stays whole; whatever is left over on the right and
        // bottom simply widens the margin on those sides.
        let outline_buf = ((self.outline as f64) * f).round() as usize;
        let outline_buf = outline_buf.min(self.bw / 2).min(self.bh / 2);
        let avail_w = self.bw.saturating_sub(outline_buf * 2);
        let avail_h = self.bh.saturating_sub(outline_buf * 2);
        self.gw = (avail_w / self.cell_buf).max(1);
        self.gh = (avail_h / self.cell_buf).max(1);
        // Whatever is left over after whole cells is split evenly instead of
        // piling up on the right and bottom, so the margin reads the same on all
        // four sides.
        self.origin_x = outline_buf + avail_w.saturating_sub(self.gw * self.cell_buf) / 2;
        self.origin_y = outline_buf + avail_h.saturating_sub(self.gh * self.cell_buf) / 2;

        // The viewport is what tells the compositor our logical size; the
        // buffer itself is in device pixels.
        if let Some(vp) = &self.viewport {
            vp.set_destination(w as i32, h as i32);
        }
        self.surface.set_buffer_scale(1);

        self.sim = Life::new(self.gw, self.gh, self.simcfg, self.seed);
        self.prev = vec![false; self.gw * self.gh];
        self.alloc_buffers(shm, pal, qh)?;

        // Regions are in surface (logical) coordinates, not buffer coordinates.
        let opaque = comp.create_region(qh, ());
        opaque.add(0, 0, w as i32, h as i32);
        self.surface.set_opaque_region(Some(&opaque));
        // Nothing here should ever eat a click.
        let empty = comp.create_region(qh, ());
        self.surface.set_input_region(Some(&empty));
        self._regions = vec![opaque, empty];

        self.force_full = true;
        self.next_tick = Instant::now();
        Ok(())
    }

    fn alloc_buffers(&mut self, shm: &WlShm, pal: &Palette, qh: &QueueHandle<App>) -> io::Result<()> {
        self.buffers.clear();
        self.shm = None;
        self._shm_fd = None;
        let stride = self.bw * 4;
        let len = stride * self.bh;
        // Hyprland rejects a buffer whose `size` is not page aligned, and a
        // pool exactly the size of one buffer, so each buffer gets a
        // page-rounded slot inside one shared pool. Because the slot size is
        // also the `size` we announce, the compositor's bump offsets come out
        // as exactly `slot * slot_len` — which is where we map them too.
        let page = 4096usize;
        let slot_len = len.div_ceil(page) * page;
        let total = slot_len * 2;
        let fd = memfd(&format!("lifebg-{}", self.id))?;
        // SAFETY: truncating a fresh memfd we own.
        unsafe {
            if libc::ftruncate(fd, total as i64) != 0 {
                return Err(io::Error::last_os_error());
            }
            // Seal shrinking so the compositor cannot have the mapping yanked
            // out from under it; it requires F_SEAL_SHRINK.
            libc::fcntl(fd, libc::F_ADD_SEALS, libc::F_SEAL_SHRINK);
        }
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut map = Mapped::map(owned.as_raw_fd(), total)?;
        let pool = shm.create_pool(owned.as_fd(), total as i32, qh, ());
        for slot in 0..2usize {
            let base = slot * (slot_len / 4);
            let n = len / 4;
            map.pixels()[base..base + n].fill(pal.base);
            let buffer = pool.create_buffer(slot_len as i32, self.bw as i32, self.bh as i32, stride as i32, Format::Argb8888, qh, BufKey { monitor: self.id, slot });
            self.buffers.push(Buffer { _pool: pool.clone(), buffer, base, busy: false });
        }
        self._shm_fd = Some(owned);
        self.shm = Some(map);
        self.next_buf = 0;
        Ok(())
    }

    /// The writable pixels of one buffer slot.
    fn slot_pixels(&mut self, slot: usize) -> &mut [u32] {
        let base = self.buffers[slot].base;
        let slots = self.buffers.len();
        let all = self.shm.as_mut().expect("buffers without a mapping").pixels();
        let per = all.len() / slots;
        &mut all[base..base + per]
    }

    fn free_buffer(&self) -> Option<usize> {
        let n = self.buffers.len();
        for off in 0..n {
            let i = (self.next_buf + off) % n;
            if !self.buffers[i].busy {
                return Some(i);
            }
        }
        None
    }

    pub fn free_now(&self) -> bool {
        self.free_buffer().is_some()
    }

    /// Surface area in logical pixels, i.e. what a whole-surface damage costs.
    pub fn surface_area(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// Advance the simulation and repaint only the cells whose colour moved.
    /// Returns `None` for "damage the whole surface".
    pub fn tick(&mut self, pal: &Palette) -> Option<Vec<Rect>> {
        self.sim.step();

        let mut changes: Vec<(u16, u16, u32)> = Vec::with_capacity(256);
        for gy in 0..self.gh {
            for gx in 0..self.gw {
                let i = gy * self.gw + gx;
                let alive = self.sim.alive(i);
                if alive == self.prev[i] {
                    continue;
                }
                self.prev[i] = alive;
                changes.push((gx as u16, gy as u16, paint::color_of(alive, pal)));
            }
        }

        // Paint every change into *every* buffer, so whichever one we attach
        // next is a complete picture even though we only damage part of it.
        // The cell is centred inside its grid slot, so `gap` is the separation
        // between neighbours rather than the inset from each side.
        let step = self.cell_buf;
        let inset = self.gap_buf / 2;
        let span = step.saturating_sub(self.gap_buf).max(1);
        let (bw, bh) = (self.bw, self.bh);
        let (ox, oy) = (self.origin_x, self.origin_y);
        for slot in 0..self.buffers.len() {
            let px = self.slot_pixels(slot);
            for &(gx, gy, col) in changes.iter() {
                let x0 = ox + gx as usize * step + inset;
                let y0 = oy + gy as usize * step + inset;
                fill_rect(px, bw, x0..(x0 + span).min(bw), y0..(y0 + span).min(bh), col);
            }
        }

        if self.force_full || self.full_damage {
            self.force_full = false;
            return None;
        }
        let f = self.fscale.max(0.5);
        let (dox, doy) = ((self.origin_x as f64 / f).floor() as i32, (self.origin_y as f64 / f).floor() as i32);
        let mut db = DamageBuilder::new(self.cell, dox, doy);
        for &(gx, gy, _) in changes.iter() {
            db.cell_changed(gx as usize, gy as usize);
        }
        db.finish(self.surface_area(), DAMAGE_MAX_PCT, self.rect_cap())
    }

    /// How many damage rectangles are acceptable for this grid before we give
    /// up and damage the whole surface instead.
    fn rect_cap(&self) -> usize {
        ((self.gw * self.gh) / DAMAGE_MAX_RECTS_DIVISOR).clamp(1, DAMAGE_RECT_HARD_CAP)
    }

    /// Attach the next free buffer, damage it, and arm a frame callback.
    ///
    /// Damage is in *logical* surface coordinates: the viewport is what maps
    /// the buffer onto the screen, and handing it buffer-space rectangles makes
    /// the compositor repaint the wrong parts and leave stale cells behind.
    pub fn present(&mut self, rects: Option<Vec<Rect>>, qh: &QueueHandle<App>) {
        if let Some(i) = self.free_buffer() {
            self.surface.attach(Some(&self.buffers[i].buffer), 0, 0);
            match rects {
                Some(list) => {
                    for r in list {
                        self.surface.damage(r.x, r.y, r.w, r.h);
                    }
                }
                None => self.surface.damage(0, 0, self.width as i32, self.height as i32),
            }
            self.buffers[i].busy = true;
            self.next_buf = (i + 1) % self.buffers.len();
        }
        self.surface.frame(qh, self.id);
        self.surface.commit();
        self.frame_armed = true;
        self.frame_since = Instant::now();
    }

    pub fn mark_released(&mut self, slot: usize) {
        if let Some(b) = self.buffers.get_mut(slot) {
            b.busy = false;
        }
    }

    pub fn take_pending_size(&mut self) -> Option<(u32, u32)> {
        self.pending_size.take()
    }

    pub fn set_fractional_scale(&mut self, factor: u32) {
        let f = factor as f64 / 120.0;
        if (f - self.fscale).abs() < 0.001 || self.width == 0 {
            self.fscale = f;
            return;
        }
        self.fscale = f;
        self.pending_size = Some((self.width, self.height));
    }

    pub fn frame_is_stale(&self) -> bool {
        self.frame_since.elapsed().as_secs_f64() > 2.0
    }
}

fn fill_rect(px: &mut [u32], pitch: usize, x: Range<usize>, y: Range<usize>, col: u32) {
    for row in y {
        let base = row * pitch;
        let start = (base + x.start).min(px.len());
        let end = (base + x.end).min(px.len()).max(start);
        px[start..end].fill(col);
    }
}

fn memfd(name: &str) -> io::Result<RawFd> {
    let cname = CString::new(name).unwrap();
    // MFD_ALLOW_SEALING matters: compositors call fcntl(F_ADD_SEALS,
    // F_SEAL_SHRINK) on every client shm fd, and that fcntl fails with EINVAL
    // on a memfd created without it.
    let flags = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
    // SAFETY: plain libc call on a name we built.
    let fd = unsafe { libc::memfd_create(cname.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

// ---------------------------------------------------------------------------
// event dispatch
// ---------------------------------------------------------------------------

impl Dispatch<WlRegistry, ()> for App {
    fn event(app: &mut Self, registry: &WlRegistry, event: RegistryEvent, _: &(), _c: &Connection, qh: &QueueHandle<Self>) {
        match event {
            RegistryEvent::Global { name, interface, version } => {
                match interface.as_str() {
                    "wl_compositor" => app.compositor = Some(registry.bind::<WlCompositor, (), _>(name, version.min(6), qh, ())),
                    "wl_shm" => app.shm = Some(registry.bind::<WlShm, (), _>(name, version.min(1), qh, ())),
                    "zwlr_layer_shell_v1" => app.layer_shell = Some(registry.bind::<ZwlrLayerShellV1, (), _>(name, version.min(4), qh, ())),
                    "wp_viewporter" => app.viewporter = Some(registry.bind::<WpViewporter, (), _>(name, version.min(1), qh, ())),
                    "wp_fractional_scale_manager_v1" => app.fractional = Some(registry.bind::<WpFractionalScaleManagerV1, (), _>(name, version.min(1), qh, ())),
                    "wl_output" => {
                        let idx = app.outputs.len();
                        let output = registry.bind::<WlOutput, usize, _>(name, version.min(4), qh, idx);
                        app.outputs.push(OutputInfo { output, global: name, name: String::new(), done: false, scale: 1, gone: false, spawns: 0 });
                    }
                    _ => {}
                }
            }
            // Hot-unplug. The registry index stays reserved so the `wl_output`
            // user data we bound above keeps pointing at the right entry.
            RegistryEvent::GlobalRemove { name } => {
                if let Some(o) = app.outputs.iter_mut().find(|o| o.global == name) {
                    o.gone = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlCompositor, ()> for App {
    fn event(_: &mut Self, _: &WlCompositor, _: CompositorEvent, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlShm, ()> for App {
    fn event(_: &mut Self, _: &WlShm, _: ShmEvent, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlShmPool, ()> for App {
    fn event(_: &mut Self, _: &WlShmPool, _: <WlShmPool as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlRegion, ()> for App {
    fn event(_: &mut Self, _: &WlRegion, _: <WlRegion as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WlSurface, ()> for App {
    fn event(_: &mut Self, _: &WlSurface, _: <WlSurface as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WpViewport, ()> for App {
    fn event(_: &mut Self, _: &WpViewport, _: <WpViewport as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WpViewporter, ()> for App {
    fn event(_: &mut Self, _: &WpViewporter, _: <WpViewporter as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WpFractionalScaleManagerV1, ()> for App {
    fn event(_: &mut Self, _: &WpFractionalScaleManagerV1, _: <WpFractionalScaleManagerV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<WpFractionalScaleV1, usize> for App {
    fn event(app: &mut Self, _: &WpFractionalScaleV1, event: FractionalEvent, id: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let FractionalEvent::PreferredScale { scale } = event {
            if let Some(m) = app.find_monitor_mut(*id) {
                m.set_fractional_scale(scale);
            }
        }
    }
}

impl Dispatch<WlOutput, usize> for App {
    fn event(app: &mut Self, _: &WlOutput, event: OutputEvent, idx: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let Some(info) = app.outputs.get_mut(*idx) else { return };
        match event {
            OutputEvent::Name { name } => info.name = name,
            OutputEvent::Scale { factor } => info.scale = factor.max(1),
            OutputEvent::Done => info.done = true,
            _ => {}
        }
    }
}

impl Dispatch<WlBuffer, BufKey> for App {
    fn event(app: &mut Self, _: &WlBuffer, event: <WlBuffer as Proxy>::Event, key: &BufKey, _: &Connection, _: &QueueHandle<Self>) {
        if let <WlBuffer as Proxy>::Event::Release = event {
            if let Some(m) = app.find_monitor_mut(key.monitor) {
                m.mark_released(key.slot);
            }
        }
    }
}

impl Dispatch<WlCallback, usize> for App {
    fn event(app: &mut Self, _: &WlCallback, _: <WlCallback as Proxy>::Event, id: &usize, _: &Connection, _: &QueueHandle<Self>) {
        if let Some(m) = app.find_monitor_mut(*id) {
            m.frame_armed = false;
        }
    }
}

impl Dispatch<ZwlrLayerShellV1, ()> for App {
    fn event(_: &mut Self, _: &ZwlrLayerShellV1, _: <ZwlrLayerShellV1 as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwlrLayerSurfaceV1, usize> for App {
    fn event(app: &mut Self, layer: &ZwlrLayerSurfaceV1, event: LayerEvent, id: &usize, _: &Connection, _: &QueueHandle<Self>) {
        match event {
            LayerEvent::Configure { serial, width, height } => {
                layer.ack_configure(serial);
                if let Some(m) = app.find_monitor_mut(*id) {
                    if m.width != width || m.height != height {
                        m.pending_size = Some((width.max(1), height.max(1)));
                        m.configured = true;
                    }
                }
            }
            LayerEvent::Closed => {
                // One output's surface going away must not take the others down
                // with it. Dropping the Monitor destroys just this surface; if
                // the output itself is still there, `sync_monitors` will put a
                // fresh one back.
                if let Some(pos) = app.monitors.iter().position(|m| m.id == *id) {
                    let m = app.monitors.remove(pos);
                    eprintln!("lifebg: compositor closed the layer surface for {}", m.name);
                }
                if app.monitors.is_empty() {
                    eprintln!("lifebg: no outputs left, exiting");
                    app.running = false;
                }
            }
            _ => {}
        }
    }
}
