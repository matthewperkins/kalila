use engine::DspSettings;
use engine::audio::AudioSettings;
use engine::rhythm::BoardSettings;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Display filters of a trace view (display only; recordings stay raw): raw, or the view's
/// bandpass (`TraceViewSettings::band_low_hz`/`band_high_hz`).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ViewFilter {
    Raw,
    #[serde(alias = "Spikes", alias = "Lfp", alias = "Hp")]
    Band,
}

impl ViewFilter {
    pub const ALL: [ViewFilter; 2] = [ViewFilter::Raw, ViewFilter::Band];
    /// Index into per-filter tables (and the shader's preset number).
    pub fn index(self) -> usize {
        match self {
            ViewFilter::Raw => 0,
            ViewFilter::Band => 1,
        }
    }
    /// One letter, for the per-row chips.
    pub fn letter(self) -> &'static str {
        match self {
            ViewFilter::Raw => "R",
            ViewFilter::Band => "B",
        }
    }
}

/// "300–6k", "10 Hz–" (no low-pass) or "–300" (no high-pass); 0 = that corner is off.
pub fn band_label(low_hz: f32, high_hz: f32) -> String {
    let hz = |f: f32| {
        if f >= 1000.0 {
            format!("{}k", (f / 1000.0 * 10.0).round() / 10.0)
        } else if f >= 10.0 {
            format!("{f:.0}")
        } else {
            format!("{f:.1}")
        }
    };
    match (low_hz > 0.0, high_hz > 0.0) {
        (true, true) => format!("{}–{}", hz(low_hz), hz(high_hz)),
        (true, false) => format!("{} Hz–", hz(low_hz)),
        (false, true) => format!("–{}", hz(high_hz)),
        (false, false) => "Band (off)".into(),
    }
}

/// Global display settings (spike tiles, frame rate) and the CAR used by spike detection and
/// audio. Each trace window has its own `TraceViewSettings`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DisplaySettings {
    pub spike_uv_range: f32,
    pub spike_tau_s: f32,
    pub spike_grid_cols: u32,
    /// First spike-tile row shown (scrolled with the mouse wheel).
    pub spike_first_row: usize,
    pub max_fps: f32,
    /// Common average reference for spike detection and audio.
    pub car: CarSettings,
}

/// Common average reference for spike detection and the audio monitor (never recording).
/// Channels are global electrode indices.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct CarSettings {
    pub spikes: bool,
    pub audio: bool,
    /// Electrodes averaged into the reference.
    pub reference: Vec<usize>,
    /// Electrodes the reference is subtracted from.
    pub subtract: Vec<usize>,
}

impl Default for DisplaySettings {
    fn default() -> Self {
        Self {
            spike_uv_range: 250.0,
            spike_tau_s: 3.0,
            spike_grid_cols: 8,
            spike_first_row: 0,
            max_fps: 144.0,
            car: CarSettings::default(),
        }
    }
}

/// A trace view's own display-only CAR (300–6k view). Global electrode indices.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct ViewCar {
    pub enabled: bool,
    pub reference: Vec<usize>,
    pub subtract: Vec<usize>,
}

/// Sweep trigger source.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub enum TrigSource {
    #[default]
    Ttl,
    /// Level crossing on one electrode (spike band).
    Spike,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub enum Slope {
    /// Falling through a negative level (the usual extracellular spike).
    #[default]
    Neg,
    Pos,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash, Default)]
pub enum TrigAlign {
    /// t = 0 at the extreme within 0.5 ms of the crossing (as the spike detector aligns).
    #[default]
    Trough,
    Crossing,
}

/// One trace window: which channels it draws and how.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct TraceViewSettings {
    /// Stable id (GPU state, window identity).
    pub id: u32,
    pub name: String,
    /// Window shown.
    pub open: bool,
    /// Headstages drawn, by name ("A1", "B1", …); empty = all.
    pub headstages: Vec<String>,
    pub aux: bool,
    pub adc: bool,
    /// Filter of every channel without its own entry in `channel_filter`.
    pub filter: ViewFilter,
    /// Per-channel filters, keyed by `"<headstage>:<index in headstage>"` (so they survive
    /// regrouping rows and channel-count changes on other ports). Empty = all use `filter`.
    pub channel_filter: std::collections::BTreeMap<String, ViewFilter>,
    /// Per-channel µV/row overrides (Ctrl+wheel over a row), keyed like `channel_filter`.
    /// Empty = every electrode uses its filter's scale (`uv_by_filter`).
    pub channel_uv: std::collections::BTreeMap<String, f32>,
    /// Corners of the Band filter (Hz; 4th-order Butterworth each side); 0 = that side off.
    pub band_low_hz: f32,
    pub band_high_hz: f32,
    pub window_s: f32,
    /// µV/row for a filter without its own entry in `uv_by_filter`.
    pub uv_per_row: f32,
    /// µV/row per filter (by `ViewFilter::index`); 0 = use `uv_per_row`.
    pub uv_by_filter: [f32; 2],
    pub rows: usize,
    pub first_row: usize,
    pub thickness: f32,
    /// Vertical range of one ADC trace row, in volts (rows are centred on 0 V).
    pub adc_v_per_row: f32,
    /// Shade the traces while a TTL input is high, plus a per-line strip at the bottom.
    pub ttl_overlay: bool,
    /// TTL lines left out of the overlay: bit n = TTL n+1 (e.g. a camera trigger).
    pub ttl_hidden: u16,
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
    /// What starts a sweep: a TTL rising edge or a level crossing on one channel.
    pub trig_source: TrigSource,
    /// Slope-trigger channel, as a `channel_filter` key ("A1:5"); "" = none yet.
    pub trig_channel: String,
    pub trig_slope: Slope,
    /// Fixed crossing level in µV (sign follows the slope); 0 = the spike detector's threshold.
    pub trig_level_uv: f32,
    pub trig_align: TrigAlign,
    /// Extra holdoff after each trigger (ms), on top of the 1 ms dead time.
    pub trig_holdoff_ms: f32,
    /// Window discriminator: reject crossings whose trough goes beyond `trig_level2_uv`.
    pub trig_window: bool,
    pub trig_level2_uv: f32,
    /// Sweep window of the slope trigger (ms; finer than the TTL window).
    pub spk_pre_ms: f32,
    pub spk_post_ms: f32,
    pub car: ViewCar,
    /// Control bar at the top of the window expanded.
    pub controls: bool,
}

impl Default for TraceViewSettings {
    fn default() -> Self {
        Self {
            id: 0,
            name: "Traces".into(),
            open: true,
            headstages: Vec::new(),
            aux: true,
            adc: true,
            filter: ViewFilter::Band,
            channel_filter: Default::default(),
            channel_uv: Default::default(),
            band_low_hz: 300.0,
            band_high_hz: 6000.0,
            window_s: 2.0,
            uv_per_row: 250.0,
            uv_by_filter: [0.0; 2],
            rows: 32,
            first_row: 0,
            thickness: 1.0,
            adc_v_per_row: 5.0,
            ttl_overlay: true,
            ttl_hidden: 0,
            per_row: 1,
            triggered: false,
            trig_line: 1,
            trig_pre_ms: 10.0,
            trig_post_ms: 40.0,
            trig_sweeps: 20,
            trig_source: TrigSource::Ttl,
            trig_channel: String::new(),
            trig_slope: Slope::Neg,
            trig_level_uv: 0.0,
            trig_align: TrigAlign::Trough,
            trig_holdoff_ms: 0.0,
            trig_window: false,
            trig_level2_uv: 300.0,
            spk_pre_ms: 1.0,
            spk_post_ms: 2.0,
            car: ViewCar::default(),
            controls: true,
        }
    }
}

impl TraceViewSettings {
    /// µV/row of electrodes drawn with filter `f`.
    pub fn uv(&self, f: ViewFilter) -> f32 {
        let v = self.uv_by_filter[f.index()];
        if v > 0.0 { v } else { self.uv_per_row }
    }
    pub fn set_uv(&mut self, f: ViewFilter, v: f32) {
        self.uv_by_filter[f.index()] = v.clamp(10.0, 5000.0);
    }
    /// µV/row of the electrode with key `key` drawn with filter `f`: its own override, else
    /// the filter's scale.
    pub fn uv_of(&self, key: Option<&str>, f: ViewFilter) -> f32 {
        key.and_then(|k| self.channel_uv.get(k))
            .copied()
            .unwrap_or_else(|| self.uv(f))
    }
    /// Filter of the electrode with key `key` (see `channel_filter`).
    pub fn filter_of(&self, key: Option<&str>) -> ViewFilter {
        key.and_then(|k| self.channel_filter.get(k))
            .copied()
            .unwrap_or(self.filter)
    }
    /// The Band filter as a `BandSpec` (corners clamped to what the GPU bank handles).
    pub fn band(&self) -> engine::dsp::BandSpec {
        engine::dsp::BandSpec {
            low_hz: if self.band_low_hz > 0.0 {
                self.band_low_hz.clamp(0.1, 5000.0) as f64
            } else {
                0.0
            },
            high_hz: if self.band_high_hz > 0.0 {
                self.band_high_hz.clamp(10.0, 15000.0) as f64
            } else {
                0.0
            },
            order: 4,
        }
    }
    pub fn band_label(&self) -> String {
        band_label(self.band_low_hz, self.band_high_hz)
    }
    /// Label of a filter as shown in the toolbar.
    pub fn filter_label(&self, f: ViewFilter) -> String {
        match f {
            ViewFilter::Raw => "Raw".into(),
            ViewFilter::Band => self.band_label(),
        }
    }
}

/// Configs from before the Band filter (2026-11-10) had four presets per view (Raw, Spikes,
/// Lfp, Hp with `hp_hz`) and a 4-entry `uv_by_filter`. Each view keeps one band: the preset
/// most of its rows used (Spikes 300–6k, Lfp 1–300, Hp `hp_hz`–), and that preset's µV/row.
fn migrate_view_presets(view: &mut serde_json::Value) {
    use serde_json::{Value, json};
    let Some(o) = view.as_object_mut() else { return };
    let preset = |v: &Value| {
        v.as_str()
            .filter(|s| ["Spikes", "Lfp", "Hp"].contains(s))
            .map(str::to_string)
    };
    let mut votes: Vec<(String, usize)> = Vec::new();
    let mut vote = |p: Option<String>, w: usize| {
        if let Some(p) = p {
            match votes.iter_mut().find(|(q, _)| *q == p) {
                Some(e) => e.1 += w,
                None => votes.push((p, w)),
            }
        }
    };
    vote(o.get("filter").and_then(preset), 1);
    if let Some(m) = o.get("channel_filter").and_then(Value::as_object) {
        for v in m.values() {
            vote(preset(v), 1);
        }
    }
    let hp = o.get("hp_hz").and_then(Value::as_f64).unwrap_or(10.0);
    let (low, high, idx) = match votes.iter().max_by_key(|(_, n)| *n).map(|(p, _)| p.as_str()) {
        Some("Lfp") => (1.0, 300.0, 2),
        Some("Hp") => (hp, 0.0, 3),
        _ => (300.0, 6000.0, 1),
    };
    if !votes.is_empty() {
        o.insert("band_low_hz".into(), json!(low));
        o.insert("band_high_hz".into(), json!(high));
    }
    if let Some(uv) = o.get("uv_by_filter").and_then(Value::as_array).filter(|a| a.len() == 4) {
        let uv = json!([uv[0], uv[idx]]);
        o.insert("uv_by_filter".into(), uv);
    }
}

/// Typed notes: presets on keys 1–9 and drafts written ahead (stamped one by one with `).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct NotesSettings {
    pub presets: Vec<String>,
    pub drafts: Vec<String>,
}

/// One board DAC output: which electrode it plays and its comparator threshold.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct DacOut {
    /// Global electrode index, or None for off. Follows its electrode across layout
    /// changes (`Config::electrode_keys`).
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
        Self {
            dacs: [DacOut::default(); 8],
            dac1_follows_audio: false,
            ttl_threshold_mode: false,
            dac_hpf_enabled: false,
            dac_hpf_hz: 300.0,
            noise_slicer: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub board: BoardSettings,
    /// Note drafts and presets.
    pub notes: NotesSettings,
    pub audio: AudioSettings,
    pub dsp: DspSettings,
    pub display: DisplaySettings,
    /// Trace windows.
    pub views: Vec<TraceViewSettings>,
    pub outputs: OutputSettings,
    pub record_root: PathBuf,
    /// "auto" (no vsync, wgpu picks immediate/mailbox/fifo), "vsync", "mailbox", "immediate", "fifo".
    pub present_mode: String,
    pub open_hardware_on_start: bool,
    pub sim_headstages: Vec<usize>,
    /// RAM for the scroll-back overview (min/max pyramids) of the recording, all windows
    /// together, in GB. Over it, the oldest fine detail is released (short old spans are then
    /// read from the file).
    pub review_ram_gb: f32,
    /// The electrodes that the settings stored as global electrode indices (DAC routes,
    /// CAR sets, audio channels) refer to: `electrode_keys[i]` is electrode `i`'s key
    /// ("A1:5"). When the layout changes, those settings are moved to follow their
    /// electrodes (`ElectrodeMap`). Empty in configs from before 2026-11-13.
    pub electrode_keys: Vec<String>,
    /// Which one-time changes of defaults this config has been through (`parse`). 1: the
    /// USB block default went from 24 KiB to 8 KiB (2026-11-24).
    pub defaults_rev: u32,
    /// Set by `load` when the file couldn't be read as a config (shown in the GUI).
    #[serde(skip)]
    pub load_error: Option<String>,
    /// Set by `parse` when it changed a setting of the saved config (shown once as a toast).
    #[serde(skip)]
    pub load_notice: Option<String>,
}

/// The latest `Config::defaults_rev`.
const DEFAULTS_REV: u32 = 1;

/// Keys ("A1:5": headstage name and index within it) of a layout's electrodes, by global
/// electrode index. Electrodes come first in the channel list, so the electrode index is
/// also the channel index.
pub fn electrode_keys(l: &engine::rhythm::StreamLayout) -> Vec<String> {
    let mut per_hs = vec![0usize; l.headstages.len()];
    l.channels
        .iter()
        .filter(|c| c.kind == engine::rhythm::ChannelKind::Electrode)
        .map(|c| {
            let hs = c.headstage.unwrap_or(0);
            let k = per_hs.get(hs).copied().unwrap_or(0);
            if let Some(n) = per_hs.get_mut(hs) {
                *n += 1;
            }
            format!("{}:{k}", l.headstages.get(hs).map_or("?", |h| h.name.as_str()))
        })
        .collect()
}

/// Moves global electrode indices from one layout to another by electrode key, so a DAC
/// route or CAR set keeps its electrode when, say, a 16-ch RHD2132 toggle or a rescan
/// renumbers the electrodes after it.
pub struct ElectrodeMap {
    to: Vec<Option<usize>>,
    /// Indices that pointed at another electrode, and that were dropped (headstage gone).
    pub moved: usize,
    pub dropped: usize,
}

impl ElectrodeMap {
    pub fn new(old: &[String], new: &[String]) -> Self {
        let to = old.iter().map(|k| new.iter().position(|n| n == k)).collect();
        Self {
            to,
            moved: 0,
            dropped: 0,
        }
    }
    /// Old index → new index; None if that electrode is gone (or `i` was out of range).
    pub fn one(&mut self, i: usize) -> Option<usize> {
        let r = self.to.get(i).copied().flatten();
        match r {
            None => self.dropped += 1,
            Some(n) if n != i => self.moved += 1,
            _ => {}
        }
        r
    }
    pub fn opt(&mut self, i: &mut Option<usize>) {
        if let Some(v) = *i {
            *i = self.one(v);
        }
    }
    pub fn set(&mut self, v: &mut Vec<usize>) {
        *v = v.iter().filter_map(|&i| self.one(i)).collect();
    }
    pub fn changed(&self) -> bool {
        self.moved + self.dropped > 0
    }
}

impl Default for Config {
    fn default() -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            board: BoardSettings::default(),
            notes: NotesSettings::default(),
            audio: AudioSettings::default(),
            dsp: DspSettings::default(),
            display: DisplaySettings::default(),
            views: vec![TraceViewSettings::default()],
            outputs: OutputSettings::default(),
            record_root: home.join("oe-recordings"),
            present_mode: "auto".into(),
            open_hardware_on_start: true,
            sim_headstages: vec![32, 32],
            review_ram_gb: engine::pyramid::DEFAULT_BUDGET as f32 / 1e9,
            electrode_keys: Vec::new(),
            defaults_rev: DEFAULTS_REV,
            load_error: None,
            load_notice: None,
        }
    }
}

impl Config {
    fn config_dir() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
    }
    pub fn path() -> PathBuf {
        Self::config_dir().join("kalila").join("config.json")
    }
    /// The program was called oers until 2026-11-13: with no kalila config yet, start from a
    /// copy of the oers one (left in place).
    fn adopt_old_config() {
        let (new, old) = (Self::path(), Self::config_dir().join("oers").join("config.json"));
        if new.exists() || !old.exists() {
            return;
        }
        match new
            .parent()
            .map(std::fs::create_dir_all)
            .transpose()
            .and_then(|_| std::fs::copy(&old, &new))
        {
            Ok(_) => log::info!(
                "copied the settings from {} (the program's former name) to {}",
                old.display(),
                new.display()
            ),
            Err(e) => log::warn!("could not copy the settings from {}: {e}", old.display()),
        }
    }
    pub fn load() -> Self {
        Self::adopt_old_config();
        match std::fs::read_to_string(Self::path()) {
            Ok(s) => Self::parse(&s).unwrap_or_else(|e| {
                // Keep the file: saving the defaults over it would lose every setting (16-ch
                // mode, DAC routes, …) for good.
                let path = Self::path();
                let bad = path.with_extension(format!("json.bad-{}", chrono::Local::now().format("%Y-%m-%d_%H-%M-%S")));
                let kept = match std::fs::rename(&path, &bad) {
                    Ok(()) => format!("it was kept as {}", bad.display()),
                    Err(re) => format!("it could not be set aside ({re})"),
                };
                let msg = format!("The config file {} could not be read ({e}); using defaults, and {kept}. Check the board settings before recording.", path.display());
                log::error!("{msg}");
                Self { load_error: Some(msg), ..Self::default() }
            }),
            Err(_) => Self::default(),
        }
    }

    /// Parse a saved config. Configs from before trace windows had the trace settings in
    /// `display` (same field names, the display CAR in `display.car`): they become view 1.
    fn parse(s: &str) -> serde_json::Result<Self> {
        let mut v: serde_json::Value = serde_json::from_str(s)?;
        if let Some(views) = v.get_mut("views").and_then(serde_json::Value::as_array_mut) {
            views.iter_mut().for_each(migrate_view_presets);
        } else if let Some(d) = v.get_mut("display") {
            migrate_view_presets(d);
        }
        let mut cfg: Self = serde_json::from_value(v.clone())?;
        if v.get("views").is_none() {
            let mut view: TraceViewSettings = v
                .get("display")
                .cloned()
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default();
            view.id = 0;
            view.name = "Traces".into();
            view.open = true;
            cfg.views = vec![view];
        }
        if cfg.views.is_empty() {
            cfg.views.push(TraceViewSettings::default());
        }
        // A missing field means a config from before the field existed (serde would fill in
        // the current default, DEFAULTS_REV).
        let rev = v.get("defaults_rev").and_then(serde_json::Value::as_u64).unwrap_or(0) as u32;
        if rev < 1 && cfg.board.usb_block_bytes == 24 * 1024 {
            // 24 KiB was the untouched default until now; another value was chosen and stays.
            cfg.board.usb_block_bytes = 8 * 1024;
            cfg.load_notice = Some(
                "USB block set to the new default, 8 KiB (was 24 KiB): about 0.75 ms delivery delay instead of 2 ms"
                    .into(),
            );
        }
        cfg.defaults_rev = DEFAULTS_REV;
        Ok(cfg)
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

#[cfg(test)]
mod tests {

    #[test]
    fn the_old_24_kib_usb_block_default_moves_to_8_kib_once() {
        let old = |b: u32| format!(r#"{{"board": {{"usb_block_bytes": {b}}}}}"#);
        // Saved before the change with the old default: moved, and the user is told.
        let c = Config::parse(&old(24576)).unwrap();
        assert_eq!((c.board.usb_block_bytes, c.defaults_rev), (8192, DEFAULTS_REV));
        assert!(c.load_notice.is_some());
        // Another value was the user's choice: kept, silently.
        let c = Config::parse(&old(16384)).unwrap();
        assert_eq!(c.board.usb_block_bytes, 16384);
        assert!(c.load_notice.is_none());
        // 24 KiB chosen after the change (saved with defaults_rev 1): kept.
        let c = Config::parse(r#"{"board": {"usb_block_bytes": 24576}, "defaults_rev": 1}"#).unwrap();
        assert_eq!(c.board.usb_block_bytes, 24576);
        assert!(c.load_notice.is_none());
        // A fresh config has the new default and needs no migration.
        assert_eq!(Config::default().board.usb_block_bytes, 8192);
        assert_eq!(Config::default().defaults_rev, DEFAULTS_REV);
    }

    /// The four board-level outputs are stored twice: in `outputs` (the panel) and in `board`
    /// (what the board was last programmed with; `App::sync_outputs` mirrors them there while
    /// acquiring). Loading keeps both as saved: nothing reconciles them at load. At run time
    /// `board` is what the board gets at open and at Apply, `outputs` from the first
    /// `sync_outputs` of each acquisition on (package 3e's analysis).
    #[test]
    fn board_and_outputs_keep_their_own_output_values_at_load() {
        let s = r#"{
            "board": {"ttl_output_mode": true, "dac_hpf_enabled": true, "dac_hpf_hz": 150.0, "noise_slicer": 20},
            "outputs": {"ttl_threshold_mode": false, "dac_hpf_enabled": false, "dac_hpf_hz": 300.0, "noise_slicer": 5}
        }"#;
        let c = Config::parse(s).unwrap();
        let b = &c.board;
        assert_eq!(
            (b.ttl_output_mode, b.dac_hpf_enabled, b.dac_hpf_hz, b.noise_slicer),
            (true, true, 150.0, 20)
        );
        let o = &c.outputs;
        assert_eq!(
            (o.ttl_threshold_mode, o.dac_hpf_enabled, o.dac_hpf_hz, o.noise_slicer),
            (false, false, 300.0, 5)
        );
        // A board section without them (or a config without one) gets the board defaults,
        // which are the panel's defaults.
        let c = Config::parse(r#"{"board": {"sample_rate": 20000}}"#).unwrap();
        let (b, o) = (&c.board, &c.outputs);
        assert_eq!(
            (b.ttl_output_mode, b.dac_hpf_enabled, b.dac_hpf_hz, b.noise_slicer),
            (o.ttl_threshold_mode, o.dac_hpf_enabled, o.dac_hpf_hz, o.noise_slicer)
        );
        assert_eq!((b.sample_rate, b.noise_slicer, b.dac_hpf_hz), (20000, 0, 300.0));
    }

    #[test]
    fn electrode_settings_follow_their_electrodes_across_a_layout_change() {
        use engine::rhythm::{Board, SimBoard};
        // A1 a 32-channel headstage, then switched to 16 channels; A2 16 channels after it.
        let old = electrode_keys(&SimBoard::new(vec![32, 16]).layout());
        let new = electrode_keys(&SimBoard::new(vec![16, 16]).layout());
        assert_eq!((old[40].as_str(), new[24].as_str()), ("A2:8", "A2:8"));
        let mut m = ElectrodeMap::new(&old, &new);
        let mut dac = Some(40);
        m.opt(&mut dac);
        assert_eq!(dac, Some(24), "the DAC keeps A2:8");
        let mut car: Vec<usize> = (32..48).collect();
        m.set(&mut car);
        assert_eq!(car, (16..32).collect::<Vec<_>>(), "A2's CAR set stays A2's");
        let mut audio = vec![3, 20, 47];
        m.set(&mut audio);
        assert_eq!(audio, vec![3, 31], "A1:20 no longer exists; A1:3 is unchanged");
        assert_eq!((m.moved, m.dropped), (18, 1));
        // Same layout: nothing changes.
        let mut same = ElectrodeMap::new(&old, &old);
        let mut v = vec![0, 40];
        same.set(&mut v);
        assert_eq!(v, vec![0, 40]);
        assert!(!same.changed());
    }

    #[test]
    fn per_filter_scale_and_channel_filter() {
        let mut v = TraceViewSettings {
            uv_per_row: 300.0,
            ..Default::default()
        };
        assert_eq!(v.uv(ViewFilter::Raw), 300.0, "unset scales fall back to uv_per_row");
        v.set_uv(ViewFilter::Raw, 1200.0);
        assert_eq!((v.uv(ViewFilter::Raw), v.uv(ViewFilter::Band)), (1200.0, 300.0));
        v.filter = ViewFilter::Raw;
        v.channel_filter.insert("B1:3".into(), ViewFilter::Band);
        assert_eq!(v.filter_of(Some("B1:3")), ViewFilter::Band);
        assert_eq!(v.filter_of(Some("B1:4")), ViewFilter::Raw);
        assert_eq!(v.filter_of(None), ViewFilter::Raw);
        // A per-row override beats the filter's scale, for that channel only.
        v.channel_uv.insert("B1:3".into(), 80.0);
        assert_eq!(
            (
                v.uv_of(Some("B1:3"), ViewFilter::Band),
                v.uv_of(Some("B1:4"), ViewFilter::Band)
            ),
            (80.0, 300.0)
        );
        v.ttl_hidden = 0b1;
        let back: TraceViewSettings = serde_json::from_str(&serde_json::to_string(&v).unwrap()).unwrap();
        assert_eq!(back, v);
        // Configs from before these fields load with no overrides and every TTL line shown.
        let old: TraceViewSettings = serde_json::from_str(r#"{"filter":"Raw"}"#).unwrap();
        assert!(old.channel_uv.is_empty() && old.ttl_hidden == 0);
        assert_eq!(band_label(300.0, 6000.0), "300–6k");
        assert_eq!(band_label(10.0, 0.0), "10 Hz–");
        assert_eq!(band_label(0.5, 300.0), "0.5–300");
    }

    #[test]
    fn old_presets_become_the_band() {
        // A Port B window as saved before the Band filter: Myomatrix rows on HP 100 Hz, four
        // rows on 300–6k, the rest raw. HP wins; its µV/row (1000) becomes the band's.
        let old = r#"{"views": [{"filter": "Raw", "hp_hz": 100.0, "uv_by_filter": [4000.0, 150.0, 2000.0, 1000.0],
            "channel_filter": {"A2:0": "Spikes", "B1:0": "Hp", "B1:1": "Hp", "B1:8": "Raw"}},
            {"filter": "Spikes", "uv_by_filter": [0.0, 200.0, 0.0, 0.0]}]}"#;
        let c = Config::parse(old).unwrap();
        let v = &c.views[0];
        assert_eq!((v.band_low_hz, v.band_high_hz), (100.0, 0.0));
        assert_eq!((v.uv(ViewFilter::Raw), v.uv(ViewFilter::Band)), (4000.0, 1000.0));
        assert_eq!(v.filter_of(Some("A2:0")), ViewFilter::Band);
        assert_eq!(v.filter_of(Some("B1:8")), ViewFilter::Raw);
        let v = &c.views[1];
        assert_eq!(
            (v.filter, v.band_low_hz, v.band_high_hz, v.uv(ViewFilter::Band)),
            (ViewFilter::Band, 300.0, 6000.0, 200.0)
        );
        assert_eq!(v.band_label(), "300–6k");
    }
    use super::*;

    #[test]
    fn old_display_settings_become_the_first_view() {
        let old = r#"{"display": {"filter": "Lfp", "window_s": 7.0, "uv_per_row": 800.0, "per_row": 4,
            "car": {"enabled": true, "spikes": true, "audio": false, "reference": [1, 2], "subtract": [2]},
            "spike_grid_cols": 9}}"#;
        let c = Config::parse(old).unwrap();
        assert_eq!(c.views.len(), 1);
        let v = &c.views[0];
        assert_eq!(
            (v.filter, v.window_s, v.uv_per_row, v.per_row),
            (ViewFilter::Band, 7.0, 800.0, 4)
        );
        assert_eq!((v.band_low_hz, v.band_high_hz), (1.0, 300.0));
        assert_eq!(
            v.car,
            ViewCar {
                enabled: true,
                reference: vec![1, 2],
                subtract: vec![2]
            }
        );
        // The global (spikes/audio) CAR keeps its switches and sets.
        assert!(c.display.car.spikes && !c.display.car.audio);
        assert_eq!(c.display.car.reference, vec![1, 2]);
        assert_eq!(c.display.spike_grid_cols, 9);
    }

    #[test]
    fn new_configs_keep_their_views() {
        let mut c = Config::default();
        c.views.push(TraceViewSettings {
            id: 1,
            name: "Port B".into(),
            headstages: vec!["B1".into()],
            ..Default::default()
        });
        let back = Config::parse(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.views, c.views);
    }
}
