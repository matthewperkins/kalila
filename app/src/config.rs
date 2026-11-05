use engine::DspSettings;
use engine::audio::AudioSettings;
use engine::rhythm::BoardSettings;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ViewFilter {
    Raw,
    Spikes,
    Lfp,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DisplaySettings {
    pub filter: ViewFilter,
    pub window_s: f32,
    pub uv_per_row: f32,
    pub rows: usize,
    pub first_row: usize,
    pub thickness: f32,
    pub spike_uv_range: f32,
    pub spike_tau_s: f32,
    pub spike_grid_cols: u32,
    pub split: f32,
    pub max_fps: f32,
    /// Vertical range of one ADC trace row, in volts (rows are centred on 0 V).
    pub adc_v_per_row: f32,
    /// Shade the traces while a TTL input is high, plus a per-line strip at the bottom.
    pub ttl_overlay: bool,
    /// Electrode channels drawn on top of each other in one row (1, 2, 4 or 8).
    pub per_row: u32,
    /// Triggered sweeps: each sweep starts at a rising edge of `trig_line`; the last
    /// `trig_sweeps` are overlaid (e.g. opto-tagging on the laser TTL).
    pub triggered: bool,
    /// TTL line, 0-based (1 = TTL2).
    pub trig_line: usize,
    pub trig_pre_ms: f32,
    pub trig_post_ms: f32,
    pub trig_sweeps: u32,
    /// Display-only common average reference, applied in the 300–6k view.
    pub car: CarSettings,
}

/// Common average reference (never recording). One pair of channel sets, switched on
/// separately for the display (300–6k view), spike detection and the audio monitor.
/// Channels are global electrode indices.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct CarSettings {
    /// Display (300–6k view).
    pub enabled: bool,
    pub spikes: bool,
    pub audio: bool,
    /// Electrodes averaged into the reference.
    pub reference: Vec<usize>,
    /// Electrodes the reference is subtracted from.
    pub subtract: Vec<usize>,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self { filter: ViewFilter::Spikes, window_s: 2.0, uv_per_row: 250.0, rows: 32, first_row: 0, thickness: 1.0, spike_uv_range: 250.0, spike_tau_s: 3.0, spike_grid_cols: 8, split: 0.62, max_fps: 144.0, adc_v_per_row: 5.0, ttl_overlay: true, per_row: 1, triggered: false, trig_line: 1, trig_pre_ms: 10.0, trig_post_ms: 40.0, trig_sweeps: 20, car: CarSettings::default() }
    }
}

/// One board DAC output: which electrode it plays and its comparator threshold.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct DacOut {
    /// Global electrode index, or None for off.
    pub electrode: Option<usize>,
    /// Comparator threshold in µV. ≥ 0: TTL high at or above it; < 0: at or below it. Drives
    /// TTL out n only in threshold mode (`BoardSettings::ttl_output_mode`).
    pub threshold_uv: f32,
}

/// DAC/TTL outputs. Sent to the board whenever acquisition starts and live on change (the
/// board-level ones are mirrored into `BoardSettings`, so they never need "Apply").
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OutputSettings {
    pub dacs: [DacOut; 8],
    /// DAC 1 (board audio L) plays the first channel on the audio monitor.
    pub dac1_follows_audio: bool,
    /// TTL out n driven by DAC n's threshold comparator (stock "DAC TTL").
    pub ttl_threshold_mode: bool,
    pub dac_hpf_enabled: bool,
    pub dac_hpf_hz: f64,
    /// Audio noise slicer on DAC 1–2, 0..127.
    pub noise_slicer: i32,
}

impl Default for OutputSettings {
    fn default() -> Self {
        Self { dacs: [DacOut::default(); 8], dac1_follows_audio: false, ttl_threshold_mode: false, dac_hpf_enabled: false, dac_hpf_hz: 300.0, noise_slicer: 0 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub board: BoardSettings,
    pub audio: AudioSettings,
    pub dsp: DspSettings,
    pub display: DisplaySettings,
    pub outputs: OutputSettings,
    pub record_root: PathBuf,
    /// "auto" (no vsync, wgpu picks immediate/mailbox/fifo), "vsync", "mailbox", "immediate", "fifo".
    pub present_mode: String,
    pub open_hardware_on_start: bool,
    pub sim_headstages: Vec<usize>,
}

impl Default for Config {
    fn default() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        Self {
            board: BoardSettings::default(),
            audio: AudioSettings::default(),
            dsp: DspSettings::default(),
            display: DisplaySettings::default(),
            outputs: OutputSettings::default(),
            record_root: home.join("oe-recordings"),
            present_mode: "auto".into(),
            open_hardware_on_start: true,
            sim_headstages: vec![32, 32],
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config"))).unwrap_or_else(|| PathBuf::from("."));
        base.join("oers").join("config.json")
    }
    pub fn load() -> Self {
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
                log::warn!("config parse error ({e}); using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }
    pub fn save(&self) {
        let p = Self::path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        // Write-then-rename so a crash never leaves a truncated config.
        let tmp = p.with_extension("json.tmp");
        if let Ok(s) = serde_json::to_string_pretty(self)
            && std::fs::write(&tmp, s).is_ok()
        {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}
