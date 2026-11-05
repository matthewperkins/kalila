use crate::config::{Config, OutputSettings, ViewFilter};
use crate::spikes::{SLOTS, SpikeCallback, SpikeFrame, SpikeGpu};
use crate::sweep::Sweeps;
use crate::traces::{MAX_CUTS, MAX_SECTIONS, RING, SweepFrame, TraceCallback, TraceConfig, TraceFrame, TraceGpu};
use crate::ttl::{TtlGeom, TtlOverlay};
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
    /// TTL words for the samples in `pending` (one per sample).
    pending_ttl: Vec<u16>,
    ttl: TtlOverlay,
    pending_nch: usize,
    disp_counter: u64,
    trace_reset: bool,
    last_trace_cfg: Option<TraceConfig>,
    /// (channel count, ADC V/row bits, channels per row) of the trace tables last sent.
    chinfo_key: Option<(usize, u32, usize)>,
    sweeps: Sweeps,
    /// Per-channel trace table last sent (resent when CAR sets or scaling change).
    last_chinfo: Option<Vec<[f32; 4]>>,
    car_window: bool,
    /// (pre, post, line, slots, filter, cols): a change clears the sweeps.
    sweep_key: Option<(u32, u32, usize, u32, ViewFilter, u32)>,
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
            pending_ttl: Vec::new(),
            ttl: TtlOverlay::default(),
            pending_nch: 0,
            disp_counter: 0,
            trace_reset: true,
            last_trace_cfg: None,
            chinfo_key: None,
            sweeps: Sweeps::default(),
            last_chinfo: None,
            car_window: false,
            sweep_key: None,
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
            was_acquiring: false,
            ttl_out_word: 0,
            dac_sent: [None; 8],
            outputs_sent: None,
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
            self.pending_ttl.clear();
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
                let n = b.data.len() / nch.max(1);
                if b.ttl.len() == n {
                    self.pending_ttl.extend_from_slice(&b.ttl);
                } else {
                    self.pending_ttl.resize(self.pending_ttl.len() + n, 0);
                }
                newest = b.acquired_at.or(newest);
            }
        }
        // Never upload more than one window (e.g. after the window was minimised).
        let max = window * nch;
        if self.pending.len() > max {
            let drop = self.pending.len() - max;
            let drop = drop - drop % nch;
            self.pending.drain(..drop);
            self.pending_ttl.drain(..(drop / nch).min(self.pending_ttl.len()));
            // Trimmed samples never reach the GPU ring: waiting triggers can't be cut.
            self.sweeps.drop_pending();
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

    /// `per_row` > 1: electrodes sharing a row get distinct colours by position in the group.
    fn channel_colors(&self, l: &StreamLayout, per_row: usize) -> Vec<[f32; 4]> {
        let mon = self.engine.audio_settings.lock().channels.clone();
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
        self.pending_ttl.clear();
        self.ttl.reset();
        self.sweeps.clear();
        self.disp_counter = 0;
        self.spike_new.clear();
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            self.board_section(ui);
            self.outputs_section(ui);
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

    /// Floating palette choosing the display CAR's reference and subtract sets.
    fn car_palette(&mut self, ctx: &egui::Context) {
        if !self.car_window {
            return;
        }
        let Some(l) = self.layout() else { return };
        // Electrodes grouped by headstage.
        let groups: Vec<(String, Vec<(usize, String)>)> = l
            .headstages
            .iter()
            .enumerate()
            .map(|(hi, h)| {
                let chans = l.channels.iter().enumerate().filter(|(_, c)| c.kind == ChannelKind::Electrode && c.headstage == Some(hi)).map(|(i, c)| (i, c.name.clone())).collect();
                (format!("{} {}", h.name, h.chip), chans)
            })
            .collect();
        let mut open = true;
        let mut changed = false;
        let car = &mut self.cfg.display.car;
        egui::Window::new("Common average reference").open(&mut open).resizable(false).collapsible(false).default_width(460.0).show(ctx, |ui| {
            ui.small("Recordings always keep the original signals.");
            ui.horizontal(|ui| {
                ui.label("Apply to");
                changed |= ui.checkbox(&mut car.enabled, "display (300–6k)").changed();
                changed |= ui.checkbox(&mut car.spikes, "spike detection").changed();
                changed |= ui.checkbox(&mut car.audio, "audio").changed();
            });
            ui.weak(format!("reference {} · subtract {}", car.reference.len(), car.subtract.len()));
            let sections = [("Reference (averaged)", &mut car.reference), ("Subtract from", &mut car.subtract)];
            let mut copy_ref = false;
            for (title, set) in sections {
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
            self.dirty_cfg = Instant::now();
        }
        if !open {
            self.car_window = false;
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
            ui.horizontal(|ui| {
                ui.label("CAR").on_hover_text("Common average reference. Recordings always keep the original signals.");
                ui.checkbox(&mut d.car.enabled, "display").on_hover_text("In the 300–6k view only");
                ui.checkbox(&mut d.car.spikes, "spikes");
                ui.checkbox(&mut d.car.audio, "audio");
                if ui.small_button("Channels…").clicked() {
                    self.car_window = true;
                }
            });
            ui.horizontal(|ui| {
                ui.weak(format!("CAR ref {} · sub {}", d.car.reference.len(), d.car.subtract.len()));
                if d.car.enabled && d.filter != ViewFilter::Spikes {
                    ui.weak("(display: 300–6k only)");
                }
            });
            ui.add(egui::Slider::new(&mut d.window_s, 0.05..=20.0).logarithmic(true).text("time window (s)"));
            ui.add(egui::Slider::new(&mut d.uv_per_row, 10.0..=5000.0).logarithmic(true).text("µV per row"));
            // Rows cover every channel: electrodes first (`per_row` to a row), then AUX and ADC.
            let k = d.per_row.clamp(1, 8) as usize;
            let (nrows_all, has_adc, nlines) = self
                .engine
                .layout()
                .map(|l| (l.num_electrodes().div_ceil(k) + l.num_channels() - l.num_electrodes(), l.channels.iter().any(|c| c.kind == ChannelKind::Adc), l.ttl_lines))
                .unwrap_or((0, false, 8));
            let nrows_all = nrows_all.max(1);
            ui.horizontal(|ui| {
                ui.label("Sweep");
                ui.selectable_value(&mut d.triggered, false, "Free");
                ui.selectable_value(&mut d.triggered, true, "Triggered").on_hover_text("Each sweep starts at a TTL rising edge; recent sweeps are overlaid (opto-tagging)");
            });
            if d.triggered {
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("trigline").width(56.0).selected_text(format!("TTL{}", d.trig_line + 1)).show_ui(ui, |ui| {
                        for i in 0..nlines.max(1) {
                            ui.selectable_value(&mut d.trig_line, i, format!("TTL{}", i + 1));
                        }
                    });
                    ui.label("rising");
                    ui.add(egui::DragValue::new(&mut d.trig_pre_ms).range(0.0..=200.0).speed(0.5).prefix("−").suffix(" ms"));
                    ui.add(egui::DragValue::new(&mut d.trig_post_ms).range(1.0..=250.0).speed(0.5).prefix("+").suffix(" ms"));
                });
                ui.horizontal(|ui| {
                    ui.add(egui::Slider::new(&mut d.trig_sweeps, 1..=50).text("sweeps"));
                    if ui.small_button("Clear").clicked() {
                        self.sweeps.clear();
                    }
                });
            }
            ui.horizontal(|ui| {
                ui.label("Channels per row");
                for n in [1u32, 2, 4, 8] {
                    ui.selectable_value(&mut d.per_row, n, n.to_string());
                }
            });
            ui.add(egui::Slider::new(&mut d.rows, 1..=nrows_all).text("rows"));
            let maxfirst = nrows_all.saturating_sub(d.rows);
            d.first_row = d.first_row.min(maxfirst);
            ui.add_enabled(maxfirst > 0, egui::Slider::new(&mut d.first_row, 0..=maxfirst).text("first row"));
            if has_adc {
                ui.add(egui::Slider::new(&mut d.adc_v_per_row, 0.1..=10.0).logarithmic(true).text("ADC V per row"))
                    .on_hover_text("ADC rows are centred on 0 V and never filtered. AUX rows span 2.5 V.");
            }
            ui.add(egui::Slider::new(&mut d.thickness, 1.0..=3.0).text("line px"));
            ui.checkbox(&mut d.ttl_overlay, "TTL overlay").on_hover_text("Shade traces while a TTL input is high; one lane per line seen high since acquisition start");
            ui.horizontal(|ui| {
                ui.label("View");
                ui.selectable_value(&mut self.view, View::Split, "Split");
                ui.selectable_value(&mut self.view, View::Traces, "Traces");
                ui.selectable_value(&mut self.view, View::Spikes, "Spikes");
            });
            ui.add(egui::Slider::new(&mut d.max_fps, 30.0..=360.0).text("max fps"));
            ui.small("Click a channel label or spike tile to monitor it on audio; Ctrl+click adds/removes it from the mix. Scroll over traces to move rows; Ctrl+scroll to scale.");
            if *d != old {
                if d.window_s != old.window_s {
                    self.trace_reset = true;
                }
                self.dirty_cfg = Instant::now();
            }
        });
    }

    fn spike_section(&mut self, ui: &mut egui::Ui) {
        let car = self.cfg.display.car.spikes;
        egui::CollapsingHeader::new(section_title(ui, "Spike detection", car)).id_salt("spike_section").default_open(true).show(ui, |ui| {
            self.car_note(ui, car);
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

    /// Inside a section using the CAR: what it is applied with and where to change it.
    fn car_note(&mut self, ui: &mut egui::Ui, on: bool) {
        if !on {
            return;
        }
        let c = &self.cfg.display.car;
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("CAR applied · ref {} · sub {}", c.reference.len(), c.subtract.len())).small().color(CAR_COLOR))
                .on_hover_text("Common average reference, switched on under Display. Recordings keep the original signals.");
            if ui.small_button("Channels…").clicked() {
                self.car_window = true;
            }
        });
    }

    fn audio_section(&mut self, ui: &mut egui::Ui) {
        let car = self.cfg.display.car.audio;
        egui::CollapsingHeader::new(section_title(ui, "Audio monitor", car)).id_salt("audio_section").default_open(true).show(ui, |ui| {
            self.car_note(ui, car);
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
        let d = self.cfg.display.clone();
        let trig = d.triggered;
        let fs = l.sample_rate;
        // Order: electrodes first, then AUX/ADC. Rows: electrodes `k` per row (overlaid),
        // analog channels one per row.
        let k = d.per_row.clamp(1, 8) as usize;
        let mut row_ch: Vec<u32> = (0..nch as u32).filter(|&i| l.channels[i as usize].kind == ChannelKind::Electrode).collect();
        row_ch.extend((0..nch as u32).filter(|&i| l.channels[i as usize].kind != ChannelKind::Electrode));
        let mut rowtab: Vec<[u32; 2]> = (0..nel).step_by(k).map(|s| [s as u32, k.min(nel - s) as u32]).collect();
        rowtab.extend((nel..nch).map(|i| [i as u32, 1]));
        let nrows_all = rowtab.len();
        // Free-running: TTL lanes under the traces. Triggered: a time axis instead.
        let ttl_lines = if d.ttl_overlay && !trig { self.ttl.active_lines(l.ttl_lines) } else { Vec::new() };
        let strip_h = if trig { 14.0 } else if ttl_lines.is_empty() { 0.0 } else { 7.0 * ttl_lines.len() as f32 + 2.0 };
        let plot = egui::Rect::from_min_max(egui::pos2(rect.min.x + label_w, rect.min.y), egui::pos2(rect.max.x, rect.max.y - strip_h));
        let strip = egui::Rect::from_min_max(egui::pos2(plot.min.x, plot.max.y + 2.0), rect.max);
        let rows = d.rows.clamp(1, nrows_all);
        let first = d.first_row.min(nrows_all - rows);
        // Interaction: scroll rows / scale.
        let resp = ui.interact(rect, ui.scope_id().with("traces"), egui::Sense::click());
        if resp.hovered() {
            let (scroll, ctrl) = ui.input(|i| (i.smooth_scroll_delta.y, i.modifiers.ctrl));
            if scroll != 0.0 {
                if ctrl {
                    self.cfg.display.uv_per_row = (d.uv_per_row * (1.0 - scroll * 0.002)).clamp(10.0, 5000.0);
                } else {
                    let step = (scroll / 20.0).round() as isize;
                    self.cfg.display.first_row = (first as isize - step).clamp(0, (nrows_all - rows) as isize) as usize;
                }
                self.dirty_cfg = Instant::now();
            }
        }
        let cols = (plot.width() * ppp).floor().max(1.0) as u32;
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
        // Triggered window in samples; together they must fit well inside the GPU ring.
        let pre = ((d.trig_pre_ms as f64 * 1e-3 * fs).round() as u32).min(RING / 4);
        let post = ((d.trig_post_ms as f64 * 1e-3 * fs).round() as u32).clamp(1, RING / 4);
        let span = pre + post;
        let nslots = if trig { d.trig_sweeps.clamp(1, 50) } else { 0 };
        let config = TraceConfig { nch, cols, window, sections, nslots };
        if self.last_trace_cfg.as_ref() != Some(&config) {
            if self.last_trace_cfg.as_ref().is_some_and(|c| c.cols != config.cols || c.window != config.window || c.nch != config.nch || c.nslots != config.nslots) {
                self.trace_reset = true;
            }
            self.last_trace_cfg = Some(config.clone());
        }
        // Old sweeps are meaningless after a change of window, line, filter or geometry.
        let sweep_key = (pre, post, d.trig_line, nslots, d.filter, cols);
        if self.trace_reset || self.sweep_key != Some(sweep_key) {
            self.sweeps.clear();
            self.sweep_key = Some(sweep_key);
        }
        let n_new = (self.pending.len() / nch.max(1)) as u32;
        let start = self.disp_counter;
        let pos0 = (start % window as u64) as u32;
        self.disp_counter += n_new as u64;
        // Fold TTL words into columns with the shader's mapping (a size/window change
        // clears columns, like the GPU state), and find trigger edges.
        if self.trace_reset {
            self.ttl.clear_cols();
        }
        let words = std::mem::take(&mut self.pending_ttl);
        let words = &words[..(n_new as usize).min(words.len())];
        self.ttl.ingest(words, pos0, window, cols);
        let sweep = if trig {
            self.sweeps.scan(words, start, d.trig_line);
            let cuts = self.sweeps.take_ready(self.disp_counter, pre, post, nslots, RING, MAX_CUTS);
            SweepFrame { ring0: (start % RING as u64) as u32, span, cuts, newest: self.sweeps.newest_slot(nslots), filled: self.sweeps.filled(nslots) }
        } else {
            SweepFrame::default()
        };
        let cursor_pos = (self.disp_counter % window as u64) as f64;
        let cursor_col = ((cursor_pos * cols as f64 / window as f64) as u32).min(cols - 1);
        let upload = Arc::new(std::mem::take(&mut self.pending));
        let row_h_px = plot.height() * ppp / rows as f32;
        // Per-channel scaling (see traces.wgsl): electrodes follow the global µV/row and the
        // display filter; AUX/ADC are drawn raw in volts on their own row range.
        // Flags: bit 0 display filter, bit 1 CAR reference, bit 2 CAR subtract (electrodes).
        let car_on = d.car.enabled && d.filter == ViewFilter::Spikes;
        let in_set = |set: &[usize], ch: usize| set.contains(&ch);
        let info: Vec<[f32; 4]> = l
            .channels
            .iter()
            .enumerate()
            .map(|(i, c)| match c.kind {
                ChannelKind::Electrode => [c.bit_volts, (1 + 2 * in_set(&d.car.reference, i) as u32 + 4 * in_set(&d.car.subtract, i) as u32) as f32, 0.0, 0.0],
                ChannelKind::Aux => [c.bit_volts, 0.0, 2.5, 0.0],
                ChannelKind::Adc => [c.bit_volts, 0.0, d.adc_v_per_row, 0.0],
            })
            .collect();
        let chinfo_key = (nch, d.adc_v_per_row.to_bits(), k);
        let send_tables = self.trace_reset || self.chinfo_key != Some(chinfo_key);
        let chinfo = (send_tables || self.last_chinfo.as_ref() != Some(&info)).then(|| {
            self.last_chinfo = Some(info.clone());
            Arc::new(info)
        });
        if self.chinfo_key.is_some_and(|c| c.2 != k) {
            self.colors_dirty = true;
        }
        self.chinfo_key = Some(chinfo_key);
        let order = send_tables.then(|| Arc::new(row_ch.clone()));
        let rowtab_gpu = send_tables.then(|| Arc::new(rowtab.clone()));
        let colors = if self.trace_reset { Some(Arc::new(self.channel_colors(&l, k))) } else { self.trace_colors.take() };
        let frame = TraceFrame {
            config,
            reset: std::mem::take(&mut self.trace_reset),
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
        };
        ui.painter().rect_filled(rect, 0.0, Color32::from_rgb(12, 13, 16));
        let p = ui.painter_at(rect);
        let span_ms = d.trig_pre_ms + d.trig_post_ms;
        let t_to_x = |t_ms: f32| plot.min.x + (t_ms + d.trig_pre_ms) / span_ms.max(1e-3) * plot.width();
        if trig {
            // Laser-on shading under the traces: newest sweep's TTL high time.
            if self.sweeps.count() > 0 {
                let w_ms = self.sweeps.newest_width().map(|w| w as f32 * 1e3 / fs as f32).unwrap_or(d.trig_post_ms).min(d.trig_post_ms);
                let c = crate::ttl::line_color(d.trig_line);
                p.rect_filled(egui::Rect::from_min_max(egui::pos2(t_to_x(0.0), plot.min.y), egui::pos2(t_to_x(w_ms).max(t_to_x(0.0) + 1.0), plot.max.y)), 0.0, Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 34));
            }
        }
        ui.painter().add(egui_wgpu::Callback::new_paint_callback(plot, TraceCallback(frame)));
        // Overlay: labels, separators, cursor / time axis.
        let rh = plot.height() / rows as f32;
        let mon = self.engine.audio_settings.lock().channels.clone();
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
        let dim = Color32::from_gray(120);
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
            let waiting = if self.sweeps.count() == 0 { format!(" · waiting for TTL{}", d.trig_line + 1) } else { String::new() };
            p.text(
                egui::pos2(plot.max.x - 4.0, plot.max.y - 4.0),
                egui::Align2::RIGHT_BOTTOM,
                format!("TTL{} ↑ · {} / {} sweeps · {:.0} µV/row{}{}", d.trig_line + 1, self.sweeps.filled(nslots), nslots, d.uv_per_row, if car_on { " · CAR" } else { "" }, waiting),
                egui::FontId::monospace(11.0),
                dim,
            );
        } else {
            self.ttl.draw(&p, &TtlGeom { plot, strip, label_x: rect.min.x + 4.0, ppp, cursor: cursor_col }, &ttl_lines);
            let x = plot.min.x + cursor_col as f32 / ppp;
            p.line_segment([egui::pos2(x, plot.min.y), egui::pos2(x, plot.max.y)], Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 40)));
            p.text(egui::pos2(plot.max.x - 4.0, plot.max.y - 4.0), egui::Align2::RIGHT_BOTTOM, format!("{:.2} s · {:.0} µV/row{}{}", d.window_s, d.uv_per_row, if car_on { " · CAR" } else { "" }, if analog_visible { format!(" · ADC {:.2} V/row · AUX 2.5 V/row", d.adc_v_per_row) } else { String::new() }), egui::FontId::monospace(11.0), dim);
        }
    }

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
        if self.colors_dirty
            && let Some(l) = self.layout()
        {
            self.colors_dirty = false;
            self.trace_colors = Some(Arc::new(self.channel_colors(&l, self.cfg.display.per_row.clamp(1, 8) as usize)));
            self.spike_colors = Some(Arc::new(self.channel_colors(&l, 1)));
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

        self.car_palette(ui.ctx());
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
