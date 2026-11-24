use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Volts per bit for RHD2000 amplifier channels, expressed in µV.
pub const ELECTRODE_UV_PER_BIT: f32 = 0.195;
/// Volts per bit for RHD2000 auxiliary inputs (V).
pub const AUX_V_PER_BIT: f32 = 0.0000374;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelKind {
    Electrode,
    Aux,
    Adc,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelInfo {
    pub name: String,
    pub kind: ChannelKind,
    /// Physical units per stored int16 LSB (µV for electrodes, V otherwise).
    pub bit_volts: f32,
    pub units: String,
    /// Index into `StreamLayout::headstages`, if any.
    pub headstage: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeadstageInfo {
    pub name: String,
    /// Headstage slot 0..8 (A1, A2, B1, … D2); indexes `BoardSettings::rhd2132_16ch`.
    #[serde(default)]
    pub slot: usize,
    pub port: usize,
    pub chip: String,
    pub num_channels: usize,
    pub first_channel: usize,
    pub has_bno: bool,
}

/// Low-rate side streams delivered alongside the main stream.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SideStreamInfo {
    pub name: String,
    pub sample_rate: f64,
    pub channel_names: Vec<String>,
    pub units: Vec<String>,
    /// Units per stored int16 LSB when recorded.
    pub bit_volts: Vec<f32>,
}

/// Describes the main continuous stream produced by a board.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamLayout {
    pub name: String,
    pub sample_rate: f64,
    pub channels: Vec<ChannelInfo>,
    pub headstages: Vec<HeadstageInfo>,
    pub ttl_lines: usize,
    pub side_streams: Vec<SideStreamInfo>,
    /// Hardware clock rate used for `Block::timestamps` (Hz). 0 if the source has none.
    pub hw_clock_hz: f64,
}

impl StreamLayout {
    pub fn num_channels(&self) -> usize {
        self.channels.len()
    }
    pub fn num_electrodes(&self) -> usize {
        self.channels.iter().filter(|c| c.kind == ChannelKind::Electrode).count()
    }
}

#[derive(Clone, Debug)]
pub struct SideSample {
    pub stream: usize,
    pub sample_number: i64,
    pub timestamp: f64,
    pub values: Vec<f32>,
}

/// A contiguous chunk of the main stream. `data` is interleaved sample-major
/// (`data[s * nch + ch]`), int16 in the same representation Open Ephys writes
/// to disk, so recording is a plain memcpy.
#[derive(Clone, Debug, Default)]
pub struct Block {
    pub first_sample: i64,
    pub num_channels: usize,
    pub data: Vec<i16>,
    /// Hardware timestamps in seconds (board clock), one per sample.
    pub timestamps: Vec<f64>,
    /// Digital input word, one per sample.
    pub ttl: Vec<u16>,
    pub side: Vec<SideSample>,
    /// Host time at which the block was completed by the acquisition thread.
    pub acquired_at: Option<std::time::Instant>,
}

impl Block {
    pub fn with_capacity(nch: usize, samples: usize) -> Self {
        Self {
            first_sample: 0,
            num_channels: nch,
            data: Vec::with_capacity(nch * samples),
            timestamps: Vec::with_capacity(samples),
            ttl: Vec::with_capacity(samples),
            side: Vec::new(),
            acquired_at: None,
        }
    }
    pub fn len(&self) -> usize {
        self.timestamps.len()
    }
    pub fn is_empty(&self) -> bool {
        self.timestamps.is_empty()
    }
    pub fn clear(&mut self) {
        self.data.clear();
        self.timestamps.clear();
        self.ttl.clear();
        self.side.clear();
    }
    pub fn sample(&self, s: usize) -> &[i16] {
        &self.data[s * self.num_channels..(s + 1) * self.num_channels]
    }
}

/// Lock-free health counters updated by the acquisition thread.
#[derive(Default, Debug)]
pub struct Health {
    pub frames: AtomicU64,
    pub samples: AtomicU64,
    /// Samples missing according to the hardware sample counter.
    pub dropped_samples: AtomicU64,
    pub discontinuities: AtomicU64,
    pub bad_frames: AtomicU64,
    /// Board DRAM buffer usage, in 1/1000 percent.
    pub hw_buffer_milli_pct: AtomicU64,
    pub read_errors: AtomicU64,
}

impl Health {
    pub fn snapshot(&self) -> HealthSnapshot {
        HealthSnapshot {
            frames: self.frames.load(Ordering::Relaxed),
            samples: self.samples.load(Ordering::Relaxed),
            dropped_samples: self.dropped_samples.load(Ordering::Relaxed),
            discontinuities: self.discontinuities.load(Ordering::Relaxed),
            bad_frames: self.bad_frames.load(Ordering::Relaxed),
            hw_buffer_pct: self.hw_buffer_milli_pct.load(Ordering::Relaxed) as f32 / 1000.0,
            read_errors: self.read_errors.load(Ordering::Relaxed),
        }
    }
    pub fn reset(&self) {
        for a in [&self.frames, &self.samples, &self.dropped_samples, &self.discontinuities, &self.bad_frames, &self.hw_buffer_milli_pct, &self.read_errors] {
            a.store(0, Ordering::Relaxed);
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HealthSnapshot {
    pub frames: u64,
    pub samples: u64,
    pub dropped_samples: u64,
    pub discontinuities: u64,
    pub bad_frames: u64,
    pub hw_buffer_pct: f32,
    pub read_errors: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BoardSettings {
    pub sample_rate: u32,
    pub lower_bandwidth: f64,
    pub upper_bandwidth: f64,
    pub dsp_enabled: bool,
    pub dsp_cutoff: f64,
    pub acquire_aux: bool,
    pub acquire_adc: bool,
    /// Per-port cable delay adjustment (0 or +1 step), as in the stock plugin.
    pub cable_delay_adjust: [i32; 4],
    pub cable_length_m: [f32; 4],
    pub fast_settle: bool,
    pub ttl_output_mode: bool,
    pub dac_hpf_enabled: bool,
    pub dac_hpf_hz: f64,
    pub fast_ttl_settle: Option<u8>,
    pub noise_slicer: i32,
    /// Board status LEDs. Off by default: their driver puts a ~1.06 kHz tone on the analog
    /// inputs (floating ADC noise rose ~9× with them on; neural channels are unaffected).
    pub leds: bool,
    /// Per headstage slot (A1, A2, B1, … D2): treat an RHD2132 as a 16-channel headstage,
    /// recording chip channels 8–23 only (stock "half channels" mode, RHD2132_16CH_OFFSET).
    /// Parsing and layout only, no register writes.
    pub rhd2132_16ch: [bool; 8],
    pub clock_divider: u32,
    /// liboni block read size in bytes. Smaller = lower latency, more USB transactions.
    pub usb_block_bytes: u32,
    /// Samples per block handed to the pipeline.
    pub block_samples: usize,
}

impl Default for BoardSettings {
    fn default() -> Self {
        Self {
            sample_rate: 30000,
            lower_bandwidth: 1.0,
            upper_bandwidth: 7500.0,
            dsp_enabled: true,
            dsp_cutoff: 0.5,
            acquire_aux: false,
            acquire_adc: false,
            cable_delay_adjust: [0; 4],
            cable_length_m: [0.914; 4],
            fast_settle: false,
            ttl_output_mode: false,
            dac_hpf_enabled: false,
            dac_hpf_hz: 300.0,
            fast_ttl_settle: None,
            noise_slicer: 0,
            leds: false,
            rhd2132_16ch: [false; 8],
            clock_divider: 1,
            // 8 KiB: about 0.75 ms mean delivery delay at 46 ch (24 KiB, stock's, is about 2 ms);
            // measured on the rig 2026-11-24, see CLAUDE.md.
            usb_block_bytes: 8 * 1024,
            block_samples: 32,
        }
    }
}

/// Commands that may be applied while acquiring (executed by the acquisition thread).
#[derive(Clone, Debug)]
pub enum LiveCommand {
    SetTtlOut(u16),
    /// Route an electrode channel (global electrode index) to a board DAC, or disable with None.
    DacRoute { dac: usize, electrode: Option<usize>, threshold_uv: f32 },
    SetLeds(bool),
    SetDacHpf { enabled: bool, hz: f64 },
    SetNoiseSlicer(i32),
    /// TTL outputs 1–8 driven by the DAC threshold comparators (stock "DAC TTL") instead of
    /// software. Rhythm MODE register, TTL-out-mode bit.
    SetTtlMode(bool),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BoardInfo {
    pub kind: String,
    pub description: Vec<(String, String)>,
}

pub type SharedHealth = Arc<Health>;

pub trait Board: Send {
    fn info(&self) -> BoardInfo;
    fn layout(&self) -> StreamLayout;
    fn settings(&self) -> &BoardSettings;
    /// Apply settings while idle (may reprogram the chips).
    fn apply_settings(&mut self, s: &BoardSettings) -> anyhow::Result<()>;
    fn available_sample_rates(&self) -> Vec<u32>;
    /// Detect headstages and tune cable delays.
    fn scan_ports(&mut self) -> anyhow::Result<()>;
    fn start(&mut self, health: SharedHealth) -> anyhow::Result<()>;
    /// Blocks until `blk` holds `settings().block_samples` samples. Never allocates
    /// once `blk` has warmed up.
    fn read_block(&mut self, blk: &mut Block) -> anyhow::Result<()>;
    fn live(&mut self, cmd: &LiveCommand) -> anyhow::Result<()>;
    fn stop(&mut self) -> anyhow::Result<()>;
}
