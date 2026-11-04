//! Spike-optimised audio monitor that preserves regional "texture".
//!
//! Two goals pull in different directions: *salience* (hear single units over the noise
//! floor) and *texture* (the character that makes e.g. globus pallidus — dense, tonic,
//! narrow fast spikes over thick multi-unit hash — sound unlike barrel cortex — sparse,
//! bursty, broader spikes with state-dependent background). Texture lives in:
//!   * firing rate, regularity and bursting (temporal envelope),
//!   * spike waveform shape (timbre: narrow spikes carry energy up to 5–7 kHz),
//!   * the amplitude distribution, including the sub-threshold multi-unit hash,
//!   * slow modulation of all of the above with brain state.
//!
//! The chain is therefore built so that every stage is either *linear and time-invariant*
//! or *static with a held calibration*, never a continuously adapting AGC:
//!
//! 1. **Band-limit** 300 Hz – 7 kHz (Butterworth). Removes LFP/hum/movement; keeps the
//!    whole spike band. Linear.
//! 2. **Held gain.** In `NoiseReferenced` mode σ (robust, spike-insensitive) is measured for
//!    ~3 s when channels are chosen or on "recalibrate", then *frozen*. Equalises electrodes
//!    of different impedance without tracking firing-rate or state changes. `Absolute` mode
//!    uses a fixed µV→FS scale, preserving true loudness differences between sites.
//! 3. **Gentle look-ahead downward expander** keyed on the *held* σ. Sounds above
//!    `gate_sigma`·σ pass at unity; the floor is lowered by at most `expansion`·24 dB.
//!    Default depth keeps hash clearly audible (≈ −9 dB). 0 = bypass. A 0.6 ms look-ahead
//!    opens the gain before spike onset so attack transients are not blunted; gain
//!    smoothing (0.15 ms up / 3 ms down) avoids zipper noise.
//! 4. **Presence** peaking EQ (default +3 dB, wide, 2.8 kHz): the ear is most sensitive at
//!    2–4 kHz. Linear, applied identically to every channel, so relative timbre is kept.
//! 5. **Safety limiter**: exactly linear below 0.5 FS (spikes sit at ~0.05–0.4 FS), only
//!    catches artefacts.
//! 6. **Constant-power stereo placement** of multiple channels (binaural segregation).
//! 7. **Windowed-sinc resampling** to the device rate, flat through the spike band, with a
//!    clock-drift servo on the *minimum* ring fill so latency stays at what jitter allows.
//!
//! `Reference` mode bypasses 3 and 4 for A/B comparison.

use crate::dsp::{BandSpec, Biquad, NoiseEstimator};
use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{info, warn};
use parking_lot::Mutex;
use rhythm::{Block, ELECTRODE_UV_PER_BIT};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum GainMode {
    /// Gain from a held noise calibration: equal hiss level per electrode.
    NoiseReferenced,
    /// Fixed scale: `absolute_fs_uv` µV = full scale.
    Absolute,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AudioSettings {
    pub enabled: bool,
    /// Electrode channels to monitor (global indices).
    pub channels: Vec<usize>,
    pub volume_db: f32,
    pub gain_mode: GainMode,
    pub absolute_fs_uv: f32,
    /// Bypass expander and presence (band-limit + gain + safety limiter only).
    pub reference: bool,
    /// Expander depth 0..1 (0 = off; 1 = floor at -24 dB).
    pub expansion: f32,
    pub gate_sigma: f32,
    pub presence_db: f32,
    pub band: BandSpec,
    pub mute: bool,
    /// Requested device buffer (frames). Smaller = lower latency.
    pub device_buffer: u32,
    /// Safety margin kept in the ring buffer (ms).
    pub margin_ms: f32,
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            channels: vec![0],
            volume_db: -6.0,
            gain_mode: GainMode::NoiseReferenced,
            absolute_fs_uv: 600.0,
            reference: false,
            expansion: 0.375,
            gate_sigma: 3.0,
            presence_db: 3.0,
            band: BandSpec { low_hz: 300.0, high_hz: 7000.0, order: 4 },
            mute: false,
            device_buffer: 128,
            margin_ms: 1.5,
        }
    }
}

#[derive(Default)]
pub struct AudioStats {
    pub underruns: AtomicU64,
    pub overruns: AtomicU64,
    pub fill_frames: AtomicU32,
    pub latency_us: AtomicU32,
    pub device_rate: AtomicU32,
    pub running: AtomicBool,
    /// Feeder is delivering audio (underruns only count while active).
    pub active: AtomicBool,
    pub ratio_ppm: AtomicU32,
    pub min_fill: AtomicU32,
    /// Request a new noise calibration.
    pub recalibrate: AtomicBool,
    /// Held σ (µV ×100) of the first monitored channel, 0 while calibrating.
    pub held_sigma_cuv: AtomicU32,
}

const CAL_SECONDS: f64 = 3.0;

struct Chain {
    bp: Vec<Biquad>,
    bpz: Vec<[f32; 2]>,
    presence: Biquad,
    pz: [f32; 2],
    noise: NoiseEstimator,
    held_sigma: Option<f32>,
    delay: Vec<f32>,
    dpos: usize,
    env: f32,
    gain: f32,
    pan: (f32, f32),
}

impl Chain {
    fn new(fs: f64, s: &AudioSettings, pan: f32) -> Self {
        let bp = s.band.sections(fs);
        let la = ((fs * 0.6e-3) as usize).max(1);
        let th = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
        Self {
            bpz: vec![[0.0; 2]; bp.len()],
            bp,
            presence: Biquad::peaking(fs, 2800.0, 0.7, s.presence_db as f64),
            pz: [0.0; 2],
            noise: NoiseEstimator::new(fs, CAL_SECONDS).with_smoothing(1.0),
            held_sigma: None,
            delay: vec![0.0; la],
            dpos: 0,
            env: 0.0,
            gain: 1.0,
            pan: (th.cos(), th.sin()),
        }
    }
}

/// Coefficients shared by all chains (sample-rate dependent).
#[derive(Clone, Copy)]
struct Dyn {
    attack: f32,
    release: f32,
    gain_up: f32,
    gain_down: f32,
}

impl Dyn {
    fn new(fs: f64) -> Self {
        let k = |tau: f64| (1.0 - (-1.0 / (fs * tau)).exp()) as f32;
        Self { attack: 1.0, release: k(4.0e-3), gain_up: k(0.15e-3), gain_down: k(3.0e-3) }
    }
}

/// The DSP part of the monitor, independent of any audio device (unit-testable).
pub struct MonitorDsp {
    fs: f64,
    chains: Vec<Chain>,
    key: (Vec<usize>, BandSpec, u32),
    dynamics: Dyn,
    samples_since_cal: u64,
}

impl MonitorDsp {
    pub fn new(fs: f64) -> Self {
        Self { fs, chains: Vec::new(), key: (Vec::new(), BandSpec::SPIKES, 0), dynamics: Dyn::new(fs), samples_since_cal: 0 }
    }

    pub fn recalibrate(&mut self) {
        for c in &mut self.chains {
            c.held_sigma = None;
            c.noise = NoiseEstimator::new(self.fs, CAL_SECONDS).with_smoothing(1.0);
        }
        self.samples_since_cal = 0;
    }

    pub fn held_sigma(&self, i: usize) -> Option<f32> {
        self.chains.get(i).and_then(|c| c.held_sigma)
    }

    /// Process a block into interleaved stereo at the acquisition rate.
    pub fn process(&mut self, s: &AudioSettings, blk: &Block, out: &mut Vec<f32>) {
        let key = (s.channels.clone(), s.band, s.presence_db.to_bits());
        if key != self.key {
            let n = s.channels.len();
            self.chains = (0..n).map(|i| Chain::new(self.fs, s, if n == 1 { 0.0 } else { -0.8 + 1.6 * i as f32 / (n - 1) as f32 })).collect();
            self.key = key;
            self.samples_since_cal = 0;
        }
        let d = self.dynamics;
        let vol = if s.mute { 0.0 } else { 10f32.powf(s.volume_db / 20.0) };
        let floor = 10f32.powf(-24.0 * s.expansion.clamp(0.0, 1.0) / 20.0);
        let expand = !s.reference && s.expansion > 0.0;
        let presence = !s.reference && s.presence_db != 0.0;
        let nch = blk.num_channels;
        let norm_ch = 1.0 / (s.channels.len().max(1) as f32).sqrt();
        for t in 0..blk.len() {
            let row = &blk.data[t * nch..(t + 1) * nch];
            self.samples_since_cal += 1;
            let (mut l, mut r) = (0.0f32, 0.0f32);
            for (c, &chi) in self.chains.iter_mut().zip(s.channels.iter()) {
                let x = row.get(chi).copied().unwrap_or(0) as f32 * ELECTRODE_UV_PER_BIT;
                let mut y = x;
                for (b, z) in c.bp.iter().zip(c.bpz.iter_mut()) {
                    y = b.tick(y, z);
                }
                if c.held_sigma.is_none() && c.noise.update(y) && c.noise.windows() >= 1 {
                    // First full median window completed: hold it.
                    c.held_sigma = Some(c.noise.sigma().max(0.5));
                }
                // While calibrating use the running estimate (already robust to spikes).
                let sigma = c.held_sigma.unwrap_or_else(|| c.noise.sigma().max(0.5));
                // Look-ahead: the envelope sees the undelayed signal.
                let a = y.abs();
                c.env += if a > c.env { d.attack } else { d.release } * (a - c.env);
                let target = if expand {
                    let rr = c.env / (s.gate_sigma * sigma);
                    let r4 = rr * rr * rr * rr;
                    floor + (1.0 - floor) * (r4 / (1.0 + r4))
                } else {
                    1.0
                };
                c.gain += if target > c.gain { d.gain_up } else { d.gain_down } * (target - c.gain);
                let dl = c.delay[c.dpos];
                c.delay[c.dpos] = y;
                c.dpos = (c.dpos + 1) % c.delay.len();
                let mut v = dl * c.gain;
                if presence {
                    v = c.presence.tick(v, &mut c.pz);
                }
                v *= match s.gain_mode {
                    // Noise σ → 0.02 FS, so a 10σ unit sits near 0.2 FS (linear region).
                    GainMode::NoiseReferenced => 0.02 / sigma,
                    GainMode::Absolute => 1.0 / s.absolute_fs_uv.max(1.0),
                };
                l += v * c.pan.0;
                r += v * c.pan.1;
            }
            out.push(soft_limit(l * vol * norm_ch));
            out.push(soft_limit(r * vol * norm_ch));
        }
    }
}

pub struct AudioMonitor {
    pub settings: Arc<Mutex<AudioSettings>>,
    stats: Arc<AudioStats>,
    _stream: Option<cpal::Stream>,
    producer: Option<rtrb::Producer<f32>>,
    fs_in: f64,
    fs_out: f64,
    dsp: MonitorDsp,
    resamp: SincResampler,
    tmp: Vec<f32>,
    integ: f64,
    last_ctl: std::time::Instant,
}

impl AudioMonitor {
    pub fn stats(&self) -> Arc<AudioStats> {
        self.stats.clone()
    }

    pub fn new(settings: Arc<Mutex<AudioSettings>>, stats: Arc<AudioStats>, fs_in: f64) -> Self {
        let s = settings.lock().clone();
        let mut m = Self {
            settings,
            stats,
            _stream: None,
            producer: None,
            fs_in,
            fs_out: 48000.0,
            dsp: MonitorDsp::new(fs_in),
            resamp: SincResampler::new(),
            tmp: Vec::new(),
            integ: 0.0,
            last_ctl: std::time::Instant::now(),
        };
        if let Err(e) = m.open_device(&s) {
            warn!("audio output unavailable: {e:#}");
        }
        m
    }

    fn open_device(&mut self, s: &AudioSettings) -> Result<()> {
        let host = cpal::default_host();
        info!("audio host: {:?}", host.id());
        let dev = host.default_output_device().ok_or_else(|| anyhow!("no output device"))?;
        let def = dev.default_output_config().context("default output config")?;
        let fs_out = def.sample_rate();
        let channels = def.channels().max(1);
        let (prod, cons) = rtrb::RingBuffer::<f32>::new((fs_out as usize / 5) * 2);
        let cons = Arc::new(Mutex::new(cons));
        let ch = channels as usize;
        let build = |buffer_size: cpal::BufferSize| {
            let st = self.stats.clone();
            let est = self.stats.clone();
            let cons = cons.clone();
            dev.build_output_stream::<f32, _, _>(
                cpal::StreamConfig { channels, sample_rate: fs_out, buffer_size },
                move |out: &mut [f32], _| {
                    let Some(mut cons) = cons.try_lock() else {
                        out.fill(0.0);
                        return;
                    };
                    let frames = out.len() / ch;
                    let avail = cons.slots() / 2;
                    st.min_fill.fetch_min(avail as u32, Ordering::Relaxed);
                    st.fill_frames.store(avail as u32, Ordering::Relaxed);
                    let n = frames.min(avail);
                    if n < frames && st.active.load(Ordering::Relaxed) {
                        st.underruns.fetch_add(1, Ordering::Relaxed);
                    }
                    if let Ok(chunk) = cons.read_chunk(n * 2) {
                        let (a, b) = chunk.as_slices();
                        let mut it = a.iter().chain(b.iter());
                        for f in 0..n {
                            let l = *it.next().unwrap();
                            let r = *it.next().unwrap();
                            let o = &mut out[f * ch..(f + 1) * ch];
                            o[0] = l;
                            if ch > 1 {
                                o[1] = r;
                                o[2..].fill(0.0);
                            }
                        }
                        chunk.commit_all();
                    }
                    out[n * ch..].fill(0.0);
                },
                move |e| {
                    warn!("audio stream error: {e}");
                    est.running.store(false, Ordering::Relaxed);
                },
                None,
            )
        };
        let stream = match build(cpal::BufferSize::Fixed(s.device_buffer)) {
            Ok(st) => st,
            Err(e) => {
                warn!("fixed buffer of {} frames rejected ({e}); using device default", s.device_buffer);
                build(cpal::BufferSize::Default)?
            }
        };
        stream.play()?;
        info!("audio output: {fs_out} Hz, {channels} ch, buffer {}", s.device_buffer);
        self.fs_out = fs_out as f64;
        self.stats.device_rate.store(fs_out, Ordering::Relaxed);
        self.stats.running.store(true, Ordering::Relaxed);
        self.stats.min_fill.store(u32::MAX, Ordering::Relaxed);
        self.producer = Some(prod);
        self._stream = Some(stream);
        Ok(())
    }

    /// Process one acquisition block (audio feeder thread).
    pub fn process(&mut self, blk: &Block) {
        let s = self.settings.lock().clone();
        self.stats.active.store(s.enabled && self.producer.is_some(), Ordering::Relaxed);
        if self.producer.is_none() || !s.enabled {
            return;
        }
        if self.stats.recalibrate.swap(false, Ordering::Relaxed) {
            self.dsp.recalibrate();
        }
        self.tmp.clear();
        self.dsp.process(&s, blk, &mut self.tmp);
        self.stats.held_sigma_cuv.store(self.dsp.held_sigma(0).map(|v| (v * 100.0) as u32).unwrap_or(0), Ordering::Relaxed);

        // Clock-drift servo on the minimum fill seen by the callback (every 100 ms).
        let st = &self.stats;
        if self.last_ctl.elapsed().as_secs_f64() > 0.1 {
            self.last_ctl = std::time::Instant::now();
            let minf = st.min_fill.swap(u32::MAX, Ordering::Relaxed);
            if minf != u32::MAX {
                let margin = s.margin_ms as f64 * 1e-3 * self.fs_out;
                let err = (minf as f64 - margin) / self.fs_out; // seconds of excess buffering
                self.integ = (self.integ + err * 0.1).clamp(-0.05, 0.05);
                let adj = (-(0.5 * err + 0.05 * self.integ)).clamp(-0.005, 0.005);
                self.resamp.adjust = adj;
                st.ratio_ppm.store(((adj * 1e6) as i32) as u32, Ordering::Relaxed);
            }
            let fill = st.fill_frames.load(Ordering::Relaxed) as f64;
            let lat = (fill + s.device_buffer as f64) / self.fs_out + SincResampler::DELAY_IN as f64 / self.fs_in + 0.6e-3;
            st.latency_us.store((lat * 1e6) as u32, Ordering::Relaxed);
        }
        let out = self.resamp.process(&self.tmp, self.fs_out / self.fs_in);
        let prod = self.producer.as_mut().unwrap();
        // Backlog guard: if the ring holds far more than needed (device start-up, a stall
        // upstream), drop this block's output instead of letting the slow drift servo work
        // it off over a minute. Costs one audible skip, only when latency would be bad.
        let used_frames = (prod.buffer().capacity() - prod.slots()) / 2;
        let limit = (s.margin_ms as f64 * 1e-3 * self.fs_out) as usize + 2 * s.device_buffer as usize + (self.fs_out * 0.004) as usize;
        if used_frames > limit {
            st.overruns.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let n = out.len().min(prod.slots()) & !1;
        if n < out.len() {
            st.overruns.fetch_add(1, Ordering::Relaxed);
        }
        if let Ok(mut chunk) = prod.write_chunk_uninit(n) {
            let (a, b) = chunk.as_mut_slices();
            let (oa, ob) = out[..n].split_at(a.len());
            for (d, s) in a.iter_mut().zip(oa) {
                d.write(*s);
            }
            for (d, s) in b.iter_mut().zip(ob) {
                d.write(*s);
            }
            unsafe { chunk.commit_all() };
        }
    }
}

/// Exactly linear below `LIMIT_KNEE` (spikes sit at ~0.05–0.4 FS and are untouched);
/// above it, a C¹-continuous tanh shoulder approaching ±1 catches artefacts.
pub const LIMIT_KNEE: f32 = 0.5;
#[inline]
pub fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= LIMIT_KNEE {
        return x;
    }
    let h = 1.0 - LIMIT_KNEE;
    let t = (a - LIMIT_KNEE) / h;
    // tanh via exp; t ≥ 0
    let e = (-2.0 * t.min(20.0)).exp();
    (LIMIT_KNEE + h * (1.0 - e) / (1.0 + e)).copysign(x)
}

/// Stereo polyphase windowed-sinc resampler (32 taps, 256 phases, Kaiser β=9) with a
/// fractional-ratio trim for clock-drift correction. Passband flat beyond 10 kHz at
/// 30 kHz input; images < -80 dB.
pub struct SincResampler {
    table: Vec<f32>, // (PHASES+1) × TAPS
    hist: Vec<[f32; 2]>,
    pos: usize,
    frac: f64,
    pub adjust: f64,
    out: Vec<f32>,
}

impl SincResampler {
    const TAPS: usize = 32;
    const PHASES: usize = 256;
    pub const DELAY_IN: usize = Self::TAPS / 2;

    pub fn new() -> Self {
        let t = Self::TAPS as f64;
        let beta = 9.0;
        let i0 = |x: f64| {
            let (mut s, mut term) = (1.0, 1.0);
            for k in 1..30 {
                term *= (x / 2.0) / k as f64;
                s += term * term;
            }
            s
        };
        let cutoff = 0.9; // fraction of input Nyquist
        let mut table = Vec::with_capacity((Self::PHASES + 1) * Self::TAPS);
        for p in 0..=Self::PHASES {
            let frac = p as f64 / Self::PHASES as f64;
            let mut row: Vec<f64> = (0..Self::TAPS)
                .map(|k| {
                    let x = k as f64 - (t / 2.0 - 1.0) - frac; // centre between taps 15 and 16
                    let sinc = if x.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * cutoff * x) };
                    let w = (x / (t / 2.0)).clamp(-1.0, 1.0);
                    sinc * cutoff * i0(beta * (1.0 - w * w).max(0.0).sqrt()) / i0(beta)
                })
                .collect();
            let sum: f64 = row.iter().sum();
            row.iter_mut().for_each(|v| *v /= sum);
            table.extend(row.into_iter().map(|v| v as f32));
        }
        Self { table, hist: vec![[0.0; 2]; 2 * Self::TAPS], pos: 0, frac: 0.0, adjust: 0.0, out: Vec::new() }
    }

    pub fn process(&mut self, inp: &[f32], ratio: f64) -> &[f32] {
        self.out.clear();
        let step = 1.0 / (ratio * (1.0 + self.adjust));
        let n = Self::TAPS;
        for fr in inp.chunks_exact(2) {
            // Mirror-write so a contiguous window of TAPS is always available.
            self.hist[self.pos] = [fr[0], fr[1]];
            self.hist[self.pos + n] = [fr[0], fr[1]];
            self.pos = (self.pos + 1) % n;
            while self.frac < 1.0 {
                let ph = self.frac * Self::PHASES as f64;
                let pi = ph as usize;
                let pf = (ph - pi as f64) as f32;
                let r0 = &self.table[pi * n..(pi + 1) * n];
                let r1 = &self.table[(pi + 1) * n..(pi + 2) * n];
                let w = &self.hist[self.pos..self.pos + n]; // oldest..newest
                let (mut l, mut r) = (0.0f32, 0.0f32);
                for k in 0..n {
                    let c = r0[k] + pf * (r1[k] - r0[k]);
                    l += c * w[k][0];
                    r += c * w[k][1];
                }
                self.out.push(l);
                self.out.push(r);
                self.frac += step;
            }
            self.frac -= 1.0;
        }
        &self.out
    }
}

impl Default for SincResampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    fn tone_gain(f: f64) -> f64 {
        let mut r = SincResampler::new();
        let fs = 30000.0;
        let inp: Vec<f32> = (0..30000).flat_map(|i| {
            let v = (2.0 * PI * f * i as f64 / fs).sin() as f32;
            [v, v]
        }).collect();
        let out = r.process(&inp, 48000.0 / 30000.0).to_vec();
        let tail: Vec<f64> = out[out.len() / 2..].iter().step_by(2).map(|&v| v as f64).collect();
        (2.0 * tail.iter().map(|v| v * v).sum::<f64>() / tail.len() as f64).sqrt()
    }

    #[test]
    fn resampler_rate_and_flatness() {
        let mut r = SincResampler::new();
        let inp = vec![0.0f32; 60000];
        let n = r.process(&inp, 48000.0 / 30000.0).len() / 2;
        assert!((n as i64 - 48000).abs() <= 2, "{n}");
        for f in [500.0, 3000.0, 7000.0, 10000.0] {
            let g = tone_gain(f);
            assert!((g - 1.0).abs() < 0.012, "gain at {f} Hz = {g}");
        }
    }

    #[test]
    fn limiter_is_monotone_bounded_and_transparent_for_spikes() {
        let mut last = -2.0;
        for i in -400..=400 {
            let x = i as f32 / 100.0;
            let y = soft_limit(x);
            assert!(y >= last - 1e-6 && y.abs() <= 1.0001);
            if x.abs() <= LIMIT_KNEE {
                assert_eq!(y, x);
            }
            last = y;
        }
    }

    /// Synthetic block: noise σ=`sigma` µV plus a spike train (`rate` Hz) of `amp` µV troughs.
    fn synth(fs: f64, secs: f64, sigma: f64, spikes: &[(f64, f64)], seed: u64) -> Block {
        let n = (fs * secs) as usize;
        let mut b = Block::with_capacity(1, n);
        let mut s = seed;
        let mut gauss = || {
            let mut u = || {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 11) as f64 / (1u64 << 53) as f64
            };
            let (a, c) = (u().max(1e-12), u());
            (-2.0 * a.ln()).sqrt() * (2.0 * PI * c).cos()
        };
        let w = (fs * 1.0e-3) as usize;
        let mut v = vec![0.0f64; n];
        for x in v.iter_mut() {
            *x = sigma * gauss();
        }
        for &(rate, amp) in spikes {
            let period = (fs / rate) as usize;
            let mut t = period / 3;
            while t + w < n {
                for k in 0..w {
                    let ph = k as f64 / w as f64;
                    v[t + k] += -amp * (PI * ph).sin() * (1.0 - 1.6 * ph);
                }
                t += period;
            }
        }
        for (i, x) in v.iter().enumerate() {
            b.data.push((x / ELECTRODE_UV_PER_BIT as f64) as i16);
            b.timestamps.push(i as f64 / fs);
            b.ttl.push(0);
        }
        b
    }

    fn run(s: &AudioSettings, b: &Block) -> Vec<f32> {
        let mut d = MonitorDsp::new(30000.0);
        let mut out = Vec::new();
        d.process(s, b, &mut out);
        out.iter().step_by(2).copied().collect()
    }

    fn peak_after(x: &[f32], from: usize) -> f32 {
        x[from..].iter().fold(0.0, |m, v| m.max(v.abs()))
    }

    /// Peak |·| of the spike-triggered average (noise averages out across spikes).
    fn spike_peaks(y: &[f32], fs: f64, rate: f64, skip_s: f64) -> f32 {
        let period = (fs / rate) as usize;
        let w = (fs * 3.0e-3) as usize;
        let mut avg = vec![0.0f32; w];
        let mut t = period / 3;
        let mut n = 0;
        while t + w < y.len() {
            if t as f64 > skip_s * fs {
                for k in 0..w {
                    avg[k] += y[t + k];
                }
                n += 1;
            }
            t += period;
        }
        avg.iter().fold(0.0f32, |m, v| m.max((v / n as f32).abs()))
    }

    #[test]
    fn expander_preserves_relative_unit_amplitudes() {
        // Two units of 8σ and 16σ (raw), recorded separately. The default chain may expand
        // near-threshold amplitudes slightly (that is what an expander does), but the
        // loudness ratio must stay within 15 % of the linear reference chain's.
        let fs = 30000.0;
        let d = AudioSettings { volume_db: -12.0, ..Default::default() };
        let r = AudioSettings { reference: true, ..d.clone() };
        let small = synth(fs, 8.0, 5.0, &[(10.0, 40.0)], 1);
        let big = synth(fs, 8.0, 5.0, &[(10.0, 80.0)], 1);
        let ratio = |s: &AudioSettings| spike_peaks(&run(s, &big), fs, 10.0, 3.5) / spike_peaks(&run(s, &small), fs, 10.0, 3.5);
        let (rd, rr) = (ratio(&d), ratio(&r));
        assert!((rd / rr - 1.0).abs() < 0.15, "default {rd} vs reference {rr}");
        assert!((rr - 2.0).abs() < 0.2, "reference ratio {rr}");
    }

    #[test]
    fn held_gain_does_not_track_firing_rate() {
        // Same electrode noise; region A fires 5 Hz, region B 80 Hz tonic.
        // After calibration the per-spike loudness must be the same (no AGC flattening),
        // so rate differences remain audible as density, not as level changes.
        let fs = 30000.0;
        let s = AudioSettings { volume_db: 0.0, ..Default::default() };
        let sparse = run(&s, &synth(fs, 6.0, 10.0, &[(5.0, 100.0)], 7));
        let dense = run(&s, &synth(fs, 6.0, 10.0, &[(80.0, 100.0)], 7));
        let ps = spike_peaks(&sparse, fs, 5.0, 4.0);
        let pd = spike_peaks(&dense, fs, 80.0, 4.0);
        assert!((pd / ps - 1.0).abs() < 0.15, "dense {pd} vs sparse {ps}");
    }

    #[test]
    fn hash_remains_audible_at_default_depth() {
        // Sub-threshold activity (2σ) must not be gated away by default.
        let fs = 30000.0;
        let s = AudioSettings { volume_db: 0.0, ..Default::default() };
        let r = AudioSettings { reference: true, ..s.clone() };
        let b = synth(fs, 5.0, 10.0, &[(200.0, 20.0)], 3);
        let rms = |x: &[f32]| (x[100_000..].iter().map(|v| v * v).sum::<f32>() / (x.len() - 100_000) as f32).sqrt();
        let ratio_db = 20.0 * (rms(&run(&s, &b)) / rms(&run(&r, &b))).log10();
        assert!(ratio_db > -10.0, "hash attenuated by {ratio_db} dB");
    }

    #[test]
    fn reference_mode_is_linear() {
        let fs = 30000.0;
        let s = AudioSettings { reference: true, gain_mode: GainMode::Absolute, volume_db: 0.0, ..Default::default() };
        let b1 = synth(fs, 1.0, 10.0, &[(20.0, 80.0)], 5);
        let mut b2 = b1.clone();
        b2.data.iter_mut().for_each(|v| *v /= 2);
        let y1 = run(&s, &b1);
        let y2 = run(&s, &b2);
        let r = peak_after(&y1, 3000) / peak_after(&y2, 3000);
        assert!((r - 2.0).abs() < 0.02, "{r}");
    }
}
