//! Min/max pyramids of the recording, for scroll-back review of long stretches.
//!
//! A pyramid holds, per channel, the (min, max) of every bucket of `BUCKET` samples and of
//! every coarser level (×4 each), so a window spanning minutes is drawn from a few thousand
//! buckets per channel instead of millions of samples. Two kinds are built live while a
//! recording runs, from the acquisition blocks:
//! - the **raw** pyramid: every channel, plus the OR of the TTL words per bucket;
//! - one **band** pyramid per trace window: that window's electrodes, band-passed with its
//!   corners and its display CAR (a min/max of a filtered signal cannot be made from the
//!   raw pyramid). Changing the band replaces the pyramid, and the stretch recorded so far is
//!   **backfilled** from `continuous.dat`, newest first, at idle priority.
//!
//! Everything here is a consumer: the builder thread reads blocks from its own lossy queue
//! and the recording file read-only. It never touches acquisition or the recorder. Gaps
//! (dropped blocks, the recording's first block, a filter's warm-up) are holes in the
//! coverage that the backfill closes.

use crate::dsp::{BandSpec, FilterBank};
use crate::record::{RecSource, RecStatus};
use crate::PipelineStats;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use parking_lot::Mutex;
use rhythm::{Block, ChannelKind, StreamLayout};
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Samples per bucket of the finest level.
pub const BUCKET: u64 = 64;
/// Levels (each ×4 coarser than the previous): 64 … 1048576 samples (35 s at 30 kHz).
pub const LEVELS: usize = 8;
const EMPTY: [i16; 2] = [i16::MAX, i16::MIN];
/// Backfill chunk (samples, a whole number of buckets) and the pause after each, which caps
/// the read rate.
const CHUNK: u64 = 960 * BUCKET;
const CHUNK_PAUSE: Duration = Duration::from_millis(10);
/// Backfill waits this long after a band change (so dragging a corner starts one rebuild).
const SETTLE: Duration = Duration::from_secs(2);
/// Capacity is reserved in steps of this many finest buckets (about 1 h at 30 kHz).
const RESERVE_BUCKETS: usize = 1 << 20;

/// Which band pyramid a trace window wants.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct BandKey {
    /// Corners as bits (`BandSpec` holds f64s).
    pub low_bits: u64,
    pub high_bits: u64,
    /// Global channel indices in the pyramid, ascending.
    pub chans: Vec<usize>,
    /// Display CAR: (reference channels, channels it is subtracted from), global indices, or
    /// None when off.
    pub car: Option<(Vec<usize>, Vec<usize>)>,
}

impl BandKey {
    pub fn band(&self) -> BandSpec {
        BandSpec { low_hz: f64::from_bits(self.low_bits), high_hz: f64::from_bits(self.high_bits), order: 4 }
    }
    /// Filter settling time (samples) read before a backfilled chunk; see `lead_in_s`.
    fn lead(&self, fs: f64) -> u64 {
        let b = self.band();
        let s = if b.low_hz > 0.0 { (3.0 / b.low_hz).max(0.05) } else { 0.05 };
        ((s * fs) as u64).div_ceil(BUCKET) * BUCKET
    }
}

/// One level: `[min, max]` per (bucket, channel), channels interleaved; untouched buckets
/// hold `EMPTY`. Coarser buckets are merged incrementally from the finer ones, so a bucket
/// at the live edge holds the aggregate of what has arrived so far.
struct Level {
    data: Vec<[i16; 2]>,
    ttl: Vec<u16>,
}

pub struct Pyramid {
    pub nch: usize,
    with_ttl: bool,
    levels: Vec<Level>,
    /// Sample ranges `[start, end)` relative to the recording start whose finest buckets are
    /// filled, sorted and merged.
    covered: Vec<(u64, u64)>,
}

impl Pyramid {
    pub fn new(nch: usize, with_ttl: bool) -> Self {
        Self { nch: nch.max(1), with_ttl, levels: (0..LEVELS).map(|_| Level { data: Vec::new(), ttl: Vec::new() }).collect(), covered: Vec::new() }
    }

    pub fn bytes(&self) -> usize {
        self.levels.iter().map(|l| l.data.len() * 4 + l.ttl.len() * 2).sum()
    }

    pub fn covered(&self) -> &[(u64, u64)] {
        &self.covered
    }

    /// Samples covered within `[start, end)`.
    pub fn covered_in(&self, start: u64, end: u64) -> u64 {
        self.covered.iter().map(|&(a, b)| b.min(end).saturating_sub(a.max(start))).sum()
    }

    /// The newest hole in `[0, end)`, as `[a, b)`, if any.
    pub fn newest_hole(&self, end: u64) -> Option<(u64, u64)> {
        let mut hole_end = end;
        for &(a, b) in self.covered.iter().rev() {
            if b < hole_end {
                return Some((b, hole_end));
            }
            hole_end = hole_end.min(a);
            if hole_end == 0 {
                return None;
            }
        }
        (hole_end > 0).then_some((0, hole_end))
    }

    fn add_covered(&mut self, start: u64, end: u64) {
        let i = self.covered.partition_point(|&(_, b)| b < start);
        let mut j = i;
        let (mut s, mut e) = (start, end);
        while j < self.covered.len() && self.covered[j].0 <= end {
            s = s.min(self.covered[j].0);
            e = e.max(self.covered[j].1);
            j += 1;
        }
        self.covered.splice(i..j, [(s, e)]);
    }

    /// Fold `n` samples (interleaved over `nch` channels) starting at sample `start` of the
    /// recording. `start` and `n` must be multiples of `BUCKET` (a trailing partial bucket is
    /// ignored). `ttl` holds one word per sample, or is empty.
    pub fn ingest(&mut self, start: u64, data: &[i16], ttl: &[u16]) {
        let nch = self.nch;
        let n = (data.len() / nch) as u64 / BUCKET * BUCKET;
        if n == 0 || !start.is_multiple_of(BUCKET) {
            return;
        }
        let b0 = start / BUCKET;
        let b1 = b0 + n / BUCKET;
        self.grow(b1);
        let mut mm = vec![EMPTY; nch];
        for b in b0..b1 {
            mm.fill(EMPTY);
            let off = ((b - b0) * BUCKET) as usize * nch;
            for s in 0..BUCKET as usize {
                let row = &data[off + s * nch..off + (s + 1) * nch];
                for (m, &v) in mm.iter_mut().zip(row) {
                    m[0] = m[0].min(v);
                    m[1] = m[1].max(v);
                }
            }
            let word = if self.with_ttl && !ttl.is_empty() {
                let o = ((b - b0) * BUCKET) as usize;
                ttl[o..(o + BUCKET as usize).min(ttl.len())].iter().fold(0u16, |a, &w| a | w)
            } else {
                0
            };
            let mut idx = b as usize;
            for (k, l) in self.levels.iter_mut().enumerate() {
                let base = idx * nch;
                if k == 0 {
                    l.data[base..base + nch].copy_from_slice(&mm);
                } else {
                    for (d, m) in l.data[base..base + nch].iter_mut().zip(&mm) {
                        d[0] = d[0].min(m[0]);
                        d[1] = d[1].max(m[1]);
                    }
                }
                if self.with_ttl {
                    l.ttl[idx] |= word;
                }
                idx >>= 2;
            }
        }
        self.add_covered(start, start + n);
    }

    /// Make room for finest buckets `[0, b1)` on every level.
    fn grow(&mut self, b1: u64) {
        let nch = self.nch;
        let mut need = b1 as usize;
        for (k, l) in self.levels.iter_mut().enumerate() {
            if l.data.len() < need * nch {
                let step = (RESERVE_BUCKETS >> (2 * k)).max(1024);
                let cap = need.div_ceil(step) * step;
                l.data.reserve_exact(cap * nch - l.data.len());
                l.data.resize(need * nch, EMPTY);
                if self.with_ttl {
                    l.ttl.reserve_exact(cap - l.ttl.len());
                    l.ttl.resize(need, 0);
                }
            }
            need = need.div_ceil(4);
        }
    }

    /// Per-column (min, max) of every channel for `cols` columns over `[t0, t0 + span)`
    /// (recording-relative samples; `t0` may be negative), as `cols × nch` with `EMPTY`
    /// where nothing is covered, plus the OR of the TTL words per column. Columns at least
    /// one bucket wide come out exact to within a bucket at each edge.
    pub fn columns(&self, t0: i64, span: u64, cols: u32) -> (Vec<[i16; 2]>, Vec<u16>) {
        let nch = self.nch;
        let cols = cols.max(1) as usize;
        let mut out = vec![EMPTY; cols * nch];
        let mut words = vec![0u16; cols];
        let spc = span / cols as u64;
        // The coarsest level whose buckets fit in a column (at least the finest).
        let k = (0..LEVELS).rev().find(|&k| BUCKET << (2 * k) <= spc.max(BUCKET)).unwrap_or(0);
        let bsz = BUCKET << (2 * k);
        let l = &self.levels[k];
        let nb = (l.data.len() / nch) as u64;
        for c in 0..cols {
            let s0 = t0 + (c as u64 * span / cols as u64) as i64;
            let s1 = (t0 + ((c + 1) as u64 * span / cols as u64) as i64).max(s0 + 1);
            if s1 <= 0 {
                continue;
            }
            let b0 = (s0.max(0) as u64 / bsz).min(nb);
            let b1 = ((s1 as u64).div_ceil(bsz)).min(nb);
            let o = &mut out[c * nch..(c + 1) * nch];
            for b in b0..b1 {
                let base = b as usize * nch;
                for (m, v) in o.iter_mut().zip(&l.data[base..base + nch]) {
                    m[0] = m[0].min(v[0]);
                    m[1] = m[1].max(v[1]);
                }
                if self.with_ttl {
                    words[c] |= l.ttl[b as usize];
                }
            }
        }
        (out, words)
    }
}

/// The pyramids of the recording being written, shared with the GUI.
#[derive(Default)]
pub struct Store {
    /// The recording they belong to (`None` before the first block of one is seen).
    pub src: Option<RecSource>,
    pub raw: Option<Pyramid>,
    /// Per trace window id: the band pyramid it asked for.
    pub bands: std::collections::HashMap<u32, (BandKey, Pyramid)>,
    /// Per trace window id: the band pyramid it wants (set by the GUI).
    pub want: std::collections::HashMap<u32, BandKey>,
    /// Recording-relative sample up to which live blocks have been folded.
    pub live_end: u64,
    /// Backfill in progress for some pyramid (holes remain below `live_end`).
    pub backfilling: bool,
}

impl Store {
    pub fn bytes(&self) -> usize {
        self.raw.as_ref().map_or(0, |p| p.bytes()) + self.bands.values().map(|(_, p)| p.bytes()).sum::<usize>()
    }
    /// The band pyramid serving window `view`, if it is the one the window wants.
    pub fn band(&self, view: u32) -> Option<&Pyramid> {
        let (k, p) = self.bands.get(&view)?;
        (self.want.get(&view) == Some(k)).then_some(p)
    }
}

/// Live filter state of one band pyramid.
struct BandLive {
    key: BandKey,
    bank: FilterBank,
    car_ref: Vec<usize>,
    car_sub: Vec<bool>,
    /// Samples still to feed before the filter is settled (after a reset or a gap).
    warm: u64,
    /// Backfill may start (after the band settled); None = not before the next change.
    backfill_at: Option<Instant>,
}

/// Fold `n` raw samples (all `nch_all` channels interleaved) into the band's channel subset,
/// filtered and CAR-corrected, as i16 in raw LSB units. Returns `n × chans.len()`.
fn band_filter(bl: &mut BandLive, data: &[i16], nch_all: usize, out: &mut Vec<i16>) {
    let chans = &bl.key.chans;
    let n = data.len() / nch_all.max(1);
    out.clear();
    out.reserve(n * chans.len());
    let mut row = vec![0f32; chans.len()];
    for s in 0..n {
        let src = &data[s * nch_all..(s + 1) * nch_all];
        for (i, &c) in chans.iter().enumerate() {
            row[i] = bl.bank.tick(i, src[c] as f32);
        }
        if !bl.car_ref.is_empty() {
            let mean = bl.car_ref.iter().map(|&i| row[i]).sum::<f32>() / bl.car_ref.len() as f32;
            for (i, v) in row.iter_mut().enumerate() {
                if bl.car_sub[i] {
                    *v -= mean;
                }
            }
        }
        out.extend(row.iter().map(|&v| v.round().clamp(-32768.0, 32767.0) as i16));
    }
}

fn band_live(key: &BandKey, fs: f64) -> BandLive {
    let nchp = key.chans.len();
    let pos = |c: usize| key.chans.iter().position(|&x| x == c);
    let (car_ref, car_sub) = match &key.car {
        Some((r, s)) => (r.iter().filter_map(|&c| pos(c)).collect(), (0..nchp).map(|i| s.contains(&key.chans[i])).collect()),
        None => (Vec::new(), vec![false; nchp]),
    };
    BandLive { bank: FilterBank::new(key.band().sections(fs), nchp), car_ref, car_sub, warm: key.lead(fs), backfill_at: Some(Instant::now() + SETTLE), key: key.clone() }
}

/// The builder thread: folds live blocks into the pyramids and backfills holes from the
/// recording file. Runs at idle priority: it yields to everything else on the machine.
pub fn pyramid_thread(rx: Receiver<Arc<Block>>, store: Arc<Mutex<Store>>, status: Arc<RecStatus>, pipe: Arc<PipelineStats>, layout: StreamLayout) {
    use thread_priority::{NormalThreadSchedulePolicy, ThreadPriority, ThreadSchedulePolicy};
    if let Err(e) = thread_priority::set_thread_priority_and_policy(thread_priority::thread_native_id(), ThreadPriority::Min, ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Idle)) {
        log::warn!("pyramid builder: idle scheduling not set ({e:?}); using normal priority");
    }
    let fs = layout.sample_rate;
    let nch = layout.num_channels();
    let mut bands: Vec<(u32, BandLive)> = Vec::new();
    // Live staging: whole samples since `stage_at`, folded a bucket at a time.
    let mut stage: Vec<i16> = Vec::new();
    let mut stage_ttl: Vec<u16> = Vec::new();
    let mut stage_at: u64 = 0;
    let mut fb = Vec::new();
    let mut file: Option<(std::path::PathBuf, File)> = None;
    let mut last_bp = pipe.rec_backpressure_us.load(Ordering::Relaxed);
    let mut bp_at = Instant::now();
    let mut last_log = Instant::now();
    let mut last_chunk = Instant::now();
    loop {
        let block = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(b) => Some(b),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // The recording: a new one resets everything.
        let src = status.source.lock().clone();
        {
            let mut st = store.lock();
            if st.src.as_ref().map(|s| &s.dat) != src.as_ref().map(|s| &s.dat) {
                st.src = src.clone();
                st.raw = src.as_ref().map(|s| Pyramid::new(s.nch, true));
                st.bands.clear();
                st.live_end = 0;
                bands.clear();
                stage.clear();
                stage_ttl.clear();
                stage_at = 0;
                file = None;
            }
            // Band pyramids the windows want.
            let want: Vec<(u32, BandKey)> = st.want.iter().map(|(v, k)| (*v, k.clone())).collect();
            bands.retain(|(v, b)| want.iter().any(|(w, k)| w == v && *k == b.key));
            st.bands.retain(|v, (k, _)| want.iter().any(|(w, wk)| w == v && wk == k));
            if st.src.is_some() {
                for (v, k) in &want {
                    if k.chans.is_empty() || k.chans.iter().any(|&c| c >= nch) {
                        continue;
                    }
                    if !bands.iter().any(|(bv, _)| bv == v) {
                        bands.push((*v, band_live(k, fs)));
                        st.bands.insert(*v, (k.clone(), Pyramid::new(k.chans.len(), false)));
                    }
                }
            }
        }
        let Some(src) = src else { continue };
        if src.nch != nch {
            continue;
        }
        // Live blocks.
        if let Some(b) = block
            && b.num_channels == nch
            && b.first_sample >= src.first_sample
        {
            {
                let at = (b.first_sample - src.first_sample) as u64;
                let expected = stage_at + (stage.len() / nch) as u64;
                if at != expected {
                    // A gap (dropped block) or the recording's first block: restart staging
                    // at the next bucket boundary; the hole is backfilled from the file.
                    stage.clear();
                    stage_ttl.clear();
                    let skip = (BUCKET - at % BUCKET) % BUCKET;
                    stage_at = at + skip;
                    for (_, bl) in bands.iter_mut() {
                        bl.warm = bl.key.lead(fs);
                    }
                    if skip < b.len() as u64 {
                        stage.extend_from_slice(&b.data[skip as usize * nch..]);
                        stage_ttl.extend_from_slice(&b.ttl[skip as usize..]);
                    }
                } else {
                    stage.extend_from_slice(&b.data);
                    stage_ttl.extend_from_slice(&b.ttl);
                }
                let whole = (stage.len() / nch) as u64 / BUCKET * BUCKET;
                if whole > 0 {
                    let n = whole as usize;
                    // Filter outside the lock; the GUI reads the store between frames.
                    let mut filtered: Vec<(u32, u64, Vec<i16>)> = Vec::new();
                    for (v, bl) in bands.iter_mut() {
                        band_filter(bl, &stage[..n * nch], nch, &mut fb);
                        // Not settled yet: feed the filter, keep the hole for the backfill.
                        let skip = (bl.warm.min(whole)).div_ceil(BUCKET) * BUCKET;
                        bl.warm = bl.warm.saturating_sub(whole);
                        if skip < whole {
                            let nchp = bl.key.chans.len();
                            filtered.push((*v, skip, fb[skip as usize * nchp..].to_vec()));
                        }
                    }
                    let mut st = store.lock();
                    if let Some(raw) = st.raw.as_mut() {
                        raw.ingest(stage_at, &stage[..n * nch], &stage_ttl[..n]);
                    }
                    for (v, skip, data) in &filtered {
                        if let Some((_, p)) = st.bands.get_mut(v) {
                            p.ingest(stage_at + skip, data, &[]);
                        }
                    }
                    st.live_end = stage_at + whole;
                    drop(st);
                    stage.drain(..n * nch);
                    stage_ttl.drain(..n);
                    stage_at += whole;
                }
            }
        }
        // Between blocks: backfill one chunk of the newest hole of one pyramid, at most one
        // per `CHUNK_PAUSE`. Not while the recorder is under backpressure: the disk comes first.
        if last_chunk.elapsed() < CHUNK_PAUSE {
            continue;
        }
        let bp = pipe.rec_backpressure_us.load(Ordering::Relaxed);
        if bp != last_bp {
            last_bp = bp;
            bp_at = Instant::now();
        }
        if bp_at.elapsed() < Duration::from_secs(2) {
            continue;
        }
        let avail = available(&src);
        let job = {
            let st = store.lock();
            let end = st.live_end.min(avail);
            let raw_hole = st.raw.as_ref().and_then(|p| p.newest_hole(end)).map(|h| (None, h));
            let band_hole = bands.iter().filter(|(_, bl)| bl.backfill_at.is_some_and(|t| t <= Instant::now())).find_map(|(v, bl)| {
                let (k, p) = st.bands.get(v)?;
                (*k == bl.key).then(|| p.newest_hole(end).map(|h| (Some(*v), h)))?
            });
            raw_hole.or(band_hole)
        };
        let Some((view, (h0, h1))) = job else {
            let mut st = store.lock();
            if st.backfilling {
                st.backfilling = false;
                log::info!("pyramids complete: {:.0} MB", st.bytes() as f64 / 1e6);
            }
            continue;
        };
        // The chunk at the top of the hole, bucket-aligned.
        let c1 = h1 / BUCKET * BUCKET;
        let c0 = c1.saturating_sub(CHUNK).max(h0.div_ceil(BUCKET) * BUCKET);
        if c1 <= c0 {
            // A hole smaller than a bucket (the recording's tail): nothing to do yet.
            continue;
        }
        let lead = match view {
            Some(v) => bands.iter().find(|(bv, _)| *bv == v).map_or(0, |(_, bl)| bl.key.lead(fs)).min(c0),
            None => 0,
        };
        let f = match &file {
            Some((p, f)) if *p == src.dat => f,
            _ => match File::open(&src.dat) {
                Ok(f) => &file.insert((src.dat.clone(), f)).1,
                Err(e) => {
                    log::warn!("pyramid backfill: opening {}: {e}", src.dat.display());
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
            },
        };
        let n = (c1 - c0 + lead) as usize;
        let mut data = vec![0i16; n * nch];
        if let Err(e) = f.read_exact_at(bytemuck::cast_slice_mut(&mut data), (c0 - lead) * nch as u64 * 2) {
            log::warn!("pyramid backfill: reading {}: {e}", src.dat.display());
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        let mut st = store.lock();
        st.backfilling = true;
        match view {
            None => {
                if let Some(raw) = st.raw.as_mut() {
                    raw.ingest(c0, &data, &[]);
                }
            }
            Some(v) => {
                if let Some((_, bl)) = bands.iter_mut().find(|(bv, _)| *bv == v) {
                    // A fresh filter for the chunk, settled over the lead-in.
                    let mut tmp = band_live(&bl.key, fs);
                    band_filter(&mut tmp, &data, nch, &mut fb);
                    let nchp = bl.key.chans.len();
                    if let Some((_, p)) = st.bands.get_mut(&v) {
                        p.ingest(c0, &fb[lead as usize * nchp..], &[]);
                    }
                }
            }
        }
        if last_log.elapsed() > Duration::from_secs(10) {
            last_log = Instant::now();
            log::debug!("pyramid backfill: {} at {:.1} s, {:.0} MB in RAM", view.map_or("raw".to_string(), |v| format!("band of view {v}")), c0 as f64 / fs, st.bytes() as f64 / 1e6);
        }
        drop(st);
        last_chunk = Instant::now();
    }
}

/// Samples of the main stream readable so far.
fn available(src: &RecSource) -> u64 {
    std::fs::metadata(&src.dat).map(|m| m.len() / (2 * src.nch.max(1) as u64)).unwrap_or(0)
}

/// The channels of a band pyramid for a window: the electrodes of its headstages.
pub fn band_channels(layout: &StreamLayout, headstages: &[String]) -> Vec<usize> {
    layout
        .channels
        .iter()
        .enumerate()
        .filter(|(_, c)| c.kind == ChannelKind::Electrode && (headstages.is_empty() || c.headstage.is_some_and(|h| layout.headstages.get(h).is_some_and(|hs| headstages.contains(&hs.name)))))
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_merge_up_and_columns_read_back() {
        let mut p = Pyramid::new(2, true);
        // 4 buckets: channel 0 = sample index, channel 1 = -(sample index); TTL bit 0 in bucket 2.
        let n = 4 * BUCKET as usize;
        let data: Vec<i16> = (0..n).flat_map(|s| [s as i16, -(s as i16)]).collect();
        let ttl: Vec<u16> = (0..n).map(|s| (s / BUCKET as usize == 2) as u16).collect();
        p.ingest(0, &data, &ttl);
        assert_eq!(p.covered(), &[(0, 4 * BUCKET)]);
        // One column per bucket.
        let (c, w) = p.columns(0, 4 * BUCKET, 4);
        assert_eq!(c[0], [0, 63]);
        assert_eq!(c[1], [-63, 0]);
        assert_eq!(c[2 * 2], [128, 191]);
        assert_eq!(w, vec![0, 0, 1, 0]);
        // One column over all four: the level-1 bucket aggregates them.
        let (c, w) = p.columns(0, 4 * BUCKET, 1);
        assert_eq!(c[0], [0, 255]);
        assert_eq!(w, vec![1]);
        // Columns before the recording or past its end are empty.
        let (c, _) = p.columns(-(BUCKET as i64), 2 * BUCKET, 2);
        assert_eq!(c[0], EMPTY);
        assert_eq!(c[2], [0, 63]);
        let (c, _) = p.columns(4 * BUCKET as i64, BUCKET, 1);
        assert_eq!(c[0], EMPTY);
    }

    #[test]
    fn every_level_matches_brute_force() {
        let nch = 3;
        let n = 2000 * BUCKET as usize;
        let mut p = Pyramid::new(nch, true);
        let mut seed = 12345u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 16) as i16
        };
        let data: Vec<i16> = (0..n * nch).map(|_| rnd()).collect();
        let ttl: Vec<u16> = (0..n).map(|s| ((s / 777) % 4) as u16).collect();
        // Ingested in uneven, out-of-order pieces (as live blocks and backfill chunks).
        let pieces = [(500, 300), (0, 500), (1200, 800), (800, 400)];
        for (b0, nb) in pieces {
            let (s0, s1) = (b0 * BUCKET as usize, (b0 + nb) * BUCKET as usize);
            p.ingest(s0 as u64, &data[s0 * nch..s1 * nch], &ttl[s0..s1]);
        }
        assert_eq!(p.covered(), &[(0, n as u64)]);
        for (t0, cols) in [(0, 2000u32), (0, 500), (0, 125), (0, 31), (0, 7), (0, 2), (40_000, 300), (40_000, 1000), (12_345, 50)] {
            let m = n - t0;
            let (c, w) = p.columns(t0 as i64, m as u64, cols);
            for col in 0..cols as usize {
                let s0 = t0 + col * m / cols as usize;
                let s1 = t0 + (col + 1) * m / cols as usize;
                // Buckets at the level used may overhang the column by less than a bucket.
                let spc = (m / cols as usize) as u64;
                let k = (0..LEVELS).rev().find(|&k| BUCKET << (2 * k) <= spc).unwrap();
                let bsz = (BUCKET << (2 * k)) as usize;
                let (e0, e1) = (s0 / bsz * bsz, (s1.div_ceil(bsz) * bsz).min(n));
                let mut word = 0;
                for ch in 0..nch {
                    let (mut mn, mut mx) = (i16::MAX, i16::MIN);
                    for s in e0..e1 {
                        mn = mn.min(data[s * nch + ch]);
                        mx = mx.max(data[s * nch + ch]);
                    }
                    assert_eq!(c[col * nch + ch], [mn, mx], "cols {cols} col {col} ch {ch} level {k}");
                }
                for s in e0..e1 {
                    word |= ttl[s];
                }
                assert_eq!(w[col], word, "cols {cols} col {col} level {k}");
            }
        }
    }

    #[test]
    fn holes_are_found_newest_first_and_close() {
        let mut p = Pyramid::new(1, false);
        let one = vec![1i16; BUCKET as usize];
        p.ingest(2 * BUCKET, &one, &[]);
        p.ingest(5 * BUCKET, &one, &[]);
        assert_eq!(p.covered(), &[(2 * BUCKET, 3 * BUCKET), (5 * BUCKET, 6 * BUCKET)]);
        assert_eq!(p.newest_hole(7 * BUCKET), Some((6 * BUCKET, 7 * BUCKET)));
        assert_eq!(p.newest_hole(6 * BUCKET), Some((3 * BUCKET, 5 * BUCKET)));
        p.ingest(3 * BUCKET, &vec![1i16; 2 * BUCKET as usize], &[]);
        assert_eq!(p.covered(), &[(2 * BUCKET, 6 * BUCKET)]);
        assert_eq!(p.newest_hole(6 * BUCKET), Some((0, 2 * BUCKET)));
        p.ingest(0, &vec![1i16; 2 * BUCKET as usize], &[]);
        assert_eq!(p.newest_hole(6 * BUCKET), None);
        assert_eq!(p.covered_in(BUCKET, 10 * BUCKET), 5 * BUCKET);
        // Unaligned or partial input is ignored.
        p.ingest(1, &one, &[]);
        p.ingest(6 * BUCKET, &one[..10], &[]);
        assert_eq!(p.covered(), &[(0, 6 * BUCKET)]);
    }

    #[test]
    fn band_filter_subtracts_the_car_and_keeps_lsb_units() {
        let key = BandKey { low_bits: 0f64.to_bits(), high_bits: 0f64.to_bits(), chans: vec![1, 2, 3], car: Some((vec![1, 2], vec![3])) };
        let mut bl = band_live(&key, 30000.0);
        // No corners: the filter is identity. 4 channels, 2 samples.
        let data = [0i16, 10, 20, 100, 0, -10, -30, 5];
        let mut out = Vec::new();
        band_filter(&mut bl, &data, 4, &mut out);
        assert_eq!(out, vec![10, 20, 85, -10, -30, 25]);
    }
}
