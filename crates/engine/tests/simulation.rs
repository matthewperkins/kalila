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
//! Seeds from 1000 also cap every file's size (`RLIMIT_FSIZE`), as a disk filling up
//! mid-recording would: the recorder must say so, and every file must read back correctly as
//! far as it goes.
//!
//! Everything is a function of the seed; a failure prints the seed, and
//! `KALILA_SIM_SEED=<n> cargo test -p engine --test simulation` reruns just that one.

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
    d.extend(encode_payload(
        STREAMS,
        c,
        |s, k| mix(c, 1000 + 3 * s as u32 + k as u32) as u16,
        |s, k| mix(c, 32 * s as u32 + k as u32) as u16,
        std::array::from_fn(|a| mix(c, 2000 + a as u32) as u16),
        ttl_of(c),
    ));
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
    let mut channels: Vec<ChannelInfo> = (0..32)
        .map(|i| ChannelInfo {
            name: format!("CH{}", i + 1),
            kind: ChannelKind::Electrode,
            bit_volts: 0.195,
            units: "uV".into(),
            headstage: Some(i / 16),
        })
        .collect();
    channels.extend((0..8).map(|i| ChannelInfo {
        name: format!("ADC{}", i + 1),
        kind: ChannelKind::Adc,
        bit_volts: 0.000_152_587_89,
        units: "V".into(),
        headstage: None,
    }));
    StreamLayout {
        name: "acquisition_board".into(),
        sample_rate: 30000.0,
        channels,
        headstages: vec![],
        ttl_lines: 16,
        side_streams: vec![],
        hw_clock_hz: 250e6,
    }
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
    let mut c: u32 = if rng.chance(300) {
        u32::MAX - rng.below(5000) as u32
    } else {
        rng.next() as u32
    };
    let mut last: Option<u32> = None;
    let mut t = 0u64;
    while accepted.len() < frames {
        t += 8333;
        // Other devices' frames (memory monitor, BNO) interleaved.
        if rng.chance(20) {
            out.push(SimFrame {
                dev: 0x0202,
                time: t,
                data: vec![0u8; 16],
            });
        }
        if let Some(l) = last {
            if rng.chance(5) {
                // Duplicate of the last frame.
                out.push(SimFrame {
                    dev: DEVICE_RHYTHM,
                    time: t,
                    data: rhythm_frame(l),
                });
                exp.bad_frames += 1;
            }
            if rng.chance(3) {
                // A stale frame from a little earlier.
                out.push(SimFrame {
                    dev: DEVICE_RHYTHM,
                    time: t,
                    data: rhythm_frame(l.wrapping_sub(1 + rng.below(40) as u32)),
                });
                exp.bad_frames += 1;
            }
        }
        if rng.chance(4) {
            // Corrupted magic.
            let mut d = rhythm_frame(c);
            d[8 + rng.below(8) as usize] ^= 0x5A;
            out.push(SimFrame {
                dev: DEVICE_RHYTHM,
                time: t,
                data: d,
            });
            exp.bad_frames += 1;
        }
        if rng.chance(3) {
            // Truncated.
            let mut d = rhythm_frame(c);
            let keep = 8 + rng.below(d.len() as u64 - 8) as usize;
            d.truncate(keep);
            out.push(SimFrame {
                dev: DEVICE_RHYTHM,
                time: t,
                data: d,
            });
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
        out.push(SimFrame {
            dev: DEVICE_RHYTHM,
            time: t,
            data: rhythm_frame(c),
        });
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
    (
        String::from_utf8(b[10..10 + hl].to_vec()).unwrap(),
        b[10 + hl..].to_vec(),
    )
}

/// Reads a 1-D `.npy` as a reader would: the records its header claims. The body must
/// hold them; with `exact`, nothing more (after a disk error, a partial tail may follow).
fn npy_vec<T: Copy + Default, const N: usize>(
    path: &Path,
    from: fn([u8; N]) -> T,
    exact: bool,
) -> Result<Vec<T>, String> {
    let (h, body) = read_npy(path);
    let n: usize = h
        .split("'shape': (")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| format!("no shape in {h}"))?;
    if body.len() < n * N || (exact && body.len() != n * N) {
        return Err(format!(
            "{}: header says {n} records, body holds {} bytes",
            path.display(),
            body.len()
        ));
    }
    Ok(body[..n * N]
        .chunks_exact(N)
        .map(|c| from(c.try_into().unwrap()))
        .collect())
}

/// Caps the size of any file this process writes (`RLIMIT_FSIZE`), so writes past it fail
/// with EFBIG as a full disk would fail them; None restores the limit.
fn file_size_limit(bytes: Option<u64>) {
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
        let mut r: libc::rlimit = std::mem::zeroed();
        libc::getrlimit(libc::RLIMIT_FSIZE, &mut r);
        r.rlim_cur = bytes.map_or(r.rlim_max, |b| b as libc::rlim_t);
        assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &r), 0);
    }
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

/// Runs one seed; returns a description of the first broken invariant. With `disk_limit`,
/// no file may grow past that many bytes: the recorder must report the error, and every
/// file must still read back correctly as far as its header (or length) says.
fn run(seed: u64, disk_limit: Option<u64>) -> Result<(), String> {
    let faulty = disk_limit.is_some();
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

    let root = std::env::temp_dir().join(format!("kalila-sim-{}-{seed}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let (tx, rx) = crossbeam_channel::unbounded();
    let status = Arc::new(RecStatus::default());
    *status.root.lock() = root.clone();
    let st = status.clone();
    fs::create_dir_all(&root).unwrap();
    file_size_limit(disk_limit);
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
            match assemble(&mut p, &mut pending, &mut b, target, true, 250_000_000, &health, || {
                it.next().ok_or(())
            }) {
                Ok(Assembled::Side(_)) => continue,
                r => break r,
            }
        };
        let end = r.is_err();
        if !b.is_empty() {
            blocks += 1;
            if b.data.len() != b.len() * NCH || b.ttl.len() != b.len() {
                return Err(format!(
                    "block {blocks} malformed: {} samples, {} values",
                    b.len(),
                    b.data.len()
                ));
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
                recordings
                    .last_mut()
                    .unwrap()
                    .extend((0..b.len() as i64).map(|i| b.first_sample + i));
                tx.send(RecMsg::Data(Arc::new(b))).unwrap();
            }
        }
        if end {
            break;
        }
    }
    tx.send(RecMsg::AcquisitionStopped).unwrap();
    tx.send(RecMsg::Shutdown).unwrap();
    let joined = recorder.join();
    file_size_limit(None);
    joined.map_err(|_| "recorder panicked".to_string())?;
    let rec_error = status.error.lock().clone();
    match (&rec_error, disk_limit) {
        (Some(e), None) => return Err(format!("recorder error: {e}")),
        // Loud: a recording too big for the limit must have been reported.
        (_, Some(lim))
            if status.errors.load(Ordering::SeqCst) == 0
                && recordings.iter().any(|r| (r.len() * NCH * 2) as u64 > lim) =>
        {
            return Err(format!("no recorder error with files capped at {lim} bytes"));
        }
        _ => {}
    }

    // The parser accepted exactly the valid, advancing frames, and counted every fault.
    let got = Expected {
        dropped_samples: health.dropped_samples.load(Ordering::Relaxed),
        discontinuities: health.discontinuities.load(Ordering::Relaxed),
        bad_frames: health.bad_frames.load(Ordering::Relaxed),
    };
    if got != exp {
        return Err(format!("health {got:?}, injected {exp:?}"));
    }

    // Each recording's files, from the files alone: one folder per Start, in order.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                if p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("recording"))
                {
                    dirs.push(p);
                } else {
                    stack.push(p);
                }
            }
        }
    }
    dirs.sort_by_key(|d| {
        d.file_name()
            .unwrap()
            .to_string_lossy()
            .trim_start_matches("recording")
            .parse::<u32>()
            .unwrap()
    });
    if dirs.len() != recordings.len() {
        return Err(format!(
            "{} recording folders, {} recordings started",
            dirs.len(),
            recordings.len()
        ));
    }
    for (k, (dir, want)) in dirs.iter().zip(&recordings).enumerate() {
        let samples = npy_vec(
            &find(dir, "sample_numbers.npy", "continuous"),
            i64::from_le_bytes,
            !faulty,
        )?;
        let ts = npy_vec(&find(dir, "timestamps.npy", "continuous"), f64::from_le_bytes, !faulty)?;
        let dat = fs::read(find(dir, "continuous.dat", "continuous")).unwrap();
        let rows = dat.len() / (NCH * 2);
        if !faulty && (&samples != want || ts.len() != samples.len() || dat.len() != samples.len() * NCH * 2) {
            let i = samples
                .iter()
                .zip(want)
                .position(|(a, b)| a != b)
                .unwrap_or(samples.len().min(want.len()));
            return Err(format!(
                "recording {k}: {} sample numbers ({} sent, first difference at {i}), {} timestamps, {} bytes of data",
                samples.len(),
                want.len(),
                ts.len(),
                dat.len()
            ));
        }
        // Each header counts every whole record that reached the disk (up to those sent),
        // not just those at the last checkpoint.
        for (name, under, size, sent) in [
            ("sample_numbers.npy", "continuous", 8, want.len()),
            ("timestamps.npy", "continuous", 8, want.len()),
        ] {
            let path = find(dir, name, under);
            let (h, body) = read_npy(&path);
            let n: usize = h
                .split("'shape': (")
                .nth(1)
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.trim().parse().ok())
                .unwrap();
            if n != sent.min(body.len() / size) {
                return Err(format!(
                    "recording {k}: {name} header says {n}, {} whole records on disk, {sent} sent",
                    body.len() / size
                ));
            }
        }
        // What's there is right, as far as it goes: the sample numbers are a prefix of
        // those sent, and every whole row of data belongs to its sample number.
        if !want.starts_with(&samples) {
            return Err(format!(
                "recording {k}: sample_numbers isn't a prefix of the samples sent"
            ));
        }
        for (i, &s) in samples.iter().take(rows).enumerate() {
            let row: Vec<i16> = dat[i * NCH * 2..(i + 1) * NCH * 2]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
            if row != row_of(counter_at[&s]) {
                return Err(format!("recording {k}: the row at file index {i} isn't sample {s}'s"));
            }
        }
        // TTL edges: replaying the per-sample words of the samples sent gives the recorded
        // events (all of them; after a disk error, a prefix).
        let words = npy_vec(&find(dir, "full_words.npy", "TTL"), u64::from_le_bytes, !faulty)?;
        let ev_samples = npy_vec(&find(dir, "sample_numbers.npy", "TTL"), i64::from_le_bytes, !faulty)?;
        let states = npy_vec(&find(dir, "states.npy", "TTL"), i16::from_le_bytes, !faulty)?;
        let mut want_ev: Vec<(i64, i16, u64)> = Vec::new();
        if let Some(&s0) = want.first() {
            let mut prev = ttl_of(counter_at[&s0]);
            for &s in &want[1..] {
                let w = ttl_of(counter_at[&s]);
                for line in 0..16 {
                    if (prev ^ w) & (1 << line) != 0 {
                        want_ev.push((s, (line + 1) * if w & (1 << line) != 0 { 1 } else { -1 }, w as u64));
                    }
                }
                prev = w;
            }
        }
        let n_ev = ev_samples.len().min(states.len()).min(words.len());
        let got_ev: Vec<(i64, i16, u64)> = (0..n_ev).map(|i| (ev_samples[i], states[i], words[i])).collect();
        let ttl_ok = if faulty {
            want_ev.starts_with(&got_ev)
        } else {
            got_ev == want_ev && n_ev == ev_samples.len()
        };
        if !ttl_ok {
            let i = got_ev
                .iter()
                .zip(&want_ev)
                .position(|(a, b)| a != b)
                .unwrap_or(got_ev.len().min(want_ev.len()));
            return Err(format!(
                "recording {k}: TTL events differ at {i}: on disk {:?}, expected {:?} ({} vs {} events)",
                got_ev.get(i),
                want_ev.get(i),
                got_ev.len(),
                want_ev.len()
            ));
        }
        // structure.oebin: always after a clean run; after a disk error, if it was written.
        match fs::read_to_string(dir.join("structure.oebin")) {
            Ok(j) => {
                let oebin: serde_json::Value =
                    serde_json::from_str(&j).map_err(|e| format!("recording {k}: structure.oebin: {e}"))?;
                let initial = oebin["events"]
                    .as_array()
                    .and_then(|a| a.iter().find_map(|e| e.get("initial_state")))
                    .and_then(|v| v.as_u64());
                let want_initial = want.first().map_or(0, |&s| ttl_of(counter_at[&s]) as u64);
                if !faulty && initial != Some(want_initial) {
                    return Err(format!(
                        "recording {k}: initial TTL state {initial:?}, expected {want_initial}"
                    ));
                }
            }
            Err(e) if !faulty => return Err(format!("recording {k}: structure.oebin: {e}")),
            Err(_) => {}
        }
    }

    // The review reader's mapping on the last recording: every sample at its own number,
    // zeros where samples were dropped.
    let last_with_data = recordings.iter().rev().find(|r| !r.is_empty());
    if let (false, Some(src), Some(want)) = (faulty, status.source.lock().clone(), last_with_data) {
        let (s0, s1) = (want[0], *want.last().unwrap() + 1);
        if src.available() != (s1 - src.first_sample) as u64 {
            return Err(format!(
                "RecSource::available {} for a span of {}",
                src.available(),
                s1 - src.first_sample
            ));
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
    // Seeds 0..200 run clean; 1000..1060 with every file capped at a seeded size (a disk
    // that fills up mid-recording). One test function, as the file-size cap is per process.
    let seeds: Vec<(u64, Option<u64>)> = match std::env::var("KALILA_SIM_SEED") {
        Ok(s) => vec![{
            let seed = s.parse().expect("KALILA_SIM_SEED");
            (seed, disk_limit_for(seed))
        }],
        Err(_) => (0..200).chain(1000..1060).map(|s| (s, disk_limit_for(s))).collect(),
    };
    let failures: Vec<String> = seeds
        .iter()
        .filter_map(|&(s, lim)| {
            run(s, lim).err().map(|e| {
                format!(
                    "seed {s}{}: {e}",
                    lim.map_or(String::new(), |l| format!(" (files capped at {l} B)"))
                )
            })
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} seeds failed (rerun one with KALILA_SIM_SEED=<n>):\n{}",
        failures.len(),
        seeds.len(),
        failures.join("\n")
    );
}

/// Seeds from 1000 run with files capped at 2 KB .. 1 MB (a recording here is up to ~1 MB).
fn disk_limit_for(seed: u64) -> Option<u64> {
    (seed >= 1000).then(|| 2048 + Rng(seed ^ 0xD15C).below(1 << 20))
}
