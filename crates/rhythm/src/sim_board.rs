//! Real-time simulated acquisition board producing plausible extracellular data.

use crate::types::*;
use anyhow::Result;
use std::f64::consts::PI;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

#[derive(Clone)]
struct Rng(u64);
impl Rng {
    #[inline]
    fn next_u64(&mut self) -> u64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    #[inline]
    fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
    #[inline]
    fn gauss(&mut self) -> f64 {
        // Irwin-Hall(4) approximation: cheap and good enough for noise.
        (self.uniform() + self.uniform() + self.uniform() + self.uniform() - 2.0) * 1.7320508
    }
}

struct Unit {
    channel: usize,
    rate_hz: f64,
    amplitude_uv: f64,
    /// Waveform template at the sampling rate, peak-normalised.
    template: Vec<f32>,
    refractory: usize,
    next_spike_in: usize,
    burstiness: f64,
}

pub struct SimBoard {
    settings: BoardSettings,
    headstage_channels: Vec<usize>,
    nch: usize,
    rng: Rng,
    units: Vec<Unit>,
    /// Active spike waveforms: (channel, unit, position, gain).
    active: Vec<(usize, usize, usize, f32)>,
    lfp_state: Vec<f64>,
    sample: i64,
    t0: Option<Instant>,
    health: Option<SharedHealth>,
    ttl_out: u16,
}

fn spike_template(fs: f64, width_ms: f64, asym: f64) -> Vec<f32> {
    let n = (fs * 2.0e-3) as usize;
    let tc = 0.4e-3;
    (0..n)
        .map(|i| {
            let t = i as f64 / fs - tc;
            let w = width_ms * 1e-3;
            let trough = -(-(t / (0.25 * w)).powi(2)).exp();
            let peak = asym * (-((t - 0.45 * w) / (0.45 * w)).powi(2)).exp();
            (trough + peak) as f32
        })
        .collect()
}

impl SimBoard {
    pub fn new(headstage_channels: Vec<usize>) -> Self {
        let mut s = Self {
            settings: BoardSettings::default(),
            nch: 0,
            headstage_channels,
            rng: Rng(0x9E3779B97F4A7C15),
            units: Vec::new(),
            active: Vec::new(),
            lfp_state: Vec::new(),
            sample: 0,
            t0: None,
            health: None,
            ttl_out: 0,
        };
        s.rebuild();
        s
    }

    fn electrodes(&self) -> usize {
        (0..self.headstage_channels.len()).map(|i| self.hs_channels(i)).sum()
    }

    /// Channels of simulated headstage `i`, honouring the RHD2132 16-channel option.
    fn hs_channels(&self, i: usize) -> usize {
        let n = self.headstage_channels[i];
        if n == 32 && self.settings.rhd2132_16ch[i % 8] { 16 } else { n }
    }

    fn rebuild(&mut self) {
        let fs = self.settings.sample_rate as f64;
        let ne = self.electrodes();
        self.nch = ne + if self.settings.acquire_aux { 3 * self.headstage_channels.len() } else { 0 } + if self.settings.acquire_adc { 8 } else { 0 };
        self.units.clear();
        let mut r = Rng(42);
        for ch in 0..ne {
            // ~60% of channels carry 1-3 units.
            if r.uniform() < 0.6 {
                for _ in 0..1 + (r.uniform() * 2.5) as usize {
                    let amp = 50.0 + 250.0 * r.uniform().powi(2);
                    self.units.push(Unit {
                        channel: ch,
                        rate_hz: 1.0 + 20.0 * r.uniform().powi(2),
                        amplitude_uv: amp,
                        template: spike_template(fs, 0.5 + 0.7 * r.uniform(), 0.2 + 0.5 * r.uniform()),
                        refractory: (fs * 2.0e-3) as usize,
                        next_spike_in: (r.uniform() * fs) as usize,
                        burstiness: r.uniform() * 0.3,
                    });
                }
            }
        }
        self.lfp_state = vec![0.0; ne];
        self.active.clear();
    }

    fn generate_sample(&mut self, blk: &mut Block) {
        let fs = self.settings.sample_rate as f64;
        let ne = self.electrodes();
        let t = self.sample as f64 / fs;
        // Spike scheduling.
        for (ui, u) in self.units.iter_mut().enumerate() {
            if u.next_spike_in == 0 {
                let g = (1.0 + 0.08 * self.rng.gauss()) as f32;
                for (dch, att) in [(0isize, 1.0f32), (-1, 0.45), (1, 0.45), (2, 0.15), (-2, 0.15)] {
                    let c = u.channel as isize + dch;
                    if c >= 0 && (c as usize) < ne {
                        self.active.push((c as usize, ui, 0, g * att));
                    }
                }
                // Theta-modulated Poisson process with refractoriness and bursts.
                let rate = u.rate_hz * (1.0 + 0.6 * (2.0 * PI * 7.0 * t).sin());
                let isi = if self.rng.uniform() < u.burstiness { fs * (0.003 + 0.004 * self.rng.uniform()) } else { -(self.rng.uniform().max(1e-12)).ln() * fs / rate.max(0.1) };
                u.next_spike_in = (isi as usize).max(u.refractory);
            } else {
                u.next_spike_in -= 1;
            }
        }
        let theta = (2.0 * PI * 7.0 * t).sin();
        let hum = (2.0 * PI * 50.0 * t).sin();
        let start = blk.data.len();
        for ch in 0..ne {
            // 1/f-ish LFP: leaky integrated noise + shared theta.
            let s = &mut self.lfp_state[ch];
            *s = *s * 0.9995 + self.rng.gauss() * 1.5;
            let v = 8.0 * self.rng.gauss() + *s + 60.0 * theta + 4.0 * hum;
            blk.data.push((v / ELECTRODE_UV_PER_BIT as f64) as i16);
        }
        let mut i = 0;
        while i < self.active.len() {
            let (ch, ui, pos, g) = self.active[i];
            let u = &self.units[ui];
            let v = u.template[pos] * g * u.amplitude_uv as f32 / ELECTRODE_UV_PER_BIT;
            let d = &mut blk.data[start + ch];
            *d = (*d as f32 + v).clamp(-32768.0, 32767.0) as i16;
            if pos + 1 >= u.template.len() {
                self.active.swap_remove(i);
            } else {
                self.active[i].2 += 1;
                i += 1;
            }
        }
        if self.settings.acquire_aux {
            for k in 0..3 * self.headstage_channels.len() {
                let a = 0.5 * (2.0 * PI * 0.5 * t + k as f64).sin();
                blk.data.push((a / AUX_V_PER_BIT as f64) as i16);
            }
        }
        if self.settings.acquire_adc {
            for k in 0..8 {
                let v = if k == 0 { 2.5 * (2.0 * PI * t).sin() } else { 0.0 };
                blk.data.push((v / 0.00015258789) as i16);
            }
        }
        // TTL line 0: 10 ms pulse every second; line 1: square wave 2 Hz.
        let ph = self.sample % self.settings.sample_rate as i64;
        let mut ttl = (ph < (fs * 0.01) as i64) as u16;
        ttl |= (((t * 4.0) as i64 % 2) as u16) << 1;
        blk.ttl.push(ttl);
        blk.timestamps.push(t);
        self.sample += 1;
    }
}

impl Board for SimBoard {
    fn info(&self) -> BoardInfo {
        BoardInfo { kind: "Simulated acquisition board".into(), description: vec![("Units".into(), self.units.len().to_string())] }
    }
    fn layout(&self) -> StreamLayout {
        let mut channels = Vec::new();
        let mut headstages = Vec::new();
        for (i, &raw) in self.headstage_channels.iter().enumerate() {
            let n = self.hs_channels(i);
            headstages.push(HeadstageInfo { name: ["A1", "A2", "B1", "B2", "C1", "C2", "D1", "D2"][i % 8].into(), slot: i % 8, port: i / 2, chip: if raw == 64 { "RHD2164" } else { "RHD2132" }.into(), num_channels: n, first_channel: channels.len(), has_bno: false });
            for _ in 0..n {
                let k = channels.len() + 1;
                channels.push(ChannelInfo { name: format!("CH{k}"), kind: ChannelKind::Electrode, bit_volts: ELECTRODE_UV_PER_BIT, units: "uV".into(), headstage: Some(i) });
            }
        }
        if self.settings.acquire_aux {
            for i in 0..3 * self.headstage_channels.len() {
                channels.push(ChannelInfo { name: format!("AUX{}", i + 1), kind: ChannelKind::Aux, bit_volts: AUX_V_PER_BIT, units: "V".into(), headstage: None });
            }
        }
        if self.settings.acquire_adc {
            for i in 0..8 {
                channels.push(ChannelInfo { name: format!("ADC{}", i + 1), kind: ChannelKind::Adc, bit_volts: 0.00015258789, units: "V".into(), headstage: None });
            }
        }
        StreamLayout { name: "acquisition_board".into(), sample_rate: self.settings.sample_rate as f64, channels, headstages, ttl_lines: 8, side_streams: vec![], hw_clock_hz: self.settings.sample_rate as f64 }
    }
    fn settings(&self) -> &BoardSettings {
        &self.settings
    }
    fn apply_settings(&mut self, s: &BoardSettings) -> Result<()> {
        self.settings = s.clone();
        self.rebuild();
        Ok(())
    }
    fn available_sample_rates(&self) -> Vec<u32> {
        vec![1000, 1250, 1500, 2000, 2500, 3000, 3333, 5000, 6250, 10000, 12500, 15000, 20000, 25000, 30000]
    }
    fn scan_ports(&mut self) -> Result<()> {
        Ok(())
    }
    fn start(&mut self, health: SharedHealth) -> Result<()> {
        health.reset();
        self.health = Some(health);
        self.sample = 0;
        self.t0 = Some(Instant::now());
        Ok(())
    }
    fn read_block(&mut self, blk: &mut Block) -> Result<()> {
        blk.clear();
        blk.num_channels = self.nch;
        blk.first_sample = self.sample;
        let n = self.settings.block_samples.max(1);
        for _ in 0..n {
            self.generate_sample(blk);
        }
        // Pace to real time.
        let due = Duration::from_secs_f64(self.sample as f64 / self.settings.sample_rate as f64);
        let el = self.t0.unwrap().elapsed();
        if due > el {
            std::thread::sleep(due - el);
        }
        if let Some(h) = &self.health {
            h.samples.fetch_add(n as u64, Ordering::Relaxed);
            h.frames.fetch_add(n as u64, Ordering::Relaxed);
        }
        Ok(())
    }
    fn live(&mut self, cmd: &LiveCommand) -> Result<()> {
        if let LiveCommand::SetTtlOut(w) = cmd {
            self.ttl_out = *w;
        }
        Ok(())
    }
    fn stop(&mut self) -> Result<()> {
        self.t0 = None;
        Ok(())
    }
}
