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
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self { filter: ViewFilter::Spikes, window_s: 2.0, uv_per_row: 250.0, rows: 32, first_row: 0, thickness: 1.0, spike_uv_range: 250.0, spike_tau_s: 3.0, spike_grid_cols: 8, split: 0.62, max_fps: 144.0 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub board: BoardSettings,
    pub audio: AudioSettings,
    pub dsp: DspSettings,
    pub display: DisplaySettings,
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
