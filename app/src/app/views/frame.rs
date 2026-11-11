//! One frame of a trace window, live or in review.
//!
//! A `ViewFrame` holds the window's settings read-only, its runtime state, and borrowed
//! handles to what it reads from the rest of `ViewsShared`. It never writes the settings:
//! changes asked for while drawing (wheel scaling, row scrolling, zoom, the trigger level,
//! row chips, audio monitoring) go into `edits`, and `ViewsShared::apply_edits` makes them
//! when the frame is done. So everything drawn in one frame uses one set of settings, and
//! a change shows from the next frame on.

use super::*;

/// Geometry of a trace plot this frame.
#[derive(Clone, Copy)]
pub(super) struct PlotGeom {
    pub rect: egui::Rect,
    pub plot: egui::Rect,
    pub strip: egui::Rect,
    pub ttl_h: f32,
    pub label_w: f32,
    pub cols: u32,
    pub rows: usize,
    pub first: usize,
    pub k: usize,
    pub ppp: f32,
}

/// The rows a window draws: channel order, row table ([first index into `row_ch`, count])
/// and each channel's display filter.
pub(super) struct RowSet {
    pub row_ch: Vec<u32>,
    pub rowtab: Vec<[u32; 2]>,
    pub filters: Vec<ViewFilter>,
}

/// A settings change asked for during a frame; applied in order after it.
pub(super) enum Edit {
    FirstRow(usize),
    /// New µV/row for whole filters and for single channels (by `chan_key`).
    Uv { filters: Vec<(ViewFilter, f32)>, channels: Vec<(String, f32)> },
    WindowS(f32),
    /// Back from review: bring the time base into the live range.
    LiveWindow,
    TrigLevel(f32),
    /// Alt+click on a row label: slope trigger on this electrode.
    TrigChannel(String),
    RowFilter { chans: Vec<usize>, filter: ViewFilter, row_ch: Vec<u32> },
    Monitor { chans: Vec<usize>, add: bool },
}

pub(super) struct ViewFrame<'a> {
    pub d: &'a TraceViewSettings,
    pub vs: &'a mut ViewState,
    pub l: &'a StreamLayout,
    pub nch: usize,
    pub audio: &'a Mutex<AudioSettings>,
    pub stats: &'a Mutex<ChannelStats>,
    pub dsp: &'a Mutex<DspSettings>,
    pub pyramids: &'a Mutex<engine::pyramid::Store>,
    pub reader: &'a Reader,
    pub rec_source: Option<&'a RecSource>,
    pub newest_sample: i64,
    pub notes: &'a crate::notes::Notes,
    pub edits: Vec<Edit>,
}

/// A time-axis label centred under its tick at `x`, top or bottom at `y`, shifted inward at
/// the plot's edges so it isn't cut off.
fn tick_label(p: &egui::Painter, x: f32, y: f32, top: bool, text: String, plot: egui::Rect, color: Color32) {
    let g = p.layout_no_wrap(text, egui::FontId::monospace(10.0), color);
    let size = g.size();
    let x0 = (x - size.x / 2.0).clamp(plot.min.x, (plot.max.x - size.x).max(plot.min.x));
    let y0 = if top { y } else { y - size.y };
    p.galley(egui::pos2(x0, y0), g, color);
}

/// Trace colours for one view: monitored channels by stereo position; electrodes sharing a
/// row by their position in the row; otherwise by headstage.
pub(super) fn view_colors(l: &StreamLayout, mon: &[usize], order: &[u32], rowtab: &[[u32; 2]]) -> Vec<[f32; 4]> {
    let mut c = channel_colors(l, mon, 1);
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

impl ViewFrame<'_> {
    pub fn draw(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let (d, l) = (self.d, self.l);
        let nch = l.num_channels();
        if nch == 0 {
            return;
        }
        let (row_ch, rowtab) = view_rows(l, d);
        let filters = chan_filters(l, d);
        let rs = RowSet { row_ch, rowtab, filters };
        if rs.rowtab.is_empty() {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "No channels selected", egui::FontId::proportional(16.0), Color32::GRAY);
            // Nothing to draw: drop the staged samples and count the window as drawn.
            self.vs.reset();
            self.vs.last_drawn = Instant::now();
            self.vs.last_data = Instant::now();
            return;
        }
        let ppp = ui.ctx().pixels_per_point();
        let used = used_filters(l, &rs.filters, &rs.row_ch);
        // Room for the per-row filter chips when the view draws electrodes.
        let label_w = if used.is_empty() { 64.0 } else { 64.0 + CHIP_W };
        let trig = d.triggered;
        let fs = l.sample_rate;
        let k = d.per_row.clamp(1, 8) as usize;
        let nrows_all = rs.rowtab.len();
        let reviewing = !trig && self.vs.review.is_some();
        // Free-running: TTL lanes under the traces (lines seen live or in the review image).
        // Triggered: a time axis instead. Review adds a time axis and the recording strip.
        let ttl_lines: Vec<usize> = if d.ttl_overlay && !trig {
            let seen = self.vs.ttl.seen() | self.vs.review.as_ref().and_then(|r| r.shown.as_ref()).map_or(0, |s| s.ttl.seen());
            (0..l.ttl_lines.min(16)).filter(|&i| seen & (1 << i) != 0 && d.ttl_hidden & (1 << i) == 0).collect()
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
        let cols = (plot.width() * ppp).floor().max(1.0) as u32;
        let g = PlotGeom { rect, plot, strip, ttl_h, label_w, cols, rows, first, k, ppp };
        if resp.hovered() {
            self.wheel(ui, &resp, &g, &rs, &used);
        }
        if !trig {
            let nav = nav_input(ui, &resp, plot);
            if nav.any() {
                self.nav(nav, cols);
            }
        }
        let bank = FilterBank::new(fs, d.band());
        if !trig && self.vs.review.is_some() {
            self.review(ui, &resp, &g, bank, &rs, &ttl_lines);
        } else {
            self.live(ui, &resp, &g, bank, &rs, &ttl_lines);
        }
    }

    /// Wheel over the plot: Ctrl scales (the row under the pointer alone; with Shift, or off
    /// the rows, whole filters), plain wheel scrolls rows. Alt+wheel is the time zoom.
    fn wheel(&mut self, ui: &egui::Ui, resp: &egui::Response, g: &PlotGeom, rs: &RowSet, used: &[ViewFilter]) {
        let (d, l) = (self.d, self.l);
        let PlotGeom { plot, rows, first, .. } = *g;
        let RowSet { row_ch, rowtab, filters } = rs;
        let nrows_all = rowtab.len();
        // egui turns Ctrl+wheel into a zoom factor (and no scroll delta): > 1 for wheel up.
        let (scroll, ctrl, alt, shift, zoom) = ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers.ctrl, i.modifiers.alt, i.modifiers.shift, i.zoom_delta()));
        if !(scroll != 0.0 || (ctrl && zoom != 1.0)) || alt {
            return;
        }
        if !ctrl {
            let step = wheel_rows(&mut self.vs.row_scroll, scroll);
            let r = (first as isize - step).clamp(0, (nrows_all - rows) as isize) as usize;
            if r != d.first_row {
                self.edits.push(Edit::FirstRow(r));
            }
            return;
        }
        let over = resp.hover_pos().filter(|p| plot.contains(*p)).map(|p| first + (((p.y - plot.min.y) / plot.height() * rows as f32) as usize).min(rows - 1));
        // The electrodes of the row under the pointer (all those overlaid in it).
        let row_chans: Vec<usize> = over
            .map(|r| {
                let [s0, n] = rowtab[r];
                (s0..s0 + n).map(|i| row_ch[i as usize] as usize).filter(|&c| l.channels[c].kind == ChannelKind::Electrode).collect()
            })
            .unwrap_or_default();
        // Wheel up: fewer µV per row, so the traces grow.
        let scale = 1.0 / zoom.clamp(0.5, 2.0);
        let edit = if !row_chans.is_empty() && !shift {
            // Ctrl+wheel over a row: that row alone.
            let channels = row_chans.iter().filter_map(|&c| chan_key(l, c).map(|k| (d.uv_of(Some(&k), filters[c]) * scale, k))).map(|(uv, k)| (k, uv.clamp(10.0, 5000.0))).collect();
            Edit::Uv { filters: Vec::new(), channels }
        } else {
            // Ctrl+Shift+wheel over a row: its whole filter; off the rows: every filter in
            // use. Per-row overrides on those filters scale along.
            let fs: Vec<ViewFilter> = if row_chans.is_empty() { if used.is_empty() { vec![d.filter] } else { used.to_vec() } } else { row_chans.iter().map(|&c| filters[c]).collect() };
            Edit::Uv {
                filters: ViewFilter::ALL.into_iter().filter(|f| fs.contains(f)).map(|f| (f, d.uv(f) * scale)).collect(),
                channels: d.channel_uv.iter().filter(|(k, _)| fs.contains(&d.filter_of(Some(k)))).map(|(k, uv)| (k.clone(), (uv * scale).clamp(10.0, 5000.0))).collect(),
            }
        };
        self.edits.push(edit);
    }

    /// Enter, move or leave scroll-back review.
    fn nav(&mut self, nav: Nav, cols: u32) {
        let fs = self.l.sample_rate;
        let window_s = self.d.window_s as f64;
        if nav.live {
            self.leave_review();
            return;
        }
        let vs = &mut *self.vs;
        // Live: zooming sets the time base, exactly like typing into the time box. It never
        // enters review; only panning back or Home does.
        if vs.review.is_none() && nav.zoom != 0.0 {
            let w = (window_s * nav.zoom).clamp(LIVE_MIN_S as f64, LIVE_MAX_S as f64) as f32;
            if w != self.d.window_s {
                self.edits.push(Edit::WindowS(w));
            }
        }
        let note = |vs: &mut ViewState, msg: String| vs.note = Some((Instant::now() + Duration::from_secs(4), msg));
        let Some(src) = self.rec_source else {
            if nav.goes_back() {
                note(vs, "Scroll-back reads the recording: start recording to review".into());
            }
            return;
        };
        if src.nch != self.nch {
            if nav.goes_back() {
                note(vs, format!("The last recording has {} channels; the board now sends {}", src.nch, self.nch));
            }
            return;
        }
        let end = src.first_sample + review::available(src);
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
            self.edits.push(Edit::WindowS((rv.span as f64 / fs) as f32));
        }
    }

    /// Back to live; the time base is brought into the live range (review reaches further).
    fn leave_review(&mut self) {
        if self.vs.review.take().is_some() {
            log::debug!("review '{}': back to live", self.d.name);
            // A zoom earlier in this frame may have asked for a span beyond the live range.
            self.edits.push(Edit::LiveWindow);
        } else if !(LIVE_MIN_S..=LIVE_MAX_S).contains(&self.d.window_s) {
            self.edits.push(Edit::LiveWindow);
        }
    }

    /// A live frame: upload the new samples, find triggers, draw the traces and overlays.
    fn live(&mut self, ui: &mut egui::Ui, resp: &egui::Response, g: &PlotGeom, bank: FilterBank, rs: &RowSet, ttl_lines: &[usize]) {
        let (d, l) = (self.d, self.l);
        let PlotGeom { rect, plot, strip, cols, rows, first, k, ppp, .. } = *g;
        let RowSet { row_ch, rowtab, filters } = rs;
        let used = &used_filters(l, filters, row_ch)[..];
        let nch = l.num_channels();
        let fs = l.sample_rate;
        let trig = d.triggered;
        let window = (d.window_s as f64 * fs).round().max(cols as f64) as u32;
        let vs = &mut *self.vs;
        // Triggered window in samples; together they must fit well inside the GPU ring.
        let (pre_ms, post_ms) = trig_window_ms(d);
        let pre = ((pre_ms as f64 * 1e-3 * fs).round() as u32).min(RING / 4);
        let post = ((post_ms as f64 * 1e-3 * fs).round() as u32).clamp(1, RING / 4);
        let span = pre + post;
        let slope_trig = trig && d.trig_source == TrigSource::Spike;
        let tc = slope_trig.then(|| trig_channel(l, d)).flatten();
        let nslots = if trig { d.trig_sweeps.clamp(1, 50) } else { 0 };
        let config = TraceConfig { nch, cols, window, bank, nslots };
        vs.last_drawn = Instant::now();
        if vs.last_cfg.as_ref() != Some(&config) {
            if let Some(c) = vs.last_cfg.as_ref().filter(|c| c.cols != config.cols || c.window != config.window || c.nch != config.nch || c.nslots != config.nslots) {
                log::info!("trace window '{}': {} → {} columns, window {} → {} samples; GPU state rebuilt", d.name, c.cols, config.cols, c.window, config.window);
                vs.reset = true;
            }
            vs.last_cfg = Some(config.clone());
        }
        // Old sweeps are meaningless after a change of window, line, filter or geometry.
        let sweep_key = sweep_hash(d, pre, post, nslots, filters, cols);
        if vs.reset || vs.sweep_key != Some(sweep_key) {
            vs.sweeps.clear();
            vs.sweep_key = Some(sweep_key);
            vs.slope_reset = true;
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
        if Arc::get_mut(&mut vs.upload).is_none() {
            vs.upload = Arc::default(); // last frame's still held by the renderer
        }
        let buf = Arc::get_mut(&mut vs.upload).expect("unique");
        buf.clear();
        buf.extend(vs.pending.drain(..n_new as usize * nch));
        let upload = vs.upload.clone();
        // The crossing level: the detector's threshold (sign per slope) or the fixed one.
        let level_uv = tc.map(|c| {
            let mag = if d.trig_level_uv > 0.0 { d.trig_level_uv } else { self.stats.lock().threshold_uv.get(c).copied().unwrap_or(0.0).abs() };
            if d.trig_slope == Slope::Neg { -mag } else { mag }
        });
        let sweep = if trig {
            match (slope_trig, tc, level_uv) {
                (false, ..) => vs.sweeps.scan(words, start, d.trig_line),
                (true, Some(c), Some(level)) => {
                    // Slope trigger: band-pass the channel (spike detector's band and CAR)
                    // and look for crossings over the new samples.
                    let st = vs.slope.get_or_insert_with(|| SlopeTrigger::new(fs));
                    if std::mem::take(&mut vs.slope_reset) {
                        st.reset();
                    }
                    let car = self.dsp.lock().car.clone().filter(|car| car.subtract.contains(&c));
                    let p = SlopeParams {
                        level,
                        slope: d.trig_slope,
                        align: d.trig_align,
                        holdoff: ((DEAD_S + d.trig_holdoff_ms as f64 * 1e-3) * fs) as u32,
                        window: d.trig_window.then(|| if d.trig_slope == Slope::Neg { -d.trig_level2_uv.abs() } else { d.trig_level2_uv.abs() }),
                    };
                    let mut found = Vec::new();
                    for i in 0..n_new as usize {
                        let row = &upload[i * nch..(i + 1) * nch];
                        let x = row[c] as f32 - car.as_ref().map_or(0.0, |car| car.mean(row));
                        st.feed(x * l.channels[c].bit_volts, start + i as u64, &p, &mut found);
                    }
                    vs.slope_count += found.len() as u64;
                    for a in found {
                        vs.sweeps.push_trigger(a);
                    }
                }
                _ => {}
            }
            let cuts = vs.sweeps.take_ready(vs.counter, pre, post, nslots, RING, MAX_CUTS);
            SweepFrame { ring0: (start % RING as u64) as u32, span, cuts, newest: vs.sweeps.newest_slot(nslots), filled: vs.sweeps.filled(nslots) }
        } else {
            SweepFrame::default()
        };
        let cursor_pos = (vs.counter % window as u64) as f64;
        let cursor_col = ((cursor_pos * cols as f64 / window as f64) as u32).min(cols - 1);
        let row_h_px = plot.height() * ppp / rows as f32;
        // Per-channel scaling (see traces.wgsl): electrodes follow the view's µV/row and
        // display filter; AUX/ADC are drawn raw in volts on their own row range.
        // Flags: bits 3-4 filter preset, bit 1 CAR reference, bit 2 CAR subtract (electrodes).
        let car_on = car_applies(d, used);
        let info = view_chinfo(l, d, filters);
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
        let colors = want_colors.then(|| Arc::new(view_colors(l, &self.audio.lock().channels, row_ch, rowtab)));
        let frame = TraceFrame {
            view: d.id,
            config,
            reset,
            upload,
            upload_off: 0,
            n_new,
            pos0,
            cursor_col,
            first_row: first as u32,
            nrows: rows as u32,
            kmax: k as u32,
            uv_per_row: d.uv(d.filter),
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
            x_shift: 0.0,
            x_scale: 1.0,
            rot: 0,
            columns: None,
        };
        ui.painter().rect_filled(rect, 0.0, Color32::from_rgb(12, 13, 16));
        let p = ui.painter_at(rect);
        let span_ms = pre_ms + post_ms;
        let t_to_x = |t_ms: f32| plot.min.x + (t_ms + pre_ms) / span_ms.max(1e-3) * plot.width();
        if trig && !slope_trig && vs.sweeps.count() > 0 {
            // Laser-on shading under the traces: newest sweep's TTL high time.
            let w_ms = vs.sweeps.newest_width().map(|w| w as f32 * 1e3 / fs as f32).unwrap_or(post_ms).min(post_ms);
            let c = crate::ttl::line_color(d.trig_line);
            p.rect_filled(egui::Rect::from_min_max(egui::pos2(t_to_x(0.0), plot.min.y), egui::pos2(t_to_x(w_ms).max(t_to_x(0.0) + 1.0), plot.max.y)), 0.0, Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 34));
        }
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(plot, TraceCallback(frame)));
        // Overlay: labels, separators, cursor / time axis.
        let analog_visible = self.row_labels(ui, &p, g, rs, resp);
        let vs = &*self.vs;
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
            let step = [0.1f32, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0].into_iter().find(|s| span_ms / s <= 12.0).unwrap_or(500.0);
            let decimals = if step < 1.0 { 1 } else { 0 };
            let mut t = -(pre_ms / step).floor() * step;
            while t <= post_ms + 1e-3 {
                let x = t_to_x(t);
                p.line_segment([egui::pos2(x, plot.max.y), egui::pos2(x, plot.max.y + 4.0)], Stroke::new(1.0, dim));
                tick_label(&p, x, rect.max.y - 1.0, false, format!("{t:.decimals$}"), plot, dim);
                t += step;
            }
            p.text(egui::pos2(rect.min.x + 4.0, rect.max.y - 1.0), egui::Align2::LEFT_BOTTOM, "ms", egui::FontId::monospace(10.0), dim);
            let source = if slope_trig {
                match (tc, level_uv) {
                    (Some(c), Some(level)) if level != 0.0 => format!("{} {} {:+.0} µV{} · t0 {}", l.channels[c].name, if d.trig_slope == Slope::Neg { "falling through" } else { "rising through" }, level, if d.trig_level_uv > 0.0 { "" } else { " (detector)" }, if d.trig_align == TrigAlign::Trough { "trough" } else { "crossing" }),
                    (Some(c), _) => format!("{} · waiting for the noise estimate", l.channels[c].name),
                    _ => "slope trigger · pick a channel (Alt+click a row label)".into(),
                }
            } else {
                format!("TTL{} rising", d.trig_line + 1)
            };
            let waiting = match (vs.sweeps.count(), slope_trig) {
                (0, false) => format!(" · waiting for TTL{}", d.trig_line + 1),
                (0, true) if tc.is_some() && level_uv.is_some_and(|l| l != 0.0) => " · waiting for a crossing".to_string(),
                _ => String::new(),
            };
            p.text(
                egui::pos2(plot.max.x - 4.0, plot.max.y - 4.0),
                egui::Align2::RIGHT_BOTTOM,
                format!("{source} · {} / {} sweeps · {}{}{}", vs.sweeps.filled(nslots), nslots, uv_text(d, used), if car_on { " · CAR" } else { "" }, waiting),
                egui::FontId::monospace(11.0),
                dim,
            );
            // The crossing level, dashed on the trigger channel's row; drag it to fix a level.
            if let (Some(c), Some(level)) = (tc, level_uv)
                && let Some(r) = (0..rows).find(|&r| {
                    let [s0, n] = rowtab[first + r];
                    (s0..s0 + n).any(|i| row_ch[i as usize] as usize == c)
                })
            {
                let rh = plot.height() / rows as f32;
                let yc = plot.min.y + (r as f32 + 0.5) * rh;
                let uv = d.uv_of(chan_key(l, c).as_deref(), filters[c]);
                let y = (yc - level * rh / uv).clamp(plot.min.y, plot.max.y);
                let col = Color32::from_rgb(255, 170, 60);
                let handle = egui::Rect::from_min_max(egui::pos2(plot.min.x, y - 4.0), egui::pos2(plot.max.x, y + 4.0));
                let hr = ui.interact(handle, ui.scope_id().with(("triglevel", d.id)), egui::Sense::drag()).on_hover_cursor(egui::CursorIcon::ResizeVertical).on_hover_text(format!("Trigger level {level:+.0} µV · drag to set a fixed level for this window"));
                if hr.dragged()
                    && let Some(pos) = hr.interact_pointer_pos()
                {
                    let v = (yc - pos.y) * uv / rh;
                    let mag = v.abs().clamp(1.0, 5000.0);
                    // The level keeps the slope's sign; dragging across the centre is ignored.
                    if (d.trig_slope == Slope::Neg) == (v < 0.0) {
                        self.edits.push(Edit::TrigLevel(mag));
                    }
                }
                let dash = if hr.hovered() || hr.dragged() { Stroke::new(1.5, col) } else { Stroke::new(1.0, Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 170)) };
                let mut x = plot.min.x;
                while x < plot.max.x {
                    p.line_segment([egui::pos2(x, y), egui::pos2((x + 6.0).min(plot.max.x), y)], dash);
                    x += 10.0;
                }
                p.text(egui::pos2(plot.min.x + 3.0, y - 2.0), egui::Align2::LEFT_BOTTOM, format!("{level:+.0} µV"), egui::FontId::monospace(10.0), col);
                if level == 0.0 {
                    p.text(egui::pos2(plot.min.x + 3.0, y - 12.0), egui::Align2::LEFT_BOTTOM, "no threshold yet", egui::FontId::monospace(10.0), col);
                }
            }
        } else {
            vs.ttl.draw(&p, &TtlGeom::live(plot, strip, rect.min.x + 4.0, ppp, cursor_col, None), ttl_lines);
            // Notes: absolute sample → position in the sweep (window position = counter % window).
            let base = self.newest_sample - (vs.pending.len() / nch) as i64 - vs.counter as i64;
            let (counter, w64) = (vs.counter as i64, window as i64);
            self.draw_note_markers(&p, plot, ui, |s| {
                let c = s - base;
                (c <= counter && c > counter - w64).then(|| plot.min.x + (c.rem_euclid(w64) as f64 * cols as f64 / w64 as f64) as f32 / ppp)
            });
            let x = plot.min.x + cursor_col as f32 / ppp;
            p.line_segment([egui::pos2(x, plot.min.y), egui::pos2(x, plot.max.y)], Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 40)));
            p.text(egui::pos2(plot.max.x - 4.0, plot.max.y - 4.0), egui::Align2::RIGHT_BOTTOM, format!("{:.2} s · {}{}{}", d.window_s, uv_text(d, used), if car_on { " · CAR" } else { "" }, if analog_visible { format!(" · ADC {:.2} V/row · AUX 2.5 V/row", d.adc_v_per_row) } else { String::new() }), egui::FontId::monospace(11.0), dim);
        }
    }

    /// One frame of a window in review: read and filter the stretch on screen (double
    /// buffered on the GPU), then draw it with labels, TTL lanes, a time axis, a "behind live"
    /// badge and a strip for the whole recording (click or drag to jump).
    fn review(&mut self, ui: &mut egui::Ui, resp: &egui::Response, g: &PlotGeom, bank: FilterBank, rs: &RowSet, ttl_lines: &[usize]) {
        let (d, l) = (self.d, self.l);
        let PlotGeom { rect, plot, strip, ttl_h, cols, rows, first, k, ppp, .. } = *g;
        let RowSet { row_ch, rowtab, filters } = rs;
        let fs = l.sample_rate;
        let nch = l.num_channels();
        let Some(src) = self.rec_source.filter(|s| s.nch == nch) else {
            self.leave_review();
            return;
        };
        let end = src.first_sample + review::available(src);
        let (min_span, max_span) = review_span_limits(cols, fs);
        let used = used_filters(l, filters, row_ch);
        let car_on = car_applies(d, &used);
        let info = Arc::new(view_chinfo(l, d, filters));
        let order = Arc::new(row_ch.clone());
        let rowtab_gpu = Arc::new(rowtab.clone());
        let colors = Arc::new(view_colors(l, &self.audio.lock().channels, row_ch, rowtab));
        let band = d.band();
        let lead = (used.iter().map(|&f| review::lead_in_s(f, &band)).fold(0.0, f64::max) * fs).round() as i64;
        let id = d.id;
        let newest = self.newest_sample;
        let vs = &mut *self.vs;
        // Not stalled: this window is drawing, just not the live samples.
        vs.last_drawn = Instant::now();
        vs.last_data = Instant::now();
        let Some(rv) = vs.review.as_mut() else { return };
        rv.clamp(src.first_sample, end, min_span, max_span);
        let key = review::Key { t0: rv.t0, span: rv.span, cols, nch, filters: filters.clone(), band: band_bits(d), car: car_on.then(|| (d.car.reference.clone(), d.car.subtract.clone())) };
        let row_h_px = plot.height() * ppp / rows as f32;
        let frame = |view: u32, config: TraceConfig, reset: bool, (upload, upload_off): (Arc<Vec<i16>>, usize), n_new: u32, pos0: u32, paint: bool, xf: (f32, f32, u32)| TraceFrame {
            view,
            config,
            reset,
            upload,
            upload_off,
            n_new,
            pos0,
            cursor_col: 0,
            first_row: first as u32,
            nrows: rows as u32,
            kmax: k as u32,
            uv_per_row: d.uv(d.filter),
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
            x_shift: xf.0,
            x_scale: xf.1,
            rot: xf.2,
            columns: None,
        };
        // Spans of a bucket or more per column come from the min/max pyramid: the whole
        // column buffer is computed here and uploaded in one go, so the image changes with the
        // pointer. While the pyramid is still being backfilled (band changed, or the raw one is
        // closing a gap), the covered part is drawn and the rest redrawn every 300 ms.
        let use_pyr = rv.span as u64 / cols.max(1) as u64 >= engine::pyramid::BUCKET;
        let mut served = false;
        if use_pyr && rv.filling.is_none() {
            let stale = rv.shown.as_ref().is_none_or(|s| s.key != key);
            let refresh = rv.shown.as_ref().is_some_and(|s| s.key == key && s.pyramid.is_some_and(|(at, complete)| !complete && at.elapsed() > Duration::from_millis(300)));
            if stale || refresh {
                let st = self.pyramids.lock();
                if let Some((columns, words, complete)) = pyramid_columns(&st, src, id, l, filters, &key) {
                    drop(st);
                    let buf = rv.shown.as_ref().map_or(0, |s| 1 - s.buf);
                    let config = TraceConfig { nch, cols, window: rv.span.max(1), bank: bank.clone(), nslots: 0 };
                    let mut fr = frame(review_key(id, buf), config.clone(), true, (Arc::new(Vec::new()), 0), 0, 0, false, (0.0, 1.0, 0));
                    fr.columns = Some(Arc::new(columns));
                    ui.painter().add(egui_wgpu::Callback::new_paint_callback(plot, TraceCallback(fr)));
                    rv.shown = Some(review::Shown { key: key.clone(), buf, config, ttl: TtlOverlay::from_columns(&words), pos_t0: 0, pyramid: Some((Instant::now(), complete)) });
                    if !complete {
                        ui.ctx().request_repaint_after(Duration::from_millis(300));
                    }
                }
            }
            served = rv.shown.as_ref().is_some_and(|s| s.key == key);
        }
        // Spans too long to read directly wait for the pyramid.
        let direct_ok = rv.span as f64 / fs <= DIRECT_MAX_S as f64;
        // One image in flight at a time; it always completes (cancelling it every frame while
        // the pointer moves would mean never updating). Meanwhile the shown image is drawn
        // panned and zoomed to the current stretch, so the traces follow the pointer.
        if !served && direct_ok && rv.filling.is_none() && rv.shown.as_ref().is_none_or(|s| s.key != key) {
            rv.generation += 1;
            // A pan to the right by less than a window appends to the shown image: the
            // filter state continues from its end, with no lead-in and no refiltering.
            let append = rv.shown.as_ref().filter(|s| {
                let d = key.t0 - s.key.t0;
                d > 0 && d < s.key.span as i64 && review::Key { t0: s.key.t0, ..key.clone() } == s.key
            });
            let (req, buf, append) = match append {
                Some(s) => {
                    let from = s.key.t0 + s.key.span as i64;
                    let n = (key.t0 - s.key.t0) as usize;
                    (review::Req { view: id, generation: rv.generation, src: src.clone(), from, n, words_from: from, words_n: n }, s.buf, true)
                }
                None => {
                    let buf = rv.shown.as_ref().map_or(0, |s| 1 - s.buf);
                    (review::Req { view: id, generation: rv.generation, src: src.clone(), from: rv.t0 - lead, n: (lead + rv.span as i64) as usize, words_from: rv.t0, words_n: rv.span as usize }, buf, false)
                }
            };
            let _ = self.reader.tx.send(req);
            rv.filling = Some(review::Filling { key: key.clone(), generation: rv.generation, buf, append, loaded: None, started: Instant::now(), loaded_at: None });
        }
        ui.painter().rect_filled(rect, 0.0, Color32::from_rgb(12, 13, 16));
        // Feed the back buffer (in chunks small enough for the GPU's binding limit).
        let mut complete = false;
        if let Some(f) = rv.filling.as_mut()
            && let Some(ld) = f.loaded.as_mut()
        {
            let total = ld.data.len() / nch.max(1);
            let chunk = ((8usize << 20) / nch.max(1)).max(1).min(total - ld.fed);
            let span = f.key.span.max(1);
            let config = TraceConfig { nch, cols: f.key.cols, window: span, bank: bank.clone(), nslots: 0 };
            let upload = (ld.data.clone(), ld.fed * nch);
            let pos0 = ((ld.pos0 as u64 + ld.fed as u64) % span as u64) as u32;
            let fr = frame(review_key(id, f.buf), config, ld.fed == 0 && !f.append, upload, chunk as u32, pos0, false, (0.0, 1.0, 0));
            ui.painter().add(egui_wgpu::Callback::new_paint_callback(plot, TraceCallback(fr)));
            if f.append && let Some(sh) = rv.shown.as_mut() {
                // Ingests run before this frame's draw, so the shown image advances with each
                // chunk (the stretch may be shorter than asked if the recorder hadn't flushed
                // that far; the rest comes with the next request).
                let w0 = ld.fed.min(ld.words.len());
                let w1 = (ld.fed + chunk).min(ld.words.len());
                sh.ttl.ingest(&ld.words[w0..w1], sh.pos_t0, span, f.key.cols);
                sh.key.t0 += chunk as i64;
                sh.pos_t0 = ((sh.pos_t0 as u64 + chunk as u64) % span as u64) as u32;
            }
            ld.fed += chunk;
            complete = ld.fed >= total;
            if !complete {
                ui.ctx().request_repaint();
            }
        }
        if complete && let Some(f) = rv.filling.take() {
            let ld = f.loaded.expect("complete implies loaded");
            let span = f.key.span.max(1);
            let read_ms = f.loaded_at.map_or(0.0, |t| (t - f.started).as_secs_f32() * 1e3);
            log::debug!("review '{}': {} {} samples in {:.1} ms (read {:.1} ms)", d.name, if f.append { "appended" } else { "filled" }, ld.data.len() / nch.max(1), f.started.elapsed().as_secs_f32() * 1e3, read_ms);
            if !f.append {
                let mut ttl = TtlOverlay::default();
                ttl.reset();
                if !ld.words.is_empty() {
                    ttl.ingest(&ld.words, 0, span, f.key.cols);
                }
                let config = TraceConfig { nch, cols: f.key.cols, window: span, bank: bank.clone(), nslots: 0 };
                rv.shown = Some(review::Shown { key: f.key, buf: f.buf, config, ttl, pos_t0: 0, pyramid: None });
            }
        }
        // Draw the shown image where its stretch falls in the current one.
        let xf = rv.shown.as_ref().map_or((0.0, 1.0, 0), |sh| {
            let x_scale = sh.key.span as f32 / rv.span.max(1) as f32;
            let x_shift = (sh.key.t0 - rv.t0) as f64 / rv.span.max(1) as f64 * cols as f64;
            (x_shift as f32, x_scale, sh.rot())
        });
        if let Some(sh) = rv.shown.as_ref() {
            let fr = frame(review_key(id, sh.buf), sh.config.clone(), false, (Arc::new(Vec::new()), 0), 0, 0, true, xf);
            ui.painter().add(egui_wgpu::Callback::new_paint_callback(plot, TraceCallback(fr)));
        }
        // Overlay.
        let p = ui.painter_at(rect);
        let dim = Color32::from_gray(120);
        let accent = Color32::from_rgb(120, 180, 255);
        let (t0, span) = (rv.t0, rv.span as i64);
        let rel = |s: i64| (s - src.first_sample) as f64 / fs;
        let ttl_strip = egui::Rect::from_min_max(strip.min, egui::pos2(strip.max.x, strip.min.y + ttl_h));
        let mut review_notes: Option<(i64, i64)> = None;
        if let Some(sh) = rv.shown.as_ref() {
            sh.ttl.draw(&p, &TtlGeom { plot, strip: ttl_strip, label_x: rect.min.x + 4.0, ppp, cursor: 0, gap: Some(0), rot: xf.2, x_shift: xf.0, x_scale: xf.1 }, ttl_lines);
            let (st0, sspan) = (sh.key.t0, sh.key.span as i64);
            review_notes = Some((st0, sspan));
        } else {
            let msg = if use_pyr && !direct_ok { "Building the overview of the recording…" } else { "Reading the recording…" };
            p.text(plot.center(), egui::Align2::CENTER_CENTER, msg, egui::FontId::proportional(15.0), dim);
        }
        // Time axis (time since the recording started).
        let axis_y = strip.min.y + ttl_h + 1.0;
        let span_s = span as f64 / fs;
        let step = [0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 20.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1200.0, 1800.0].into_iter().find(|s| span_s / s <= 8.0).unwrap_or(3600.0);
        let x_of = |s: i64| plot.min.x + ((s - t0) as f64 / span.max(1) as f64) as f32 * plot.width();
        let mut t = (rel(t0) / step).ceil() * step;
        while t <= rel(t0 + span) + 1e-9 {
            let x = x_of(src.first_sample + (t * fs).round() as i64);
            p.line_segment([egui::pos2(x, axis_y), egui::pos2(x, axis_y + 4.0)], Stroke::new(1.0, dim));
            tick_label(&p, x, axis_y + 4.0, true, review::fmt_time(t, step < 0.1), plot, dim);
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
        let loading = if rv.filling.is_some() {
            " · reading"
        } else if rv.shown.as_ref().is_some_and(|s| s.pyramid.is_some_and(|(_, complete)| !complete)) {
            " · building the overview"
        } else {
            ""
        };
        let err = rv.error.clone();
        let badge = format!("REVIEW  {} – {}  ·  {} behind live{loading}", review::fmt_time(rel(t0), span_s < 1.0), review::fmt_time(rel(t0 + span), span_s < 1.0), review::fmt_time(behind, false));
        let live_r = egui::Rect::from_min_size(egui::pos2(plot.max.x - 74.0, plot.min.y + 4.0), egui::vec2(70.0, 20.0));
        p.text(egui::pos2(live_r.min.x - 8.0, live_r.center().y), egui::Align2::RIGHT_CENTER, badge, egui::FontId::monospace(11.0), accent);
        let go_live = ui.put(live_r, egui::Button::new("LIVE >")).on_hover_text("Back to live (End)").clicked();
        if let Some(e) = err {
            p.text(plot.center(), egui::Align2::CENTER_CENTER, e, egui::FontId::proportional(13.0), Color32::from_rgb(240, 120, 100));
        }
        p.text(
            egui::pos2(plot.max.x - 4.0, plot.max.y - 4.0),
            egui::Align2::RIGHT_BOTTOM,
            format!("{} · {}{}", if span_s < 1.0 { format!("{:.0} ms", span_s * 1e3) } else { format!("{span_s:.2} s") }, uv_text(d, &used), if car_on { " · CAR" } else { "" }),
            egui::FontId::monospace(11.0),
            dim,
        );
        if let Some((st0, sspan)) = review_notes {
            self.draw_note_markers(&p, plot, ui, |s| (s >= st0 && s < st0 + sspan).then(|| plot.min.x + ((s - st0) as f64 / sspan.max(1) as f64) as f32 * plot.width()));
        }
        let _ = self.row_labels(ui, &p, g, rs, resp);
        if go_live {
            self.leave_review();
        }
    }

    /// Channel labels down the left edge of a trace plot; clicking one monitors it on audio
    /// (Ctrl/Shift adds or removes it). Returns whether an AUX/ADC row is visible.
    fn row_labels(&mut self, ui: &egui::Ui, p: &egui::Painter, g: &PlotGeom, rs: &RowSet, resp: &egui::Response) -> bool {
        let (d, l) = (self.d, self.l);
        let PlotGeom { rect, plot, rows, first, label_w, .. } = *g;
        let RowSet { row_ch, rowtab, filters } = rs;
        let rh = plot.height() / rows as f32;
        let mon = self.audio.lock().channels.clone();
        let click = resp.clicked().then(|| resp.interact_pointer_pos()).flatten();
        let alt = ui.input(|i| i.modifiers.alt);
        let mut toggle: Option<Vec<usize>> = None;
        let mut trig_pick: Option<usize> = None;
        let mut set_filter: Option<(Vec<usize>, ViewFilter)> = None;
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
                // Filter chip: one segment per preset; a row overlaying channels with
                // different filters shows each of them hatched.
                if is_electrode {
                    let h = (rh - 2.0).clamp(8.0, 14.0);
                    let chip = egui::Rect::from_min_size(egui::pos2(rect.min.x + label_w - CHIP_W - 3.0, y - h / 2.0), egui::vec2(CHIP_W, h));
                    let here: Vec<ViewFilter> = ViewFilter::ALL.into_iter().filter(|f| chans.iter().any(|&c| filters[c] == *f)).collect();
                    let seg_w = CHIP_W / ViewFilter::ALL.len() as f32;
                    for (j, f) in ViewFilter::ALL.into_iter().enumerate() {
                        let seg = egui::Rect::from_min_size(egui::pos2(chip.min.x + j as f32 * seg_w, chip.min.y), egui::vec2(seg_w, h));
                        let sr = ui.interact(seg, ui.scope_id().with(("rowfilt", d.id, first + r, j)), egui::Sense::click()).on_hover_text(filter_help(f, d));
                        let on = here.contains(&f);
                        let fill = if on { ui.visuals().selection.bg_fill } else if sr.hovered() { Color32::from_gray(60) } else { Color32::from_gray(34) };
                        p.rect_filled(seg.shrink(0.5), 2.0, fill);
                        if on && here.len() > 1 {
                            hatch(p, seg.shrink(0.5));
                        }
                        p.text(seg.center(), egui::Align2::CENTER_CENTER, f.letter(), egui::FontId::monospace((h * 0.8).min(10.0)), if on { Color32::WHITE } else { Color32::from_gray(140) });
                        if sr.clicked() {
                            set_filter = Some((chans.clone(), f));
                        }
                    }
                }
            }
            // Audio monitors electrodes only; a grouped row monitors the whole group.
            // Alt+click makes the row's (first) channel the window's slope trigger.
            if let Some(pos) = click
                && is_electrode
                && lab.contains(pos)
            {
                if alt {
                    trig_pick = Some(ch);
                } else {
                    toggle = Some(chans);
                }
            }
        }
        if let Some(chans) = toggle {
            let add = ui.input(|i| i.modifiers.command || i.modifiers.shift);
            self.edits.push(Edit::Monitor { chans, add });
        }
        if let (Some(ch), Some(key)) = (trig_pick, trig_pick.and_then(|c| chan_key(l, c))) {
            self.edits.push(Edit::TrigChannel(key));
            self.vs.review = None;
            self.vs.note = Some((Instant::now() + Duration::from_secs(2), format!("Slope trigger on {}", l.channels[ch].name)));
        }
        if let Some((chans, filter)) = set_filter {
            self.edits.push(Edit::RowFilter { chans, filter, row_ch: row_ch.clone() });
        }
        analog_visible
    }

    /// Note markers over a plot: `x_of` maps a sample to x (None = off screen).
    fn draw_note_markers(&self, p: &egui::Painter, plot: egui::Rect, ui: &egui::Ui, x_of: impl Fn(i64) -> Option<f32>) {
        let col = Color32::from_rgb(255, 200, 90);
        let hover = ui.input(|i| i.pointer.hover_pos());
        for n in &self.notes.notes {
            let Some(x) = x_of(n.sample) else { continue };
            p.line_segment([egui::pos2(x, plot.min.y), egui::pos2(x, plot.max.y)], Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 200, 90, 110)));
            let short: String = n.text.chars().take(32).collect();
            let more = if n.text.chars().count() > 32 { "…" } else { "" };
            let r = p.text(egui::pos2(x + 3.0, plot.min.y + 2.0), egui::Align2::LEFT_TOP, format!("{short}{more}"), egui::FontId::proportional(12.0), col);
            if hover.is_some_and(|h| r.expand(3.0).contains(h) || ((h.x - x).abs() < 3.0 && plot.contains(h))) {
                let full = format!("{}  {}", note_time(self.l.sample_rate, self.rec_source, n.sample), n.text);
                let g = p.layout(full, egui::FontId::proportional(13.0), Color32::WHITE, 420.0);
                let at = egui::pos2(x + 6.0, plot.min.y + 20.0);
                p.rect_filled(egui::Rect::from_min_size(at, g.size()).expand(5.0), 4.0, Color32::from_rgba_unmultiplied(20, 20, 24, 235));
                p.galley(at, g, Color32::WHITE);
            }
        }
    }
}
