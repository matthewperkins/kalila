//! Trace views: each is its own native window with its own channels, filter, CAR, scaling
//! and sweep mode. The main window keeps the controls and the spike tiles.

/// The key that scales and adds on this platform, as egui's `modifiers.command` reads it.
pub(crate) const MOD: &str = if cfg!(target_os = "macos") { "Cmd" } else { "Ctrl" };

use super::*;
use crate::config::{Slope, TraceViewSettings, TrigAlign, TrigSource, ViewCar};
use crate::review::{self, Reader, ReviewView};
use crate::slope::{DEAD_S, SlopeParams, SlopeTrigger};
use crossbeam_channel::Receiver;
use engine::audio::{AudioSettings, AudioStats};
use engine::record::RecSource;
use engine::rhythm::Block;
use engine::{ChannelStats, DspSettings};
use parking_lot::Mutex;
use std::collections::VecDeque;

/// Trace-view state shared by the main window and the trace windows. Each trace window is a
/// deferred viewport: it is drawn whenever its own surface is due, even while the main window
/// is hidden (Wayland withholds frames from hidden windows, which would otherwise stall it).
mod frame;
use frame::{Edit, ViewFrame};
mod model;
use model::{
    band_bits, band_key, car_applies, chan_filters, pyramid_columns, set_row_filter, sweep_hash, trig_channel,
    trig_window_ms, used_filters, uv_text, view_chinfo,
};
pub(super) use model::{chan_key, view_rows};

/// A sample as a time: since the recording started while recording, else since acquisition.
fn note_time(fs: f64, rec: Option<&RecSource>, s: i64) -> String {
    match rec {
        Some(r) if s >= r.first_sample => format!("rec {}", review::fmt_time((s - r.first_sample) as f64 / fs, true)),
        _ => review::fmt_time(s as f64 / fs, true),
    }
}

pub(super) type SharedViews = Arc<Mutex<ViewsShared>>;

/// Runtime state of one trace view: staged samples and what was last sent to its GPU state.
pub(super) struct ViewState {
    pub id: u32,
    /// A deque, so trimming the oldest samples costs only what is dropped: with a `Vec`, every
    /// block shifted a whole window (tens of MB for a 20 s window), which starved the UI thread
    /// whenever a window missed a few frames and froze all windows for many seconds.
    pending: VecDeque<i16>,
    /// This frame's upload, reused once the renderer has dropped last frame's reference.
    upload: Arc<Vec<i16>>,
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
    /// Hash of everything a sweep depends on (window, trigger, filters, geometry): a change
    /// clears the sweeps.
    sweep_key: Option<u64>,
    /// Slope trigger on this view's trigger channel (made at the stream's rate).
    slope: Option<SlopeTrigger>,
    /// The slope trigger's input lost continuity (restart it).
    slope_reset: bool,
    /// Slope triggers found this acquisition.
    slope_count: u64,
    pub colors_dirty: bool,
    // Diagnostics: when the view was last drawn / last got samples, and whether a stall
    // warning is outstanding.
    last_drawn: Instant,
    last_data: Instant,
    stalled: bool,
    /// Scroll-back review (None = live).
    pub review: Option<ReviewView>,
    /// Wheel movement not yet turned into rows (`wheel_rows`).
    row_scroll: f32,
    /// A short message on the plot (e.g. why review can't start), until the instant.
    note: Option<(Instant, String)>,
}

impl ViewState {
    pub fn new(id: u32) -> Self {
        Self {
            id,
            pending: VecDeque::new(),
            upload: Arc::default(),
            pending_ttl: VecDeque::new(),
            ttl: TtlOverlay::default(),
            counter: 0,
            reset: true,
            last_cfg: None,
            tables_key: None,
            last_chinfo: None,
            sweeps: Sweeps::default(),
            sweep_key: None,
            slope: None,
            slope_reset: true,
            slope_count: 0,
            colors_dirty: true,
            last_drawn: Instant::now(),
            last_data: Instant::now(),
            stalled: false,
            review: None,
            row_scroll: 0.0,
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
        self.slope_reset = true;
        self.slope_count = 0;
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
            self.slope_reset = true;
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
    /// Spike detection settings (CAR) and per-channel thresholds, for the slope trigger.
    dsp: Arc<Mutex<DspSettings>>,
    stats: Arc<Mutex<ChannelStats>>,
    /// Min/max pyramids of the recording (engine), for review of long spans.
    pyramids: Arc<Mutex<engine::pyramid::Store>>,
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
    /// Absolute sample number just after the newest displayed block, and when that block
    /// was acquired.
    newest_sample: i64,
    newest_at: Option<Instant>,
    /// Typed notes (any window), drafts and presets.
    pub notes: crate::notes::Notes,
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

/// Time-window limits (s). Live keeps up to one window of samples per view in memory, so it
/// stops at 20 s; review reads from the recording and reaches further.
const LIVE_MIN_S: f32 = 0.05;
const LIVE_MAX_S: f32 = 20.0;
const REVIEW_MIN_S: f32 = 0.005;
/// Spans over `DIRECT_MAX_S` come from the min/max pyramid only.
const REVIEW_MAX_S: f32 = 3600.0;
const DIRECT_MAX_S: f32 = 60.0;

/// Review spans in samples: at least one sample per column, within the review limits.
fn review_span_limits(cols: u32, fs: f64) -> (u32, u32) {
    (
        cols.max((REVIEW_MIN_S as f64 * fs) as u32),
        (REVIEW_MAX_S as f64 * fs) as u32,
    )
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
                n.zoom_at = i
                    .pointer
                    .hover_pos()
                    .map_or(0.5, |p| ((p.x - plot.min.x) as f64 / w).clamp(0.0, 1.0));
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

/// Width of the per-row filter chip (points), right of the channel label.
const CHIP_W: f32 = 44.0;

/// Grey diagonal stripes over a button: "no single setting" (rows differ).
fn hatch(p: &egui::Painter, r: egui::Rect) {
    let p = p.with_clip_rect(r.intersect(p.clip_rect()));
    let stroke = Stroke::new(2.0, Color32::from_rgba_unmultiplied(0, 0, 0, 120));
    let mut x = r.min.x - r.height();
    while x < r.max.x {
        p.line_segment([egui::pos2(x, r.max.y), egui::pos2(x + r.height(), r.min.y)], stroke);
        x += 6.0;
    }
}

/// Hover text of a filter.
fn filter_help(f: ViewFilter, d: &TraceViewSettings) -> String {
    match f {
        ViewFilter::Raw => "Raw: unfiltered".into(),
        ViewFilter::Band => format!(
            "Band: {} (4th-order Butterworth each side; the window's corners; display CAR applies here)",
            d.band_label()
        ),
    }
}

/// The reference / subtract set editor shared by both CAR palettes: counts, then one block of
/// per-headstage All / None buttons and electrode toggles for each set. Returns whether
/// anything changed (the caller sorts the sets and persists).
fn car_sets_ui(ui: &mut egui::Ui, groups: &[(String, Vec<(usize, String)>)], car: &mut ViewCar) -> bool {
    let mut changed = false;
    ui.weak(format!(
        "reference {} · subtract {}",
        car.reference.len(),
        car.subtract.len()
    ));
    let mut copy_ref = false;
    for (title, set) in [
        ("Reference (averaged)", &mut car.reference),
        ("Subtract from", &mut car.subtract),
    ] {
        ui.separator();
        ui.horizontal(|ui| {
            ui.strong(title);
            if title.starts_with("Subtract") && ui.small_button("same as reference").clicked() {
                copy_ref = true;
            }
        });
        for (gname, chans) in groups {
            ui.horizontal(|ui| {
                ui.label(RichText::new(gname).small());
                if ui.small_button("All").clicked() {
                    set.extend(
                        chans
                            .iter()
                            .map(|(i, _)| *i)
                            .filter(|i| !set.contains(i))
                            .collect::<Vec<_>>(),
                    );
                    changed = true;
                }
                if ui.small_button("None").clicked() {
                    set.retain(|x| !chans.iter().any(|(i, _)| i == x));
                    changed = true;
                }
            });
            egui::Grid::new((title, gname.as_str()))
                .spacing([2.0, 2.0])
                .show(ui, |ui| {
                    for (n, (i, name)) in chans.iter().enumerate() {
                        let on = set.contains(i);
                        let b = egui::Button::new(RichText::new(name.trim_start_matches("CH")).monospace().size(11.0))
                            .selected(on)
                            .min_size(egui::vec2(26.0, 0.0));
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
    changed
}

/// Electrodes of the given headstages (all if empty), grouped by headstage, for the CAR window.
fn electrode_groups(l: &StreamLayout, only: &[String]) -> Vec<(String, Vec<(usize, String)>)> {
    l.headstages
        .iter()
        .enumerate()
        .filter(|(_, h)| only.is_empty() || only.contains(&h.name))
        .map(|(hi, h)| {
            let chans = l
                .channels
                .iter()
                .enumerate()
                .filter(|(_, c)| c.kind == ChannelKind::Electrode && c.headstage == Some(hi))
                .map(|(i, c)| (i, c.name.clone()))
                .collect();
            (format!("{} {}", h.name, h.chip), chans)
        })
        .collect()
}

impl ViewsShared {
    pub fn new(
        settings: Vec<TraceViewSettings>,
        audio: Arc<Mutex<AudioSettings>>,
        audio_stats: Arc<AudioStats>,
        dsp: Arc<Mutex<DspSettings>>,
        stats: Arc<Mutex<ChannelStats>>,
        pyramids: Arc<Mutex<engine::pyramid::Store>>,
    ) -> Self {
        let mut s = Self {
            settings,
            states: Vec::new(),
            layout: None,
            rx: None,
            nch: 0,
            audio,
            audio_stats,
            dsp,
            stats,
            pyramids,
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
            newest_at: None,
            notes: Default::default(),
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
        log::debug!("trace views reset (new layout or acquisition)");
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
        // Tell the pyramid builder which band pyramid each window wants.
        {
            let l = self.layout.as_ref().expect("checked above");
            let mut st = self.pyramids.lock();
            st.want.retain(|id, _| self.settings.iter().any(|v| v.id == *id));
            for v in &self.settings {
                let key = band_key(l, v);
                if st.want.get(&v.id) != Some(&key) {
                    st.want.insert(v.id, key);
                }
            }
        }
        let Some(rx) = &self.rx else { return };
        let mut newest: Option<Instant> = None;
        let blocks: Vec<Arc<Block>> = rx.try_iter().filter(|b| b.num_channels == nch).collect();
        for b in &blocks {
            newest = b.acquired_at.or(newest);
            let end = b.first_sample + b.len() as i64;
            // The sample counter restarted (new acquisition): earlier notes are of another clock.
            if end + (fs as i64) < self.newest_sample {
                self.newest_sample = 0;
                self.notes.notes.clear();
            }
            if end >= self.newest_sample {
                self.newest_sample = end;
                self.newest_at = b.acquired_at.or(self.newest_at);
            }
        }
        for r in self.reader.rx.try_iter() {
            if let Some(rv) = self
                .states
                .iter_mut()
                .find(|v| v.id == r.view)
                .and_then(|v| v.review.as_mut())
            {
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

    /// The sample being acquired now: the newest block plus the time since it was acquired
    /// (so a note's stamp doesn't include the display latency). None when not acquiring.
    pub fn now_sample(&self) -> Option<i64> {
        let fs = self.layout.as_ref()?.sample_rate;
        if !self.acquiring || self.newest_sample == 0 {
            return None;
        }
        let ahead = self.newest_at.map_or(0.0, |t| t.elapsed().as_secs_f64().min(0.5)) * fs;
        Some(self.newest_sample + ahead as i64)
    }

    /// A sample as a time: since the recording started while recording, else since acquisition.
    pub fn note_time(&self, s: i64) -> String {
        note_time(
            self.layout.as_ref().map_or(30000.0, |l| l.sample_rate),
            self.rec_source.as_ref(),
            s,
        )
    }

    /// Note keys for this window's frame, and the note bar. Call before any other widget.
    pub fn notes_frame(&mut self, ui: &egui::Ui) {
        let typing = ui.ctx().egui_wants_keyboard_input();
        let now = self.now_sample();
        ui.ctx().input_mut(|i| self.notes.handle(&mut i.events, typing, now));
        let vp = ui.ctx().viewport_id();
        let times: Vec<(i64, String)> = self
            .notes
            .compose
            .as_ref()
            .map(|c| vec![(c.sample, self.note_time(c.sample))])
            .unwrap_or_default();
        self.notes.draw(ui.ctx(), vp, |s| {
            times.iter().find(|t| t.0 == s).map(|t| t.1.clone()).unwrap_or_default()
        });
    }

    /// One trace window's whole UI (a deferred viewport callback).
    pub fn window(&mut self, ui: &mut egui::Ui, id: u32) {
        self.drain();
        let Some(vi) = self.settings.iter().position(|v| v.id == id) else {
            return;
        };
        if vi >= self.states.len() {
            return;
        }
        self.notes_frame(ui);
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
        #[cfg(target_os = "macos")]
        if self.acquiring
            && self.states[vi].review.is_none()
            && let Some(t) = self.newest_at
        {
            crate::presented::frame_shows(t);
        }
        // Pace this window's own frames (continuous while acquiring).
        let period = Duration::from_secs_f32(1.0 / self.max_fps.max(10.0));
        ui.ctx().request_repaint_after(if self.acquiring {
            period
        } else {
            Duration::from_millis(250)
        });
    }

    /// Click on a channel label: monitor just these channels; Ctrl (or Shift) toggles them in
    /// or out of the mix.
    fn monitor_click(&mut self, chs: Vec<usize>, add: bool) {
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
        let ttl_seen = self.states[vi].ttl.seen()
            | self.states[vi]
                .review
                .as_ref()
                .and_then(|r| r.shown.as_ref())
                .map_or(0, |s| s.ttl.seen());
        let stats = self.stats.clone();
        let d = &mut self.settings[vi];
        let old = d.clone();
        let mut clear_sweeps = false;
        let mut open_car = false;
        // Filters in use among the drawn electrodes; more than one = per-row filters.
        let used = l
            .as_ref()
            .map(|l| used_filters(l, &chan_filters(l, d), &view_rows(l, d).0))
            .unwrap_or_default();
        let mixed = used.len() > 1;
        let uniform = used.first().copied().filter(|_| !mixed).unwrap_or(d.filter);
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
            // Filtering palette: one column per preset, with "every row", its µV/row, and
            // its own setting (HP corner, CAR). The row chips beside the traces set single rows.
            let btn = ui.button("Filter settings").on_hover_text("Raw or Band on every row, µV/row per filter, band corners and CAR (the chips beside the traces set single rows)");
            egui::Popup::from_toggle_button_response(&btn).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                egui::Grid::new(("filter-palette", id)).num_columns(3).spacing([10.0, 6.0]).min_col_width(64.0).show(ui, |ui| {
                    ui.label("");
                    for f in ViewFilter::ALL {
                        let label = match f {
                            ViewFilter::Raw => "Raw".to_string(),
                            ViewFilter::Band => format!("Band {}", d.band_label()),
                        };
                        ui.vertical_centered(|ui| ui.strong(label).on_hover_text(filter_help(f, d)));
                    }
                    ui.end_row();
                    ui.label("All channels");
                    for f in ViewFilter::ALL {
                        ui.vertical_centered(|ui| {
                            let r = ui.selectable_label(!mixed && uniform == f, " all rows ").on_hover_text(format!("{} on every row (clears per-row choices)", d.filter_label(f)));
                            if mixed && used.contains(&f) {
                                hatch(ui.painter(), r.rect.shrink(1.0));
                            }
                            if r.clicked() {
                                d.filter = f;
                                d.channel_filter.clear();
                            }
                        });
                    }
                    ui.end_row();
                    ui.label("µV/row").on_hover_text(format!("Each filter keeps its own range. {MOD}+wheel over a row scales that row alone, {MOD}+Shift+wheel its whole filter; setting a value here puts every row of that filter back on it"));
                    for f in ViewFilter::ALL {
                        let mut uv = d.uv(f);
                        if ui.add(egui::DragValue::new(&mut uv).range(10.0..=5000.0).speed(2.0)).changed() {
                            d.set_uv(f, uv);
                            let keys: Vec<String> = d.channel_uv.keys().filter(|k| d.filter_of(Some(k)) == f).cloned().collect();
                            for k in keys {
                                d.channel_uv.remove(&k);
                            }
                        }
                    }
                    ui.end_row();
                    ui.label("Corners").on_hover_text("High-pass and low-pass corners of the Band filter (4th-order Butterworth each); 0 = that side off");
                    ui.label("");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut d.band_low_hz).range(0.0..=5000.0).speed(1.0).suffix(" Hz")).on_hover_text("High-pass corner (0 = none)");
                        ui.label("–");
                        ui.add(egui::DragValue::new(&mut d.band_high_hz).range(0.0..=15000.0).speed(10.0).suffix(" Hz")).on_hover_text("Low-pass corner (0 = none)");
                    });
                    ui.end_row();
                    ui.label("");
                    ui.label("");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut d.car.enabled, "CAR").on_hover_text("Display-only common average reference, on the Band rows");
                        if ui.small_button("…").on_hover_text(format!("CAR channels: reference {} · subtract {}", d.car.reference.len(), d.car.subtract.len())).clicked() {
                            open_car = true;
                        }
                    });
                    ui.end_row();
                });
            });
        });
        if d.controls {
            ui.horizontal_wrapped(|ui| {
                // One time base for live and review; review reaches further (5 ms to 60 s).
                let range = if reviewing { REVIEW_MIN_S..=REVIEW_MAX_S } else { LIVE_MIN_S..=LIVE_MAX_S };
                ui.add(egui::DragValue::new(&mut d.window_s).range(range).speed(0.02).suffix(" s")).on_hover_text(if reviewing { "Time window (review: 5 ms – 60 s)" } else { "Time window" });
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
                ui.selectable_value(&mut d.triggered, true, "Triggered").on_hover_text("Each sweep starts at a trigger (TTL rising edge, or a level crossing on one channel); recent sweeps are overlaid");
                if d.triggered {
                    let nlines = l.as_ref().map(|l| l.ttl_lines).unwrap_or(8).max(1);
                    // Trigger: a TTL line, or one of this view's electrodes (slope trigger).
                    let tc = l.as_ref().and_then(|l| trig_channel(l, d));
                    let sel = match d.trig_source {
                        TrigSource::Ttl => format!("TTL{}", d.trig_line + 1),
                        TrigSource::Spike => match (tc, &l) {
                            (Some(c), Some(l)) => format!("{} {}", l.channels[c].name, if d.trig_slope == Slope::Neg { "falling" } else { "rising" }),
                            _ => "Spike: pick a channel".into(),
                        },
                    };
                    egui::ComboBox::from_id_salt(("trigsrc", id)).width(120.0).selected_text(sel).show_ui(ui, |ui| {
                        for i in 0..nlines {
                            if ui.selectable_label(d.trig_source == TrigSource::Ttl && d.trig_line == i, format!("TTL{} rising", i + 1)).clicked() {
                                d.trig_source = TrigSource::Ttl;
                                d.trig_line = i;
                            }
                        }
                        if let Some(l) = &l {
                            ui.separator();
                            ui.small("Slope trigger on a channel (Alt+click a row label)");
                            let (row_ch, _) = view_rows(l, d);
                            for &c in row_ch.iter().filter(|&&c| l.channels[c as usize].kind == ChannelKind::Electrode) {
                                let Some(key) = chan_key(l, c as usize) else { continue };
                                if ui.selectable_label(d.trig_source == TrigSource::Spike && d.trig_channel == key, &l.channels[c as usize].name).clicked() {
                                    d.trig_source = TrigSource::Spike;
                                    d.trig_channel = key;
                                }
                            }
                        }
                    });
                    if d.trig_source == TrigSource::Spike {
                        ui.selectable_value(&mut d.trig_slope, Slope::Neg, "falling").on_hover_text("Trigger when the spike-band signal falls through a negative level (the usual extracellular spike)");
                        ui.selectable_value(&mut d.trig_slope, Slope::Pos, "rising").on_hover_text("Trigger when the spike-band signal rises through a positive level");
                        // Level: the spike detector's threshold for the channel, or a fixed µV
                        // (also set by dragging the dashed line on the trace).
                        let auto_uv = tc.map(|c| stats.lock().threshold_uv.get(c).copied().unwrap_or(0.0).abs()).unwrap_or(0.0);
                        let sign = if d.trig_slope == Slope::Neg { "−" } else { "+" };
                        if d.trig_level_uv == 0.0 {
                            let txt = if auto_uv > 0.0 { format!("level {sign}{auto_uv:.0} µV (detector)") } else { "level: detector (waiting for σ)".to_string() };
                            if ui.button(txt).on_hover_text("The spike detector's threshold for this channel. Click, or drag the dashed line, to fix a level for this window").clicked() {
                                d.trig_level_uv = if auto_uv > 0.0 { auto_uv } else { 50.0 };
                            }
                        } else {
                            ui.add(egui::DragValue::new(&mut d.trig_level_uv).range(1.0..=5000.0).speed(0.5).max_decimals(0).prefix(format!("level {sign}")).suffix(" µV")).on_hover_text("Fixed crossing level for this window (drag the dashed line too)");
                            if ui.small_button("detector").on_hover_text("Back to the spike detector's threshold").clicked() {
                                d.trig_level_uv = 0.0;
                            }
                        }
                        egui::ComboBox::from_id_salt(("trigalign", id)).width(80.0).selected_text(if d.trig_align == TrigAlign::Trough { "t0 trough" } else { "t0 crossing" }).show_ui(ui, |ui| {
                            ui.selectable_value(&mut d.trig_align, TrigAlign::Trough, "t0 trough").on_hover_text("t = 0 at the extreme within 0.5 ms of the crossing (as the spike detector aligns)");
                            ui.selectable_value(&mut d.trig_align, TrigAlign::Crossing, "t0 crossing").on_hover_text("t = 0 at the crossing sample");
                        });
                        ui.add(egui::DragValue::new(&mut d.trig_holdoff_ms).range(0.0..=1000.0).speed(0.1).prefix("holdoff ").suffix(" ms")).on_hover_text("Extra time after each trigger before the next can start (a 1 ms dead time always applies)");
                        ui.checkbox(&mut d.trig_window, "window").on_hover_text("Window discriminator: reject crossings whose trough goes beyond the second level");
                        if d.trig_window {
                            ui.add(egui::DragValue::new(&mut d.trig_level2_uv).range(1.0..=5000.0).speed(1.0).prefix(format!("to {sign}")).suffix(" µV"));
                        }
                        ui.add(egui::DragValue::new(&mut d.spk_pre_ms).range(0.1..=50.0).speed(0.05).fixed_decimals(1).prefix("−").suffix(" ms"));
                        ui.add(egui::DragValue::new(&mut d.spk_post_ms).range(0.1..=100.0).speed(0.05).fixed_decimals(1).prefix("+").suffix(" ms"));
                    } else {
                        ui.add(egui::DragValue::new(&mut d.trig_pre_ms).range(0.0..=200.0).speed(0.5).prefix("−").suffix(" ms"));
                        ui.add(egui::DragValue::new(&mut d.trig_post_ms).range(1.0..=250.0).speed(0.5).prefix("+").suffix(" ms"));
                    }
                    ui.add(egui::DragValue::new(&mut d.trig_sweeps).range(1..=50).suffix(" sweeps"));
                    if ui.small_button("Clear").clicked() {
                        clear_sweeps = true;
                    }
                } else {
                    ui.checkbox(&mut d.ttl_overlay, "TTL overlay").on_hover_text("Shade traces while a TTL input is high; one lane per line seen high since acquisition start");
                    if d.ttl_overlay {
                        let nlines = l.as_ref().map(|l| l.ttl_lines).unwrap_or(8).clamp(1, 16);
                        let shown = (0..nlines).filter(|i| d.ttl_hidden & (1 << i) == 0).count();
                        let btn = ui.small_button(if shown == nlines { "lines: all".to_string() } else { format!("lines: {shown}/{nlines}") }).on_hover_text("Which TTL lines the overlay shows");
                        egui::Popup::from_toggle_button_response(&btn).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(|ui| {
                            for i in 0..nlines {
                                let mut on = d.ttl_hidden & (1 << i) == 0;
                                let seen = ttl_seen & (1 << i) != 0;
                                let c = if seen { crate::ttl::line_color(i) } else { Color32::from_gray(110) };
                                let r = ui.checkbox(&mut on, RichText::new(format!("TTL{}", i + 1)).color(c));
                                let r = if seen { r } else { r.on_hover_text("Not seen high yet") };
                                if r.changed() {
                                    d.ttl_hidden ^= 1 << i;
                                }
                            }
                            ui.horizontal(|ui| {
                                if ui.small_button("all").clicked() {
                                    d.ttl_hidden = 0;
                                }
                                if ui.small_button("none").clicked() {
                                    d.ttl_hidden = u16::MAX;
                                }
                            });
                        });
                    }
                }
            });
        }
        let reset =
            d.window_s != old.window_s || d.headstages != old.headstages || d.aux != old.aux || d.adc != old.adc;
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
        let Some(l) = self.layout.as_ref() else {
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "No board",
                egui::FontId::proportional(18.0),
                Color32::GRAY,
            );
            return;
        };
        let mut f = ViewFrame {
            d: &self.settings[vi],
            vs: &mut self.states[vi],
            l,
            nch: self.nch,
            audio: &self.audio,
            stats: &self.stats,
            dsp: &self.dsp,
            pyramids: &self.pyramids,
            reader: &self.reader,
            rec_source: self.rec_source.as_ref(),
            newest_sample: self.newest_sample,
            notes: &self.notes,
            edits: Vec::new(),
        };
        f.draw(ui, rect);
        let edits = f.edits;
        self.apply_edits(vi, edits);
    }

    /// Make the settings changes a trace window's frame asked for (see `frame`).
    fn apply_edits(&mut self, vi: usize, edits: Vec<Edit>) {
        if edits.is_empty() {
            return;
        }
        self.dirty = true;
        for e in edits {
            let v = &mut self.settings[vi];
            match e {
                Edit::FirstRow(r) => v.first_row = r,
                Edit::Uv { filters, channels } => {
                    for (f, uv) in filters {
                        v.set_uv(f, uv);
                    }
                    v.channel_uv.extend(channels);
                }
                Edit::WindowS(w) => v.window_s = w,
                Edit::LiveWindow => v.window_s = v.window_s.clamp(LIVE_MIN_S, LIVE_MAX_S),
                Edit::TrigLevel(uv) => v.trig_level_uv = uv,
                Edit::TrigChannel(key) => {
                    v.triggered = true;
                    v.trig_source = TrigSource::Spike;
                    v.trig_channel = key;
                }
                Edit::RowFilter { chans, filter, row_ch } => {
                    if let Some(l) = &self.layout {
                        set_row_filter(l, v, &chans, filter, &row_ch);
                    }
                }
                Edit::Monitor { chans, add } => self.monitor_click(chans, add),
            }
        }
    }

    /// This view's display-CAR window (only this view's electrodes).
    fn car_palette(&mut self, ctx: &egui::Context, id: u32) {
        let Some(l) = self.layout.clone() else { return };
        let mut open = true;
        let mut changed = false;
        let Some(v) = self.settings.iter_mut().find(|v| v.id == id) else {
            self.car_open = None;
            return;
        };
        let (title, groups, car) = (
            format!("CAR: {}", v.name),
            electrode_groups(&l, &v.headstages),
            &mut v.car,
        );
        egui::Window::new(title)
            .id(egui::Id::unique(("car-window", format!("View({id})"))))
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.small("Recordings always keep the original signals.");
                ui.horizontal(|ui| {
                    ui.label("Apply to");
                    changed |= ui.checkbox(&mut car.enabled, "this window (300–6k view)").changed();
                });
                changed |= car_sets_ui(ui, &groups, car);
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
        let ports: Vec<char> = self
            .engine
            .layout()
            .map(|l| {
                let mut p: Vec<char> = l.headstages.iter().filter_map(|h| h.name.chars().next()).collect();
                p.dedup();
                p
            })
            .unwrap_or_default();
        let layout = self.layout();
        let next_id = self.new_view_id();
        egui::CollapsingHeader::new(RichText::new("Trace windows").strong()).id_salt("display_section").default_open(true).show(ui, |ui| {
            let mut sh = self.shared.lock();
            let mut delete = None;
            let mut changed = false;
            for (i, v) in sh.settings.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    changed |= ui.checkbox(&mut v.open, "").on_hover_text("Show this window").changed();
                    let car = if v.car.enabled && (v.filter == ViewFilter::Band || v.channel_filter.values().any(|f| *f == ViewFilter::Band)) { " · CAR" } else { "" };
                    let chans = if v.headstages.is_empty() { "all".to_string() } else { v.headstages.join(", ") };
                    ui.label(RichText::new(&v.name).strong());
                    ui.weak(format!("{chans} · {}{car}", if v.channel_filter.is_empty() { v.filter_label(v.filter) } else { "per-row filters".into() }));
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
            ui.small(format!("Each window has its own channels, filter, CAR and scaling (control bar at its top). Click a channel label or spike tile to monitor it on audio; {MOD}+click adds/removes it from the mix. Scroll over traces to move rows; {MOD}+scroll to scale."));
        });
    }

    /// Register every open trace view as its own (deferred) native window.
    pub(super) fn trace_windows(&mut self, ctx: &egui::Context) {
        let open: Vec<(u32, String)> = self
            .shared
            .lock()
            .settings
            .iter()
            .filter(|v| v.open)
            .map(|v| (v.id, v.name.clone()))
            .collect();
        for (id, name) in open {
            let mut b = egui::ViewportBuilder::default()
                .with_title(format!("kalila — {name}"))
                .with_app_id("kalila")
                .with_inner_size([1400.0, 900.0]);
            if let Some(icon) = &self.icon {
                b = b.with_icon(icon.clone());
            }
            let shared = self.shared.clone();
            ctx.show_viewport_deferred(
                egui::ViewportId::from_hash_of(("trace-view", id)),
                b,
                move |ui, _class| shared.lock().window(ui, id),
            );
        }
    }

    /// The global CAR window (spike detection and audio), in the main window.
    pub(super) fn car_palette(&mut self, ctx: &egui::Context) {
        if !self.car_window {
            return;
        }
        let Some(l) = self.layout() else { return };
        let mut open = true;
        let mut changed = false;
        // The global sets are edited through a copy (same shape as a view's CAR).
        let g = &self.cfg.display.car;
        let mut global = ViewCar {
            enabled: false,
            reference: g.reference.clone(),
            subtract: g.subtract.clone(),
        };
        let (title, groups, car) = ("CAR: spike detection and audio", electrode_groups(&l, &[]), &mut global);
        let (mut spikes, mut audio) = (self.cfg.display.car.spikes, self.cfg.display.car.audio);
        egui::Window::new(title)
            .id(egui::Id::unique(("car-window", "Global".to_string())))
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .default_width(460.0)
            .show(ctx, |ui| {
                ui.small("Recordings always keep the original signals.");
                ui.horizontal(|ui| {
                    ui.label("Apply to");
                    changed |= ui.checkbox(&mut spikes, "spike detection").changed();
                    changed |= ui.checkbox(&mut audio, "audio").changed();
                });
                changed |= car_sets_ui(ui, &groups, car);
            });
        if changed {
            car.reference.sort_unstable();
            car.subtract.sort_unstable();
            let g = &mut self.cfg.display.car;
            g.reference = global.reference.clone();
            g.subtract = global.subtract.clone();
            g.spikes = spikes;
            g.audio = audio;
            self.dirty_cfg = Instant::now();
        }
        if !open {
            self.car_window = false;
        }
    }
}
