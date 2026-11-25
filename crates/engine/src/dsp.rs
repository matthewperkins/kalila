//! Filters, robust noise estimation and spike detection.

use std::f64::consts::PI;

/// Common average reference on raw electrode samples. Applied before the band-pass filters,
/// which is equivalent to applying it after them (both are linear and every electrode uses
/// the same filter; `pyramid.rs` has a test that the CPU paths agree). Channels are global electrode indices.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Car {
    pub reference: Vec<usize>,
    pub subtract: Vec<usize>,
}

impl Car {
    /// Mean of the reference electrodes in one raw sample row, in raw LSB (0 if none).
    pub fn mean(&self, row: &[i16]) -> f32 {
        let (mut sum, mut n) = (0i64, 0usize);
        for &ch in &self.reference {
            if let Some(&v) = row.get(ch) {
                sum += v as i64;
                n += 1;
            }
        }
        if n == 0 { 0.0 } else { sum as f32 / n as f32 }
    }

    /// Per-channel "subtract the reference" mask over `nch` channels.
    pub fn mask(&self, nch: usize) -> Vec<bool> {
        let mut m = vec![false; nch];
        for &ch in &self.subtract {
            if ch < nch {
                m[ch] = true;
            }
        }
        m
    }
}

impl Car {
    /// Resolve this CAR for blocks of `stride` channels filtered on `nch` channels (the
    /// electrodes, which come first in a row). None when it would change nothing.
    pub fn plan(&self, stride: usize, nch: usize) -> Option<CarPlan> {
        let reference = self.reference.iter().copied().filter(|&c| c < stride).collect();
        CarPlan::new(reference, self.mask(nch))
    }
}

/// A CAR resolved for one block layout, so a block is filtered without looking at the
/// choice per sample: the columns whose mean is the reference, and which filtered channels
/// it is subtracted from.
#[derive(Clone, Debug, PartialEq)]
pub struct CarPlan {
    /// Columns of a raw row averaged into the reference (all inside the row).
    reference: Vec<usize>,
    /// Per filtered channel: subtract the reference from it.
    subtract: Vec<bool>,
}

impl CarPlan {
    /// None when there is no reference or nothing to subtract it from.
    pub fn new(reference: Vec<usize>, subtract: Vec<bool>) -> Option<Self> {
        (!reference.is_empty() && subtract.contains(&true)).then_some(Self { reference, subtract })
    }
}

/// Biquad coefficients (normalised, a0 = 1).
#[derive(Clone, Copy, Debug, Default)]
pub struct Biquad {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl Biquad {
    fn from_f64(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Self {
            b0: (b0 / a0) as f32,
            b1: (b1 / a0) as f32,
            b2: (b2 / a0) as f32,
            a1: (a1 / a0) as f32,
            a2: (a2 / a0) as f32,
        }
    }
    pub fn lowpass(fs: f64, f: f64, q: f64) -> Self {
        let w = 2.0 * PI * f / fs;
        let (s, c) = w.sin_cos();
        let al = s / (2.0 * q);
        Self::from_f64((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + al, -2.0 * c, 1.0 - al)
    }
    pub fn highpass(fs: f64, f: f64, q: f64) -> Self {
        let w = 2.0 * PI * f / fs;
        let (s, c) = w.sin_cos();
        let al = s / (2.0 * q);
        Self::from_f64(
            (1.0 + c) / 2.0,
            -(1.0 + c),
            (1.0 + c) / 2.0,
            1.0 + al,
            -2.0 * c,
            1.0 - al,
        )
    }
    pub fn peaking(fs: f64, f: f64, q: f64, gain_db: f64) -> Self {
        let a = 10f64.powf(gain_db / 40.0);
        let w = 2.0 * PI * f / fs;
        let (s, c) = w.sin_cos();
        let al = s / (2.0 * q);
        Self::from_f64(
            1.0 + al * a,
            -2.0 * c,
            1.0 - al * a,
            1.0 + al / a,
            -2.0 * c,
            1.0 - al / a,
        )
    }
    /// Transposed direct form II; `z` is the 2-element state.
    #[inline(always)]
    pub fn tick(&self, x: f32, z: &mut [f32; 2]) -> f32 {
        let y = self.b0 * x + z[0];
        z[0] = self.b1 * x - self.a1 * y + z[1];
        z[1] = self.b2 * x - self.a2 * y;
        y
    }
}

/// Butterworth section Qs for an even order.
fn butter_qs(order: usize) -> Vec<f64> {
    (0..order / 2)
        .map(|k| 1.0 / (2.0 * ((2 * k + 1) as f64 * PI / (2.0 * order as f64)).cos()))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BandSpec {
    /// High-pass corner (Hz), 0 = none.
    pub low_hz: f64,
    /// Low-pass corner (Hz), 0 = none.
    pub high_hz: f64,
    /// Even Butterworth order for each side.
    pub order: usize,
}

impl BandSpec {
    pub const SPIKES: BandSpec = BandSpec {
        low_hz: 300.0,
        high_hz: 6000.0,
        order: 4,
    };
    pub fn sections(&self, fs: f64) -> Vec<Biquad> {
        let mut v = Vec::new();
        let nyq = fs / 2.0;
        if self.low_hz > 0.0 && self.low_hz < nyq {
            v.extend(
                butter_qs(self.order)
                    .into_iter()
                    .map(|q| Biquad::highpass(fs, self.low_hz, q)),
            );
        }
        if self.high_hz > 0.0 && self.high_hz < nyq * 0.98 {
            v.extend(
                butter_qs(self.order)
                    .into_iter()
                    .map(|q| Biquad::lowpass(fs, self.high_hz, q)),
            );
        }
        v
    }
}

/// Multichannel cascade working on interleaved data.
#[derive(Clone, Debug)]
pub struct FilterBank {
    pub sections: Vec<Biquad>,
    nch: usize,
    state: Vec<[f32; 2]>,
    /// Scratch for [`FilterBank::process`]: the reference mean of each sample.
    means: Vec<f32>,
}

impl FilterBank {
    pub fn new(sections: Vec<Biquad>, nch: usize) -> Self {
        let n = sections.len() * nch;
        Self {
            sections,
            nch,
            state: vec![[0.0; 2]; n],
            means: Vec::new(),
        }
    }
    #[inline]
    pub fn tick(&mut self, ch: usize, mut x: f32) -> f32 {
        let ns = self.sections.len();
        let st = &mut self.state[ch * ns..(ch + 1) * ns];
        for (s, z) in self.sections.iter().zip(st.iter_mut()) {
            x = s.tick(x, z);
        }
        x
    }
    pub fn num_channels(&self) -> usize {
        self.nch
    }

    /// Filter a whole block. `data` holds raw rows of `stride` channels; filtered channel
    /// `i` reads column `cols[i]` (column `i` when `cols` is None). Per sample the result is
    /// `filter((x − reference mean) · scale)`, the reference being the mean of the plan's
    /// columns in raw LSB, subtracted only on the plan's channels. `out` receives
    /// `n × num_channels` values, sample-major. The CAR is resolved once, into one mean per
    /// sample; each channel then runs through the block alone, so its filter state stays in
    /// registers. Each value is computed with the same operations in the same order as
    /// [`FilterBank::tick`], so the result is bit-identical to feeding it sample by sample.
    pub fn process(
        &mut self,
        data: &[i16],
        stride: usize,
        cols: Option<&[usize]>,
        scale: f32,
        car: Option<&CarPlan>,
        out: &mut [f32],
    ) {
        let nch = self.nch;
        let n = data.len() / stride.max(1);
        assert!(out.len() >= n * nch, "output too short for the block");
        assert!(cols.is_none_or(|c| c.len() == nch && c.iter().all(|&c| c < stride)));
        assert!(cols.is_some() || nch <= stride);
        if let Some(plan) = car {
            assert_eq!(plan.subtract.len(), nch);
            self.means.clear();
            self.means.extend(data.chunks_exact(stride).map(|row| {
                let sum: i64 = plan.reference.iter().map(|&c| row[c] as i64).sum();
                sum as f32 / plan.reference.len() as f32
            }));
        }
        let ns = self.sections.len();
        for ch in 0..nch {
            let col = cols.map_or(ch, |c| c[ch]);
            let st = &mut self.state[ch * ns..(ch + 1) * ns];
            let sub = car.is_some_and(|p| p.subtract[ch]);
            let rows = data.chunks_exact(stride).zip(out.chunks_exact_mut(nch));
            if sub {
                for ((row, o), &m) in rows.zip(&self.means) {
                    o[ch] = Self::cascade(&self.sections, st, (row[col] as f32 - m) * scale);
                }
            } else {
                for (row, o) in rows {
                    o[ch] = Self::cascade(&self.sections, st, row[col] as f32 * scale);
                }
            }
        }
    }

    #[inline(always)]
    fn cascade(sections: &[Biquad], st: &mut [[f32; 2]], mut x: f32) -> f32 {
        for (s, z) in sections.iter().zip(st.iter_mut()) {
            x = s.tick(x, z);
        }
        x
    }
}

/// Robust noise estimate σ = median(|x|)/0.6745 (Quiroga et al. 2004).
///
/// The median tolerates heavy spike contamination (dense tonic firing, multi-unit hash),
/// unlike mean-|x| or RMS estimators which inflate σ in high-rate regions such as GP and
/// would raise thresholds / lower gain there. Computed over windows of `window_s` on every
/// `decim`-th sample, then smoothed across windows.
#[derive(Clone, Debug)]
pub struct NoiseEstimator {
    buf: Vec<f32>,
    scratch: Vec<f32>,
    mask: Vec<bool>,
    guard: usize,
    cap: usize,
    decim: usize,
    count: usize,
    sigma: f32,
    windows: u32,
    smooth: f32,
}

impl NoiseEstimator {
    /// `window_s`: median window. `smooth`: weight of the newest window (1 = no smoothing).
    pub fn new(fs: f64, window_s: f64) -> Self {
        let decim = ((fs / 10000.0) as usize).max(1);
        let cap = ((fs * window_s) as usize / decim).max(64);
        let guard = ((fs * 1.0e-3) as usize / decim).max(1);
        Self {
            buf: Vec::with_capacity(cap),
            scratch: Vec::with_capacity(cap),
            mask: Vec::with_capacity(cap),
            guard,
            cap,
            decim,
            count: 0,
            sigma: 0.0,
            windows: 0,
            smooth: 0.3,
        }
    }
    pub fn with_smoothing(mut self, w: f32) -> Self {
        self.smooth = w;
        self
    }
    /// Returns true when a window completed (and `sigma()` was updated).
    #[inline]
    pub fn update(&mut self, x: f32) -> bool {
        self.count += 1;
        if self.count < self.decim {
            return false;
        }
        self.count = 0;
        self.buf.push(x.abs());
        // Quick provisional estimate early on so consumers have something immediately.
        let early = self.windows == 0 && self.buf.len() == 256;
        if self.buf.len() < self.cap && !early {
            return false;
        }
        let est = self.two_pass();
        if early {
            self.sigma = est;
            return true;
        }
        self.buf.clear();
        self.sigma = if self.windows == 0 {
            est
        } else {
            self.sigma + self.smooth * (est - self.sigma)
        };
        self.windows += 1;
        true
    }

    fn median_sigma(v: &mut [f32]) -> f32 {
        if v.is_empty() {
            return 0.0;
        }
        let mid = v.len() / 2;
        let (_, m, _) = v.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
        *m / 0.6745
    }

    /// Median estimate, then re-estimate excluding ±1 ms around every sample > 4σ₀.
    fn two_pass(&mut self) -> f32 {
        self.scratch.clear();
        self.scratch.extend_from_slice(&self.buf);
        let s0 = Self::median_sigma(&mut self.scratch);
        let thr = 4.0 * s0;
        let n = self.buf.len();
        // Mark ±guard around every supra-threshold sample, then keep the rest.
        self.mask.clear();
        self.mask.resize(n, false);
        for i in 0..n {
            if self.buf[i] > thr {
                let lo = i.saturating_sub(self.guard);
                let hi = (i + self.guard + 1).min(n);
                self.mask[lo..hi].fill(true);
            }
        }
        self.scratch.clear();
        self.scratch
            .extend(self.buf.iter().zip(&self.mask).filter(|(_, m)| !**m).map(|(v, _)| *v));
        if self.scratch.len() < n / 4 {
            return s0; // pathological: mostly "spikes"; fall back
        }
        Self::median_sigma(&mut self.scratch)
    }

    #[inline]
    pub fn sigma(&self) -> f32 {
        self.sigma
    }
    /// Number of complete windows so far.
    pub fn windows(&self) -> u32 {
        self.windows
    }
}

#[derive(Clone, Debug)]
pub struct Spike {
    pub channel: usize,
    pub sample_number: i64,
    /// Filtered waveform in µV, `PRE + POST` samples, peak at index `PRE`.
    pub waveform: Vec<f32>,
    pub peak_uv: f32,
    pub threshold_uv: f32,
}

pub const SPIKE_PRE: usize = 10;
pub const SPIKE_POST: usize = 30;
pub const SPIKE_LEN: usize = SPIKE_PRE + SPIKE_POST;

#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ThresholdMode {
    /// Threshold at -k·σ (σ from the robust noise estimate).
    Sigma(f32),
    /// Fixed threshold in µV (negative).
    Fixed(f32),
}

/// Per-channel threshold-crossing detector on band-passed data.
pub struct SpikeDetector {
    pub mode: ThresholdMode,
    nch: usize,
    hist: Vec<f32>, // ring per channel, length H
    hpos: usize,
    noise: Vec<NoiseEstimator>,
    /// For each channel: samples remaining until a pending detection can be emitted (0 = none).
    pending: Vec<(i64, usize, f32)>,
    dead: Vec<usize>,
    dead_samples: usize,
    window: usize,
}

const HIST: usize = 64;

impl SpikeDetector {
    pub fn new(fs: f64, nch: usize, mode: ThresholdMode) -> Self {
        Self {
            mode,
            nch,
            hist: vec![0.0; nch * HIST],
            hpos: 0,
            noise: vec![NoiseEstimator::new(fs, 1.0); nch],
            pending: vec![(0, 0, 0.0); nch],
            dead: vec![0; nch],
            dead_samples: (fs * 1.0e-3) as usize,
            window: ((fs * 0.5e-3) as usize).max(1),
        }
    }
    pub fn sigma(&self, ch: usize) -> f32 {
        self.noise[ch].sigma()
    }
    pub fn threshold(&self, ch: usize) -> f32 {
        match self.mode {
            ThresholdMode::Sigma(k) => -k * self.noise[ch].sigma(),
            ThresholdMode::Fixed(t) => -t.abs(),
        }
    }

    /// Feed one sample per channel (`x[ch]`, µV, filtered) with absolute sample number `n`.
    pub fn push(&mut self, n: i64, x: &[f32], out: &mut Vec<Spike>) {
        let p = self.hpos;
        for (ch, &v) in x[..self.nch].iter().enumerate() {
            self.hist[ch * HIST + p] = v;
            self.noise[ch].update(v);
            let thr = self.threshold(ch);
            if self.dead[ch] > 0 {
                self.dead[ch] -= 1;
            }
            let pend = &mut self.pending[ch];
            if pend.1 > 0 {
                // Track the trough within the alignment window.
                if v < pend.2 {
                    *pend = (n, pend.1, v);
                }
                pend.1 -= 1;
                if pend.1 == 0 {
                    // Wait until POST samples after the peak are in history.
                }
            } else if self.dead[ch] == 0 && v < thr && thr < 0.0 {
                *pend = (n, self.window, v);
                self.dead[ch] = self.dead_samples;
            }
        }
        // Emit spikes whose post-peak samples are complete.
        for ch in 0..self.nch {
            let (peak_n, rem, peak_v) = self.pending[ch];
            if rem == 0 && peak_v != 0.0 && n - peak_n >= SPIKE_POST as i64 - 1 {
                let back = (n - peak_n) as usize; // samples since peak
                let mut w = Vec::with_capacity(SPIKE_LEN);
                for k in 0..SPIKE_LEN {
                    let age = back + SPIKE_PRE - k; // how many samples ago
                    w.push(self.hist[ch * HIST + (p + HIST - age % HIST) % HIST]);
                }
                out.push(Spike {
                    channel: ch,
                    sample_number: peak_n,
                    waveform: w,
                    peak_uv: peak_v,
                    threshold_uv: self.threshold(ch),
                });
                self.pending[ch] = (0, 0, 0.0);
            }
        }
        self.hpos = (p + 1) % HIST;
    }
}

const _: () = assert!(SPIKE_LEN + 16 < HIST);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn car_mean_and_mask() {
        let c = Car {
            reference: vec![0, 2, 9],
            subtract: vec![1, 2, 9],
        };
        // Channel 9 is out of range for a 4-channel row: ignored in both.
        assert_eq!(c.mean(&[10, 100, -4, 7]), 3.0);
        assert_eq!(c.mask(4), vec![false, true, true, false]);
        assert_eq!(Car::default().mean(&[5, 5]), 0.0);
    }

    fn gain_at(sections: &[Biquad], fs: f64, f: f64) -> f64 {
        let mut z = vec![[0.0f32; 2]; sections.len()];
        let n = (fs * 2.0) as usize;
        let mut peak = 0.0f64;
        for i in 0..n {
            let mut x = (2.0 * PI * f * i as f64 / fs).sin() as f32;
            for (s, zz) in sections.iter().zip(z.iter_mut()) {
                x = s.tick(x, zz);
            }
            if i > n / 2 {
                peak = peak.max(x.abs() as f64);
            }
        }
        peak
    }

    #[test]
    fn bandpass_response() {
        let s = BandSpec::SPIKES.sections(30000.0);
        assert!((gain_at(&s, 30000.0, 1500.0) - 1.0).abs() < 0.05);
        assert!((gain_at(&s, 30000.0, 300.0) - std::f64::consts::FRAC_1_SQRT_2).abs() < 0.05);
        assert!(gain_at(&s, 30000.0, 50.0) < 1e-3);
    }

    #[test]
    fn noise_estimator_is_robust() {
        let mut e = NoiseEstimator::new(30000.0, 1.0);
        let mut r = 12345u64;
        for i in 0..300000 {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            let u1 = (r >> 11) as f64 / (1u64 << 53) as f64;
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            let u2 = (r >> 11) as f64 / (1u64 << 53) as f64;
            let g = (-2.0 * u1.max(1e-12).ln()).sqrt() * (2.0 * PI * u2).cos();
            let spike = if i % 300 < 30 { -150.0 } else { 0.0 }; // 10% duty large spikes (dense tonic firing)
            e.update((10.0 * g + spike) as f32);
        }
        assert!((e.sigma() - 10.0).abs() < 1.0, "{}", e.sigma());
    }

    #[test]
    fn detects_and_aligns_spike() {
        let mut d = SpikeDetector::new(30000.0, 1, ThresholdMode::Fixed(50.0));
        let mut out = Vec::new();
        for n in 0..200i64 {
            let v = match n {
                100 => -60.0,
                101 => -120.0,
                102 => -80.0,
                _ => 0.0,
            };
            d.push(n, &[v], &mut out);
        }
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].sample_number, 101);
        assert_eq!(out[0].waveform[SPIKE_PRE], -120.0);
        assert_eq!(out[0].waveform[SPIKE_PRE - 1], -60.0);
        assert_eq!(out[0].waveform[SPIKE_PRE + 1], -80.0);
    }

    /// `dsp_thread`'s loop as it was before the block API (CAR per sample, `tick` per channel).
    fn per_sample(bank: &mut FilterBank, car: Option<&Car>, data: &[i16], stride: usize, nel: usize) -> Vec<f32> {
        let mask = car.map(|c| c.mask(nel));
        let mut out = Vec::new();
        for r in data.chunks_exact(stride) {
            let (c, mask) = match (car, &mask) {
                (Some(c), Some(m)) => (c.mean(r), m.as_slice()),
                _ => (0.0, &[][..]),
            };
            for (ch, &raw) in r[..nel].iter().enumerate() {
                let x = raw as f32 - if mask.get(ch).copied().unwrap_or(false) { c } else { 0.0 };
                out.push(bank.tick(ch, x * 0.195));
            }
        }
        out
    }

    fn noise_data(n: usize, stride: usize, seed: u64) -> Vec<i16> {
        let mut r = seed;
        (0..n * stride)
            .map(|_| {
                r ^= r << 13;
                r ^= r >> 7;
                r ^= r << 17;
                ((r >> 40) as i64 % 3000 - 1500) as i16
            })
            .collect()
    }

    /// The block API gives bit-identical values to the per-sample loop it replaces, with and
    /// without a CAR (one reference column outside the electrodes, one beyond the row), over
    /// blocks of uneven sizes.
    #[test]
    fn process_is_bit_identical_to_the_per_sample_loop() {
        let (stride, nel) = (24, 16);
        let data = noise_data(20000, stride, 99);
        let cars = [
            None,
            Some(Car {
                reference: vec![0, 3, 5, 20, 99],
                subtract: vec![1, 3, 5, 15, 40],
            }),
            Some(Car {
                reference: vec![99],
                subtract: vec![1],
            }),
            Some(Car {
                reference: vec![2],
                subtract: vec![],
            }),
        ];
        for car in &cars {
            let sections = BandSpec::SPIKES.sections(30000.0);
            let mut old = FilterBank::new(sections.clone(), nel);
            let mut new = FilterBank::new(sections, nel);
            let plan = car.as_ref().and_then(|c| c.plan(stride, nel));
            let mut at = 0;
            for len in [1usize, 64, 7, 5000, 128, 14800] {
                let blk = &data[at * stride..(at + len) * stride];
                at += len;
                let want = per_sample(&mut old, car.as_ref(), blk, stride, nel);
                let mut got = vec![0f32; len * nel];
                new.process(blk, stride, None, 0.195, plan.as_ref(), &mut got);
                assert!(
                    want.iter().zip(&got).all(|(a, b)| a.to_bits() == b.to_bits()),
                    "{car:?}"
                );
            }
        }
    }

    /// Old against new, 30 kHz x 64 channels, CAR on, with the board's 32-sample blocks and
    /// with 1024.
    /// `cargo test --release -p engine process_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn process_speed() {
        let (stride, nel, secs) = (64, 64, 10usize);
        let data = noise_data(30000 * secs, stride, 5);
        let car = Car {
            reference: (0..32).collect(),
            subtract: (0..64).collect(),
        };
        for block in [32usize, 1024] {
            let sections = BandSpec::SPIKES.sections(30000.0);
            let mut old = FilterBank::new(sections.clone(), nel);
            let t0 = std::time::Instant::now();
            let mut sink = 0f32;
            for b in data.chunks(block * stride) {
                sink += per_sample(&mut old, Some(&car), b, stride, nel)[0];
            }
            let t_old = t0.elapsed();
            let mut new = FilterBank::new(sections, nel);
            let plan = car.plan(stride, nel);
            let mut rows = vec![0f32; block * nel];
            let t0 = std::time::Instant::now();
            for b in data.chunks(block * stride) {
                new.process(b, stride, None, 0.195, plan.as_ref(), &mut rows);
                sink += rows[0];
            }
            let t_new = t0.elapsed();
            eprintln!(
                "{nel} ch, {secs} s, {block}-sample blocks: per-sample {t_old:?}, block {t_new:?} (x{:.2}) [{sink}]",
                t_old.as_secs_f64() / t_new.as_secs_f64()
            );
        }
    }
}
