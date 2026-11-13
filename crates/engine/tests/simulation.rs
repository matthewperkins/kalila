//! Deterministic simulation of acquisition → recorder, under seeded faults.
//!
//! For each seed, a frame stream like the board's (Rhythm frames from `encode_payload`, plus
//! frames from other devices) is generated with faults: dropped frames (counter jumps, as a
//! DRAM overflow gives), duplicated and stale frames, corrupted and truncated frames, and a
//! counter that wraps at 2^32. It runs through the same block assembly as the board
//! (`rhythm::assemble`) and the real recorder, with recordings started and stopped at
//! seeded blocks. Then a checker proves, from the files alone, that each recording holds
//! exactly the samples that were accepted while it was open: every value at its own sample
//! number, sample numbers strictly increasing, TTL edges as the samples say, the health
//! counters equal to the injected faults, and `RecSource::read` putting each sample at its
//! number across the gaps.
//!
//! Everything is a function of the seed; a failure prints the seed, and
//! `OERS_SIM_SEED=<n> cargo test -p engine --test simulation` reruns just that one.

use engine::record::{RecMsg, RecStatus, run_recorder};
use rhythm::assemble::{Assembled, DeviceFrame, assemble};
use rhythm::oni_board::{CHIP_ID_RHD2132, CHIP_ID_RHD2216, DEVICE_RHYTHM};
use rhythm::parser::{RhythmParser, encode_payload};
use rhythm::{Block, ChannelInfo, ChannelKind, Health, StreamLayout};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Two streams as on the rig: an RHD2132 in 16-channel mode and an RHD2216, plus 8 ADC.
const STREAMS: usize = 2;
const NCH: usize = 16 + 16 + 8;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // splitmix64
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }
}

struct SimFrame {
    dev: u32,
    time: u64,
    data: Vec<u8>,
}
impl DeviceFrame for SimFrame {
    fn dev_idx(&self) -> u32 {
        self.dev
    }
    fn time(&self) -> u64 {
        self.time
    }
    fn data(&self) -> &[u8] {
        &self.data
    }
}

fn mix(c: u32, k: u32) -> u32 {
    let mut x = c.wrapping_mul(0x9E37_79B1) ^ k.wrapping_mul(0x85EB_CA77);
    x ^= x >> 15;
    x.wrapping_mul(0xC2B2_AE3D) ^ (x >> 13)
}

/// The TTL word of the frame with counter `c`: lines change every 64–256 samples.
fn ttl_of(c: u32) -> u16 {
    (mix(c >> 6, 7) & 0x00FF) as u16
}

/// A valid Rhythm payload for counter `c`, with the 8-byte hub timestamp in front.
fn rhythm_frame(c: u32) -> Vec<u8> {
    let mut d = vec![0u8; 8];
    d.extend(encode_payload(STREAMS, c, |s, k| mix(c, 1000 + 3 * s as u32 + k as u32) as u16, |s, k| mix(c, 32 * s as u32 + k as u32) as u16, std::array::from_fn(|a| mix(c, 2000 + a as u32) as u16), ttl_of(c)));
    d
}

fn parser() -> RhythmParser {
    RhythmParser::new(vec![16, 16], vec![CHIP_ID_RHD2132, CHIP_ID_RHD2216], false, true, false)
}

/// The row the parser makes of counter `c`'s frame (channel mapping is tested elsewhere;
/// this test is about which samples end up where).
fn row_of(c: u32) -> Vec<i16> {
    let mut b = Block::default();
    let f = rhythm_frame(c);
    let _ = parser().parse(&f[8..], 0.0, &mut b, &Health::default());
    assert_eq!(b.data.len(), NCH);
    b.data
}

fn layout() -> StreamLayout {
    let mut channels: Vec<ChannelInfo> = (0..32).map(|i| ChannelInfo { name: format!("CH{}", i + 1), kind: ChannelKind::Electrode, bit_volts: 0.195, units: "uV".into(), headstage: Some(i / 16) }).collect();
    channels.extend((0..8).map(|i| ChannelInfo { name: format!("ADC{}", i + 1), kind: ChannelKind::Adc, bit_volts: 0.000_152_587_89, units: "V".into(), headstage: None }));
    StreamLayout { name: "acquisition_board".into(), sample_rate: 30000.0, channels, headstages: vec![], ttl_lines: 16, side_streams: vec![], hw_clock_hz: 250e6 }
}

/// What the generator injected, and what the parser must therefore report.
#[derive(Default, Debug, PartialEq)]
struct Expected {
    dropped_samples: u64,
    discontinuities: u64,
    bad_frames: u64,
}

/// The frame stream for one seed: the frames in order, plus the counters of the frames the
/// parser must accept, in order, and the expected health counters.
fn frame_stream(rng: &mut Rng, frames: usize) -> (Vec<SimFrame>, Vec<u32>, Expected) {
    let mut out = Vec::with_capacity(frames + frames / 10);
    let mut accepted = Vec::new();
    let mut exp = Expected::default();
    // Some seeds start just before the 32-bit counter wraps.
    let mut c: u32 = if rng.chance(300) { u32::MAX - rng.below(5000) as u32 } else { rng.next() as u32 };
    let mut last: Option<u32> = None;
    let mut t = 0u64;
    while accepted.len() < frames {
        t += 8333;
        // Other devices' frames (memory monitor, BNO) interleaved.
        if rng.chance(20) {
            out.push(SimFrame { dev: 0x0202, time: t, data: vec![0u8; 16] });
        }
        if let Some(l) = last {
            if rng.chance(5) {
                // Duplicate of the last frame.
                out.push(SimFrame { dev: DEVICE_RHYTHM, time: t, data: rhythm_frame(l) });
                exp.bad_frames += 1;
            }
            if rng.chance(3) {
                // A stale frame from a little earlier.
                out.push(SimFrame { dev: DEVICE_RHYTHM, time: t, data: rhythm_frame(l.wrapping_sub(1 + rng.below(40) as u32)) });
                exp.bad_frames += 1;
            }
        }
        if rng.chance(4) {
            // Corrupted magic.
            let mut d = rhythm_frame(c);
            d[8 + rng.below(8) as usize] ^= 0x5A;
            out.push(SimFrame { dev: DEVICE_RHYTHM, time: t, data: d });
            exp.bad_frames += 1;
        }
        if rng.chance(3) {
            // Truncated.
            let mut d = rhythm_frame(c);
            let keep = 8 + rng.below(d.len() as u64 - 8) as usize;
            d.truncate(keep);
            out.push(SimFrame { dev: DEVICE_RHYTHM, time: t, data: d });
            exp.bad_frames += 1;
        }
        if last.is_some() && rng.chance(2) {
            // Dropped frames: a gap in the counter (1 to ~3000 samples).
            let max = if rng.chance(100) { 3000 } else { 40 };
            let k = 1 + rng.below(max) as u32;
            c = c.wrapping_add(k);
            exp.dropped_samples += k as u64;
            exp.discontinuities += 1;
        }
        out.push(SimFrame { dev: DEVICE_RHYTHM, time: t, data: rhythm_frame(c) });
        accepted.push(c);
        last = Some(c);
        c = c.wrapping_add(1);
    }
    (out, accepted, exp)
}

fn read_npy(path: &Path) -> (String, Vec<u8>) {
    let b = fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert_eq!(&b[..8], b"\x93NUMPY\x01\x00", "{}", path.display());
    let hl = u16::from_le_bytes([b[8], b[9]]) as usize;
    (String::from_utf8(b[10..10 + hl].to_vec()).unwrap(), b[10 + hl..].to_vec())
}

/// Reads a 1-D `.npy`, checking its header's shape against its body.
fn npy_vec<T: Copy + Default, const N: usize>(path: &Path, from: fn([u8; N]) -> T) -> Vec<T> {
    let (h, body) = read_npy(path);
    let n: usize = h.split("'shape': (").nth(1).and_then(|s| s.split(',').next()).and_then(|s| s.trim().parse().ok()).unwrap_or_else(|| panic!("shape in {h}"));
    assert_eq!(body.len(), n * N, "{}: header says {n} records, body holds {}", path.display(), body.len() / N);
    body.chunks_exact(N).map(|c| from(c.try_into().unwrap())).collect()
}

fn find(dir: &Path, name: &str, under: &str) -> PathBuf {
    fn walk(d: &Path, name: &str, under: &str, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, name, under, out);
            } else if p.file_name().is_some_and(|n| n == name) && p.to_string_lossy().contains(under) {
                out.push(p);
            }
        }
    }
    let mut v = Vec::new();
    walk(dir, name, under, &mut v);
    assert_eq!(v.len(), 1, "{name} under {under} in {}: {v:?}", dir.display());
    v.pop().unwrap()
}

/// Runs one seed; returns a description of the first broken invariant.
fn run(seed: u64) -> Result<(), String> {
    let mut rng = Rng(seed);
    let frames = 2_000 + rng.below(12_000) as usize;
    let (stream, accepted, exp) = frame_stream(&mut rng, frames);
    let target = [1usize, 7, 32, 32, 32, 64][rng.below(6) as usize];

    // The sample number of each accepted frame, as the parser numbers them (0 first).
    let mut number = Vec::with_capacity(accepted.len());
    let mut n = 0i64;
    for (i, &c) in accepted.iter().enumerate() {
        if i > 0 {
            n += c.wrapping_sub(accepted[i - 1]) as i64;
        }
        number.push(n);
    }
    let counter_at: BTreeMap<i64, u32> = number.iter().copied().zip(accepted.iter().copied()).collect();

    let root = std::env::temp_dir().join(format!("oers-sim-{}-{seed}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let (tx, rx) = crossbeam_channel::unbounded();
    let status = Arc::new(RecStatus::default());
    *status.root.lock() = root.clone();
    let st = status.clone();
    let recorder = std::thread::spawn(move || run_recorder(rx, st));
    tx.send(RecMsg::AcquisitionStarted(layout())).unwrap();

    // Acquisition: the board's block assembly over the simulated stream. Recordings open
    // and close at seeded blocks; the oracle keeps what each was sent.
    let health = Health::default();
    let mut p = parser();
    let mut pending: Option<SimFrame> = None;
    let mut it = stream.into_iter();
    let mut recordings: Vec<Vec<i64>> = Vec::new();
    let mut recording = false;
    let mut blocks = 0usize;
    loop {
        let mut b = Block::with_capacity(NCH, target);
        b.num_channels = p.num_channels();
        let r = loop {
            match assemble(&mut p, &mut pending, &mut b, target, true, 250_000_000, &health, || it.next().ok_or(())) {
                Ok(Assembled::Side(_)) => continue,
                r => break r,
            }
        };
        let end = r.is_err();
        if !b.is_empty() {
            blocks += 1;
            if b.data.len() != b.len() * NCH || b.ttl.len() != b.len() {
                return Err(format!("block {blocks} malformed: {} samples, {} values", b.len(), b.data.len()));
            }
            if !recording && rng.chance(if recordings.is_empty() { 200 } else { 20 }) {
                tx.send(RecMsg::Start).unwrap();
                recordings.push(Vec::new());
                recording = true;
            } else if recording && rng.chance(8) {
                tx.send(RecMsg::Stop).unwrap();
                recording = false;
            }
            if recording {
                recordings.last_mut().unwrap().extend((0..b.len() as i64).map(|i| b.first_sample + i));
                tx.send(RecMsg::Data(Arc::new(b))).unwrap();
            }
        }
        if end {
            break;
        }
    }
    tx.send(RecMsg::AcquisitionStopped).unwrap();
    tx.send(RecMsg::Shutdown).unwrap();
    recorder.join().map_err(|_| "recorder panicked".to_string())?;
    if let Some(e) = status.error.lock().clone() {
        return Err(format!("recorder error: {e}"));
    }

    // The parser accepted exactly the valid, advancing frames, and counted every fault.
    let got = Expected { dropped_samples: health.dropped_samples.load(Ordering::Relaxed), discontinuities: health.discontinuities.load(Ordering::Relaxed), bad_frames: health.bad_frames.load(Ordering::Relaxed) };
    if got != exp {
        return Err(format!("health {got:?}, injected {exp:?}"));
    }

    // Each recording's files, from the files alone.
    recordings.retain(|r| !r.is_empty());
    let mut dirs: Vec<PathBuf> = Vec::new();
    if root.exists() {
        let mut stack = vec![root.clone()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("recording")) {
                        dirs.push(p);
                    } else {
                        stack.push(p);
                    }
                }
            }
        }
    }
    dirs.sort_by_key(|d| d.file_name().unwrap().to_string_lossy().trim_start_matches("recording").parse::<u32>().unwrap());
    // Recordings that got no samples leave empty folders: skip those.
    dirs.retain(|d| fs::metadata(find(d, "continuous.dat", "continuous")).map(|m| m.len() > 0).unwrap_or(false));
    if dirs.len() != recordings.len() {
        return Err(format!("{} recordings with data on disk, {} sent", dirs.len(), recordings.len()));
    }
    for (k, (dir, want)) in dirs.iter().zip(&recordings).enumerate() {
        let samples = npy_vec(&find(dir, "sample_numbers.npy", "continuous"), i64::from_le_bytes);
        if &samples != want {
            let i = samples.iter().zip(want).position(|(a, b)| a != b).unwrap_or(samples.len().min(want.len()));
            return Err(format!("recording {k}: sample_numbers differ at {i} (file {:?}, sent {:?}; {} vs {} samples)", samples.get(i), want.get(i), samples.len(), want.len()));
        }
        if samples.windows(2).any(|w| w[1] <= w[0]) {
            return Err(format!("recording {k}: sample numbers not strictly increasing"));
        }
        let ts = npy_vec(&find(dir, "timestamps.npy", "continuous"), f64::from_le_bytes);
        if ts.len() != samples.len() {
            return Err(format!("recording {k}: {} timestamps for {} samples", ts.len(), samples.len()));
        }
        let dat = fs::read(find(dir, "continuous.dat", "continuous")).unwrap();
        if dat.len() != samples.len() * NCH * 2 {
            return Err(format!("recording {k}: continuous.dat holds {} bytes for {} samples", dat.len(), samples.len()));
        }
        for (i, &s) in samples.iter().enumerate() {
            let row: Vec<i16> = dat[i * NCH * 2..(i + 1) * NCH * 2].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
            if row != row_of(counter_at[&s]) {
                return Err(format!("recording {k}: the row at file index {i} isn't sample {s}'s"));
            }
        }
        // TTL edges: replaying the per-sample words gives exactly the recorded events.
        let words = npy_vec(&find(dir, "full_words.npy", "TTL"), u64::from_le_bytes);
        let ev_samples = npy_vec(&find(dir, "sample_numbers.npy", "TTL"), i64::from_le_bytes);
        let states = npy_vec(&find(dir, "states.npy", "TTL"), i16::from_le_bytes);
        let mut want_ev: Vec<(i64, i16, u64)> = Vec::new();
        let mut prev = ttl_of(counter_at[&samples[0]]);
        for &s in &samples[1..] {
            let w = ttl_of(counter_at[&s]);
            for line in 0..16 {
                if (prev ^ w) & (1 << line) != 0 {
                    want_ev.push((s, (line + 1) * if w & (1 << line) != 0 { 1 } else { -1 }, w as u64));
                }
            }
            prev = w;
        }
        let got_ev: Vec<(i64, i16, u64)> = ev_samples.iter().zip(&states).zip(&words).map(|((&s, &st), &w)| (s, st, w)).collect();
        if got_ev != want_ev {
            let i = got_ev.iter().zip(&want_ev).position(|(a, b)| a != b).unwrap_or(got_ev.len().min(want_ev.len()));
            return Err(format!("recording {k}: TTL events differ at {i}: on disk {:?}, expected {:?} ({} vs {} events)", got_ev.get(i), want_ev.get(i), got_ev.len(), want_ev.len()));
        }
        let oebin: serde_json::Value = serde_json::from_str(&fs::read_to_string(dir.join("structure.oebin")).unwrap()).map_err(|e| format!("recording {k}: structure.oebin: {e}"))?;
        let initial = oebin["events"].as_array().and_then(|a| a.iter().find_map(|e| e.get("initial_state"))).and_then(|v| v.as_u64());
        if initial != Some(ttl_of(counter_at[&samples[0]]) as u64) {
            return Err(format!("recording {k}: initial TTL state {initial:?}"));
        }
    }

    // The review reader's mapping on the last recording: every sample at its own number,
    // zeros where samples were dropped.
    if let (Some(src), Some(want)) = (status.source.lock().clone(), recordings.last()) {
        let (s0, s1) = (want[0], *want.last().unwrap() + 1);
        if src.available() != (s1 - src.first_sample) as u64 {
            return Err(format!("RecSource::available {} for a span of {}", src.available(), s1 - src.first_sample));
        }
        let f = fs::File::open(&src.dat).unwrap();
        let data = src.read(&f, s0, (s1 - s0) as usize).map_err(|e| e.to_string())?;
        for s in s0..s1 {
            let row = &data[(s - s0) as usize * NCH..(s - s0 + 1) as usize * NCH];
            let ok = match counter_at.get(&s).filter(|_| want.binary_search(&s).is_ok()) {
                Some(&c) => row == row_of(c).as_slice(),
                None => row.iter().all(|&v| v == 0),
            };
            if !ok {
                return Err(format!("RecSource::read: sample {s} is not where it belongs"));
            }
        }
    }
    let _ = fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn acquisition_to_recorder_under_seeded_faults() {
    let seeds: Vec<u64> = match std::env::var("OERS_SIM_SEED") {
        Ok(s) => vec![s.parse().expect("OERS_SIM_SEED")],
        Err(_) => (0..200).collect(),
    };
    let failures: Vec<String> = seeds.iter().filter_map(|&s| run(s).err().map(|e| format!("seed {s}: {e}"))).collect();
    assert!(failures.is_empty(), "{} of {} seeds failed (rerun one with OERS_SIM_SEED=<n>):\n{}", failures.len(), seeds.len(), failures.join("\n"));
}
