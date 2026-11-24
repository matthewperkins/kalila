use crate::config::{Config, ElectrodeMap, OutputSettings, ViewFilter, electrode_keys};
use crate::spikes::{SLOTS, SpikeCallback, SpikeFrame, SpikeGpu};
use crate::sweep::Sweeps;
use crate::traces::{FilterBank, MAX_CUTS, RING, SweepFrame, TraceCallback, TraceConfig, TraceFrame, TraceGpus, review_key};
use crate::ttl::{TtlGeom, TtlOverlay};
use eframe::egui::{self, Color32, RichText, Stroke};
use eframe::egui_wgpu;
use engine::audio::GainMode;
use engine::dsp::{Spike, ThresholdMode};
use engine::rhythm::{BoardSettings, ChannelKind, LiveCommand, StreamLayout};
use engine::{BoardChoice, Engine};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

mod views;
use views::{CarTarget, SharedViews, ViewsShared};

const PALETTE: [[f32; 3]; 8] = [
    [0.40, 0.76, 0.95],
    [0.55, 0.85, 0.45],
    [0.98, 0.62, 0.35],
    [0.85, 0.50, 0.90],
    [0.95, 0.85, 0.40],
    [0.45, 0.90, 0.80],
    [0.95, 0.50, 0.55],
    [0.70, 0.70, 0.95],
];
/// Colour of an audio-monitored channel, from its stereo position in the mix: amber (left)
/// through the monitor yellow (centre, also a lone channel) to lime (right).
fn monitor_color(i: usize, n: usize) -> [f32; 4] {
    const L: [f32; 3] = [1.0, 0.62, 0.15];
    const C: [f32; 3] = [1.0, 0.95, 0.35];
    const R: [f32; 3] = [0.72, 1.0, 0.25];
    let p = engine::audio::pan_position(i, n) / 0.8;
    let (a, b, t) = if p < 0.0 { (C, L, -p) } else { (C, R, p) };
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t, 1.0]
}

fn monitor_color32(mon: &[usize], ch: usize) -> Option<Color32> {
    let i = mon.iter().position(|&c| c == ch)?;
    let c = monitor_color(i, mon.len());
    Some(Color32::from_rgb((c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8))
}
/// Colours for electrodes overlaid in one row (by position in the group); no yellow, which
/// marks the audio-monitored channel.
const GROUP_PALETTE: [[f32; 3]; 8] = [[0.92, 0.92, 0.92], [0.35, 0.80, 1.00], [1.00, 0.42, 0.72], [0.50, 0.92, 0.50], [1.00, 0.60, 0.25], [0.65, 0.55, 1.00], [0.30, 0.90, 0.85], [1.00, 0.35, 0.35]];

/// How long the "not closing while recording" message stays up.
const CLOSE_REFUSED_MSG: Duration = Duration::from_secs(4);

/// Badge colour marking where the common average reference is applied.
const CAR_COLOR: Color32 = Color32::from_rgb(90, 200, 190);

/// A bold section heading, with a "CAR" badge when the reference is applied in that section.
fn section_title(ui: &egui::Ui, name: &str, car: bool) -> egui::WidgetText {
    let mut job = egui::text::LayoutJob::default();
    RichText::new(name).strong().append_to(&mut job, ui.style(), egui::FontSelection::Default, egui::Align::Center);
    if car {
        RichText::new("  CAR").small().strong().color(CAR_COLOR).append_to(&mut job, ui.style(), egui::FontSelection::Default, egui::Align::Center);
    }
    job.into()
}

pub struct App {
    cfg: Config,
    engine: Engine,
    /// Board settings being edited (applied on "Apply").
    edit_board: BoardSettings,
    /// Trace views (windows), shared with their deferred viewports. Authoritative for the
    /// view settings (copied into `cfg.views` on save).
    shared: SharedViews,
    render_state: egui_wgpu::RenderState,
    icon: Option<Arc<egui::IconData>>,
    /// Scripted test input (`KALILA_UI_SCRIPT`), off by default.
    ui_script: Option<crate::uiscript::UiScript>,
    /// Which CAR the CAR window is editing, if open.
    car_window: Option<CarTarget>,
    colors_dirty: bool,
    spike_colors: Option<Arc<Vec<[f32; 4]>>>,
    // spikes
    spike_slot: Vec<usize>,
    spike_new: Vec<(usize, usize, f32, Arc<Spike>)>,
    spike_times: Vec<VecDeque<f64>>,
    spike_reset: bool,
    t0: Instant,
    // stats
    frame_times: VecDeque<f32>,
    last_frame: Instant,
    confirm_stop: bool,
    audio_text: String,
    gpu_name: String,
    toast: Option<(String, Instant)>,
    /// When a window close was last refused because a recording is running (shows a message).
    close_refused: Option<Instant>,
    dirty_cfg: Instant,
    /// Wheel movement over the spike grid not yet turned into rows (`wheel_rows`).
    spike_scroll: f32,
    /// Audio underruns: (start of the current minute, the total then, the count in the minute
    /// before), so the chip shows recent trouble, not the total since startup.
    underrun_window: std::cell::Cell<(Instant, u64, u64)>,
    autostart: bool,
    // outputs
    was_acquiring: bool,
    /// Manual TTL-out word (reset to 0 at every acquisition start, as the board does).
    ttl_out_word: u16,
    /// Last DAC route sent per DAC: (electrode, threshold µV).
    dac_sent: [Option<(Option<usize>, f32)>; 8],
    /// Last board-level output settings sent.
    outputs_sent: Option<(bool, bool, u64, i32)>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config, autostart: bool) -> Self {
        let rs = cc.wgpu_render_state.as_ref().expect("wgpu renderer required");
        let info = rs.adapter.get_info();
        log::info!("GPU: {} ({:?}, driver {} {})", info.name, info.backend, info.driver, info.driver_info);
        {
            let mut r = rs.renderer.write();
            r.callback_resources.insert(TraceGpus::new(rs.target_format));
            r.callback_resources.insert(SpikeGpu::new(&rs.device, rs.target_format));
        }
        style(&cc.egui_ctx);
        let mut engine = Engine::new(cfg.record_root.clone());
        *engine.audio_settings.lock() = cfg.audio.clone();
        *engine.dsp_settings.lock() = cfg.dsp.clone();
        engine.pyramids.lock().budget = (cfg.review_ram_gb.max(0.1) as f64 * 1e9) as usize;
        let choice = if cfg.open_hardware_on_start { BoardChoice::Hardware } else { BoardChoice::Simulated(cfg.sim_headstages.clone()) };
        engine.open_board(choice, cfg.board.clone());
        if cfg.load_error.is_some() {
            engine.last_error = cfg.load_error.clone();
        }
        let audio_text = channels_to_text(&cfg.audio.channels);
        let shared = Arc::new(parking_lot::Mutex::new(ViewsShared::new(cfg.views.clone(), engine.audio_settings.clone(), engine.audio_stats.clone(), engine.dsp_settings.clone(), engine.channel_stats.clone(), engine.pyramids.clone())));
        shared.lock().notes = crate::notes::Notes::new(&cfg.notes);
        Self {
            edit_board: cfg.board.clone(),
            cfg,
            engine,
            shared,
            render_state: rs.clone(),
            icon: eframe::icon_data::from_png_bytes(include_bytes!("../../packaging/icon/kalila-128.png")).ok().map(Arc::new),
            ui_script: crate::uiscript::UiScript::from_env(),
            car_window: None,
            colors_dirty: true,
            spike_colors: None,
            spike_slot: Vec::new(),
            spike_new: Vec::new(),
            spike_times: Vec::new(),
            spike_reset: true,
            t0: Instant::now(),
            frame_times: VecDeque::new(),
            last_frame: Instant::now(),
            confirm_stop: false,
            audio_text,
            gpu_name: format!("{} ({:?})", info.name, info.backend),
            toast: None,
            close_refused: None,
            dirty_cfg: Instant::now(),
            spike_scroll: 0.0,
            underrun_window: std::cell::Cell::new((Instant::now(), 0, 0)),
            autostart,
            was_acquiring: false,
            ttl_out_word: 0,
            dac_sent: [None; 8],
            outputs_sent: None,
        }
        .with_load_notice()
    }

    fn with_load_notice(mut self) -> Self {
        if let Some(n) = self.cfg.load_notice.take() {
            log::info!("{n}");
            self.toast(n);
        }
        self
    }

    fn layout(&self) -> Option<StreamLayout> {
        self.engine.layout().cloned()
    }

    fn electrode_rows(&self) -> usize {
        self.engine.layout().map(|l| l.num_electrodes()).unwrap_or(0)
    }

    fn toast(&mut self, s: impl Into<String>) {
        self.toast = Some((s.into(), Instant::now()));
    }

    fn save_cfg(&mut self) {
        self.cfg.audio = self.engine.audio_settings.lock().clone();
        self.cfg.dsp = self.engine.dsp_settings.lock().clone();
        let sh = self.shared.lock();
        self.cfg.views = sh.settings.clone();
        self.cfg.notes = sh.notes.settings();
        drop(sh);
        self.cfg.save();
    }

    /// Drain engine queues (non-blocking) into GPU staging.
    fn drain(&mut self) {
        let Some(l) = self.engine.layout() else { return };
        let nel = l.num_electrodes();
        {
            let rx = self.engine.display_rx().cloned();
            let mut sh = self.shared.lock();
            if sh.layout.as_ref().map(layout_key) != Some(layout_key(l)) {
                sh.layout = Some(l.clone());
            }
            if sh.rx.as_ref().map(|r| rx.as_ref().is_some_and(|n| n.same_channel(r))) != Some(true) {
                sh.rx = rx;
            }
            sh.acquiring = self.engine.is_acquiring();
            sh.max_fps = self.cfg.display.max_fps;
            let src = self.engine.rec_status.source.lock().clone();
            if sh.rec_source != src {
                sh.rec_source = src;
            }
            sh.drain();
            sh.check_stalls();
        }
        // Spikes.
        if self.spike_slot.len() != nel {
            self.spike_slot = vec![0; nel];
            self.spike_times = vec![VecDeque::new(); nel];
            self.spike_reset = true;
        }
        let now = self.t0.elapsed().as_secs_f64();
        if let Some(rx) = self.engine.spike_rx() {
            for sp in rx.try_iter() {
                if sp.channel >= nel {
                    continue;
                }
                let slot = self.spike_slot[sp.channel];
                self.spike_slot[sp.channel] = (slot + 1) % SLOTS;
                self.spike_times[sp.channel].push_back(now);
                self.spike_new.push((sp.channel, slot, now as f32, Arc::new(sp)));
            }
            // Drained only while the main window draws: when it's hidden for hours, keep just
            // what can still be shown, the newest spike in each tile slot (nel × SLOTS).
            if self.spike_new.len() > 2 * nel * SLOTS {
                compact_newest_per_slot(&mut self.spike_new);
            }
        }
        for q in &mut self.spike_times {
            while q.front().is_some_and(|&t| now - t > 2.0) {
                q.pop_front();
            }
        }
    }

    fn channel_colors(&self, l: &StreamLayout, per_row: usize) -> Vec<[f32; 4]> {
        channel_colors(l, &self.engine.audio_settings.lock().channels, per_row)
    }

    // ------------------------------------------------------------------ UI parts
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let acquiring = self.engine.is_acquiring();
            let recording = self.engine.is_recording();
            let busy = self.engine.is_busy();
            if let Some(b) = self.engine.busy_label() {
                ui.spinner();
                ui.label(format!("{b}…"));
            } else if let Some(i) = self.engine.info() {
                ui.label(RichText::new(&i.kind).strong());
            } else {
                ui.label(RichText::new("No board").color(Color32::LIGHT_RED));
            }
            // One switch to the other board; greyed out while acquiring or busy.
            let sim = self.engine.is_simulated();
            let can_switch = !acquiring && !busy;
            if sim {
                if ui.add_enabled(can_switch, egui::Button::new("Switch to: Open board")).on_hover_text("Open the ONI acquisition board (FT600)").clicked() {
                    self.cfg.open_hardware_on_start = true;
                    self.engine.open_board(BoardChoice::Hardware, self.edit_board.clone());
                    self.reset_views();
                }
            } else if ui.add_enabled(can_switch, egui::Button::new("Switch to: Simulator")).on_hover_text("Real-time simulated headstages").clicked() {
                self.engine.open_board(BoardChoice::Simulated(self.cfg.sim_headstages.clone()), self.edit_board.clone());
                self.reset_views();
            }
            ui.separator();
            let can_start = self.engine.has_board() && !busy;
            if !acquiring {
                if ui.add_enabled(can_start, egui::Button::new(RichText::new("Acquire").strong()).min_size(egui::vec2(100.0, 0.0))).on_hover_text("Ctrl+Space").clicked() || (can_start && ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Space))) {
                    self.reset_views();
                    if let Err(e) = self.engine.start_acquisition() {
                        self.engine.last_error = Some(format!("{e:#}"));
                    }
                }
            } else if ui.add(egui::Button::new(RichText::new("Stop").strong()).min_size(egui::vec2(100.0, 0.0))).clicked() || ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::Space)) {
                if recording {
                    self.confirm_stop = true;
                } else {
                    self.engine.stop_acquisition();
                }
            }
            let rec_btn = if recording { egui::Button::new(RichText::new("Recording").color(Color32::WHITE).strong()).fill(Color32::from_rgb(190, 30, 30)) } else { egui::Button::new(RichText::new("Record").color(Color32::from_rgb(230, 80, 80))) };
            // A recording can always be stopped, even if acquisition ended under it.
            let rec_ok = acquiring || recording;
            if ui.add_enabled(rec_ok, rec_btn).on_hover_text("Ctrl+R").clicked() || (rec_ok && ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::R))) {
                if recording {
                    self.engine.stop_recording();
                } else {
                    *self.engine.rec_status.root.lock() = self.cfg.record_root.clone();
                    self.engine.start_recording();
                }
            }
            if let Some(t) = self.engine.acquisition_time() {
                let s = t.as_secs();
                ui.monospace(format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60));
            }
            if recording {
                let mb = self.engine.rec_status.bytes.load(Ordering::Relaxed) as f64 / 1e6;
                ui.monospace(format!("{mb:.0} MB"));
            }
            ui.separator();
            self.health_chips(ui);
            // Acquisition board only, at the right-hand end: Rescan (or Retry if it failed to open).
            if !sim {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.engine.has_board() || busy {
                        if ui.add_enabled(can_switch, egui::Button::new("Rescan")).on_hover_text("Detect headstages and tune cable delays").clicked() {
                            self.engine.rescan();
                            self.reset_views();
                        }
                    } else if ui.add_enabled(can_switch, egui::Button::new("Retry")).on_hover_text("Try opening the acquisition board again").clicked() {
                        self.engine.open_board(BoardChoice::Hardware, self.edit_board.clone());
                        self.reset_views();
                    }
                });
            }
        });
    }

    fn health_chips(&self, ui: &mut egui::Ui) {
        let h = self.engine.health.snapshot();
        let p = &self.engine.pipeline;
        let chip = |ui: &mut egui::Ui, ok: bool, text: String, tip: &str| {
            let c = if ok { Color32::from_rgb(90, 170, 90) } else { Color32::from_rgb(220, 70, 60) };
            ui.label(RichText::new(text).color(c).monospace()).on_hover_text(tip);
        };
        if self.engine.is_acquiring() {
            chip(ui, h.dropped_samples == 0 && h.bad_frames == 0, format!("lost {} / bad {}", h.dropped_samples, h.bad_frames), "Samples missing according to the hardware sample counter / malformed frames. Must be 0.");
            if h.hw_buffer_pct > 0.0 || h.frames > 0 {
                chip(ui, h.hw_buffer_pct < 50.0, format!("hw buf {:.1}%", h.hw_buffer_pct), "Acquisition board DRAM FIFO usage. Rising = host not keeping up.");
            }
            chip(ui, self.engine.rt_priority(), if self.engine.rt_priority() { "RT".into() } else { "no RT".into() }, "Acquisition thread real-time scheduling (SCHED_FIFO). See README to enable.");
            let a = &self.engine.audio_stats;
            if a.running.load(Ordering::Relaxed) {
                // Red at 5 or more underruns within about the last minute (this minute so far, or
                // the whole previous one); the rig normally sees about one per 30 s.
                let total = a.underruns.load(Ordering::Relaxed);
                let mut w = self.underrun_window.get();
                if total < w.1 {
                    w = (Instant::now(), total, 0); // counter reset (audio restarted)
                }
                if w.0.elapsed() >= Duration::from_secs(60) {
                    w = (Instant::now(), total, total - w.1);
                }
                self.underrun_window.set(w);
                let recent = (total - w.1).max(w.2);
                chip(ui, recent < 5, format!("audio {:.1} ms", a.latency_us.load(Ordering::Relaxed) as f32 / 1000.0), &format!("Audio latency after acquisition (ring + device buffer + filters). Red at 5 or more underruns within the last minute or so ({recent} recently, {total} since start)."));
            }
            let (lat, lat_max) = { let sh = self.shared.lock(); (sh.disp_latency, sh.disp_latency_max) };
            chip(ui, lat_max < 25.0, format!("display {lat:.1} ms"), "Sample arrival → GPU submit (excludes compositor scan-out).");
            let (rv_bytes, rv_budget) = { let st = self.engine.pyramids.lock(); (st.bytes(), st.budget) };
            if rv_bytes > 0 {
                chip(ui, rv_bytes <= rv_budget, format!("review {:.2} / {:.1} GB", rv_bytes as f64 / 1e9, rv_budget as f64 / 1e9), "RAM held by the scroll-back overview of this recording (review_ram_gb in the config). Over it, the oldest fine detail is released; short spans there are read from the file instead.");
            }
            let dd = p.display_dropped.load(Ordering::Relaxed) + p.dsp_dropped.load(Ordering::Relaxed) + p.audio_dropped.load(Ordering::Relaxed);
            if dd > 0 {
                chip(ui, false, format!("viz drops {dd}"), "Blocks skipped by display/DSP/audio consumers (never affects recording).");
            }
        }
        if let Some(e) = self.engine.rec_status.error.lock().clone() {
            chip(ui, false, "REC ERROR".into(), &e);
        } else {
            // The message clears when the next recording starts; the count stays.
            let n = self.engine.rec_status.errors.load(Ordering::Relaxed);
            if n > 0 {
                chip(ui, false, format!("rec errors {n}"), "Earlier recordings in this session reported errors (disk full, write failures). Their files hold the data up to the error; the details are in the log.");
            }
        }
        let fps = if self.frame_times.is_empty() { 0.0 } else { self.frame_times.len() as f32 / self.frame_times.iter().sum::<f32>() };
        ui.label(RichText::new(format!("{fps:.0} fps")).weak().monospace()).on_hover_text(&self.gpu_name);
    }

    fn reset_views(&mut self) {
        self.shared.lock().reset_all();
        self.spike_reset = true;
        self.colors_dirty = true;
        self.spike_new.clear();
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            self.board_section(ui);
            self.outputs_section(ui);
            self.trace_windows_section(ui);
            self.spike_section(ui);
            self.audio_section(ui);
            self.record_section(ui);
            self.notes_section(ui);
        });
    }

    /// Notes: how to type them, drafts written ahead, presets on 1–9, the latest notes.
    fn notes_section(&mut self, ui: &mut egui::Ui) {
        let mut sh = self.shared.lock();
        egui::CollapsingHeader::new(RichText::new("Notes").strong()).id_salt("notes_section").default_open(true).show(ui, |ui| {
            ui.small("In any kalila window: / starts a note at that moment; Enter or \\ saves it, Esc cancels. ` stamps the next draft, 1–9 a preset. Saved in the recording as Open Ephys messages.");
            ui.label("Drafts (` stamps the top one)");
            let id = egui::Id::unique("note-draft-input");
            let mut draft = ui.data_mut(|d| d.get_temp::<String>(id).unwrap_or_default());
            let r = ui.add(egui::TextEdit::singleline(&mut draft).hint_text("Write ahead, Enter to queue").desired_width(f32::INFINITY));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !draft.trim().is_empty() {
                sh.notes.drafts.push_back(draft.trim().to_string());
                sh.notes.changed = true;
                draft.clear();
                r.request_focus();
            }
            ui.data_mut(|d| d.insert_temp(id, draft));
            let mut remove = None;
            for (i, d) in sh.notes.drafts.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.small_button("×").on_hover_text("Remove this draft").clicked() {
                        remove = Some(i);
                    }
                    let t = if i == 0 { RichText::new(format!("` {d}")).strong() } else { RichText::new(format!("  {d}")) };
                    ui.label(t);
                });
            }
            if let Some(i) = remove {
                sh.notes.drafts.remove(i);
                sh.notes.changed = true;
            }
            egui::CollapsingHeader::new("Presets (keys 1–9)").id_salt("note-presets").show(ui, |ui| {
                for k in 0..9 {
                    ui.horizontal(|ui| {
                        ui.monospace(format!("{}", k + 1));
                        if ui.add(egui::TextEdit::singleline(&mut sh.notes.presets[k]).desired_width(f32::INFINITY)).changed() {
                            sh.notes.changed = true;
                        }
                    });
                }
            });
            if !sh.notes.notes.is_empty() {
                ui.label("Latest");
                let latest: Vec<(String, String)> = sh.notes.notes.iter().rev().take(8).map(|n| (sh.note_time(n.sample), n.text.clone())).collect();
                for (t, text) in latest {
                    ui.horizontal_wrapped(|ui| {
                        ui.monospace(t);
                        ui.label(text);
                    });
                }
            }
        });
    }

    fn board_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("Board").strong()).default_open(true).show(ui, |ui| {
            // Settings can be edited while acquiring: Apply then stops, applies and restarts.
            // Not while recording (that would split the recording) or while the board is busy.
            let acquiring = self.engine.is_acquiring();
            let recording = self.engine.is_recording();
            let editable = self.engine.has_board() && !self.engine.is_busy() && !recording;
            let leds_before = self.edit_board.leds;
            if let Some(l) = self.layout() {
                for h in &l.headstages {
                    ui.label(format!("{}  {}  {} ch{}", h.name, h.chip, h.num_channels, if h.has_bno { "  + IMU" } else { "" }));
                }
                if l.headstages.is_empty() {
                    ui.colored_label(Color32::LIGHT_RED, "No headstages detected");
                }
            }
            if let Some(i) = self.engine.info() {
                egui::CollapsingHeader::new("Details").show(ui, |ui| {
                    for (k, v) in &i.description {
                        ui.label(format!("{k}: {v}"));
                    }
                });
            }
            // RHD2132 headstages that can run as 16-channel boards (chip channels 8–23).
            let rhd2132: Vec<(usize, String)> =
                self.layout().map(|l| l.headstages.iter().filter(|h| h.chip == "RHD2132").map(|h| (h.slot, h.name.clone())).collect()).unwrap_or_default();
            ui.add_enabled_ui(editable, |ui| {
                let b = &mut self.edit_board;
                if !rhd2132.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label("16-ch RHD2132");
                        for (slot, name) in &rhd2132 {
                            ui.checkbox(&mut b.rhd2132_16ch[*slot], name.as_str())
                                .on_hover_text("Headstage wires only 16 of the chip's 32 inputs: record chip channels 8–23 (stock \"half channels\")");
                        }
                    });
                }
                let rates = self.engine.available_sample_rates();
                egui::ComboBox::from_label("Sample rate").selected_text(format!("{} Hz", b.sample_rate)).show_ui(ui, |ui| {
                    for r in rates {
                        ui.selectable_value(&mut b.sample_rate, r, format!("{r} Hz"));
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Analog band");
                    ui.add(egui::DragValue::new(&mut b.lower_bandwidth).range(0.1..=500.0).speed(0.1).suffix(" Hz"));
                    ui.label("–");
                    ui.add(egui::DragValue::new(&mut b.upper_bandwidth).range(100.0..=20000.0).speed(10.0).suffix(" Hz"));
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut b.dsp_enabled, "DSP offset removal");
                    ui.add_enabled(b.dsp_enabled, egui::DragValue::new(&mut b.dsp_cutoff).range(0.01..=1000.0).speed(0.05).suffix(" Hz"));
                });
                ui.horizontal(|ui| {
                    ui.checkbox(&mut b.acquire_aux, "AUX");
                    ui.checkbox(&mut b.acquire_adc, "ADC");
                    ui.checkbox(&mut b.fast_settle, "Fast settle");
                    ui.checkbox(&mut b.leds, "LEDs").on_hover_text("Applies immediately while acquiring");
                });
                ui.horizontal(|ui| {
                    ui.label("Cable delay +1");
                    for (p, n) in ["A", "B", "C", "D"].iter().enumerate() {
                        let mut on = b.cable_delay_adjust[p] > 0;
                        if ui.checkbox(&mut on, *n).changed() {
                            b.cable_delay_adjust[p] = on as i32;
                        }
                    }
                });
                egui::CollapsingHeader::new("Latency / transport").show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("USB block");
                        egui::ComboBox::from_id_salt("usbblk").selected_text(format!("{} KiB", b.usb_block_bytes / 1024)).show_ui(ui, |ui| {
                            for k in [4u32, 8, 12, 16, 24, 32] {
                                ui.selectable_value(&mut b.usb_block_bytes, k * 1024, format!("{k} KiB"));
                            }
                        });
                    });
                    ui.add(egui::Slider::new(&mut b.block_samples, 8..=256).text("samples / block"));
                    ui.small("Smaller USB blocks lower latency at the cost of more USB transactions and CPU. 8 KiB is the default (stock uses 24 KiB).");
                });
            });
            // LEDs are a live setting: no restart needed.
            if acquiring && self.edit_board.leds != leds_before {
                let on = self.edit_board.leds;
                self.engine.send_live(LiveCommand::SetLeds(on));
                self.cfg.board.leds = on;
                self.cfg.save();
            }
            let changed = self.engine.board_settings().is_some_and(|s| s != self.edit_board);
            ui.horizontal(|ui| {
                let label = if acquiring { "Apply (restarts acquisition)" } else { "Apply" };
                if ui.add_enabled(editable && changed, egui::Button::new(label)).clicked() {
                    if acquiring {
                        // Restarted when the board comes back from the apply (see `ui`).
                        self.engine.stop_acquisition();
                        self.autostart = true;
                    }
                    self.cfg.board = self.edit_board.clone();
                    self.engine.apply_board_settings(self.edit_board.clone());
                    self.cfg.save();
                    self.reset_views();
                }
                if changed && ui.add_enabled(editable, egui::Button::new("Revert")).clicked() {
                    self.edit_board = self.engine.board_settings().unwrap();
                }
                if recording {
                    ui.weak("Stop recording to change board settings");
                }
            });
            if recording {
                // The settings block is locked while recording; keep the LED switch available.
                let mut leds = self.edit_board.leds;
                if ui.checkbox(&mut leds, "LEDs (live)").changed() {
                    self.edit_board.leds = leds;
                    self.cfg.board.leds = leds;
                    self.engine.send_live(LiveCommand::SetLeds(leds));
                }
            }
        });
    }

    /// DAC/TTL outputs. All live: sent while acquiring, and again at every acquisition start
    /// (`sync_outputs`). Writes only the board's DAC-select/threshold, HPF, DAC-control
    /// (noise slicer), MODE (TTL-out mode) registers and the TTL-out device.
    fn outputs_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("Outputs (DAC / TTL)").strong()).default_open(false).show(ui, |ui| {
            let acquiring = self.engine.is_acquiring();
            let names: Vec<String> = self.layout().map(|l| l.channels.iter().filter(|c| c.kind == ChannelKind::Electrode).map(|c| c.name.clone()).collect()).unwrap_or_default();
            let audio_first = self.engine.audio_settings.lock().channels.first().copied();
            let o = &mut self.cfg.outputs;
            let old = o.clone();
            ui.small("Applied live while acquiring and re-sent at every start.");
            ui.checkbox(&mut o.dac1_follows_audio, "DAC 1 follows audio monitor").on_hover_text("DAC 1 (board audio L) plays the first channel on the audio monitor");
            egui::Grid::new("dacs").num_columns(3).spacing([6.0, 3.0]).show(ui, |ui| {
                for d in 0..8 {
                    ui.label(format!("DAC {}{}", d + 1, match d {
                        0 => " L",
                        1 => " R",
                        _ => "",
                    }))
                    .on_hover_text(if d < 2 { "Also the board's audio output (noise slicer applies)" } else { "Analog output" });
                    let follow = d == 0 && o.dac1_follows_audio;
                    let cur = if follow { audio_first } else { o.dacs[d].electrode };
                    let text = match cur {
                        Some(e) => names.get(e).cloned().unwrap_or_else(|| format!("#{}", e + 1)),
                        None => "off".into(),
                    };
                    ui.add_enabled_ui(!follow, |ui| {
                        egui::ComboBox::from_id_salt(("dac", d)).width(64.0).selected_text(if follow { format!("♪ {text}") } else { text }).show_ui(ui, |ui| {
                            ui.selectable_value(&mut o.dacs[d].electrode, None, "off");
                            for (i, n) in names.iter().enumerate() {
                                ui.selectable_value(&mut o.dacs[d].electrode, Some(i), n);
                            }
                        });
                    });
                    ui.add(egui::DragValue::new(&mut o.dacs[d].threshold_uv).range(-5000.0..=5000.0).speed(1.0).suffix(" µV"))
                        .on_hover_text("Comparator threshold: ≥ 0 fires at or above it, < 0 at or below it. Drives TTL out n in threshold mode.");
                    ui.end_row();
                }
            });
            ui.checkbox(&mut o.ttl_threshold_mode, "Threshold → TTL out").on_hover_text("TTL out n goes high while DAC n crosses its threshold (stock \"DAC TTL\")");
            ui.horizontal(|ui| {
                ui.checkbox(&mut o.dac_hpf_enabled, "DAC high-pass");
                ui.add_enabled(o.dac_hpf_enabled, egui::DragValue::new(&mut o.dac_hpf_hz).range(1.0..=5000.0).speed(5.0).suffix(" Hz"));
            });
            ui.add(egui::Slider::new(&mut o.noise_slicer, 0..=127).text("noise slicer (DAC L/R)"));
            ui.horizontal(|ui| {
                ui.label("TTL out");
                let manual = acquiring && !o.ttl_threshold_mode;
                for i in 0..8 {
                    let on = self.ttl_out_word & (1 << i) != 0;
                    let b = egui::Button::new(RichText::new(format!("{}", i + 1)).monospace()).selected(on).min_size(egui::vec2(18.0, 0.0));
                    if ui.add_enabled(manual, b).on_hover_text("Toggle TTL output line").clicked() {
                        self.ttl_out_word ^= 1 << i;
                        self.engine.send_live(LiveCommand::SetTtlOut(self.ttl_out_word));
                    }
                }
            });
            if o.ttl_threshold_mode {
                ui.weak("TTL out follows the DAC comparators (manual off)");
            } else if !acquiring {
                ui.weak("Manual TTL out needs acquisition running");
            }
            if *o != old {
                self.dirty_cfg = Instant::now();
            }
        });
    }

    /// When the electrode layout changes (16-ch toggle, rescan, another board), move every
    /// setting stored as a global electrode index so it keeps pointing at the same electrode:
    /// DAC routes (and so threshold → TTL out), the spike/audio CAR sets, each trace
    /// window's CAR sets and the audio channels. Electrodes whose headstage is gone are
    /// dropped from them, with a message.
    fn remap_electrodes(&mut self) {
        let Some(keys) = self.engine.layout().map(electrode_keys) else { return };
        if keys.is_empty() || keys == self.cfg.electrode_keys {
            return;
        }
        self.dirty_cfg = Instant::now();
        if self.cfg.electrode_keys.is_empty() {
            // A config from before the keys were saved: its indices mean this layout.
            self.cfg.electrode_keys = keys;
            return;
        }
        let mut m = ElectrodeMap::new(&self.cfg.electrode_keys, &keys);
        for d in &mut self.cfg.outputs.dacs {
            m.opt(&mut d.electrode);
        }
        m.set(&mut self.cfg.display.car.reference);
        m.set(&mut self.cfg.display.car.subtract);
        for v in &mut self.shared.lock().settings {
            m.set(&mut v.car.reference);
            m.set(&mut v.car.subtract);
        }
        {
            let mut a = self.engine.audio_settings.lock();
            m.set(&mut a.channels);
            self.audio_text = channels_to_text(&a.channels);
        }
        self.cfg.electrode_keys = keys;
        if m.changed() {
            self.colors_dirty = true;
            let mut msg = format!("Channel layout changed: DAC routes, CAR sets and audio channels moved to follow their electrodes ({} moved", m.moved);
            if m.dropped > 0 {
                msg += &format!(", {} dropped: their headstage is gone", m.dropped);
            }
            msg += ")";
            log::info!("{msg}");
            self.toast(msg);
        }
    }

    /// Send DAC routes and board-level output settings that differ from what the board last
    /// got; `force` re-sends everything (acquisition start).
    fn sync_outputs(&mut self, force: bool) {
        if !self.engine.is_acquiring() {
            return;
        }
        let nel = self.electrode_rows();
        let o: OutputSettings = self.cfg.outputs.clone();
        let audio_first = self.engine.audio_settings.lock().channels.first().copied();
        for d in 0..8 {
            let e = if d == 0 && o.dac1_follows_audio { audio_first } else { o.dacs[d].electrode };
            let want = (e.filter(|&e| e < nel), o.dacs[d].threshold_uv);
            if force || self.dac_sent[d] != Some(want) {
                self.engine.send_live(LiveCommand::DacRoute { dac: d, electrode: want.0, threshold_uv: want.1 });
                self.dac_sent[d] = Some(want);
            }
        }
        let want = (o.ttl_threshold_mode, o.dac_hpf_enabled, o.dac_hpf_hz.to_bits(), o.noise_slicer);
        if force || self.outputs_sent != Some(want) {
            self.engine.send_live(LiveCommand::SetTtlMode(o.ttl_threshold_mode));
            self.engine.send_live(LiveCommand::SetDacHpf { enabled: o.dac_hpf_enabled, hz: o.dac_hpf_hz });
            self.engine.send_live(LiveCommand::SetNoiseSlicer(o.noise_slicer));
            self.outputs_sent = Some(want);
            // Mirror into the board settings so they never show up as "changed" (Apply).
            for b in [&mut self.edit_board, &mut self.cfg.board] {
                b.ttl_output_mode = o.ttl_threshold_mode;
                b.dac_hpf_enabled = o.dac_hpf_enabled;
                b.dac_hpf_hz = o.dac_hpf_hz;
                b.noise_slicer = o.noise_slicer;
            }
        }
    }

    fn spike_section(&mut self, ui: &mut egui::Ui) {
        let car = self.cfg.display.car.spikes;
        egui::CollapsingHeader::new(section_title(ui, "Spike detection", car)).id_salt("spike_section").default_open(true).show(ui, |ui| {
            self.car_row(ui, false);
            let mut s = self.engine.dsp_settings.lock().clone();
            let old = s.clone();
            ui.checkbox(&mut s.detect, "Detect");
            let mut sigma_mode = matches!(s.threshold, ThresholdMode::Sigma(_));
            ui.horizontal(|ui| {
                ui.radio_value(&mut sigma_mode, true, "× σ (MAD)");
                ui.radio_value(&mut sigma_mode, false, "fixed µV");
            });
            s.threshold = match (sigma_mode, s.threshold) {
                (true, ThresholdMode::Sigma(mut k)) => {
                    ui.add(egui::Slider::new(&mut k, 2.0..=10.0).text("threshold σ"));
                    ThresholdMode::Sigma(k)
                }
                (true, ThresholdMode::Fixed(_)) => ThresholdMode::Sigma(4.5),
                (false, ThresholdMode::Fixed(mut t)) => {
                    ui.add(egui::Slider::new(&mut t, 10.0..=500.0).text("threshold µV"));
                    ThresholdMode::Fixed(t)
                }
                (false, ThresholdMode::Sigma(_)) => ThresholdMode::Fixed(60.0),
            };
            let d = &mut self.cfg.display;
            ui.add(egui::Slider::new(&mut d.spike_uv_range, 25.0..=1000.0).logarithmic(true).text("± µV"));
            ui.add(egui::Slider::new(&mut d.spike_tau_s, 0.2..=20.0).logarithmic(true).text("persistence (s)"));
            ui.add(egui::Slider::new(&mut d.spike_grid_cols, 1..=16).text("columns"));
            if s != old {
                *self.engine.dsp_settings.lock() = s;
                self.dirty_cfg = Instant::now();
            }
        });
    }

    /// CAR switch for spike detection (`audio` false) or audio (`audio` true); both use the
    /// global reference/subtract sets, edited in the CAR window.
    fn car_row(&mut self, ui: &mut egui::Ui, audio: bool) {
        let c = &mut self.cfg.display.car;
        ui.horizontal(|ui| {
            let on = if audio { &mut c.audio } else { &mut c.spikes };
            if ui.checkbox(on, RichText::new("CAR").color(if *on { CAR_COLOR } else { Color32::GRAY })).on_hover_text("Common average reference. Recordings keep the original signals.").changed() {
                self.dirty_cfg = Instant::now();
            }
            ui.weak(format!("ref {} · sub {}", c.reference.len(), c.subtract.len()));
            if ui.small_button("Channels…").on_hover_text("Shared by spike detection and audio").clicked() {
                self.car_window = Some(CarTarget::Global);
            }
        });
    }

    fn audio_section(&mut self, ui: &mut egui::Ui) {
        let car = self.cfg.display.car.audio;
        egui::CollapsingHeader::new(section_title(ui, "Audio monitor", car)).id_salt("audio_section").default_open(true).show(ui, |ui| {
            self.car_row(ui, true);
            let mut a = self.engine.audio_settings.lock().clone();
            let old = a.clone();
            ui.horizontal(|ui| {
                ui.checkbox(&mut a.enabled, "On");
                ui.checkbox(&mut a.mute, "Mute");
                ui.toggle_value(&mut a.reference, "A/B: reference").on_hover_text("Bypass expander and presence EQ (band-limit + gain + safety limiter only)");
            });
            ui.horizontal(|ui| {
                ui.label("Channels");
                let r = ui.add(egui::TextEdit::singleline(&mut self.audio_text).desired_width(120.0).hint_text("e.g. 1, 5, 9"));
                if r.lost_focus() {
                    a.channels = text_to_channels(&self.audio_text, self.electrode_rows());
                    self.audio_text = channels_to_text(&a.channels);
                }
            });
            ui.add(egui::Slider::new(&mut a.volume_db, -40.0..=12.0).text("volume dB"));
            ui.horizontal(|ui| {
                ui.radio_value(&mut a.gain_mode, GainMode::NoiseReferenced, "Noise-referenced");
                ui.radio_value(&mut a.gain_mode, GainMode::Absolute, "Absolute µV");
            });
            if a.gain_mode == GainMode::Absolute {
                ui.add(egui::Slider::new(&mut a.absolute_fs_uv, 50.0..=5000.0).logarithmic(true).text("µV full scale"));
            } else {
                ui.horizontal(|ui| {
                    let s = self.engine.audio_stats.held_sigma_cuv.load(Ordering::Relaxed);
                    if s == 0 {
                        ui.weak("calibrating noise…");
                    } else {
                        ui.label(format!("held σ = {:.1} µV", s as f32 / 100.0));
                    }
                    if ui.small_button("Recalibrate").clicked() {
                        self.engine.audio_stats.recalibrate.store(true, Ordering::Relaxed);
                    }
                });
            }
            ui.add_enabled(!a.reference, egui::Slider::new(&mut a.expansion, 0.0..=1.0).text("noise expansion")).on_hover_text("0 = off. Lowers the floor between spikes by up to 24 dB; keep low to preserve multi-unit texture.");
            ui.add_enabled(!a.reference && a.expansion > 0.0, egui::Slider::new(&mut a.gate_sigma, 1.5..=8.0).text("expander knee σ"));
            ui.add_enabled(!a.reference, egui::Slider::new(&mut a.presence_db, 0.0..=8.0).text("presence dB (2.8 kHz)"));
            ui.horizontal(|ui| {
                ui.label("Band");
                ui.add(egui::DragValue::new(&mut a.band.low_hz).range(50.0..=1500.0).speed(5.0).suffix(" Hz"));
                ui.label("–");
                ui.add(egui::DragValue::new(&mut a.band.high_hz).range(2000.0..=12000.0).speed(50.0).suffix(" Hz"));
            });
            let st = &self.engine.audio_stats;
            if st.running.load(Ordering::Relaxed) {
                ui.small(format!(
                    "{} Hz · latency {:.1} ms · underruns {} · drift {:+} ppm",
                    st.device_rate.load(Ordering::Relaxed),
                    st.latency_us.load(Ordering::Relaxed) as f32 / 1000.0,
                    st.underruns.load(Ordering::Relaxed),
                    st.ratio_ppm.load(Ordering::Relaxed) as i32
                ));
            }
            if a != old {
                if a.channels != old.channels {
                    self.colors_dirty = true;
                }
                *self.engine.audio_settings.lock() = a;
                self.dirty_cfg = Instant::now();
            }
        });
    }

    fn record_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("Recording").strong()).default_open(true).show(ui, |ui| {
            let mut s = self.cfg.record_root.display().to_string();
            ui.label("Root directory");
            if ui.add_enabled(!self.engine.is_recording(), egui::TextEdit::singleline(&mut s)).changed() {
                self.cfg.record_root = s.into();
                self.dirty_cfg = Instant::now();
            }
            if ui.add_enabled(!self.engine.is_recording(), egui::Button::new("New session directory")).clicked() {
                self.engine.rec_status.new_session.store(true, Ordering::SeqCst);
                self.toast("Next recording starts a new date-time directory");
            }
            ui.small("A new date-time directory also starts on the first recording of each day.")
                .on_hover_text("Recordings within one session directory become experiment1, experiment2, … (one per acquisition run) and recording1, recording2, … (one per Record press).");
            ui.small("Open Ephys Binary format (structure.oebin), readable by open-ephys-python-tools, SpikeInterface and Kilosort.");
            if let Some(d) = self.engine.rec_status.current_dir.lock().as_ref() {
                ui.small(format!("Last: {}", d.display()));
            }
        });
    }

    // ------------------------------------------------------------------ views
    /// Push the CAR sets to spike detection and the audio monitor when switched on there.
    fn sync_car(&mut self) {
        let c = &self.cfg.display.car;
        let want = |on: bool| on.then(|| Arc::new(engine::dsp::Car { reference: c.reference.clone(), subtract: c.subtract.clone() }));
        let (spk, aud) = (want(c.spikes), want(c.audio));
        {
            let mut s = self.engine.dsp_settings.lock();
            if s.car != spk {
                s.car = spk;
            }
        }
        let mut a = self.engine.audio_settings.lock();
        if a.car != aud {
            a.car = aud;
        }
    }

    /// Click on a channel (trace label or spike tile): monitor just these channels. With
    /// Ctrl (or Shift) held, toggle them in/out of the monitored set instead (up to 8).
    fn monitor_click(&mut self, ui: &egui::Ui, chs: Vec<usize>) {
        let add = ui.input(|i| i.modifiers.command || i.modifiers.shift);
        let mut a = self.engine.audio_settings.lock();
        apply_monitor_click(&mut a, chs, add);
        self.audio_text = channels_to_text(&a.channels);
        self.colors_dirty = true;
        self.engine.audio_stats.recalibrate.store(true, Ordering::Relaxed);
    }

    fn spike_view(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let Some(l) = self.layout() else { return };
        let nel = l.num_electrodes();
        if nel == 0 {
            return;
        }
        let ppp = ui.ctx().pixels_per_point();
        let d = self.cfg.display.clone();
        let gc = d.spike_grid_cols.max(1) as usize;
        let cell_w = rect.width() / gc as f32;
        let cell_h = (cell_w * 0.75).max(40.0);
        let rows_fit = ((rect.height() / cell_h).floor() as usize).max(1);
        // Mouse wheel scrolls the grid by rows of tiles.
        let total_rows = nel.div_ceil(gc);
        let max_first = total_rows.saturating_sub(rows_fit);
        let hovered = ui.rect_contains_pointer(rect);
        let scroll = if hovered { ui.input(|i| i.smooth_scroll_delta.y) } else { 0.0 };
        let mut first_row = self.cfg.display.spike_first_row.min(max_first);
        let step = wheel_rows(&mut self.spike_scroll, scroll);
        if step != 0 {
            first_row = (first_row as isize - step).clamp(0, max_first as isize) as usize;
            self.dirty_cfg = Instant::now();
        }
        self.cfg.display.spike_first_row = first_row;
        let first = first_row * gc;
        let ncells = (gc * rows_fit).min(nel.saturating_sub(first));
        let now = self.t0.elapsed().as_secs_f32();
        let frame = SpikeFrame {
            nch: nel,
            reset: std::mem::take(&mut self.spike_reset),
            new: std::mem::take(&mut self.spike_new),
            vp_px: [rect.width() * ppp, rect.height() * ppp],
            cell_px: [cell_w * ppp, cell_h * ppp],
            grid_cols: gc as u32,
            first_ch: first as u32,
            ncells: ncells as u32,
            uv_range: d.spike_uv_range,
            now,
            tau: d.spike_tau_s,
            thickness: ppp.max(1.0),
            colors: self.spike_colors.take().map(|c| Arc::new(c.iter().take(nel).copied().map(|mut x| {
                x[3] = 0.55;
                x
            }).collect())),
        };
        ui.painter().rect_filled(rect, 0.0, Color32::from_rgb(10, 10, 12));
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(rect, SpikeCallback(frame)));
        let resp = ui.interact(rect, ui.scope_id().with("spike_tiles"), egui::Sense::click());
        let click = resp.clicked().then(|| resp.interact_pointer_pos()).flatten();
        let mut clicked_ch = None;
        let mon = self.engine.audio_settings.lock().channels.clone();
        let p = ui.painter_at(rect);
        let cs = self.engine.channel_stats.lock().clone();
        for i in 0..ncells {
            let ch = first + i;
            let o = rect.min + egui::vec2((i % gc) as f32 * cell_w, (i / gc) as f32 * cell_h);
            let cell = egui::Rect::from_min_size(o, egui::vec2(cell_w, cell_h));
            if click.is_some_and(|pos| cell.contains(pos)) {
                clicked_ch = Some(ch);
            }
            let mc = monitor_color32(&mon, ch);
            let monitored = mc.is_some();
            let (border, label) = match mc {
                Some(c) => (Stroke::new(1.5, c), c),
                None => (Stroke::new(1.0, Color32::from_gray(35)), Color32::from_gray(160)),
            };
            p.rect_stroke(cell.shrink(1.0), 2.0, border, egui::StrokeKind::Inside);
            let rate = self.spike_times.get(ch).map(|q| q.len() as f32 / 2.0).unwrap_or(0.0);
            p.with_clip_rect(cell.shrink(1.0)).text(o + egui::vec2(5.0, 3.0), egui::Align2::LEFT_TOP, format!("{}{}  {:.1} Hz", l.channels[ch].name, if monitored { " ♪" } else { "" }, rate), egui::FontId::monospace(10.5), label);
            if let Some(&thr) = cs.threshold_uv.get(ch) {
                let k = (cell_h * 0.5 - 4.0) / d.spike_uv_range;
                let y = o.y + cell_h * 0.5 - thr * k;
                if y > o.y && y < o.y + cell_h {
                    p.line_segment([egui::pos2(o.x + 4.0, y), egui::pos2(o.x + cell_w - 4.0, y)], Stroke::new(1.0, Color32::from_rgba_unmultiplied(220, 80, 80, 90)));
                }
                if let Some(s) = cs.sigma_uv.get(ch) {
                    p.text(o + egui::vec2(cell_w - 5.0, cell_h - 3.0), egui::Align2::RIGHT_BOTTOM, format!("σ {s:.1}"), egui::FontId::monospace(10.0), Color32::from_gray(110));
                }
            }
        }
        if let Some(ch) = clicked_ch {
            self.monitor_click(ui, vec![ch]);
        }
    }
}

/// Every note also goes to `$XDG_STATE_HOME/kalila/notes.log` (default `~/.local/state`) (tab-separated: local time, sample,
/// time, recording folder or "-", text), so notes typed while not recording are kept too.
fn append_notes_log(sample: i64, when: &str, dir: Option<&std::path::Path>, text: &str) {
    use std::io::Write;
    let state = std::env::var_os("XDG_STATE_HOME").map(std::path::PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state")));
    let Some(state) = state else { return };
    let d = state.join("kalila");
    let _ = std::fs::create_dir_all(&d);
    let line = format!("{}\t{sample}\t{when}\t{}\t{}\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"), dir.map_or("-".into(), |p| p.display().to_string()), text.replace(['\t', '\n'], " "));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(d.join("notes.log")) {
        let _ = f.write_all(line.as_bytes());
    }
}

impl eframe::App for App {
    /// Scripted test input, injected into one window's own input (never the system's).
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Some(s) = self.ui_script.as_mut() {
            s.hook(ctx, raw_input);
        }
    }

    /// Runs every pass, also while the main window is hidden (Wayland then withholds its
    /// frames and eframe runs only this): board, outputs and the display/spike queues.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let was_board = self.engine.has_board();
        self.engine.poll();
        if !was_board && self.engine.has_board() {
            self.edit_board = self.engine.board_settings().unwrap_or_default();
            self.reset_views();
            if std::mem::take(&mut self.autostart)
                && let Err(e) = self.engine.start_acquisition()
            {
                self.engine.last_error = Some(format!("{e:#}"));
            }
        }
        if !self.engine.is_busy() && self.engine.board_settings().is_some_and(|s| s != self.edit_board) && self.edit_board == self.cfg.board {
            // Board coerced some values (e.g. actual DSP cutoff): reflect them.
            if let Some(s) = self.engine.board_settings() {
                self.edit_board = s.clone();
                self.cfg.board = s;
            }
        }
        // Before anything sends electrode indices to the board or the engine.
        self.remap_electrodes();
        // Outputs: everything on each acquisition start (the board resets TTL out to 0),
        // then only changes (e.g. DAC 1 following a new audio channel).
        let acquiring = self.engine.is_acquiring();
        if acquiring && !self.was_acquiring {
            self.ttl_out_word = 0;
            self.sync_outputs(true);
        } else {
            self.sync_outputs(false);
        }
        self.was_acquiring = acquiring;
        self.sync_car();
        self.drain();
        {
            let mut sh = self.shared.lock();
            if std::mem::take(&mut sh.monitor_changed) {
                self.audio_text = channels_to_text(&self.engine.audio_settings.lock().channels);
                self.colors_dirty = true;
            }
            if std::mem::take(&mut sh.dirty) || std::mem::take(&mut sh.notes.changed) {
                self.dirty_cfg = Instant::now();
            }
            // Finished notes: into the recording (if recording) and always into the notes log.
            let done = std::mem::take(&mut sh.notes.out);
            if !done.is_empty() {
                let recording = self.engine.is_recording();
                let dir = self.engine.rec_status.current_dir.lock().clone().filter(|_| recording);
                for f in done {
                    let when = sh.note_time(f.note.sample);
                    log::info!("note at sample {} ({when}){}: {}", f.note.sample, if recording { "" } else { ", not recording" }, f.note.text);
                    append_notes_log(f.note.sample, &when, dir.as_deref(), &f.note.text);
                    if recording {
                        self.engine.record_message(f.note.sample, f.note.text, f.keys);
                    }
                }
            }
            let removed = std::mem::take(&mut sh.removed);
            if self.colors_dirty {
                sh.colors_changed();
            }
            drop(sh);
            if let Some(g) = self.render_state.renderer.write().callback_resources.get_mut::<TraceGpus>() {
                for id in removed {
                    g.remove(id);
                }
            }
        }
        if self.colors_dirty
            && let Some(l) = self.layout()
        {
            self.colors_dirty = false;
            self.spike_colors = Some(Arc::new(self.channel_colors(&l, 1)));
        }
        if self.dirty_cfg.elapsed() > Duration::from_secs(1) && self.dirty_cfg.elapsed() < Duration::from_secs(2) {
            self.save_cfg();
            self.dirty_cfg = Instant::now() - Duration::from_secs(10);
        }
        // Keep this pass running while acquiring, also when the window is hidden.
        let period = Duration::from_secs_f32(1.0 / self.cfg.display.max_fps.max(10.0));
        if self.engine.is_acquiring() || self.engine.is_busy() {
            ctx.request_repaint_after(period.saturating_sub(self.last_frame.elapsed()));
        } else {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Note keys first, so an open note gets every key of this frame.
        self.shared.lock().notes_frame(ui);
        let dt = self.last_frame.elapsed().as_secs_f32();
        self.last_frame = Instant::now();
        self.frame_times.push_back(dt);
        while self.frame_times.len() > 60 {
            self.frame_times.pop_front();
        }

        egui::Panel::top("top").show(ui, |ui| self.top_bar(ui));
        if let Some(e) = self.engine.last_error.clone() {
            egui::Panel::top("err").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(Color32::from_rgb(240, 90, 80), format!("⚠ {e}"));
                    if ui.small_button("Use simulator").clicked() {
                        self.engine.open_board(BoardChoice::Simulated(self.cfg.sim_headstages.clone()), self.edit_board.clone());
                    }
                    if ui.small_button("Dismiss").clicked() {
                        self.engine.last_error = None;
                    }
                });
            });
        }
        egui::Panel::left("side").resizable(true).default_size(300.0).show(ui, |ui| self.side_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            let full = ui.available_rect_before_wrap();
            self.spike_view(ui, full);
            ui.allocate_rect(full, egui::Sense::hover());
        });

        self.car_palette(ui.ctx(), CarTarget::Global);
        self.trace_windows(ui.ctx());
        // Window close (Hyprland killactive, Meta+C): refused while recording; stop it first.
        if ui.ctx().input(|i| i.viewport().close_requested()) && self.engine.is_recording() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_refused = Some(Instant::now());
        }
        if let Some(t) = self.close_refused {
            if t.elapsed() < CLOSE_REFUSED_MSG && self.engine.is_recording() {
                egui::Area::new(egui::Id::unique("close-refused")).anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0)).order(egui::Order::Foreground).show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).fill(Color32::from_rgb(120, 25, 25)).inner_margin(16.0).show(ui, |ui| {
                        ui.label(RichText::new("Recording in progress, not closing").size(20.0).strong().color(Color32::WHITE));
                        ui.label(RichText::new("Stop the recording first (Recording button or Ctrl+R), then close.").size(15.0).color(Color32::WHITE));
                    });
                });
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            } else {
                self.close_refused = None;
            }
        }
        if self.confirm_stop {
            egui::Modal::new(egui::Id::unique("confirm-stop")).show(ui.ctx(), |ui| {
                ui.heading("Stop acquisition?");
                ui.label("A recording is in progress. Stopping acquisition will end it.");
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Stop recording and acquisition").color(Color32::from_rgb(240, 90, 80))).clicked() {
                        self.engine.stop_recording();
                        std::thread::sleep(Duration::from_millis(50));
                        self.engine.stop_acquisition();
                        self.confirm_stop = false;
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm_stop = false;
                    }
                });
            });
        }
        if let Some((t, at)) = &self.toast {
            if at.elapsed() < Duration::from_secs(3) {
                egui::Area::new(egui::Id::unique("toast")).anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -20.0)).show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| ui.label(t.as_str()));
                });
            } else {
                self.toast = None;
            }
        }
    }

    fn on_exit(&mut self) {
        self.engine.stop_recording();
        // Let the recorder drain its queue and finalise the files before acquisition stops
        // and the process exits.
        let t0 = Instant::now();
        while self.engine.is_recording() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        self.engine.stop_acquisition();
        self.save_cfg();
    }
}

/// Identifies a channel layout (a change means a new board, scan or channel set).
fn layout_key(l: &StreamLayout) -> (usize, u64, usize, Vec<(String, usize)>) {
    (l.num_channels(), l.sample_rate.to_bits(), l.ttl_lines, l.headstages.iter().map(|h| (h.name.clone(), h.num_channels)).collect())
}

/// Channel colours: monitored channels by stereo position; with `per_row` > 1, electrodes
/// sharing a row by position in the group; otherwise by headstage.
fn channel_colors(l: &StreamLayout, mon: &[usize], per_row: usize) -> Vec<[f32; 4]> {
        l.channels
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if let Some(k) = mon.iter().position(|&m| m == i) {
                    monitor_color(k, mon.len())
                } else if per_row > 1 && c.kind == ChannelKind::Electrode {
                    let p = GROUP_PALETTE[i % per_row];
                    [p[0], p[1], p[2], 0.9]
                } else {
                    let p = PALETTE[c.headstage.unwrap_or(7) % PALETTE.len()];
                    let a = if c.kind == ChannelKind::Electrode { 0.95 } else { 0.7 };
                    [p[0], p[1], p[2], a]
                }
            })
            .collect()
}

/// Apply a channel click to the audio-monitored set: replace it, or (with `add`) toggle each
/// channel in or out of the mix (at most 8).
/// Points of wheel movement per row scrolled.
const WHEEL_PT_PER_ROW: f32 = 20.0;

/// Whole rows to scroll for this frame's wheel movement (positive = wheel up). egui spreads a
/// notch over several frames, so the movement is summed in `acc` until it makes a row;
/// rounding each frame's share on its own lost most of a gentle notch. A change of direction
/// starts afresh.
fn wheel_rows(acc: &mut f32, scroll: f32) -> isize {
    if scroll == 0.0 {
        return 0;
    }
    if *acc * scroll < 0.0 {
        *acc = 0.0;
    }
    *acc += scroll;
    let rows = (*acc / WHEEL_PT_PER_ROW).trunc();
    *acc -= rows * WHEEL_PT_PER_ROW;
    rows as isize
}

fn apply_monitor_click(a: &mut engine::audio::AudioSettings, chs: Vec<usize>, add: bool) {
    if add {
        for ch in chs {
            if let Some(i) = a.channels.iter().position(|&c| c == ch) {
                a.channels.remove(i);
            } else if a.channels.len() < 8 {
                a.channels.push(ch);
            }
        }
    } else {
        a.channels = chs.into_iter().take(8).collect();
    }
}

fn style(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 5.0);
    });
}

/// Keeps the last entry for each (channel, slot), in order: later spikes overwrite earlier
/// ones in a slot, so the dropped ones would never have been seen.
fn compact_newest_per_slot<T>(v: &mut Vec<(usize, usize, f32, T)>) {
    let mut seen = std::collections::HashSet::new();
    let mut keep: Vec<bool> = v.iter().rev().map(|e| seen.insert((e.0, e.1))).collect();
    keep.reverse();
    let mut k = keep.into_iter();
    v.retain(|_| k.next().unwrap());
}

fn channels_to_text(v: &[usize]) -> String {
    v.iter().map(|c| (c + 1).to_string()).collect::<Vec<_>>().join(", ")
}

fn text_to_channels(s: &str, max: usize) -> Vec<usize> {
    let mut v: Vec<usize> = s.split(|c: char| c == ',' || c.is_whitespace()).filter_map(|t| t.trim().parse::<usize>().ok()).filter(|&c| c >= 1 && (max == 0 || c <= max)).map(|c| c - 1).collect();
    v.dedup();
    v.truncate(8);
    v
}

#[cfg(test)]
mod wheel_tests {
    use super::{compact_newest_per_slot, wheel_rows};

    #[test]
    fn hidden_spike_backlog_keeps_the_newest_per_slot() {
        // Channel 0 slots 0,1,0,1,0 and channel 1 slot 0, in time order (value = time).
        let mut v = vec![(0, 0, 1.0, ()), (0, 1, 2.0, ()), (1, 0, 3.0, ()), (0, 0, 4.0, ()), (0, 1, 5.0, ()), (0, 0, 6.0, ())];
        compact_newest_per_slot(&mut v);
        let t: Vec<f32> = v.iter().map(|e| e.2).collect();
        assert_eq!(t, vec![3.0, 5.0, 6.0]);
    }

    #[test]
    fn a_notch_spread_over_frames_scrolls_its_rows() {
        // One 40-point notch as egui smooths it: no single frame reaches half a row.
        let mut acc = 0.0;
        let frames = [9.0, 8.0, 7.0, 5.0, 4.0, 3.0, 2.0, 1.5, 0.5];
        let rows: isize = frames.iter().map(|&s| wheel_rows(&mut acc, s)).sum();
        assert_eq!(rows, 2);
        assert!(acc.abs() < 1e-3);
    }

    #[test]
    fn direction_change_starts_afresh() {
        let mut acc = 0.0;
        assert_eq!(wheel_rows(&mut acc, 15.0), 0);
        assert_eq!(wheel_rows(&mut acc, -15.0), 0, "the 15 up is dropped, not cancelled");
        assert_eq!(wheel_rows(&mut acc, -6.0), -1);
        assert_eq!(wheel_rows(&mut acc, -45.0), -2);
    }
}
