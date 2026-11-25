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
//! **RAM budget.** Levels are stored in chunks of `CHUNK_SAMPLES` (about 9 min at 30 kHz),
//! aligned across levels. When the pyramids together exceed `Store::budget`, the oldest
//! chunks of the finest level are released (then of level 1), in every pyramid alike; the
//! chunk at the live edge is always kept, and coarser levels are never released. Spans the
//! released levels would serve (under about 28 s at 814 columns) are then read from the file
//! directly by the GUI: `columns` returns `None` for them.
//!
//! Everything here is a consumer: the builder thread reads blocks from its own lossy queue
//! and the recording file read-only. It never touches acquisition or the recorder. Gaps
//! (dropped blocks, the recording's first block, a filter's warm-up) are holes in the
//! coverage that the backfill closes.

use crate::PipelineStats;
use crate::dsp::{BandSpec, CarPlan, FilterBank};
use crate::record::{RecSource, RecStatus};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use parking_lot::Mutex;
use rhythm::{Block, ChannelKind, StreamLayout};
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::Ordering;
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
/// Samples per storage chunk, on every level (2^24: 9.3 min at 30 kHz). A whole number of
/// buckets at the coarsest level.
#[cfg(not(test))]
const CHUNK_SAMPLES: u64 = 1 << 24;
#[cfg(test)]
const CHUNK_SAMPLES: u64 = 1 << 20;
const _: () = assert!(CHUNK_SAMPLES.is_multiple_of(BUCKET << (2 * (LEVELS as u64 - 1))));
/// Levels whose old chunks may be released under the RAM budget. The spans they serve are
/// short enough for the GUI to read directly (≤ `DIRECT_MAX_S` there).
pub const RELEASABLE: usize = 2;
/// Default RAM budget for all pyramids together.
pub const DEFAULT_BUDGET: usize = 1_500_000_000;

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
        BandSpec {
            low_hz: f64::from_bits(self.low_bits),
            high_hz: f64::from_bits(self.high_bits),
            order: 4,
        }
    }
    /// Filter settling time (samples) read before a backfilled chunk; see `lead_in_s`.
    fn lead(&self, fs: f64) -> u64 {
        let b = self.band();
        let s = if b.low_hz > 0.0 {
            (3.0 / b.low_hz).max(0.05)
        } else {
            0.05
        };
        ((s * fs) as u64).div_ceil(BUCKET) * BUCKET
    }
}

/// One level: `[min, max]` per (bucket, channel), channels interleaved, in chunks of
/// `per` buckets allocated when first written; untouched buckets hold `EMPTY`. Coarser
/// buckets are merged incrementally from the finer ones, so a bucket at the live edge holds
/// the aggregate of what has arrived so far.
struct Level {
    /// Buckets per chunk.
    per: usize,
    data: Vec<Option<Box<[[i16; 2]]>>>,
    ttl: Vec<Option<Box<[u16]>>>,
    /// Chunks below this have been released (and are no longer written).
    floor: usize,
}

impl Level {
    fn new(k: usize) -> Self {
        Self {
            per: (CHUNK_SAMPLES / (BUCKET << (2 * k))) as usize,
            data: Vec::new(),
            ttl: Vec::new(),
            floor: 0,
        }
    }
    /// Bucket `b`'s `nch` values, allocating its chunk; None if the chunk was released.
    fn slot(&mut self, b: usize, nch: usize, with_ttl: bool) -> Option<(&mut [[i16; 2]], Option<&mut u16>)> {
        let (c, o) = (b / self.per, b % self.per);
        if c < self.floor {
            return None;
        }
        if self.data.len() <= c {
            self.data.resize_with(c + 1, || None);
            self.ttl.resize_with(c + 1, || None);
        }
        let per = self.per;
        let d = self.data[c].get_or_insert_with(|| vec![EMPTY; per * nch].into_boxed_slice());
        let t = if with_ttl {
            Some(&mut self.ttl[c].get_or_insert_with(|| vec![0; per].into_boxed_slice())[o])
        } else {
            None
        };
        Some((&mut d[o * nch..(o + 1) * nch], t))
    }
    /// Bucket `b`'s values and TTL word, if its chunk holds data.
    fn get(&self, b: usize, nch: usize) -> Option<(&[[i16; 2]], u16)> {
        let (c, o) = (b / self.per, b % self.per);
        let d = self.data.get(c)?.as_ref()?;
        let t = self.ttl.get(c).and_then(|t| t.as_ref()).map_or(0, |t| t[o]);
        Some((&d[o * nch..(o + 1) * nch], t))
    }
    fn bytes(&self) -> usize {
        self.data.iter().flatten().map(|d| d.len() * 4).sum::<usize>()
            + self.ttl.iter().flatten().map(|t| t.len() * 2).sum::<usize>()
    }
    /// Release the chunks below `floor`.
    fn release_below(&mut self, floor: usize) {
        self.floor = self.floor.max(floor);
        for c in 0..self.floor.min(self.data.len()) {
            self.data[c] = None;
            self.ttl[c] = None;
        }
    }
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
        Self {
            nch: nch.max(1),
            with_ttl,
            levels: (0..LEVELS).map(Level::new).collect(),
            covered: Vec::new(),
        }
    }

    pub fn bytes(&self) -> usize {
        self.levels.iter().map(Level::bytes).sum()
    }

    /// Release level `k`'s chunks below chunk `floor` (see `Store::enforce_budget`).
    fn release_below(&mut self, k: usize, floor: usize) {
        self.levels[k].release_below(floor);
    }

    pub fn covered(&self) -> &[(u64, u64)] {
        &self.covered
    }

    /// Samples covered within `[start, end)`.
    pub fn covered_in(&self, start: u64, end: u64) -> u64 {
        self.covered
            .iter()
            .map(|&(a, b)| b.min(end).saturating_sub(a.max(start)))
            .sum()
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
        let with_ttl = self.with_ttl;
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
                ttl[o..(o + BUCKET as usize).min(ttl.len())]
                    .iter()
                    .fold(0u16, |a, &w| a | w)
            } else {
                0
            };
            let mut idx = b as usize;
            for (k, l) in self.levels.iter_mut().enumerate() {
                // A released chunk (old, over the RAM budget) is not written again.
                if let Some((d, t)) = l.slot(idx, nch, with_ttl) {
                    if k == 0 {
                        d.copy_from_slice(&mm);
                    } else {
                        for (d, m) in d.iter_mut().zip(&mm) {
                            d[0] = d[0].min(m[0]);
                            d[1] = d[1].max(m[1]);
                        }
                    }
                    if let Some(t) = t {
                        *t |= word;
                    }
                }
                idx >>= 2;
            }
        }
        self.add_covered(start, start + n);
    }

    /// Per-column (min, max) of every channel for `cols` columns over `[t0, t0 + span)`
    /// (recording-relative samples; `t0` may be negative), as `cols × nch` with `EMPTY`
    /// where nothing is covered, plus the OR of the TTL words per column. Columns at least
    /// one bucket wide come out exact to within a bucket at each edge.
    ///
    /// If the level this span needs has been released for part of it (RAM budget), the result
    /// is None, so the caller reads the file instead; with `coarser_ok` (spans too long to
    /// read), the finest level still held is used, with buckets wider than a column.
    pub fn columns(&self, t0: i64, span: u64, cols: u32, coarser_ok: bool) -> Option<(Vec<[i16; 2]>, Vec<u16>)> {
        let nch = self.nch;
        let cols = cols.max(1) as usize;
        let spc = span / cols as u64;
        // The coarsest level whose buckets fit in a column (at least the finest).
        let mut k = (0..LEVELS)
            .rev()
            .find(|&k| BUCKET << (2 * k) <= spc.max(BUCKET))
            .unwrap_or(0);
        while (t0.max(0) as u64) < self.levels[k].floor as u64 * CHUNK_SAMPLES {
            if !coarser_ok || k + 1 == LEVELS {
                return None;
            }
            k += 1;
        }
        let bsz = BUCKET << (2 * k);
        let l = &self.levels[k];
        let mut out = vec![EMPTY; cols * nch];
        let mut words = vec![0u16; cols];
        for c in 0..cols {
            let s0 = t0 + (c as u64 * span / cols as u64) as i64;
            let s1 = (t0 + ((c + 1) as u64 * span / cols as u64) as i64).max(s0 + 1);
            if s1 <= 0 {
                continue;
            }
            let b0 = s0.max(0) as u64 / bsz;
            let b1 = (s1 as u64).div_ceil(bsz);
            let o = &mut out[c * nch..(c + 1) * nch];
            for b in b0..b1 {
                let Some((vals, word)) = l.get(b as usize, nch) else {
                    continue;
                };
                for (m, v) in o.iter_mut().zip(vals) {
                    m[0] = m[0].min(v[0]);
                    m[1] = m[1].max(v[1]);
                }
                words[c] |= word;
            }
        }
        Some((out, words))
    }
}

/// The pyramids of the recording being written, shared with the GUI.
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
    /// RAM budget for all pyramids together (bytes); see the module docs.
    pub budget: usize,
    /// Per releasable level: chunks below this are released in every pyramid.
    floor: [usize; RELEASABLE],
    /// The budget couldn't be met: warned, and not again until it is met once more.
    over_warned: bool,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            src: None,
            raw: None,
            bands: Default::default(),
            want: Default::default(),
            live_end: 0,
            backfilling: false,
            budget: DEFAULT_BUDGET,
            floor: [0; RELEASABLE],
            over_warned: false,
        }
    }
}

impl Store {
    pub fn bytes(&self) -> usize {
        self.raw.as_ref().map_or(0, |p| p.bytes()) + self.bands.values().map(|(_, p)| p.bytes()).sum::<usize>()
    }
    fn pyramids_mut(&mut self) -> impl Iterator<Item = &mut Pyramid> {
        self.raw.iter_mut().chain(self.bands.values_mut().map(|(_, p)| p))
    }

    /// A new recording: everything released and the floors back at its start.
    fn reset(&mut self, src: Option<RecSource>) {
        self.raw = src.as_ref().map(|s| Pyramid::new(s.nch, true));
        self.src = src;
        self.bands.clear();
        self.live_end = 0;
        self.floor = [0; RELEASABLE];
        self.over_warned = false;
    }

    /// A new pyramid starts with the floors the others have.
    fn add_band(&mut self, view: u32, key: BandKey) {
        let mut p = Pyramid::new(key.chans.len(), false);
        for (k, &f) in self.floor.iter().enumerate() {
            p.release_below(k, f);
        }
        self.bands.insert(view, (key, p));
    }

    /// Over the budget: release the oldest chunks of the finest level (then of level 1) in
    /// every pyramid, never the chunk at the live edge.
    pub fn enforce_budget(&mut self) {
        // The chunk holding the newest sample.
        let live = (self.live_end.saturating_sub(1) / CHUNK_SAMPLES) as usize;
        if self.bytes() <= self.budget {
            self.over_warned = false;
            return;
        }
        while self.bytes() > self.budget {
            let Some(k) = (0..RELEASABLE).find(|&k| self.floor[k] < live) else {
                if !self.over_warned {
                    self.over_warned = true;
                    log::warn!(
                        "review pyramids: {:.2} GB, over the {:.2} GB budget with only the coarse levels and the newest stretch left",
                        self.bytes() as f64 / 1e9,
                        self.budget as f64 / 1e9
                    );
                }
                return;
            };
            self.floor[k] += 1;
            let f = self.floor[k];
            for p in self.pyramids_mut() {
                p.release_below(k, f);
            }
            let fs = self.src.as_ref().map_or(30000.0, |s| s.sample_rate);
            log::info!(
                "review pyramids: level {k} released before {:.1} min of the recording ({:.0} MB left)",
                (f as u64 * CHUNK_SAMPLES) as f64 / fs / 60.0,
                self.bytes() as f64 / 1e6
            );
        }
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
    /// The display CAR resolved for this band's channels (None when off or empty).
    car: Option<CarPlan>,
    /// Scratch: the filtered block before rounding.
    rows: Vec<f32>,
    /// Samples still to feed before the filter is settled (after a reset or a gap).
    warm: u64,
    /// Backfill may start (after the band settled); None = not before the next change.
    backfill_at: Option<Instant>,
}

/// Fold `n` raw samples (all `nch_all` channels interleaved) into the band's channel subset,
/// filtered and CAR-corrected, as i16 in raw LSB units. Returns `n × chans.len()`.
///
/// The CAR is taken from the raw samples and subtracted before the filter (as the spike
/// detector and the slope trigger do); the filter is linear and the same on every channel,
/// so this equals filtering first, to float rounding.
fn band_filter(bl: &mut BandLive, data: &[i16], nch_all: usize, out: &mut Vec<i16>) {
    let nchp = bl.key.chans.len();
    let n = data.len() / nch_all.max(1);
    bl.rows.resize(n * nchp, 0.0);
    bl.bank
        .process(data, nch_all, Some(&bl.key.chans), 1.0, bl.car.as_ref(), &mut bl.rows);
    out.clear();
    out.extend(bl.rows.iter().map(|&v| v.round().clamp(-32768.0, 32767.0) as i16));
}

fn band_live(key: &BandKey, fs: f64) -> BandLive {
    let nchp = key.chans.len();
    let car = key.car.as_ref().and_then(|(r, s)| {
        let pos = |c: usize| key.chans.contains(&c).then_some(c);
        CarPlan::new(
            r.iter().filter_map(|&c| pos(c)).collect(),
            key.chans.iter().map(|c| s.contains(c)).collect(),
        )
    });
    BandLive {
        bank: FilterBank::new(key.band().sections(fs), nchp),
        car,
        rows: Vec::new(),
        warm: key.lead(fs),
        backfill_at: Some(Instant::now() + SETTLE),
        key: key.clone(),
    }
}

/// The builder thread: folds live blocks into the pyramids and backfills holes from the
/// recording file. Runs at idle priority: it yields to everything else on the machine.
pub fn pyramid_thread(
    rx: Receiver<Arc<Block>>,
    store: Arc<Mutex<Store>>,
    status: Arc<RecStatus>,
    pipe: Arc<PipelineStats>,
    layout: StreamLayout,
) {
    if let Err(e) = crate::sched::set_idle() {
        log::warn!("pyramid builder: idle scheduling not set ({e}); using normal priority");
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
    // The recording that already existed when this acquisition run began. Sample numbers
    // restart at 0 with every run, so this run's blocks must never be folded into it.
    let earlier = status.source.lock().as_ref().map(|s| s.dat.clone());
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
                st.reset(src.clone());
                bands.clear();
                stage.clear();
                stage_ttl.clear();
                stage_at = 0;
                file = None;
            }
            // Band pyramids the windows want.
            let want: Vec<(u32, BandKey)> = st.want.iter().map(|(v, k)| (*v, k.clone())).collect();
            bands.retain(|(v, b)| want.iter().any(|(w, k)| w == v && *k == b.key));
            st.bands
                .retain(|v, (k, _)| want.iter().any(|(w, wk)| w == v && wk == k));
            if st.src.is_some() {
                for (v, k) in &want {
                    if k.chans.is_empty() || k.chans.iter().any(|&c| c >= nch) {
                        continue;
                    }
                    if !bands.iter().any(|(bv, _)| bv == v) {
                        bands.push((*v, band_live(k, fs)));
                        st.add_band(*v, k.clone());
                    }
                }
            }
        }
        let Some(src) = src else { continue };
        if src.nch != nch {
            continue;
        }
        let avail = available(&src);
        // Live blocks go in only while they belong to the recording: one started in this
        // run and still being written, or (after Stop) blocks that reached its file. Blocks
        // after the end are not in the file, and would also push the recording's own detail
        // out of the RAM budget.
        let this_run = earlier.as_ref() != Some(&src.dat);
        let writing = this_run && status.recording.load(Ordering::Relaxed);
        if let Some(b) = block
            && this_run
            && b.num_channels == nch
            && b.first_sample >= src.first_sample
            && (writing || (b.first_sample - src.first_sample) as u64 + b.len() as u64 <= avail)
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
                    st.enforce_budget();
                    drop(st);
                    stage.drain(..n * nch);
                    stage_ttl.drain(..n);
                    stage_at += whole;
                }
            }
        }
        // Not being written: the file is the whole recording, so its end is the live edge.
        // A tail the live path skipped is backfilled; blocks it took between Stop and the
        // recorder acting on it (sent, but not written) fall beyond the edge.
        if !writing {
            let mut st = store.lock();
            if st.live_end != avail {
                st.live_end = avail;
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
        let job = {
            let st = store.lock();
            let end = st.live_end.min(avail);
            let raw_hole = st.raw.as_ref().and_then(|p| p.newest_hole(end)).map(|h| (None, h));
            let band_hole = bands
                .iter()
                .filter(|(_, bl)| bl.backfill_at.is_some_and(|t| t <= Instant::now()))
                .find_map(|(v, bl)| {
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
            Some(v) => bands
                .iter()
                .find(|(bv, _)| *bv == v)
                .map_or(0, |(_, bl)| bl.key.lead(fs))
                .min(c0),
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
        // Pyramid positions are sample numbers from the start (not file offsets: see gaps).
        let data = match src.read(f, src.first_sample + (c0 - lead) as i64, n) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("pyramid backfill: reading {}: {e}", src.dat.display());
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        };
        // Filter before taking the lock, as the live path does: every GUI frame takes it, and
        // this thread runs at idle priority (a chunk with a long lead-in filters for 0.1 s+).
        let band_nchp = view.and_then(|v| bands.iter().find(|(bv, _)| *bv == v)).map(|(_, bl)| {
            // A fresh filter for the chunk, settled over the lead-in.
            let mut tmp = band_live(&bl.key, fs);
            band_filter(&mut tmp, &data, nch, &mut fb);
            bl.key.chans.len()
        });
        let mut st = store.lock();
        st.backfilling = true;
        match (view, band_nchp) {
            (None, _) => {
                if let Some(raw) = st.raw.as_mut() {
                    raw.ingest(c0, &data, &[]);
                }
            }
            (Some(v), Some(nchp)) => {
                if let Some((_, p)) = st.bands.get_mut(&v) {
                    p.ingest(c0, &fb[lead as usize * nchp..], &[]);
                }
            }
            (Some(_), None) => {}
        }
        st.enforce_budget();
        if last_log.elapsed() > Duration::from_secs(10) {
            last_log = Instant::now();
            log::debug!(
                "pyramid backfill: {} at {:.1} s, {:.0} MB in RAM",
                view.map_or("raw".to_string(), |v| format!("band of view {v}")),
                c0 as f64 / fs,
                st.bytes() as f64 / 1e6
            );
        }
        drop(st);
        last_chunk = Instant::now();
    }
}

/// Samples of the main stream readable so far.
pub fn available_samples(src: &RecSource) -> u64 {
    available(src)
}

fn available(src: &RecSource) -> u64 {
    src.available()
}

/// The channels of a band pyramid for a window: the electrodes of its headstages.
pub fn band_channels(layout: &StreamLayout, headstages: &[String]) -> Vec<usize> {
    layout
        .channels
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.kind == ChannelKind::Electrode
                && (headstages.is_empty()
                    || c.headstage
                        .is_some_and(|h| layout.headstages.get(h).is_some_and(|hs| headstages.contains(&hs.name))))
        })
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
        let (c, w) = p.columns(0, 4 * BUCKET, 4, false).unwrap();
        assert_eq!(c[0], [0, 63]);
        assert_eq!(c[1], [-63, 0]);
        assert_eq!(c[2 * 2], [128, 191]);
        assert_eq!(w, vec![0, 0, 1, 0]);
        // One column over all four: the level-1 bucket aggregates them.
        let (c, w) = p.columns(0, 4 * BUCKET, 1, false).unwrap();
        assert_eq!(c[0], [0, 255]);
        assert_eq!(w, vec![1]);
        // Columns before the recording or past its end are empty.
        let (c, _) = p.columns(-(BUCKET as i64), 2 * BUCKET, 2, false).unwrap();
        assert_eq!(c[0], EMPTY);
        assert_eq!(c[2], [0, 63]);
        let (c, _) = p.columns(4 * BUCKET as i64, BUCKET, 1, false).unwrap();
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
        for (t0, cols) in [
            (0, 2000u32),
            (0, 500),
            (0, 125),
            (0, 31),
            (0, 7),
            (0, 2),
            (40_000, 300),
            (40_000, 1000),
            (12_345, 50),
        ] {
            let m = n - t0;
            let (c, w) = p.columns(t0 as i64, m as u64, cols, false).unwrap();
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
                for &t in &ttl[e0..e1] {
                    word |= t;
                }
                assert_eq!(w[col], word, "cols {cols} col {col} level {k}");
            }
        }
    }

    #[test]
    fn the_budget_releases_old_fine_levels_first() {
        let nch = 2;
        let chunks = 5u64;
        let n = (chunks * CHUNK_SAMPLES) as usize;
        let data: Vec<i16> = (0..n * nch).map(|i| ((i * 7919) % 20011) as i16 - 10000).collect();
        let mut st = Store {
            raw: Some(Pyramid::new(nch, false)),
            ..Default::default()
        };
        // Fill in one-chunk steps, as live blocks would, with room for about 1.5 chunks of
        // every level.
        let per_chunk = {
            let mut p = Pyramid::new(nch, false);
            p.ingest(0, &data[..CHUNK_SAMPLES as usize * nch], &[]);
            p.bytes()
        };
        st.budget = per_chunk * 3 / 2;
        for c in 0..chunks {
            let (s0, s1) = ((c * CHUNK_SAMPLES) as usize, ((c + 1) * CHUNK_SAMPLES) as usize);
            st.raw
                .as_mut()
                .unwrap()
                .ingest(s0 as u64, &data[s0 * nch..s1 * nch], &[]);
            st.live_end = s1 as u64;
            st.enforce_budget();
            assert!(st.bytes() <= st.budget, "chunk {c}: {} > {}", st.bytes(), st.budget);
        }
        // Level 0 is released up to the live chunk; level 1 kept.
        assert_eq!(st.floor[0], chunks as usize - 1);
        let p = st.raw.as_ref().unwrap();
        let cols = 100u32;
        // An old span at finest-level resolution: not served (the GUI reads the file).
        assert!(p.columns(0, 100 * BUCKET, cols, false).is_none());
        // Unless the caller can't read the file for it: then the finest level still held.
        let (c, _) = p.columns(0, 100 * BUCKET, cols, true).unwrap();
        assert!(c[0][0] <= c[0][1], "level 1 data, wider than a column");
        // An old span that a coarser level serves: still exact.
        let span = 64 * 4 * BUCKET * cols as u64;
        let (c, _) = p.columns(0, span, cols, false).unwrap();
        let bsz = (BUCKET << 6) as usize;
        for ch in 0..nch {
            let s1 = span as usize / cols as usize;
            let (mn, mx) = (0..s1.div_ceil(bsz) * bsz)
                .map(|s| data[s * nch + ch])
                .fold((i16::MAX, i16::MIN), |(a, b), v| (a.min(v), b.max(v)));
            assert_eq!(c[ch], [mn, mx]);
        }
        // The newest stretch at full resolution.
        let t0 = (chunks - 1) * CHUNK_SAMPLES;
        let (c, _) = p.columns(t0 as i64, 100 * BUCKET, cols, false).unwrap();
        let s = t0 as usize;
        let (mn, mx) = (s..s + BUCKET as usize)
            .map(|s| data[s * nch])
            .fold((i16::MAX, i16::MIN), |(a, b), v| (a.min(v), b.max(v)));
        assert_eq!(c[0], [mn, mx]);
        // A late backfill into a released chunk doesn't bring it back.
        let before = st.bytes();
        st.raw.as_mut().unwrap().ingest(0, &data[..BUCKET as usize * nch], &[]);
        assert_eq!(st.bytes(), before);
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
        let key = BandKey {
            low_bits: 0f64.to_bits(),
            high_bits: 0f64.to_bits(),
            chans: vec![1, 2, 3],
            car: Some((vec![1, 2], vec![3])),
        };
        let mut bl = band_live(&key, 30000.0);
        // No corners: the filter is identity. 4 channels, 2 samples.
        let data = [0i16, 10, 20, 100, 0, -10, -30, 5];
        let mut out = Vec::new();
        band_filter(&mut bl, &data, 4, &mut out);
        assert_eq!(out, vec![10, 20, 85, -10, -30, 25]);
    }

    /// Small xorshift for seeded test data.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        fn gauss(&mut self) -> f64 {
            let (u1, u2) = (self.next().max(1e-12), self.next());
            (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
        }
    }

    /// A synthetic block: `n` rows of `stride` channels (the first `nel` are electrodes):
    /// noise, a common-mode artefact (a 50 Hz drift plus a wideband burst) and a few spikes.
    fn synthetic(n: usize, stride: usize, nel: usize, seed: u64) -> Vec<i16> {
        let mut r = Rng(seed);
        let mut data = vec![0i16; n * stride];
        for t in 0..n {
            let burst = if (t / 900) % 7 == 3 { 600.0 * r.gauss() } else { 0.0 };
            let common = 1500.0 * (2.0 * std::f64::consts::PI * 50.0 * t as f64 / 30000.0).sin() + burst;
            for ch in 0..stride {
                let mut v = if ch < nel {
                    common + 20.0 * r.gauss()
                } else {
                    1000.0 * r.gauss()
                };
                // A biphasic spike on a few electrodes.
                if ch < nel && ch % 3 == 1 {
                    let k = (t + 97 * ch) % 4000;
                    if k < 20 {
                        v -= 500.0 * (-((k as f64 - 6.0) / 3.0).powi(2)).exp();
                    }
                }
                data[t * stride + ch] = v.round().clamp(-32768.0, 32767.0) as i16;
            }
        }
        data
    }

    /// The same CAR + band-pass through the three CPU implementations, on one block:
    /// `dsp_thread`'s loop (CAR in LSB, then ×0.195, then filter; copied from lib.rs),
    /// `band_filter` (filter in LSB, then CAR, rounded to i16), and the app's slope trigger
    /// (`frame.rs`: CAR in LSB, ×bit_volts, then a one-channel `FilterBank`; copied).
    #[test]
    fn car_and_band_pass_agree_across_the_three_cpu_paths() {
        use crate::dsp::{BandSpec, Car, FilterBank};
        use rhythm::ELECTRODE_UV_PER_BIT;
        let (fs, stride, nel, n) = (30000.0, 20, 12, 90000);
        let data = synthetic(n, stride, nel, 0x5eed);
        let reference = vec![2usize, 3, 5];
        let subtract = vec![3usize, 6, 8];
        let car = Car {
            reference: reference.clone(),
            subtract: subtract.clone(),
        };

        // dsp_thread.
        let mask = car.mask(nel);
        let mut bank = FilterBank::new(BandSpec::SPIKES.sections(fs), nel);
        let mut dsp = vec![0f32; n * nel];
        for t in 0..n {
            let r = &data[t * stride..(t + 1) * stride];
            let c = car.mean(r);
            for ch in 0..nel {
                let x = r[ch] as f32 - if mask[ch] { c } else { 0.0 };
                dsp[t * nel + ch] = bank.tick(ch, x * ELECTRODE_UV_PER_BIT);
            }
        }

        // band_filter, on a subset of the electrodes holding the whole CAR.
        let chans = vec![1usize, 2, 3, 5, 6, 8];
        let key = BandKey {
            low_bits: 300f64.to_bits(),
            high_bits: 6000f64.to_bits(),
            chans: chans.clone(),
            car: Some((reference, subtract)),
        };
        let mut bl = band_live(&key, fs);
        let mut pyr = Vec::new();
        band_filter(&mut bl, &data, stride, &mut pyr);

        // The slope trigger's computation, per channel.
        let mut worst_slope = 0f32;
        let mut worst_pyr = 0f32;
        let mut peak = 0f32;
        let settled = 6000; // 0.2 s
        for (i, &c) in chans.iter().enumerate() {
            let mut one = FilterBank::new(BandSpec::SPIKES.sections(fs), 1);
            let car_c = Some(&car).filter(|car| car.subtract.contains(&c));
            for t in 0..n {
                let row = &data[t * stride..(t + 1) * stride];
                let x = row[c] as f32 - car_c.map_or(0.0, |car| car.mean(row));
                let slope = one.tick(0, x * ELECTRODE_UV_PER_BIT);
                let d = dsp[t * nel + c];
                if t >= settled {
                    worst_slope = worst_slope.max((slope - d).abs());
                    let lsb = pyr[t * chans.len() + i] as f32;
                    worst_pyr = worst_pyr.max((lsb - d / ELECTRODE_UV_PER_BIT).abs());
                    peak = peak.max(d.abs());
                }
            }
        }
        eprintln!("oracle: peak {peak:.1} uV, slope vs dsp {worst_slope:e} uV, band_filter vs dsp {worst_pyr:.4} LSB");
        // Same operations in the same order: identical.
        assert_eq!(worst_slope, 0.0);
        // band_filter rounds to whole LSB (0.5) and filters before the CAR: float error only.
        assert!(worst_pyr <= 0.5 + 0.02, "{worst_pyr}");
        assert!(peak > 50.0, "the spikes must come through");
    }

    /// The per-sample `band_filter` as it was before the block API: filter in LSB, then the
    /// CAR over the filtered values.
    fn band_filter_per_sample(key: &BandKey, bank: &mut FilterBank, data: &[i16], nch_all: usize) -> Vec<i16> {
        let chans = &key.chans;
        let pos = |c: usize| chans.iter().position(|&x| x == c);
        let (car_ref, car_sub): (Vec<usize>, Vec<bool>) = match &key.car {
            Some((r, s)) => (
                r.iter().filter_map(|&c| pos(c)).collect(),
                (0..chans.len()).map(|i| s.contains(&chans[i])).collect(),
            ),
            None => (Vec::new(), vec![false; chans.len()]),
        };
        let mut out = Vec::new();
        let mut row = vec![0f32; chans.len()];
        for src in data.chunks_exact(nch_all) {
            for (i, &c) in chans.iter().enumerate() {
                row[i] = bank.tick(i, src[c] as f32);
            }
            if !car_ref.is_empty() {
                let mean = car_ref.iter().map(|&i| row[i]).sum::<f32>() / car_ref.len() as f32;
                for (i, v) in row.iter_mut().enumerate() {
                    if car_sub[i] {
                        *v -= mean;
                    }
                }
            }
            out.extend(row.iter().map(|&v| v.round().clamp(-32768.0, 32767.0) as i16));
        }
        out
    }

    /// The block `band_filter` against the old per-sample one, over blocks of uneven sizes
    /// (filter state carries across them). The CAR now comes before the filter, so the two
    /// differ by float rounding only: a whole LSB at most, after rounding to i16.
    #[test]
    fn band_filter_block_matches_the_per_sample_one() {
        let (fs, stride) = (30000.0, 20);
        let data = synthetic(60000, stride, 12, 7);
        let key = BandKey {
            low_bits: 300f64.to_bits(),
            high_bits: 6000f64.to_bits(),
            chans: vec![1, 2, 3, 5, 6, 8],
            car: Some((vec![2, 3, 5, 9], vec![3, 6, 8])),
        };
        let mut bl = band_live(&key, fs);
        let mut old = band_live(&key, fs);
        let (mut worst, mut differing, mut total) = (0i32, 0usize, 0usize);
        let mut at = 0;
        for len in [64usize, 1, 997, 4096, 32, 10000, 3, 20000, 24807] {
            let blk = &data[at * stride..(at + len) * stride];
            at += len;
            let mut new = Vec::new();
            band_filter(&mut bl, blk, stride, &mut new);
            let o = band_filter_per_sample(&key, &mut old.bank, blk, stride);
            assert_eq!(new.len(), o.len());
            for (a, b) in new.iter().zip(&o) {
                let d = (*a as i32 - *b as i32).abs();
                worst = worst.max(d);
                differing += (d != 0) as usize;
                total += 1;
            }
        }
        eprintln!("band_filter block vs per-sample: worst {worst} LSB, {differing} of {total} values differ");
        assert!(worst <= 1);
    }

    /// Old against new, 30 kHz, 32 of 64 channels with a CAR, in 2048-sample stages.
    /// `cargo test --release -p engine band_filter_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn band_filter_speed() {
        let (fs, stride, secs) = (30000.0, 64, 10usize);
        let data = synthetic(30000 * secs, stride, 60, 11);
        let key = BandKey {
            low_bits: 300f64.to_bits(),
            high_bits: 6000f64.to_bits(),
            chans: (0..32).collect(),
            car: Some(((0..32).collect(), (0..32).collect())),
        };
        let block = 2048;
        let t0 = Instant::now();
        let mut old = band_live(&key, fs);
        let mut sink = 0i64;
        for b in data.chunks(block * stride) {
            sink += band_filter_per_sample(&key, &mut old.bank, b, stride)[0] as i64;
        }
        let t_old = t0.elapsed();
        let t0 = Instant::now();
        let mut bl = band_live(&key, fs);
        let mut out = Vec::new();
        for b in data.chunks(block * stride) {
            band_filter(&mut bl, b, stride, &mut out);
            sink += out[0] as i64;
        }
        let t_new = t0.elapsed();
        eprintln!(
            "band_filter, 32 of 64 ch, {secs} s: per-sample {t_old:?}, block {t_new:?} (x{:.2}) [{sink}]",
            t_old.as_secs_f64() / t_new.as_secs_f64()
        );
    }
}
