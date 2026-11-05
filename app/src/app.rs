use crate::config::{Config, ViewFilter};
use crate::spikes::{SLOTS, SpikeCallback, SpikeFrame, SpikeGpu};
use crate::traces::{MAX_SECTIONS, TraceCallback, TraceConfig, TraceFrame, TraceGpu};
use eframe::egui::{self, Color32, RichText, Stroke};
use eframe::egui_wgpu;
use engine::audio::GainMode;
use engine::dsp::{BandSpec, Spike, ThresholdMode};
use engine::rhythm::{BoardSettings, ChannelKind, LiveCommand, StreamLayout};
use engine::{BoardChoice, Engine};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

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
const MONITOR_COLOR: [f32; 4] = [1.0, 0.95, 0.35, 1.0];

#[derive(PartialEq, Clone, Copy)]
enum View {
    Split,
    Traces,
    Spikes,
}

pub struct App {
    cfg: Config,
    engine: Engine,
    /// Board settings being edited (applied on "Apply").
    edit_board: BoardSettings,
    view: View,
    // trace staging
    pending: Vec<i16>,
    pending_nch: usize,
    disp_counter: u64,
    trace_reset: bool,
    last_trace_cfg: Option<TraceConfig>,
    /// (channel count, ADC V/row bits) of the per-channel trace table last sent to the GPU.
    chinfo_key: Option<(usize, u32)>,
    colors_dirty: bool,
    trace_colors: Option<Arc<Vec<[f32; 4]>>>,
    spike_colors: Option<Arc<Vec<[f32; 4]>>>,
    // spikes
    spike_slot: Vec<usize>,
    spike_new: Vec<(usize, usize, f32, Arc<Spike>)>,
    spike_times: Vec<VecDeque<f64>>,
    spike_reset: bool,
    t0: Instant,
    // stats
    disp_latency: f32,
    disp_latency_max: f32,
    latency_window: Instant,
    frame_times: VecDeque<f32>,
    last_frame: Instant,
    confirm_stop: bool,
    audio_text: String,
    gpu_name: String,
    toast: Option<(String, Instant)>,
    dirty_cfg: Instant,
    autostart: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config, autostart: bool) -> Self {
        let rs = cc.wgpu_render_state.as_ref().expect("wgpu renderer required");
        let info = rs.adapter.get_info();
        log::info!("GPU: {} ({:?}, driver {} {})", info.name, info.backend, info.driver, info.driver_info);
        {
            let mut r = rs.renderer.write();
            r.callback_resources.insert(TraceGpu::new(&rs.device, rs.target_format));
            r.callback_resources.insert(SpikeGpu::new(&rs.device, rs.target_format));
        }
        style(&cc.egui_ctx);
        let mut engine = Engine::new(cfg.record_root.clone());
        *engine.audio_settings.lock() = cfg.audio.clone();
        *engine.dsp_settings.lock() = cfg.dsp.clone();
        let choice = if cfg.open_hardware_on_start { BoardChoice::Hardware } else { BoardChoice::Simulated(cfg.sim_headstages.clone()) };
        engine.open_board(choice, cfg.board.clone());
        let audio_text = channels_to_text(&cfg.audio.channels);
        Self {
            edit_board: cfg.board.clone(),
            cfg,
            engine,
            view: View::Split,
            pending: Vec::new(),
            pending_nch: 0,
            disp_counter: 0,
            trace_reset: true,
            last_trace_cfg: None,
            chinfo_key: None,
            colors_dirty: true,
            trace_colors: None,
            spike_colors: None,
            spike_slot: Vec::new(),
            spike_new: Vec::new(),
            spike_times: Vec::new(),
            spike_reset: true,
            t0: Instant::now(),
            disp_latency: 0.0,
            disp_latency_max: 0.0,
            latency_window: Instant::now(),
            frame_times: VecDeque::new(),
            last_frame: Instant::now(),
            confirm_stop: false,
            audio_text,
            gpu_name: format!("{} ({:?})", info.name, info.backend),
            toast: None,
            dirty_cfg: Instant::now(),
            autostart,
        }
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
        self.cfg.save();
    }

    /// Drain engine queues (non-blocking) into GPU staging.
    fn drain(&mut self) {
        let Some(l) = self.engine.layout() else { return };
        let nch = l.num_channels();
        let nel = l.num_electrodes();
        let window = (self.cfg.display.window_s as f64 * l.sample_rate).max(1.0) as usize;
        if nch != self.pending_nch {
            self.pending.clear();
            self.pending_nch = nch;
            self.trace_reset = true;
        }
        let mut newest: Option<Instant> = None;
        if let Some(rx) = self.engine.display_rx() {
            for b in rx.try_iter() {
                if b.num_channels != nch {
                    continue;
                }
                self.pending.extend_from_slice(&b.data);
                newest = b.acquired_at.or(newest);
            }
        }
        // Never upload more than one window (e.g. after the window was minimised).
        let max = window * nch;
        if self.pending.len() > max {
            let drop = self.pending.len() - max;
            let drop = drop - drop % nch;
            self.pending.drain(..drop);
            self.disp_counter += (drop / nch) as u64;
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
        }
        for q in &mut self.spike_times {
            while q.front().is_some_and(|&t| now - t > 2.0) {
                q.pop_front();
            }
        }
    }

    fn channel_colors(&self, l: &StreamLayout) -> Vec<[f32; 4]> {
        let mon = self.engine.audio_settings.lock().channels.clone();
        l.channels
            .iter()
            .enumerate()
            .map(|(i, c)| {
                if mon.contains(&i) {
                    MONITOR_COLOR
                } else {
                    let p = PALETTE[c.headstage.unwrap_or(7) % PALETTE.len()];
                    let a = if c.kind == ChannelKind::Electrode { 0.95 } else { 0.7 };
                    [p[0], p[1], p[2], a]
                }
            })
            .collect()
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
            ui.add_enabled_ui(!acquiring && !busy, |ui| {
                if ui.button("Open board").on_hover_text("Open the ONI acquisition board (FT600)").clicked() {
                    self.cfg.open_hardware_on_start = true;
                    self.engine.open_board(BoardChoice::Hardware, self.edit_board.clone());
                    self.reset_views();
                }
                if ui.button("Simulator").on_hover_text("Real-time simulated headstages").clicked() {
                    self.engine.open_board(BoardChoice::Simulated(self.cfg.sim_headstages.clone()), self.edit_board.clone());
                    self.reset_views();
                }
                if ui.add_enabled(self.engine.has_board(), egui::Button::new("Rescan")).on_hover_text("Detect headstages and tune cable delays").clicked() {
                    self.engine.rescan();
                    self.reset_views();
                }
            });
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
            if ui.add_enabled(acquiring, rec_btn).on_hover_text("Ctrl+R").clicked() || (acquiring && ui.input(|i| i.modifiers.ctrl && i.key_pressed(egui::Key::R))) {
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
                chip(ui, a.underruns.load(Ordering::Relaxed) < 5, format!("audio {:.1} ms", a.latency_us.load(Ordering::Relaxed) as f32 / 1000.0), "Audio latency after acquisition (ring + device buffer + filters). Underruns shown red.");
            }
            chip(ui, self.disp_latency_max < 25.0, format!("display {:.1} ms", self.disp_latency), "Sample arrival → GPU submit (excludes compositor scan-out).");
            let dd = p.display_dropped.load(Ordering::Relaxed) + p.dsp_dropped.load(Ordering::Relaxed) + p.audio_dropped.load(Ordering::Relaxed);
            if dd > 0 {
                chip(ui, false, format!("viz drops {dd}"), "Blocks skipped by display/DSP/audio consumers (never affects recording).");
            }
        }
        if let Some(e) = self.engine.rec_status.error.lock().clone() {
            chip(ui, false, "REC ERROR".into(), &e);
        }
        let fps = if self.frame_times.is_empty() { 0.0 } else { self.frame_times.len() as f32 / self.frame_times.iter().sum::<f32>() };
        ui.label(RichText::new(format!("{fps:.0} fps")).weak().monospace()).on_hover_text(&self.gpu_name);
    }

    fn reset_views(&mut self) {
        self.trace_reset = true;
        self.spike_reset = true;
        self.colors_dirty = true;
        self.pending.clear();
        self.disp_counter = 0;
        self.spike_new.clear();
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            self.board_section(ui);
            self.display_section(ui);
            self.spike_section(ui);
            self.audio_section(ui);
            self.record_section(ui);
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
                    ui.small("Smaller USB blocks lower latency at the cost of more USB transactions; 24 KiB is the stock default.");
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

    fn display_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("Display").strong()).default_open(true).show(ui, |ui| {
            let d = &mut self.cfg.display;
            let old = d.clone();
            ui.horizontal(|ui| {
                ui.label("Filter");
                ui.selectable_value(&mut d.filter, ViewFilter::Raw, "Raw");
                ui.selectable_value(&mut d.filter, ViewFilter::Spikes, "300–6k");
                ui.selectable_value(&mut d.filter, ViewFilter::Lfp, "LFP");
            });
            ui.add(egui::Slider::new(&mut d.window_s, 0.05..=20.0).logarithmic(true).text("time window (s)"));
            ui.add(egui::Slider::new(&mut d.uv_per_row, 10.0..=5000.0).logarithmic(true).text("µV per row"));
            // Rows cover every channel: electrodes first, then AUX and ADC.
            let (nrows_all, has_adc) = self.engine.layout().map(|l| (l.num_channels(), l.channels.iter().any(|c| c.kind == ChannelKind::Adc))).unwrap_or((0, false));
            let nrows_all = nrows_all.max(1);
            ui.add(egui::Slider::new(&mut d.rows, 1..=nrows_all).text("rows"));
            let maxfirst = nrows_all.saturating_sub(d.rows);
            d.first_row = d.first_row.min(maxfirst);
            ui.add_enabled(maxfirst > 0, egui::Slider::new(&mut d.first_row, 0..=maxfirst).text("first row"));
            if has_adc {
                ui.add(egui::Slider::new(&mut d.adc_v_per_row, 0.1..=10.0).logarithmic(true).text("ADC V per row"))
                    .on_hover_text("ADC rows are centred on 0 V and never filtered. AUX rows span 2.5 V.");
            }
            ui.add(egui::Slider::new(&mut d.thickness, 1.0..=3.0).text("line px"));
            ui.horizontal(|ui| {
                ui.label("View");
                ui.selectable_value(&mut self.view, View::Split, "Split");
                ui.selectable_value(&mut self.view, View::Traces, "Traces");
                ui.selectable_value(&mut self.view, View::Spikes, "Spikes");
            });
            ui.add(egui::Slider::new(&mut d.max_fps, 30.0..=360.0).text("max fps"));
            ui.small("Click a channel label to monitor it on audio. Scroll over traces to move rows; Ctrl+scroll to scale.");
            if *d != old {
                if d.window_s != old.window_s {
                    self.trace_reset = true;
                }
                self.dirty_cfg = Instant::now();
            }
        });
    }

    fn spike_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("Spike detection").strong()).default_open(true).show(ui, |ui| {
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

    fn audio_section(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(RichText::new("Audio monitor").strong()).default_open(true).show(ui, |ui| {
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
            ui.small("Open Ephys Binary format (structure.oebin), readable by open-ephys-python-tools, SpikeInterface and Kilosort.");
            if let Some(d) = self.engine.rec_status.current_dir.lock().as_ref() {
                ui.small(format!("Last: {}", d.display()));
            }
        });
    }

    // ------------------------------------------------------------------ views
    fn trace_view(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let Some(l) = self.layout() else {
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, "No board", egui::FontId::proportional(18.0), Color32::GRAY);
            return;
        };
        let nel = l.num_electrodes();
        let nch = l.num_channels();
        if nel == 0 {
            return;
        }
        let ppp = ui.ctx().pixels_per_point();
        let label_w = 64.0;
        let plot = egui::Rect::from_min_max(egui::pos2(rect.min.x + label_w, rect.min.y), rect.max);
        let d = self.cfg.display.clone();
        let rows = d.rows.clamp(1, nch);
        let first = d.first_row.min(nch - rows);
        // Interaction: scroll rows / scale.
        let resp = ui.interact(rect, ui.scope_id().with("traces"), egui::Sense::click());
        if resp.hovered() {
            let (scroll, ctrl) = ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers.ctrl));
            if scroll != 0.0 {
                if ctrl {
                    self.cfg.display.uv_per_row = (d.uv_per_row * (1.0 - scroll * 0.002)).clamp(10.0, 5000.0);
                } else {
                    let step = (scroll / 20.0).round() as isize;
                    self.cfg.display.first_row = (first as isize - step).clamp(0, (nch - rows) as isize) as usize;
                }
                self.dirty_cfg = Instant::now();
            }
        }
        let cols = (plot.width() * ppp).floor().max(1.0) as u32;
        let window = (d.window_s as f64 * l.sample_rate).round().max(cols as f64) as u32;
        let sections: Vec<[f32; 5]> = match d.filter {
            ViewFilter::Raw => vec![],
            ViewFilter::Spikes => BandSpec::SPIKES.sections(l.sample_rate),
            ViewFilter::Lfp => BandSpec::LFP.sections(l.sample_rate),
        }
        .into_iter()
        .take(MAX_SECTIONS)
        .map(|b| [b.b0, b.b1, b.b2, b.a1, b.a2])
        .collect();
        let config = TraceConfig { nch, cols, window, sections };
        if self.last_trace_cfg.as_ref() != Some(&config) {
            if self.last_trace_cfg.as_ref().is_some_and(|c| c.cols != config.cols || c.window != config.window || c.nch != config.nch) {
                self.trace_reset = true;
            }
            self.last_trace_cfg = Some(config.clone());
        }
        let n_new = (self.pending.len() / nch.max(1)) as u32;
        let pos0 = (self.disp_counter % window as u64) as u32;
        self.disp_counter += n_new as u64;
        let cursor_pos = (self.disp_counter % window as u64) as f64;
        let cursor_col = ((cursor_pos * cols as f64 / window as f64) as u32).min(cols - 1);
        let upload = Arc::new(std::mem::take(&mut self.pending));
        let row_h_px = plot.height() * ppp / rows as f32;
        // Row -> channel: electrodes first, then AUX/ADC. Labels use the same mapping.
        let mut row_ch: Vec<u32> = (0..nch as u32).filter(|&i| l.channels[i as usize].kind == ChannelKind::Electrode).collect();
        row_ch.extend((0..nch as u32).filter(|&i| l.channels[i as usize].kind != ChannelKind::Electrode));
        // Per-channel scaling (see traces.wgsl): electrodes follow the global µV/row and the
        // display filter; AUX/ADC are drawn raw in volts on their own row range.
        let chinfo_key = (nch, d.adc_v_per_row.to_bits());
        let chinfo = (self.trace_reset || self.chinfo_key != Some(chinfo_key)).then(|| {
            self.chinfo_key = Some(chinfo_key);
            Arc::new(
                l.channels
                    .iter()
                    .map(|c| match c.kind {
                        ChannelKind::Electrode => [c.bit_volts, 1.0, 0.0, 0.0],
                        ChannelKind::Aux => [c.bit_volts, 0.0, 2.5, 0.0],
                        ChannelKind::Adc => [c.bit_volts, 0.0, d.adc_v_per_row, 0.0],
                    })
                    .collect::<Vec<_>>(),
            )
        });
        let order = self.trace_reset.then(|| Arc::new(row_ch.clone()));
        let colors = if self.trace_reset { Some(Arc::new(self.channel_colors(&l))) } else { self.trace_colors.take() };
        let frame = TraceFrame { config, reset: std::mem::take(&mut self.trace_reset), upload, n_new, pos0, cursor_col, first_row: first as u32, nrows: rows as u32, uv_per_row: d.uv_per_row, row_h_px, thickness_px: d.thickness * ppp.max(1.0), colors, order, chinfo };
        ui.painter().rect_filled(rect, 0.0, Color32::from_rgb(12, 13, 16));
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(plot, TraceCallback(frame)));
        // Overlay: labels, separators, cursor time.
        let p = ui.painter_at(rect);
        let rh = plot.height() / rows as f32;
        let mon = self.engine.audio_settings.lock().channels.clone();
        let click = resp.clicked().then(|| resp.interact_pointer_pos()).flatten();
        let mut toggle = None;
        let mut analog_visible = false;
        for r in 0..rows {
            let ch = row_ch[first + r] as usize;
            let is_electrode = l.channels[ch].kind == ChannelKind::Electrode;
            analog_visible |= !is_electrode;
            let y = plot.min.y + (r as f32 + 0.5) * rh;
            let lab = egui::Rect::from_min_size(egui::pos2(rect.min.x, y - rh / 2.0), egui::vec2(label_w, rh));
            let monitored = mon.contains(&ch);
            if rh >= 9.0 {
                let txt = format!("{}{}", l.channels[ch].name, if monitored { " ♪" } else { "" });
                let col = if monitored { Color32::from_rgb(255, 240, 90) } else { Color32::from_gray(150) };
                p.text(egui::pos2(rect.min.x + 4.0, y), egui::Align2::LEFT_CENTER, txt, egui::FontId::monospace((rh * 0.6).clamp(8.0, 13.0)), col);
            }
            // Audio monitors electrodes only.
            if let Some(pos) = click
                && is_electrode
                && lab.contains(pos)
            {
                toggle = Some(ch);
            }
        }
        if let Some(ch) = toggle {
            let mut a = self.engine.audio_settings.lock();
            if ui.input(|i| i.modifiers.shift) {
                if let Some(i) = a.channels.iter().position(|&c| c == ch) {
                    a.channels.remove(i);
                } else if a.channels.len() < 8 {
                    a.channels.push(ch);
                }
            } else {
                a.channels = vec![ch];
            }
            self.audio_text = channels_to_text(&a.channels);
            self.colors_dirty = true;
            self.engine.audio_stats.recalibrate.store(true, Ordering::Relaxed);
        }
        let x = plot.min.x + cursor_col as f32 / ppp;
        p.line_segment([egui::pos2(x, plot.min.y), egui::pos2(x, plot.max.y)], Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 40)));
        p.text(egui::pos2(plot.max.x - 4.0, plot.max.y - 4.0), egui::Align2::RIGHT_BOTTOM, if analog_visible { format!("{:.2} s · {:.0} µV/row · ADC {:.2} V/row · AUX 2.5 V/row", d.window_s, d.uv_per_row, d.adc_v_per_row) } else { format!("{:.2} s · {:.0} µV/row", d.window_s, d.uv_per_row) }, egui::FontId::monospace(11.0), Color32::from_gray(120));
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
        let first = (self.cfg.display.first_row.min(nel - 1) / gc) * gc;
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
        let p = ui.painter_at(rect);
        let cs = self.engine.channel_stats.lock().clone();
        for i in 0..ncells {
            let ch = first + i;
            let o = rect.min + egui::vec2((i % gc) as f32 * cell_w, (i / gc) as f32 * cell_h);
            let cell = egui::Rect::from_min_size(o, egui::vec2(cell_w, cell_h));
            p.rect_stroke(cell.shrink(1.0), 2.0, Stroke::new(1.0, Color32::from_gray(35)), egui::StrokeKind::Inside);
            let rate = self.spike_times.get(ch).map(|q| q.len() as f32 / 2.0).unwrap_or(0.0);
            p.text(o + egui::vec2(5.0, 3.0), egui::Align2::LEFT_TOP, format!("{}  {:.1} Hz", l.channels[ch].name, rate), egui::FontId::monospace(10.5), Color32::from_gray(160));
            if let Some(&thr) = cs.threshold_uv.get(ch) {
                let k = (cell_h * 0.5 - 4.0) / d.spike_uv_range;
                let y = o.y + cell_h * 0.5 - thr * k;
                if y > o.y && y < o.y + cell_h {
                    p.line_segment([egui::pos2(o.x + 4.0, y), egui::pos2(o.x + cell_w - 4.0, y)], Stroke::new(1.0, Color32::from_rgba_unmultiplied(220, 80, 80, 90)));
                }
                if let Some(s) = cs.sigma_uv.get(ch) {
                    p.text(o + egui::vec2(cell_w - 5.0, 3.0), egui::Align2::RIGHT_TOP, format!("σ {s:.1}"), egui::FontId::monospace(10.0), Color32::from_gray(110));
                }
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let dt = self.last_frame.elapsed().as_secs_f32();
        self.last_frame = Instant::now();
        self.frame_times.push_back(dt);
        while self.frame_times.len() > 60 {
            self.frame_times.pop_front();
        }
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
        self.drain();
        if self.colors_dirty
            && let Some(l) = self.layout()
        {
            self.colors_dirty = false;
            let c = Arc::new(self.channel_colors(&l));
            self.trace_colors = Some(c.clone());
            self.spike_colors = Some(c);
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
            match self.view {
                View::Traces => self.trace_view(ui, full),
                View::Spikes => self.spike_view(ui, full),
                View::Split => {
                    let split = self.cfg.display.split;
                    let (top, bottom) = full.split_top_bottom_at_fraction(split);
                    self.trace_view(ui, top.shrink2(egui::vec2(0.0, 1.0)));
                    self.spike_view(ui, bottom.shrink2(egui::vec2(0.0, 1.0)));
                }
            }
            ui.allocate_rect(full, egui::Sense::hover());
        });

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
        if self.dirty_cfg.elapsed() > Duration::from_secs(1) && self.dirty_cfg.elapsed() < Duration::from_secs(2) {
            self.save_cfg();
            self.dirty_cfg = Instant::now() - Duration::from_secs(10);
        }
        // Pace frames: continuous while acquiring, capped at max_fps.
        let period = Duration::from_secs_f32(1.0 / self.cfg.display.max_fps.max(10.0));
        if self.engine.is_acquiring() || self.engine.is_busy() {
            ui.ctx().request_repaint_after(period.saturating_sub(self.last_frame.elapsed()));
        } else {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
    }

    fn on_exit(&mut self) {
        self.engine.stop_recording();
        self.engine.stop_acquisition();
        self.save_cfg();
    }
}

fn style(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(6.0, 5.0);
    });
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
