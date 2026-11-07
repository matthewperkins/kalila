//! Trace views: each is its own native window with its own channels, filter, CAR, scaling
//! and sweep mode. The main window keeps the controls and the spike tiles.

use super::*;
use crate::config::{TraceViewSettings, ViewCar};
use crossbeam_channel::Receiver;
use engine::audio::{AudioSettings, AudioStats};
use engine::rhythm::Block;
use parking_lot::Mutex;
use crate::review::{self, Reader, ReviewView};
use engine::record::RecSource;
use std::collections::VecDeque;

/// Trace-view state shared by the main window and the trace windows. Each trace window is a
/// deferred viewport: it is drawn whenever its own surface is due, even while the main window
/// is hidden (Wayland withholds frames from hidden windows, which would otherwise stall it).
pub(super) type SharedViews = Arc<Mutex<ViewsShared>>;

/// Runtime state of one trace view: staged samples and what was last sent to its GPU state.
pub(super) struct ViewState {
    pub id: u32,
    /// A deque, so trimming the oldest samples costs only what is dropped: with a `Vec`, every
    /// block shifted a whole window (tens of MB for a 20 s window), which starved the UI thread
    /// whenever a window missed a few frames and froze all windows for many seconds.
    pending: VecDeque<i16>,
    /// TTL words for the samples in `pending` (one per sample).
    pending_ttl: VecDeque<u16>,
    ttl: TtlOverlay,
    /// Samples handed to the GPU since the last reset (sweep cursor, trigger positions).
    counter: u64,
    reset: bool,
    last_cfg: Option<TraceConfig>,
    /// (row order, row table, ADC V/row bits) last sent.
    tables_key: Option<(Vec<u32>, Vec<[u32; 2]>, u32)>,
    last_chinfo: Option<Vec<[f32; 4]>>,
    sweeps: Sweeps,
    /// (pre, post, line, slots, filter, cols): a change clears the sweeps.
    sweep_key: Option<(u32, u32, usize, u32, ViewFilter, u32)>,
    pub colors_dirty: bool,
    // Diagnostics: when the view was last drawn / last got samples, and whether a stall
    // warning is outstanding.
    last_drawn: Instant,
    last_data: Instant,
    stalled: bool,
    /// Scroll-back review (None = live).
    pub review: Option<ReviewView>,
    /// A short message on the plot (e.g. why review can't start), until the instant.
    note: Option<(Instant, String)>,
}

impl ViewState {
    pub fn new(id: u32) -> Self {
        Self {
            id,
            pending: VecDeque::new(),
            pending_ttl: VecDeque::new(),
            ttl: TtlOverlay::default(),
            counter: 0,
            reset: true,
            last_cfg: None,
            tables_key: None,
            last_chinfo: None,
            sweeps: Sweeps::default(),
            sweep_key: None,
            colors_dirty: true,
            last_drawn: Instant::now(),
            last_data: Instant::now(),
            stalled: false,
            review: None,
            note: None,
        }
    }

    pub fn reset(&mut self) {
        self.reset = true;
        self.colors_dirty = true;
        self.pending.clear();
        self.pending_ttl.clear();
        self.ttl.reset();
        self.sweeps.clear();
        self.counter = 0;
    }

    /// Stage a block; keep at most one window of samples (e.g. while the window is hidden).
    pub fn push(&mut self, b: &engine::rhythm::Block, nch: usize, window: usize) {
        self.pending.extend(b.data.iter().copied());
        let n = b.data.len() / nch.max(1);
        if b.ttl.len() == n {
            self.pending_ttl.extend(b.ttl.iter().copied());
        } else {
            self.pending_ttl.resize(self.pending_ttl.len() + n, 0);
        }
        let max = window * nch;
        if self.pending.len() > max {
            let drop = self.pending.len() - max;
            let drop = drop - drop % nch;
            self.pending.drain(..drop);
            self.pending_ttl.drain(..(drop / nch).min(self.pending_ttl.len()));
            // Trimmed samples never reach the GPU ring: waiting triggers can't be cut.
            self.sweeps.drop_pending();
            self.counter += (drop / nch) as u64;
        }
    }
}

pub(super) struct ViewsShared {
    /// Authoritative view settings (copied into `Config::views` when saving).
    pub settings: Vec<TraceViewSettings>,
    states: Vec<ViewState>,
    pub layout: Option<StreamLayout>,
    /// Display blocks from the engine; drained by whichever window draws first.
    pub rx: Option<Receiver<Arc<Block>>>,
    nch: usize,
    audio: Arc<Mutex<AudioSettings>>,
    audio_stats: Arc<AudioStats>,
    /// View whose CAR window is open.
    car_open: Option<u32>,
    /// Settings changed in a trace window: the main window saves the config.
    pub dirty: bool,
    /// The audio-monitored set changed from a trace window (main window refreshes).
    pub monitor_changed: bool,
    pub max_fps: f32,
    pub acquiring: bool,
    /// Views deleted since the main window last freed their GPU state.
    pub removed: Vec<u32>,
    pub disp_latency: f32,
    pub disp_latency_max: f32,
    latency_window: Instant,
    /// Reads the recording back for scroll-back review.
    reader: Reader,
    /// The recording being written (or the last one), from the recorder.
    pub rec_source: Option<RecSource>,
    /// Absolute sample number just after the newest displayed block.
    newest_sample: i64,
}

/// Navigation asked for in one frame (scroll-back review).
#[derive(Default)]
struct Nav {
    /// Pan by this fraction of the span (negative = back in time).
    pan: f64,
    /// Zoom by this factor (> 1 = more time) about `zoom_at` (0..1 across the plot).
    zoom: f64,
    zoom_at: f64,
    home: bool,
    live: bool,
}

impl Nav {
    fn any(&self) -> bool {
        self.pan != 0.0 || self.zoom != 0.0 || self.home || self.live
    }
    fn goes_back(&self) -> bool {
        self.pan < 0.0 || self.home
    }
}

/// What the CAR window edits.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum CarTarget {
    /// Spike detection and audio (global sets).
    Global,
    /// A trace view's display CAR, by view id.
    View(u32),
}

/// Geometry of a trace plot, for drawing a review frame.
#[derive(Clone, Copy)]
struct ReviewGeom {
    rect: egui::Rect,
    plot: egui::Rect,
    strip: egui::Rect,
    ttl_h: f32,
    label_w: f32,
    cols: u32,
    rows: usize,
    first: usize,
    k: usize,
    ppp: f32,
}

/// Time-window limits (s). Live keeps up to one window of samples per view in memory, so it
/// stops at 20 s; review reads from the recording and reaches further.
const LIVE_MIN_S: f32 = 0.05;
const LIVE_MAX_S: f32 = 20.0;
const REVIEW_MIN_S: f32 = 0.005;
const REVIEW_MAX_S: f32 = 60.0;

/// Review spans in samples: at least one sample per column, within the review limits.
fn review_span_limits(cols: u32, fs: f64) -> (u32, u32) {
    (cols.max((REVIEW_MIN_S as f64 * fs) as u32), (REVIEW_MAX_S as f64 * fs) as u32)
}

/// Review navigation from this frame's input over a trace window.
fn nav_input(ui: &egui::Ui, resp: &egui::Response, plot: egui::Rect) -> Nav {
    let mut n = Nav::default();
    let w = plot.width().max(1.0) as f64;
    if resp.dragged_by(egui::PointerButton::Primary) {
        n.pan -= resp.drag_delta().x as f64 / w;
    }
    let typing = ui.ctx().egui_wants_keyboard_input();
    ui.input(|i| {
        if resp.hovered() {
            // Sideways swipe, or Shift+wheel (egui turns it into horizontal scrolling).
            n.pan -= i.smooth_scroll_delta.x as f64 / w;
            if i.modifiers.alt && i.smooth_scroll_delta.y != 0.0 {
                n.zoom = (-(i.smooth_scroll_delta.y as f64) * 0.004).exp();
                n.zoom_at = i.pointer.hover_pos().map_or(0.5, |p| ((p.x - plot.min.x) as f64 / w).clamp(0.0, 1.0));
            }
        }
        if !typing {
            let step = if i.modifiers.shift { 0.1 } else { 1.0 };
            if i.key_pressed(egui::Key::ArrowLeft) {
                n.pan -= step;
            }
            if i.key_pressed(egui::Key::ArrowRight) {
                n.pan += step;
            }
            if i.key_pressed(egui::Key::Home) {
                n.home = true;
            }
            if i.key_pressed(egui::Key::End) {
                n.live = true;
            }
            if i.key_pressed(egui::Key::Minus) {
                (n.zoom, n.zoom_at) = (1.25, 0.5);
            }
            if i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals) {
                (n.zoom, n.zoom_at) = (0.8, 0.5);
            }
        }
    });
    n
}

/// Per-channel GPU info for a view (see traces.wgsl): gain; flags (bit 0 display filter,
/// bit 1 CAR reference, bit 2 CAR subtract; electrodes only); units per row; centre.
fn view_chinfo(l: &StreamLayout, d: &TraceViewSettings) -> Vec<[f32; 4]> {
    let in_set = |set: &[usize], ch: usize| set.contains(&ch);
    l.channels
        .iter()
        .enumerate()
        .map(|(i, c)| match c.kind {
            ChannelKind::Electrode => [c.bit_volts, (1 + 2 * in_set(&d.car.reference, i) as u32 + 4 * in_set(&d.car.subtract, i) as u32) as f32, 0.0, 0.0],
            ChannelKind::Aux => [c.bit_volts, 0.0, 2.5, 0.0],
            ChannelKind::Adc => [c.bit_volts, 0.0, d.adc_v_per_row, 0.0],
        })
        .collect()
}

/// Rows of one view: channel order (electrodes, then AUX/ADC) and per-row (first, count).
pub(super) fn view_rows(l: &StreamLayout, v: &TraceViewSettings) -> (Vec<u32>, Vec<[u32; 2]>) {
    let k = v.per_row.clamp(1, 8) as usize;
    let hs_on = |hs: Option<usize>| v.headstages.is_empty() || hs.and_then(|i| l.headstages.get(i)).is_some_and(|h| v.headstages.contains(&h.name));
    let el: Vec<u32> = (0..l.num_channels() as u32).filter(|&i| {
        let c = &l.channels[i as usize];
        c.kind == ChannelKind::Electrode && hs_on(c.headstage)
    }).collect();
    let an: Vec<u32> = (0..l.num_channels() as u32).filter(|&i| match l.channels[i as usize].kind {
        ChannelKind::Aux => v.aux,
        ChannelKind::Adc => v.adc,
        ChannelKind::Electrode => false,
    }).collect();
    let mut rowtab: Vec<[u32; 2]> = (0..el.len()).step_by(k).map(|s| [s as u32, k.min(el.len() - s) as u32]).collect();
    rowtab.extend((el.len()..el.len() + an.len()).map(|i| [i as u32, 1]));
    let mut order = el;
    order.extend(an);
    (order, rowtab)
}

/// Electrodes of the given headstages (all if empty), grouped by headstage, for the CAR window.
fn electrode_groups(l: &StreamLayout, only: &[String]) -> Vec<(String, Vec<(usize, String)>)> {
    l.headstages
        .iter()
        .enumerate()
        .filter(|(_, h)| only.is_empty() || only.contains(&h.name))
        .map(|(hi, h)| {
            let chans = l.channels.iter().enumerate().filter(|(_, c)| c.kind == ChannelKind::Electrode && c.headstage == Some(hi)).map(|(i, c)| (i, c.name.clone())).collect();
            (format!("{} {}", h.name, h.chip), chans)
        })
        .collect()
}


impl ViewsShared {
    pub fn new(settings: Vec<TraceViewSettings>, audio: Arc<Mutex<AudioSettings>>, audio_stats: Arc<AudioStats>) -> Self {
        let mut s = Self {
            settings,
            states: Vec::new(),
            layout: None,
            rx: None,
            nch: 0,
            audio,
            audio_stats,
            car_open: None,
            dirty: false,
            monitor_changed: false,
            max_fps: 144.0,
            acquiring: false,
            removed: Vec::new(),
            disp_latency: 0.0,
            disp_latency_max: 0.0,
            latency_window: Instant::now(),
            reader: Reader::spawn(),
            rec_source: None,
            newest_sample: 0,
        };
        s.sync_states();
        s
    }

    /// Make the runtime states match the settings (same order, by id).
    pub fn sync_states(&mut self) {
        let ids: Vec<u32> = self.settings.iter().map(|v| v.id).collect();
        if self.states.iter().map(|v| v.id).eq(ids.iter().copied()) {
            return;
        }
        let mut old: Vec<ViewState> = std::mem::take(&mut self.states);
        for id in ids {
            match old.iter().position(|v| v.id == id) {
                Some(i) => self.states.push(old.swap_remove(i)),
                None => self.states.push(ViewState::new(id)),
            }
        }
        self.removed.extend(old.iter().map(|v| v.id));
    }

    /// New layout or acquisition: every view starts again (and leaves review).
    pub fn reset_all(&mut self) {
        for v in &mut self.states {
            v.reset();
            v.review = None;
        }
    }

    pub fn colors_changed(&mut self) {
        for v in &mut self.states {
            v.colors_dirty = true;
        }
    }

    /// Pull new display blocks into every view (each keeps at most one window of its own).
    pub fn drain(&mut self) {
        self.sync_states();
        let Some(l) = &self.layout else { return };
        let nch = l.num_channels();
        let fs = l.sample_rate;
        if nch != self.nch {
            self.nch = nch;
            self.reset_all();
        }
        let Some(rx) = &self.rx else { return };
        let mut newest: Option<Instant> = None;
        let blocks: Vec<Arc<Block>> = rx.try_iter().filter(|b| b.num_channels == nch).collect();
        for b in &blocks {
            newest = b.acquired_at.or(newest);
            self.newest_sample = self.newest_sample.max(b.first_sample + b.len() as i64);
        }
        for r in self.reader.rx.try_iter() {
            if let Some(rv) = self.states.iter_mut().find(|v| v.id == r.view).and_then(|v| v.review.as_mut()) {
                rv.accept(r);
            }
        }
        for (vs, v) in self.states.iter_mut().zip(&self.settings) {
            // Capped at the live maximum: a longer review time base must not grow the live buffer.
            let window = (v.window_s.min(LIVE_MAX_S) as f64 * fs).max(1.0) as usize;
            for b in &blocks {
                vs.push(b, nch, window);
            }
        }
        if let Some(t) = newest {
            let ms = t.elapsed().as_secs_f32() * 1e3;
            self.disp_latency = 0.9 * self.disp_latency + 0.1 * ms;
            self.disp_latency_max = self.disp_latency_max.max(ms);
        }
        if self.latency_window.elapsed() > Duration::from_secs(2) {
            self.latency_window = Instant::now();
            self.disp_latency_max = 0.0;
        }
    }

    /// Log when an open trace window stops being drawn, or stops consuming samples, while
    /// acquiring (and when it recovers).
    pub fn check_stalls(&mut self) {
        let acquiring = self.acquiring;
        for (vs, v) in self.states.iter_mut().zip(&self.settings) {
            let since = vs.last_drawn.elapsed().max(vs.last_data.elapsed());
            if acquiring && v.open && since > Duration::from_secs(2) {
                if !vs.stalled {
                    log::warn!(
                        "trace window '{}' stalled: last drawn {:.1} s ago, last samples {:.1} s ago, {} samples pending",
                        v.name,
                        vs.last_drawn.elapsed().as_secs_f32(),
                        vs.last_data.elapsed().as_secs_f32(),
                        vs.pending.len() / self.nch.max(1)
                    );
                    vs.stalled = true;
                }
            } else if vs.stalled && since < Duration::from_millis(500) {
                log::info!("trace window '{}' running again", v.name);
                vs.stalled = false;
            }
            if !acquiring || !v.open {
                vs.last_drawn = Instant::now();
                vs.last_data = Instant::now();
            }
        }
    }

    /// One trace window's whole UI (a deferred viewport callback).
    pub fn window(&mut self, ui: &mut egui::Ui, id: u32) {
        self.drain();
        let Some(vi) = self.settings.iter().position(|v| v.id == id) else { return };
        if vi >= self.states.len() {
            return;
        }
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            // Closing a trace window only hides it (reopen from "Trace windows").
            self.settings[vi].open = false;
            self.dirty = true;
        }
        egui::Panel::top(("view-controls", id)).show(ui, |ui| self.view_controls(ui, vi));
        egui::CentralPanel::default().show(ui, |ui| {
            let full = ui.available_rect_before_wrap();
            self.trace_view(ui, full, vi);
            ui.allocate_rect(full, egui::Sense::hover());
        });
        if self.car_open == Some(id) {
            self.car_palette(ui.ctx(), id);
        }
        // Pace this window's own frames (continuous while acquiring).
        let period = Duration::from_secs_f32(1.0 / self.max_fps.max(10.0));
        ui.ctx().request_repaint_after(if self.acquiring { period } else { Duration::from_millis(250) });
    }

    /// Click on a channel label: monitor just these channels; Ctrl (or Shift) toggles them in
    /// or out of the mix.
    fn monitor_click(&mut self, ui: &egui::Ui, chs: Vec<usize>) {
        let add = ui.input(|i| i.modifiers.command || i.modifiers.shift);
        apply_monitor_click(&mut self.audio.lock(), chs, add);
        self.audio_stats.recalibrate.store(true, Ordering::Relaxed);
        self.monitor_changed = true;
        self.colors_changed();
    }

    /// The control bar at the top of a trace window.
    fn view_controls(&mut self, ui: &mut egui::Ui, vi: usize) {
        let l = self.layout.clone();
        let id = self.settings[vi].id;
        let reviewing = self.states[vi].review.is_some();
        let d = &mut self.settings[vi];
        let old = d.clone();
        let mut clear_sweeps = false;
        let mut open_car = false;
        ui.horizontal_wrapped(|ui| {
            ui.toggle_value(&mut d.controls, "Controls").on_hover_text("Show or hide this window's settings");
            ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(90.0));
            ui.separator();
            if let Some(l) = &l {
                ui.label("Channels");
                let all = d.headstages.is_empty();
                if ui.selectable_label(all, "all").clicked() {
                    d.headstages.clear();
                }
                for h in &l.headstages {
                    let on = !all && d.headstages.contains(&h.name);
                    if ui.selectable_label(on, &h.name).on_hover_text(format!("{} {} ({} ch)", h.name, h.chip, h.num_channels)).clicked() {
                        if on {
                            d.headstages.retain(|n| n != &h.name);
                        } else {
                            d.headstages.push(h.name.clone());
                        }
                    }
                }
                if l.channels.iter().any(|c| c.kind == ChannelKind::Aux) {
                    ui.toggle_value(&mut d.aux, "AUX");
                }
                if l.channels.iter().any(|c| c.kind == ChannelKind::Adc) {
                    ui.toggle_value(&mut d.adc, "ADC");
                }
                ui.separator();
            }
            ui.selectable_value(&mut d.filter, ViewFilter::Raw, "Raw");
            ui.selectable_value(&mut d.filter, ViewFilter::Spikes, "300–6k");
            ui.selectable_value(&mut d.filter, ViewFilter::Lfp, "LFP");
            ui.separator();
            ui.add_enabled(d.filter == ViewFilter::Spikes, egui::Checkbox::new(&mut d.car.enabled, "CAR")).on_hover_text("Display-only common average reference, in the 300–6k view");
            if ui.small_button("Channels…").on_hover_text(format!("CAR reference {} · subtract {}", d.car.reference.len(), d.car.subtract.len())).clicked() {
                open_car = true;
            }
        });
        if d.controls {
            ui.horizontal_wrapped(|ui| {
                // One time base for live and review; review reaches further (5 ms to 60 s).
                let range = if reviewing { REVIEW_MIN_S..=REVIEW_MAX_S } else { LIVE_MIN_S..=LIVE_MAX_S };
                ui.add(egui::DragValue::new(&mut d.window_s).range(range).speed(0.02).suffix(" s")).on_hover_text(if reviewing { "Time window (review: 5 ms – 60 s)" } else { "Time window" });
                ui.add(egui::DragValue::new(&mut d.uv_per_row).range(10.0..=5000.0).speed(2.0).suffix(" µV/row"));
                ui.add(egui::DragValue::new(&mut d.rows).range(1..=512).prefix("rows "));
                ui.label("per row");
                for n in [1u32, 2, 4, 8] {
                    ui.selectable_value(&mut d.per_row, n, n.to_string());
                }
                ui.add(egui::DragValue::new(&mut d.thickness).range(1.0..=3.0).speed(0.05).suffix(" px"));
                if d.adc {
                    ui.add(egui::DragValue::new(&mut d.adc_v_per_row).range(0.1..=10.0).speed(0.05).suffix(" V/row ADC"));
                }
                ui.separator();
                ui.selectable_value(&mut d.triggered, false, "Free");
                ui.selectable_value(&mut d.triggered, true, "Triggered").on_hover_text("Each sweep starts at a TTL rising edge; recent sweeps are overlaid (opto-tagging)");
                if d.triggered {
                    let nlines = l.as_ref().map(|l| l.ttl_lines).unwrap_or(8).max(1);
                    egui::ComboBox::from_id_salt(("trigline", id)).width(56.0).selected_text(format!("TTL{}", d.trig_line + 1)).show_ui(ui, |ui| {
                        for i in 0..nlines {
                            ui.selectable_value(&mut d.trig_line, i, format!("TTL{}", i + 1));
                        }
                    });
                    ui.add(egui::DragValue::new(&mut d.trig_pre_ms).range(0.0..=200.0).speed(0.5).prefix("−").suffix(" ms"));
                    ui.add(egui::DragValue::new(&mut d.trig_post_ms).range(1.0..=250.0).speed(0.5).prefix("+").suffix(" ms"));
                    ui.add(egui::DragValue::new(&mut d.trig_sweeps).range(1..=50).suffix(" sweeps"));
                    if ui.small_button("Clear").clicked() {
                        clear_sweeps = true;
                    }
                } else {
                    ui.checkbox(&mut d.ttl_overlay, "TTL overlay").on_hover_text("Shade traces while a TTL input is high; one lane per line seen high since acquisition start");
                }
            });
        }
        let reset = d.window_s != old.window_s || d.headstages != old.headstages || d.aux != old.aux || d.adc != old.adc;
        let changed = *d != old;
        // In review, a new time window re-reads the same moment (kept centred) at that span.
        if d.window_s != old.window_s
            && let (Some(rv), Some(l)) = (self.states[vi].review.as_mut(), &l)
        {
            let span = (d.window_s as f64 * l.sample_rate).round().max(1.0) as i64;
            let centre = rv.t0 + rv.span as i64 / 2;
            rv.span = span as u32;
            rv.t0 = centre - span / 2;
        }
        if open_car {
            self.car_open = Some(id);
        }
        if clear_sweeps {
            self.states[vi].sweeps.clear();
        }
        if reset {
            self.states[vi].reset();
        }
        if changed {
            self.dirty = true;
        }
    }

    fn trace_view(&mut self, ui: &mut egui::Ui, rect: egui::Rect, vi: usize) {
        let Some(l) = self.layout.clone() else {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "No board", egui::FontId::proportional(18.0), Color32::GRAY);
            return;
        };
        let nch = l.num_channels();
        if nch == 0 {
            return;
        }
        let d = self.settings[vi].clone();
        let (row_ch, rowtab) = view_rows(&l, &d);
        if rowtab.is_empty() {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "No channels selected", egui::FontId::proportional(16.0), Color32::GRAY);
            // Nothing to draw: drop the staged samples and count the window as drawn.
            let vs = &mut self.states[vi];
            vs.reset();
            vs.last_drawn = Instant::now();
            vs.last_data = Instant::now();
            return;
        }
        let ppp = ui.ctx().pixels_per_point();
        let label_w = 64.0;
        let trig = d.triggered;
        let fs = l.sample_rate;
        let k = d.per_row.clamp(1, 8) as usize;
        let nrows_all = rowtab.len();
        let vs = &mut self.states[vi];
        let reviewing = !trig && vs.review.is_some();
        // Free-running: TTL lanes under the traces (lines seen live or in the review image).
        // Triggered: a time axis instead. Review adds a time axis and the recording strip.
        let ttl_lines: Vec<usize> = if d.ttl_overlay && !trig {
            let seen = vs.ttl.seen() | vs.review.as_ref().and_then(|r| r.shown.as_ref()).map_or(0, |s| s.ttl.seen());
            (0..l.ttl_lines.min(16)).filter(|&i| seen & (1 << i) != 0).collect()
        } else {
            Vec::new()
        };
        let ttl_h = if ttl_lines.is_empty() { 0.0 } else { 7.0 * ttl_lines.len() as f32 + 2.0 };
        let strip_h = if trig { 14.0 } else if reviewing { ttl_h + 26.0 } else { ttl_h };
        let plot = egui::Rect::from_min_max(egui::pos2(rect.min.x + label_w, rect.min.y), egui::pos2(rect.max.x, rect.max.y - strip_h));
        let strip = egui::Rect::from_min_max(egui::pos2(plot.min.x, plot.max.y + 2.0), rect.max);
        let rows = d.rows.clamp(1, nrows_all);
        let first = d.first_row.min(nrows_all - rows);
        // Interaction: wheel scrolls rows, Ctrl+wheel scales; scroll-back review with a drag,
        // Shift+wheel or a sideways swipe, Alt+wheel to zoom, arrows, Home and End.
        let resp = ui.interact(rect, ui.scope_id().with(("traces", d.id)), egui::Sense::click_and_drag());
        if resp.hovered() {
            let (scroll, ctrl, alt) = ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers.ctrl, i.modifiers.alt));
            if scroll != 0.0 && !alt {
                let v = &mut self.settings[vi];
                if ctrl {
                    v.uv_per_row = (d.uv_per_row * (1.0 - scroll * 0.002)).clamp(10.0, 5000.0);
                } else {
                    let step = (scroll / 20.0).round() as isize;
                    v.first_row = (first as isize - step).clamp(0, (nrows_all - rows) as isize) as usize;
                }
                self.dirty = true;
            }
        }
        let cols = (plot.width() * ppp).floor().max(1.0) as u32;
        if !trig {
            let nav = nav_input(ui, &resp, plot);
            if nav.any() {
                self.apply_nav(vi, nav, cols);
            }
        }
        let in_review = !trig && self.states[vi].review.is_some();
        let window = (d.window_s as f64 * fs).round().max(cols as f64) as u32;
        let sections: Vec<[f32; 5]> = match d.filter {
            ViewFilter::Raw => vec![],
            ViewFilter::Spikes => BandSpec::SPIKES.sections(fs),
            ViewFilter::Lfp => BandSpec::LFP.sections(fs),
        }
        .into_iter()
        .take(MAX_SECTIONS)
        .map(|b| [b.b0, b.b1, b.b2, b.a1, b.a2])
        .collect();
        if in_review {
            let g = ReviewGeom { rect, plot, strip, ttl_h, label_w, cols, rows, first, k, ppp };
            self.review_view(ui, &resp, vi, &l, &d, &g, sections, &row_ch, &rowtab, &ttl_lines);
            return;
        }
        let vs = &mut self.states[vi];
        // Triggered window in samples; together they must fit well inside the GPU ring.
        let pre = ((d.trig_pre_ms as f64 * 1e-3 * fs).round() as u32).min(RING / 4);
        let post = ((d.trig_post_ms as f64 * 1e-3 * fs).round() as u32).clamp(1, RING / 4);
        let span = pre + post;
        let nslots = if trig { d.trig_sweeps.clamp(1, 50) } else { 0 };
        let config = TraceConfig { nch, cols, window, sections, nslots };
        vs.last_drawn = Instant::now();
        if vs.last_cfg.as_ref() != Some(&config) {
            if let Some(c) = vs.last_cfg.as_ref().filter(|c| c.cols != config.cols || c.window != config.window || c.nch != config.nch || c.nslots != config.nslots) {
                log::info!("trace window '{}': {} → {} columns, window {} → {} samples; GPU state rebuilt", d.name, c.cols, config.cols, c.window, config.window);
                vs.reset = true;
            }
            vs.last_cfg = Some(config.clone());
        }
        // Old sweeps are meaningless after a change of window, line, filter or geometry.
        let sweep_key = (pre, post, d.trig_line, nslots, d.filter, cols);
        if vs.reset || vs.sweep_key != Some(sweep_key) {
            vs.sweeps.clear();
            vs.sweep_key = Some(sweep_key);
        }
        // Upload a backlog (e.g. after the window was hidden) in chunks: the GPU staging buffers
        // are sized for one frame's upload, and a whole 20 s window at once would exceed the
        // 128 MB storage-binding limit. 32768 samples (≤ 8 MB) per frame catches up 20 s in ~18 frames.
        let max_chunk = (32768usize).min((8 << 20) / (2 * nch)).max(1);
        let n_new = (vs.pending.len() / nch).min(max_chunk) as u32;
        if (vs.pending.len() / nch) > n_new as usize {
            ui.ctx().request_repaint();
        }
        if n_new > 0 {
            vs.last_data = Instant::now();
        }
        let start = vs.counter;
        let pos0 = (start % window as u64) as u32;
        vs.counter += n_new as u64;
        // Fold TTL words into columns with the shader's mapping (a size/window change
        // clears columns, like the GPU state), and find trigger edges.
        if vs.reset {
            vs.ttl.clear_cols();
        }
        let words: Vec<u16> = vs.pending_ttl.drain(..(n_new as usize).min(vs.pending_ttl.len())).collect();
        let words = &words[..];
        vs.ttl.ingest(words, pos0, window, cols);
        let sweep = if trig {
            vs.sweeps.scan(words, start, d.trig_line);
            let cuts = vs.sweeps.take_ready(vs.counter, pre, post, nslots, RING, MAX_CUTS);
            SweepFrame { ring0: (start % RING as u64) as u32, span, cuts, newest: vs.sweeps.newest_slot(nslots), filled: vs.sweeps.filled(nslots) }
        } else {
            SweepFrame::default()
        };
        let cursor_pos = (vs.counter % window as u64) as f64;
        let cursor_col = ((cursor_pos * cols as f64 / window as f64) as u32).min(cols - 1);
        let upload: Arc<Vec<i16>> = Arc::new(vs.pending.drain(..n_new as usize * nch).collect());
        let row_h_px = plot.height() * ppp / rows as f32;
        // Per-channel scaling (see traces.wgsl): electrodes follow the view's µV/row and
        // display filter; AUX/ADC are drawn raw in volts on their own row range.
        // Flags: bit 0 display filter, bit 1 CAR reference, bit 2 CAR subtract (electrodes).
        let car_on = d.car.enabled && d.filter == ViewFilter::Spikes;
        let info = view_chinfo(&l, &d);
        let tables_key = (row_ch.clone(), rowtab.clone(), d.adc_v_per_row.to_bits());
        let send_tables = vs.reset || vs.tables_key.as_ref() != Some(&tables_key);
        if vs.tables_key.as_ref().is_some_and(|t| t.1 != tables_key.1) {
            vs.colors_dirty = true;
        }
        vs.tables_key = Some(tables_key);
        let chinfo = (send_tables || vs.last_chinfo.as_ref() != Some(&info)).then(|| {
            vs.last_chinfo = Some(info.clone());
            Arc::new(info)
        });
        let order = send_tables.then(|| Arc::new(row_ch.clone()));
        let rowtab_gpu = send_tables.then(|| Arc::new(rowtab.clone()));
        let want_colors = vs.reset || vs.colors_dirty;
        vs.colors_dirty = false;
        let reset = std::mem::take(&mut vs.reset);
        let colors = want_colors.then(|| Arc::new(self.view_colors(&l, &row_ch, &rowtab)));
        let frame = TraceFrame {
            view: d.id,
            config,
            reset,
            upload,
            n_new,
            pos0,
            cursor_col,
            first_row: first as u32,
            nrows: rows as u32,
            kmax: k as u32,
            uv_per_row: d.uv_per_row,
            row_h_px,
            thickness_px: d.thickness * ppp.max(1.0),
            colors,
            order,
            chinfo,
            rowtab: rowtab_gpu,
            sweep,
            car: car_on,
            gap: None,
            paint: true,
        };
        ui.painter().rect_filled(rect, 0.0, Color32::from_rgb(12, 13, 16));
        let p = ui.painter_at(rect);
        let span_ms = d.trig_pre_ms + d.trig_post_ms;
        let t_to_x = |t_ms: f32| plot.min.x + (t_ms + d.trig_pre_ms) / span_ms.max(1e-3) * plot.width();
        let vs = &self.states[vi];
        if trig && vs.sweeps.count() > 0 {
            // Laser-on shading under the traces: newest sweep's TTL high time.
            let w_ms = vs.sweeps.newest_width().map(|w| w as f32 * 1e3 / fs as f32).unwrap_or(d.trig_post_ms).min(d.trig_post_ms);
            let c = crate::ttl::line_color(d.trig_line);
            p.rect_filled(egui::Rect::from_min_max(egui::pos2(t_to_x(0.0), plot.min.y), egui::pos2(t_to_x(w_ms).max(t_to_x(0.0) + 1.0), plot.max.y)), 0.0, Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 34));
        }
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(plot, TraceCallback(frame)));
        // Overlay: labels, separators, cursor / time axis.
        let analog_visible = self.row_labels(ui, &p, rect, plot, rows, first, &row_ch, &rowtab, &l, &resp, label_w);
        let vs = &self.states[vi];
        let dim = Color32::from_gray(120);
        if let Some((until, msg)) = &vs.note
            && Instant::now() < *until
        {
            p.text(plot.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::proportional(15.0), Color32::from_rgb(230, 200, 120));
        }
        if trig {
            // t = 0 line and a time axis in ms.
            let x0 = t_to_x(0.0);
            p.line_segment([egui::pos2(x0, plot.min.y), egui::pos2(x0, plot.max.y)], Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 70)));
            let step = [1.0f32, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0].into_iter().find(|s| span_ms / s <= 12.0).unwrap_or(500.0);
            let mut t = -(d.trig_pre_ms / step).floor() * step;
            while t <= d.trig_post_ms + 1e-3 {
                let x = t_to_x(t);
                p.line_segment([egui::pos2(x, plot.max.y), egui::pos2(x, plot.max.y + 4.0)], Stroke::new(1.0, dim));
                p.text(egui::pos2(x, rect.max.y - 1.0), egui::Align2::CENTER_BOTTOM, format!("{t:.0}"), egui::FontId::monospace(10.0), dim);
                t += step;
            }
            p.text(egui::pos2(rect.min.x + 4.0, rect.max.y - 1.0), egui::Align2::LEFT_BOTTOM, "ms", egui::FontId::monospace(10.0), dim);
            let waiting = if vs.sweeps.count() == 0 { format!(" · waiting for TTL{}", d.trig_line + 1) } else { String::new() };
            p.text(
                egui::pos2(plot.max.x - 4.0, plot.max.y - 4.0),
                egui::Align2::RIGHT_BOTTOM,
                format!("TTL{} rising · {} / {} sweeps · {:.0} µV/row{}{}", d.trig_line + 1, vs.sweeps.filled(nslots), nslots, d.uv_per_row, if car_on { " · CAR" } else { "" }, waiting),
                egui::FontId::monospace(11.0),
                dim,
            );
        } else {
            vs.ttl.draw(&p, &TtlGeom { plot, strip, label_x: rect.min.x + 4.0, ppp, cursor: cursor_col, gap: None }, &ttl_lines);
            let x = plot.min.x + cursor_col as f32 / ppp;
            p.line_segment([egui::pos2(x, plot.min.y), egui::pos2(x, plot.max.y)], Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 40)));
            p.text(egui::pos2(plot.max.x - 4.0, plot.max.y - 4.0), egui::Align2::RIGHT_BOTTOM, format!("{:.2} s · {:.0} µV/row{}{}", d.window_s, d.uv_per_row, if car_on { " · CAR" } else { "" }, if analog_visible { format!(" · ADC {:.2} V/row · AUX 2.5 V/row", d.adc_v_per_row) } else { String::new() }), egui::FontId::monospace(11.0), dim);
        }
    }

    /// Enter, move or leave scroll-back review in view `vi`.
    fn apply_nav(&mut self, vi: usize, nav: Nav, cols: u32) {
        let fs = self.layout.as_ref().map_or(30000.0, |l| l.sample_rate);
        let window_s = self.settings[vi].window_s as f64;
        let nch = self.nch;
        let vs = &mut self.states[vi];
        if nav.live {
            self.leave_review(vi);
            return;
        }
        // Live: zooming sets the time base, exactly like typing into the time box. It never
        // enters review; only panning back or Home does.
        if vs.review.is_none() && nav.zoom != 0.0 {
            let w = (window_s * nav.zoom).clamp(LIVE_MIN_S as f64, LIVE_MAX_S as f64) as f32;
            if w != self.settings[vi].window_s {
                self.settings[vi].window_s = w;
                self.dirty = true;
            }
        }
        let note = |vs: &mut ViewState, msg: String| vs.note = Some((Instant::now() + Duration::from_secs(4), msg));
        let Some(src) = self.rec_source.clone() else {
            if nav.goes_back() {
                note(vs, "Scroll-back reads the recording: start recording to review".into());
            }
            return;
        };
        if src.nch != nch {
            if nav.goes_back() {
                note(vs, format!("The last recording has {} channels; the board now sends {nch}", src.nch));
            }
            return;
        }
        let end = src.first_sample + review::available(&src);
        let (min_span, max_span) = review_span_limits(cols, fs);
        let rv = match vs.review.as_mut() {
            Some(rv) => rv,
            None => {
                if !nav.goes_back() {
                    return;
                }
                let span = ((window_s * fs).round() as u32).clamp(min_span, max_span);
                vs.review.insert(ReviewView::new(end - span as i64, span))
            }
        };
        if nav.zoom != 0.0 {
            rv.zoom(nav.zoom, nav.zoom_at);
        }
        rv.t0 += (nav.pan * rv.span as f64).round() as i64;
        if nav.home {
            rv.t0 = src.first_sample;
        }
        rv.clamp(src.first_sample, end, min_span, max_span);
        // Zooming sets the window's time base, so the box shows what is on screen.
        if nav.zoom != 0.0 {
            self.settings[vi].window_s = (rv.span as f64 / fs) as f32;
            self.dirty = true;
        }
    }

    /// Back to live; the time base is brought into the live range (review reaches further).
    fn leave_review(&mut self, vi: usize) {
        self.states[vi].review = None;
        let w = &mut self.settings[vi].window_s;
        let live = w.clamp(LIVE_MIN_S, LIVE_MAX_S);
        if live != *w {
            *w = live;
            self.dirty = true;
        }
    }

    /// One frame of a window in review: read and filter the stretch on screen (double
    /// buffered on the GPU), then draw it with labels, TTL lanes, a time axis, a "behind live"
    /// badge and a strip for the whole recording (click or drag to jump).
    #[allow(clippy::too_many_arguments)]
    fn review_view(&mut self, ui: &mut egui::Ui, resp: &egui::Response, vi: usize, l: &StreamLayout, d: &TraceViewSettings, g: &ReviewGeom, sections: Vec<[f32; 5]>, row_ch: &[u32], rowtab: &[[u32; 2]], ttl_lines: &[usize]) {
        let ReviewGeom { rect, plot, strip, ttl_h, label_w, cols, rows, first, k, ppp } = *g;
        let fs = l.sample_rate;
        let nch = l.num_channels();
        let Some(src) = self.rec_source.clone().filter(|s| s.nch == nch) else {
            self.leave_review(vi);
            return;
        };
        let end = src.first_sample + review::available(&src);
        let (min_span, max_span) = review_span_limits(cols, fs);
        let car_on = d.car.enabled && d.filter == ViewFilter::Spikes;
        let info = Arc::new(view_chinfo(l, d));
        let order = Arc::new(row_ch.to_vec());
        let rowtab_gpu = Arc::new(rowtab.to_vec());
        let colors = Arc::new(self.view_colors(l, row_ch, rowtab));
        let lead = (review::lead_in_s(d.filter) * fs).round() as i64;
        let id = d.id;
        let newest = self.newest_sample;
        let vs = &mut self.states[vi];
        // Not stalled: this window is drawing, just not the live samples.
        vs.last_drawn = Instant::now();
        vs.last_data = Instant::now();
        let Some(rv) = vs.review.as_mut() else { return };
        rv.clamp(src.first_sample, end, min_span, max_span);
        let key = review::Key { t0: rv.t0, span: rv.span, cols, nch, filter: d.filter, car: car_on.then(|| (d.car.reference.clone(), d.car.subtract.clone())) };
        if rv.shown.as_ref().is_none_or(|s| s.key != key) && rv.filling.as_ref().is_none_or(|f| f.key != key) {
            rv.generation += 1;
            let buf = rv.shown.as_ref().map_or(0, |s| 1 - s.buf);
            let req = review::Req { view: id, generation: rv.generation, src: src.clone(), from: rv.t0 - lead, n: (lead + rv.span as i64) as usize, words_from: rv.t0, words_n: rv.span as usize };
            let _ = self.reader.tx.send(req);
            rv.filling = Some(review::Filling { key: key.clone(), generation: rv.generation, buf, loaded: None });
        }
        let row_h_px = plot.height() * ppp / rows as f32;
        let frame = |view: u32, config: TraceConfig, reset: bool, upload: Arc<Vec<i16>>, n_new: u32, pos0: u32, paint: bool| TraceFrame {
            view,
            config,
            reset,
            upload,
            n_new,
            pos0,
            cursor_col: 0,
            first_row: first as u32,
            nrows: rows as u32,
            kmax: k as u32,
            uv_per_row: d.uv_per_row,
            row_h_px,
            thickness_px: d.thickness * ppp.max(1.0),
            colors: Some(colors.clone()),
            order: Some(order.clone()),
            chinfo: Some(info.clone()),
            rowtab: Some(rowtab_gpu.clone()),
            sweep: SweepFrame::default(),
            car: car_on,
            gap: Some(0),
            paint,
        };
        ui.painter().rect_filled(rect, 0.0, Color32::from_rgb(12, 13, 16));
        // Feed the back buffer (in chunks small enough for the GPU's binding limit).
        let mut complete = false;
        if let Some(f) = rv.filling.as_mut()
            && let Some(ld) = f.loaded.as_mut()
        {
            let total = ld.data.len() / nch.max(1);
            let chunk = ((8usize << 20) / nch.max(1)).max(1).min(total - ld.fed);
            let span = f.key.span.max(1);
            let config = TraceConfig { nch, cols: f.key.cols, window: span, sections: sections.clone(), nslots: 0 };
            let upload = Arc::new(ld.data[ld.fed * nch..(ld.fed + chunk) * nch].to_vec());
            let pos0 = ((ld.pos0 as u64 + ld.fed as u64) % span as u64) as u32;
            let fr = frame(review_key(id, f.buf), config, ld.fed == 0, upload, chunk as u32, pos0, false);
            ui.painter().add(egui_wgpu::Callback::new_paint_callback(plot, TraceCallback(fr)));
            ld.fed += chunk;
            complete = ld.fed >= total;
            if !complete {
                ui.ctx().request_repaint();
            }
        }
        if complete && let Some(f) = rv.filling.take() {
            let ld = f.loaded.expect("complete implies loaded");
            let mut ttl = TtlOverlay::default();
            ttl.reset();
            if !ld.words.is_empty() {
                ttl.ingest(&ld.words, 0, f.key.span.max(1), f.key.cols);
            }
            let config = TraceConfig { nch, cols: f.key.cols, window: f.key.span.max(1), sections: sections.clone(), nslots: 0 };
            rv.shown = Some(review::Shown { key: f.key, buf: f.buf, config, ttl });
        }
        if let Some(sh) = rv.shown.as_ref() {
            let fr = frame(review_key(id, sh.buf), sh.config.clone(), false, Arc::new(Vec::new()), 0, 0, true);
            ui.painter().add(egui_wgpu::Callback::new_paint_callback(plot, TraceCallback(fr)));
        }
        // Overlay.
        let p = ui.painter_at(rect);
        let dim = Color32::from_gray(120);
        let accent = Color32::from_rgb(120, 180, 255);
        let (t0, span) = (rv.t0, rv.span as i64);
        let rel = |s: i64| (s - src.first_sample) as f64 / fs;
        let ttl_strip = egui::Rect::from_min_max(strip.min, egui::pos2(strip.max.x, strip.min.y + ttl_h));
        if let Some(sh) = rv.shown.as_ref() {
            sh.ttl.draw(&p, &TtlGeom { plot, strip: ttl_strip, label_x: rect.min.x + 4.0, ppp, cursor: 0, gap: Some(0) }, ttl_lines);
        } else {
            p.text(plot.center(), egui::Align2::CENTER_CENTER, "Reading the recording…", egui::FontId::proportional(15.0), dim);
        }
        // Time axis (time since the recording started).
        let axis_y = strip.min.y + ttl_h + 1.0;
        let span_s = span as f64 / fs;
        let step = [0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0].into_iter().find(|s| span_s / s <= 8.0).unwrap_or(120.0);
        let x_of = |s: i64| plot.min.x + ((s - t0) as f64 / span.max(1) as f64) as f32 * plot.width();
        let mut t = (rel(t0) / step).ceil() * step;
        while t <= rel(t0 + span) + 1e-9 {
            let x = x_of(src.first_sample + (t * fs).round() as i64);
            p.line_segment([egui::pos2(x, axis_y), egui::pos2(x, axis_y + 4.0)], Stroke::new(1.0, dim));
            p.text(egui::pos2(x, axis_y + 4.0), egui::Align2::CENTER_TOP, review::fmt_time(t, step < 0.1), egui::FontId::monospace(10.0), dim);
            t += step;
        }
        // The whole recording so far, with the stretch on screen.
        let ov = egui::Rect::from_min_max(egui::pos2(plot.min.x, rect.max.y - 7.0), egui::pos2(plot.max.x, rect.max.y - 1.0));
        let total = (end - src.first_sample).max(1) as f64;
        let ox = |s: i64| ov.min.x + ((s - src.first_sample) as f64 / total).clamp(0.0, 1.0) as f32 * ov.width();
        p.rect_filled(ov, 2.0, Color32::from_gray(38));
        let (bx0, bx1) = (ox(t0), ox(t0 + span));
        p.rect_filled(egui::Rect::from_min_max(egui::pos2(bx0, ov.min.y - 1.0), egui::pos2(bx1.max(bx0 + 3.0), ov.max.y + 1.0)), 1.0, accent);
        let or = ui.interact(ov.expand2(egui::vec2(0.0, 3.0)), ui.scope_id().with(("overview", id)), egui::Sense::click_and_drag()).on_hover_text("The whole recording: click or drag to jump");
        if (or.clicked() || or.dragged())
            && let Some(pos) = or.interact_pointer_pos()
        {
            rv.t0 = src.first_sample + (((pos.x - ov.min.x) / ov.width()) as f64 * total) as i64 - span / 2;
            rv.clamp(src.first_sample, end, min_span, max_span);
        }
        // Badge and the way back.
        let behind = (newest - (t0 + span)).max(0) as f64 / fs;
        let loading = if rv.filling.is_some() { " · reading" } else { "" };
        let err = rv.error.clone();
        let badge = format!("REVIEW  {} – {}  ·  {} behind live{loading}", review::fmt_time(rel(t0), span_s < 1.0), review::fmt_time(rel(t0 + span), span_s < 1.0), review::fmt_time(behind, false));
        let live_r = egui::Rect::from_min_size(egui::pos2(plot.max.x - 74.0, plot.min.y + 4.0), egui::vec2(70.0, 20.0));
        p.text(egui::pos2(live_r.min.x - 8.0, live_r.center().y), egui::Align2::RIGHT_CENTER, badge, egui::FontId::monospace(11.0), accent);
        let go_live = ui.put(live_r, egui::Button::new("LIVE ▸")).on_hover_text("Back to live (End)").clicked();
        if let Some(e) = err {
            p.text(plot.center(), egui::Align2::CENTER_CENTER, e, egui::FontId::proportional(13.0), Color32::from_rgb(240, 120, 100));
        }
        p.text(
            egui::pos2(plot.max.x - 4.0, plot.max.y - 4.0),
            egui::Align2::RIGHT_BOTTOM,
            format!("{} · {:.0} µV/row{}", if span_s < 1.0 { format!("{:.0} ms", span_s * 1e3) } else { format!("{span_s:.2} s") }, d.uv_per_row, if car_on { " · CAR" } else { "" }),
            egui::FontId::monospace(11.0),
            dim,
        );
        let _ = self.row_labels(ui, &p, rect, plot, rows, first, row_ch, rowtab, l, resp, label_w);
        if go_live {
            self.leave_review(vi);
        }
    }

    /// Channel labels down the left edge of a trace plot; clicking one monitors it on audio
    /// (Ctrl/Shift adds or removes it). Returns whether an AUX/ADC row is visible.
    #[allow(clippy::too_many_arguments)]
    fn row_labels(&mut self, ui: &egui::Ui, p: &egui::Painter, rect: egui::Rect, plot: egui::Rect, rows: usize, first: usize, row_ch: &[u32], rowtab: &[[u32; 2]], l: &StreamLayout, resp: &egui::Response, label_w: f32) -> bool {
        let rh = plot.height() / rows as f32;
        let mon = self.audio.lock().channels.clone();
        let click = resp.clicked().then(|| resp.interact_pointer_pos()).flatten();
        let mut toggle: Option<Vec<usize>> = None;
        let mut analog_visible = false;
        for r in 0..rows {
            let [s0, n] = rowtab[first + r];
            let chans: Vec<usize> = (s0..s0 + n).map(|i| row_ch[i as usize] as usize).collect();
            let ch = chans[0];
            let is_electrode = l.channels[ch].kind == ChannelKind::Electrode;
            analog_visible |= !is_electrode;
            let y = plot.min.y + (r as f32 + 0.5) * rh;
            let lab = egui::Rect::from_min_size(egui::pos2(rect.min.x, y - rh / 2.0), egui::vec2(label_w, rh));
            let monitored = chans.iter().any(|c| mon.contains(c));
            if rh >= 9.0 {
                let name = if chans.len() == 1 { l.channels[ch].name.clone() } else { format!("{}–{}", l.channels[ch].name, l.channels[*chans.last().unwrap()].name.trim_start_matches("CH")) };
                let txt = format!("{name}{}", if monitored { " ♪" } else { "" });
                let col = chans.iter().find_map(|&c| monitor_color32(&mon, c)).unwrap_or(Color32::from_gray(150));
                p.text(egui::pos2(rect.min.x + 4.0, y), egui::Align2::LEFT_CENTER, txt, egui::FontId::monospace((rh * 0.6).clamp(8.0, 13.0)), col);
            }
            // Audio monitors electrodes only; a grouped row monitors the whole group.
            if let Some(pos) = click
                && is_electrode
                && lab.contains(pos)
            {
                toggle = Some(chans);
            }
        }
        if let Some(chs) = toggle {
            self.monitor_click(ui, chs);
        }
        analog_visible
    }

    /// Trace colours for one view: monitored channels by stereo position; electrodes sharing a
    /// row by their position in the row; otherwise by headstage.
    fn view_colors(&self, l: &StreamLayout, order: &[u32], rowtab: &[[u32; 2]]) -> Vec<[f32; 4]> {
        let mon = self.audio.lock().channels.clone();
        let mut c = channel_colors(l, &mon, 1);
        for &[s0, n] in rowtab {
            if n > 1 {
                for j in 0..n {
                    let ch = order[(s0 + j) as usize] as usize;
                    if !mon.contains(&ch) {
                        let p = GROUP_PALETTE[j as usize % GROUP_PALETTE.len()];
                        c[ch] = [p[0], p[1], p[2], 0.9];
                    }
                }
            }
        }
        c
    }

    /// This view's display-CAR window (only this view's electrodes).
    fn car_palette(&mut self, ctx: &egui::Context, id: u32) {
        let target = CarTarget::View(id);
        let Some(l) = self.layout.clone() else { return };
        let mut open = true;
        let mut changed = false;
        let Some(v) = self.settings.iter_mut().find(|v| v.id == id) else {
            self.car_open = None;
            return;
        };
        let (title, groups, car) = (format!("CAR: {}", v.name), electrode_groups(&l, &v.headstages), &mut v.car);
        let (mut spikes, mut audio) = (false, false);
        egui::Window::new(title).id(egui::Id::unique(("car-window", format!("{target:?}")))).open(&mut open).resizable(false).collapsible(false).default_width(460.0).show(ctx, |ui| {
            ui.small("Recordings always keep the original signals.");
            ui.horizontal(|ui| {
                ui.label("Apply to");
                match target {
                    CarTarget::Global => {
                        changed |= ui.checkbox(&mut spikes, "spike detection").changed();
                        changed |= ui.checkbox(&mut audio, "audio").changed();
                    }
                    CarTarget::View(_) => {
                        changed |= ui.checkbox(&mut car.enabled, "this window (300–6k view)").changed();
                    }
                }
            });
            ui.weak(format!("reference {} · subtract {}", car.reference.len(), car.subtract.len()));
            let mut copy_ref = false;
            for (title, set) in [("Reference (averaged)", &mut car.reference), ("Subtract from", &mut car.subtract)] {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.strong(title);
                    if title.starts_with("Subtract") && ui.small_button("same as reference").clicked() {
                        copy_ref = true;
                    }
                });
                for (gname, chans) in &groups {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(gname).small());
                        if ui.small_button("All").clicked() {
                            set.extend(chans.iter().map(|(i, _)| *i).filter(|i| !set.contains(i)).collect::<Vec<_>>());
                            changed = true;
                        }
                        if ui.small_button("None").clicked() {
                            set.retain(|x| !chans.iter().any(|(i, _)| i == x));
                            changed = true;
                        }
                    });
                    egui::Grid::new((title, gname.as_str())).spacing([2.0, 2.0]).show(ui, |ui| {
                        for (n, (i, name)) in chans.iter().enumerate() {
                            let on = set.contains(i);
                            let b = egui::Button::new(RichText::new(name.trim_start_matches("CH")).monospace().size(11.0)).selected(on).min_size(egui::vec2(26.0, 0.0));
                            if ui.add(b).on_hover_text(name.as_str()).clicked() {
                                if on {
                                    set.retain(|x| x != i);
                                } else {
                                    set.push(*i);
                                }
                                changed = true;
                            }
                            if n % 16 == 15 {
                                ui.end_row();
                            }
                        }
                    });
                }
            }
            if copy_ref {
                car.subtract = car.reference.clone();
                changed = true;
            }
        });
        if changed {
            car.reference.sort_unstable();
            car.subtract.sort_unstable();
            self.dirty = true;
        }
        if !open {
            self.car_open = None;
        }
    }
}

impl App {
    fn new_view_id(&self) -> u32 {
        self.shared.lock().settings.iter().map(|v| v.id + 1).max().unwrap_or(0)
    }

    /// Side panel: the list of trace windows.
    pub(super) fn trace_windows_section(&mut self, ui: &mut egui::Ui) {
        let ports: Vec<char> = self.engine.layout().map(|l| {
            let mut p: Vec<char> = l.headstages.iter().filter_map(|h| h.name.chars().next()).collect();
            p.dedup();
            p
        }).unwrap_or_default();
        let layout = self.layout();
        let next_id = self.new_view_id();
        egui::CollapsingHeader::new(RichText::new("Trace windows").strong()).id_salt("display_section").default_open(true).show(ui, |ui| {
            let mut sh = self.shared.lock();
            let mut delete = None;
            let mut changed = false;
            for (i, v) in sh.settings.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    changed |= ui.checkbox(&mut v.open, "").on_hover_text("Show this window").changed();
                    let car = if v.car.enabled && v.filter == ViewFilter::Spikes { " · CAR" } else { "" };
                    let chans = if v.headstages.is_empty() { "all".to_string() } else { v.headstages.join(", ") };
                    ui.label(RichText::new(&v.name).strong());
                    ui.weak(format!("{chans} · {}{car}", match v.filter {
                        ViewFilter::Raw => "raw",
                        ViewFilter::Spikes => "300–6k",
                        ViewFilter::Lfp => "LFP",
                    }));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Delete").clicked() {
                            delete = Some(i);
                        }
                    });
                });
            }
            if let Some(i) = delete {
                sh.settings.remove(i);
                changed = true;
            }
            ui.horizontal(|ui| {
                if ui.button("New window").clicked() {
                    sh.settings.push(TraceViewSettings { id: next_id, name: format!("Traces {}", next_id + 1), ..Default::default() });
                    changed = true;
                }
                if ui.add_enabled(!ports.is_empty(), egui::Button::new("One window per port")).on_hover_text("Add a window for each port's headstages (e.g. Port A, Port B)").clicked() {
                    for (k, port) in ports.iter().enumerate() {
                        let hs: Vec<String> = layout.iter().flat_map(|l| l.headstages.iter()).filter(|h| h.name.starts_with(*port)).map(|h| h.name.clone()).collect();
                        sh.settings.push(TraceViewSettings { id: next_id + k as u32, name: format!("Port {port}"), headstages: hs, aux: false, adc: false, ..Default::default() });
                    }
                    changed = true;
                }
            });
            if changed {
                sh.sync_states();
                drop(sh);
                self.dirty_cfg = Instant::now();
            }
            ui.add(egui::Slider::new(&mut self.cfg.display.max_fps, 30.0..=360.0).text("max fps"));
            ui.small("Each window has its own channels, filter, CAR and scaling (control bar at its top). Click a channel label or spike tile to monitor it on audio; Ctrl+click adds/removes it from the mix. Scroll over traces to move rows; Ctrl+scroll to scale.");
        });
    }

    /// Register every open trace view as its own (deferred) native window.
    pub(super) fn trace_windows(&mut self, ctx: &egui::Context) {
        let open: Vec<(u32, String)> = self.shared.lock().settings.iter().filter(|v| v.open).map(|v| (v.id, v.name.clone())).collect();
        for (id, name) in open {
            let mut b = egui::ViewportBuilder::default().with_title(format!("oers — {name}")).with_app_id("oers").with_inner_size([1400.0, 900.0]);
            if let Some(icon) = &self.icon {
                b = b.with_icon(icon.clone());
            }
            let shared = self.shared.clone();
            ctx.show_viewport_deferred(egui::ViewportId::from_hash_of(("trace-view", id)), b, move |ui, _class| shared.lock().window(ui, id));
        }
    }

    /// The global CAR window (spike detection and audio), in the main window.
    pub(super) fn car_palette(&mut self, ctx: &egui::Context, target: CarTarget) {
        if self.car_window != Some(target) {
            return;
        }
        let Some(l) = self.layout() else { return };
        let mut open = true;
        let mut changed = false;
        // The global sets are edited through a copy (same shape as a view's CAR).
        let g = &self.cfg.display.car;
        let mut global = ViewCar { enabled: false, reference: g.reference.clone(), subtract: g.subtract.clone() };
        let (title, groups, car) = match target {
            CarTarget::Global => ("CAR: spike detection and audio".to_string(), electrode_groups(&l, &[]), &mut global),
            // A view's CAR window lives in that view's own window (ViewsShared::car_palette).
            CarTarget::View(_) => return,
        };
        let (mut spikes, mut audio) = (self.cfg.display.car.spikes, self.cfg.display.car.audio);
        egui::Window::new(title).id(egui::Id::unique(("car-window", format!("{target:?}")))).open(&mut open).resizable(false).collapsible(false).default_width(460.0).show(ctx, |ui| {
            ui.small("Recordings always keep the original signals.");
            ui.horizontal(|ui| {
                ui.label("Apply to");
                match target {
                    CarTarget::Global => {
                        changed |= ui.checkbox(&mut spikes, "spike detection").changed();
                        changed |= ui.checkbox(&mut audio, "audio").changed();
                    }
                    CarTarget::View(_) => {
                        changed |= ui.checkbox(&mut car.enabled, "this window (300–6k view)").changed();
                    }
                }
            });
            ui.weak(format!("reference {} · subtract {}", car.reference.len(), car.subtract.len()));
            let mut copy_ref = false;
            for (title, set) in [("Reference (averaged)", &mut car.reference), ("Subtract from", &mut car.subtract)] {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.strong(title);
                    if title.starts_with("Subtract") && ui.small_button("same as reference").clicked() {
                        copy_ref = true;
                    }
                });
                for (gname, chans) in &groups {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(gname).small());
                        if ui.small_button("All").clicked() {
                            set.extend(chans.iter().map(|(i, _)| *i).filter(|i| !set.contains(i)).collect::<Vec<_>>());
                            changed = true;
                        }
                        if ui.small_button("None").clicked() {
                            set.retain(|x| !chans.iter().any(|(i, _)| i == x));
                            changed = true;
                        }
                    });
                    egui::Grid::new((title, gname.as_str())).spacing([2.0, 2.0]).show(ui, |ui| {
                        for (n, (i, name)) in chans.iter().enumerate() {
                            let on = set.contains(i);
                            let b = egui::Button::new(RichText::new(name.trim_start_matches("CH")).monospace().size(11.0)).selected(on).min_size(egui::vec2(26.0, 0.0));
                            if ui.add(b).on_hover_text(name.as_str()).clicked() {
                                if on {
                                    set.retain(|x| x != i);
                                } else {
                                    set.push(*i);
                                }
                                changed = true;
                            }
                            if n % 16 == 15 {
                                ui.end_row();
                            }
                        }
                    });
                }
            }
            if copy_ref {
                car.subtract = car.reference.clone();
                changed = true;
            }
        });
        if changed {
            car.reference.sort_unstable();
            car.subtract.sort_unstable();
            if target == CarTarget::Global {
                let g = &mut self.cfg.display.car;
                g.reference = global.reference.clone();
                g.subtract = global.subtract.clone();
                g.spikes = spikes;
                g.audio = audio;
            }
            self.dirty_cfg = Instant::now();
        }
        if !open {
            self.car_window = None;
        }
    }
}
