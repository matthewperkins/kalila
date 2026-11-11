//! Scroll-back review: reading the recording back while it is written.
//!
//! A reader thread serves requests for a stretch of the main stream (raw int16, all channels
//! interleaved, as in `continuous.dat`) plus the TTL words of the visible part, rebuilt from the
//! recording's TTL edge files. The recorder flushes about once a second, so the newest second
//! or so is not readable yet; that is the right edge of review. Reads never touch the
//! acquisition or recorder threads: they only see what the page cache already holds.

use crossbeam_channel::{Receiver, Sender};
use engine::record::RecSource;
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// One stretch to read: `n` samples from absolute sample `from` (clamped to what exists), and
/// the TTL words for `[words_from, words_from + words_n)`.
pub struct Req {
    pub view: u32,
    pub generation: u64,
    pub src: RecSource,
    pub from: i64,
    pub n: usize,
    pub words_from: i64,
    pub words_n: usize,
}

pub struct Resp {
    pub view: u32,
    pub generation: u64,
    /// First sample actually read and the samples (n × nch interleaved).
    pub from: i64,
    pub data: Vec<i16>,
    /// One TTL word per sample of the requested word range (0 before the first edge).
    pub words: Vec<u16>,
    pub error: Option<String>,
}

pub struct Reader {
    pub tx: Sender<Req>,
    pub rx: Receiver<Resp>,
}

impl Reader {
    pub fn spawn() -> Self {
        let (tx, req_rx) = crossbeam_channel::unbounded::<Req>();
        let (resp_tx, rx) = crossbeam_channel::unbounded::<Resp>();
        std::thread::Builder::new()
            .name("review-reader".into())
            .spawn(move || {
                let mut files: HashMap<PathBuf, File> = HashMap::new();
                let mut ttl = TtlEdges::default();
                while let Ok(first) = req_rx.recv() {
                    // Only the newest request per view matters (e.g. while dragging).
                    let mut latest: HashMap<u32, Req> = HashMap::new();
                    latest.insert(first.view, first);
                    for r in req_rx.try_iter() {
                        latest.insert(r.view, r);
                    }
                    for (_, r) in latest {
                        let resp = serve(&r, &mut files, &mut ttl);
                        if resp_tx.send(resp).is_err() {
                            return;
                        }
                    }
                }
            })
            .expect("spawning the review reader");
        Self { tx, rx }
    }
}

/// Samples of the main stream readable so far (the recorder flushes about once a second).
pub fn available(src: &RecSource) -> i64 {
    std::fs::metadata(&src.dat).map(|m| (m.len() / (2 * src.nch.max(1) as u64)) as i64).unwrap_or(0)
}

fn serve(r: &Req, files: &mut HashMap<PathBuf, File>, ttl: &mut TtlEdges) -> Resp {
    let mut resp = Resp { view: r.view, generation: r.generation, from: r.from, data: Vec::new(), words: Vec::new(), error: None };
    let nch = r.src.nch.max(1);
    let mut read = || -> std::io::Result<(i64, Vec<i16>)> {
        let f = match files.get(&r.src.dat) {
            Some(f) => f,
            None => {
                let f = File::open(&r.src.dat)?;
                files.insert(r.src.dat.clone(), f);
                &files[&r.src.dat]
            }
        };
        let avail = (f.metadata()?.len() / (2 * nch as u64)) as i64;
        let start = (r.from - r.src.first_sample).clamp(0, avail);
        let end = (r.from - r.src.first_sample + r.n as i64).clamp(start, avail);
        let n = (end - start) as usize;
        // Read straight into the sample buffer (the file is little-endian, as the host).
        let mut data = vec![0i16; n * nch];
        f.read_exact_at(bytemuck::cast_slice_mut(&mut data), start as u64 * nch as u64 * 2)?;
        Ok((r.src.first_sample + start, data))
    };
    match read() {
        Ok((from, data)) => {
            resp.from = from;
            resp.data = data;
        }
        Err(e) => resp.error = Some(format!("reading {}: {e}", r.src.dat.display())),
    }
    if r.words_n > 0 {
        ttl.refresh(&r.src.ttl_dir);
        resp.words = ttl.words(r.words_from, r.words_n);
    }
    resp
}

/// The recording's TTL edges (sample number and full word after each edge), read
/// incrementally as the files grow.
#[derive(Default)]
struct TtlEdges {
    dir: PathBuf,
    samples: Vec<i64>,
    words: Vec<u64>,
}

impl TtlEdges {
    fn refresh(&mut self, dir: &Path) {
        if self.dir != dir {
            *self = TtlEdges { dir: dir.to_owned(), ..Default::default() };
        }
        let have = self.samples.len();
        let (Some(s), Some(w)) = (read_npy_8(&dir.join("sample_numbers.npy"), have), read_npy_8(&dir.join("full_words.npy"), have)) else { return };
        // The two files are flushed separately: keep only edges present in both.
        let n = s.len().min(w.len());
        self.samples.extend(s[..n].iter().map(|b| i64::from_le_bytes(*b)));
        self.words.extend(w[..n].iter().map(|b| u64::from_le_bytes(*b)));
    }

    /// The TTL word for each sample of `[from, from + n)`.
    fn words(&self, from: i64, n: usize) -> Vec<u16> {
        let mut i = self.samples.partition_point(|&s| s <= from);
        let mut cur = if i > 0 { self.words[i - 1] as u16 } else { 0 };
        let mut out = Vec::with_capacity(n);
        for k in 0..n as i64 {
            let s = from + k;
            while i < self.samples.len() && self.samples[i] <= s {
                cur = self.words[i] as u16;
                i += 1;
            }
            out.push(cur);
        }
        out
    }
}

/// 8-byte records of a 1-D `.npy` file from record `skip` on (complete records only).
fn read_npy_8(path: &Path, skip: usize) -> Option<Vec<[u8; 8]>> {
    let f = File::open(path).ok()?;
    let mut head = [0u8; 10];
    f.read_exact_at(&mut head, 0).ok()?;
    if &head[..6] != b"\x93NUMPY" {
        return None;
    }
    let data0 = 10 + u16::from_le_bytes([head[8], head[9]]) as u64;
    let len = f.metadata().ok()?.len();
    let total = (len.saturating_sub(data0) / 8) as usize;
    if total <= skip {
        return Some(Vec::new());
    }
    let mut bytes = vec![0u8; (total - skip) * 8];
    f.read_exact_at(&mut bytes, data0 + skip as u64 * 8).ok()?;
    Some(bytes.chunks_exact(8).map(|c| c.try_into().unwrap()).collect())
}

/// Filter settling time read before the visible stretch, so its left edge shows the filtered
/// signal and not the filter's start-up transient (zero state). The 4th-order high-pass's
/// slowest pole (ζ ≈ 0.38) decays to 1e-3 in about 3 / f_low; a low-pass alone settles in
/// a few ms.
pub fn lead_in_s(filter: crate::config::ViewFilter, band: &engine::dsp::BandSpec) -> f64 {
    match filter {
        crate::config::ViewFilter::Raw => 0.0,
        crate::config::ViewFilter::Band if band.low_hz > 0.0 => (3.0 / band.low_hz).max(0.05),
        crate::config::ViewFilter::Band => 0.05,
    }
}

/// Time since the recording started, for the review axis: "m:ss.s" or "h:mm:ss".
pub fn fmt_time(s: f64, fine: bool) -> String {
    let s = s.max(0.0);
    let h = (s / 3600.0).floor();
    let m = ((s - h * 3600.0) / 60.0).floor();
    let sec = s - h * 3600.0 - m * 60.0;
    match (h > 0.0, fine) {
        (true, _) => format!("{h:.0}:{m:02.0}:{sec:02.0}"),
        (false, true) => format!("{m:.0}:{sec:06.3}"),
        (false, false) => format!("{m:.0}:{sec:04.1}"),
    }
}

/// What a review image was made from; a change means reading and filtering again.
#[derive(Clone, PartialEq, Debug)]
pub struct Key {
    pub t0: i64,
    pub span: u32,
    pub cols: u32,
    pub nch: usize,
    /// Every channel's display filter, and the band corners (bits).
    pub filters: Vec<crate::config::ViewFilter>,
    pub band: (u32, u32),
    pub car: Option<(Vec<usize>, Vec<usize>)>,
}

/// A complete review image in GPU state `buf`.
pub struct Shown {
    pub key: Key,
    pub buf: u8,
    pub config: crate::traces::TraceConfig,
    pub ttl: crate::ttl::TtlOverlay,
    /// Window position of the sample `key.t0` (0 after a fresh fill; appends advance it).
    pub pos_t0: u32,
    /// Drawn from the min/max pyramid: when, and whether every column was covered (if not,
    /// the backfill is still running and the image is redrawn periodically).
    pub pyramid: Option<(std::time::Instant, bool)>,
}

impl Shown {
    /// The data column at the left edge, with the shader's column mapping.
    pub fn rot(&self) -> u32 {
        let (span, cols) = (self.key.span.max(1), self.key.cols);
        ((self.pos_t0 as f32 * cols as f32 / span as f32) as u32).min(cols.saturating_sub(1))
    }
}

/// The image being built: either in the other GPU state (double buffering: the shown one
/// stays on screen until this one is complete, so dragging never blanks the traces), or an
/// append of the samples just after the shown image into its own state (a pan to the right
/// by less than a window, which continues the filter state instead of refiltering it all).
pub struct Filling {
    pub key: Key,
    pub generation: u64,
    pub buf: u8,
    pub append: bool,
    pub loaded: Option<Loaded>,
    pub started: std::time::Instant,
    /// When the reader's answer arrived.
    pub loaded_at: Option<std::time::Instant>,
}

pub struct Loaded {
    /// Shared with the GPU uploads, which take it chunk by chunk without copying.
    pub data: std::sync::Arc<Vec<i16>>,
    /// Samples already handed to the GPU.
    pub fed: usize,
    /// Window position of the first sample read (the lead-in ends exactly at position 0).
    pub pos0: u32,
    pub words: Vec<u16>,
}

/// Review state of one trace window: which stretch is on screen.
pub struct ReviewView {
    /// Absolute sample number at the left edge, and samples across the plot.
    pub t0: i64,
    pub span: u32,
    pub generation: u64,
    pub shown: Option<Shown>,
    pub filling: Option<Filling>,
    pub error: Option<String>,
}

impl ReviewView {
    pub fn new(t0: i64, span: u32) -> Self {
        Self { t0, span, generation: 0, shown: None, filling: None, error: None }
    }

    /// Keep the span within limits and the stretch inside the recording `[first, end)`.
    pub fn clamp(&mut self, first: i64, end: i64, min_span: u32, max_span: u32) {
        // No wider than the recording so far (once it is longer than the minimum span).
        let max_span = max_span.min((end - first).clamp(min_span.max(1) as i64, u32::MAX as i64) as u32);
        self.span = self.span.clamp(min_span.max(1), max_span.max(min_span.max(1)));
        let last_t0 = (end - self.span as i64).max(first);
        self.t0 = self.t0.clamp(first, last_t0);
    }

    /// Zoom by `factor` (> 1 shows more time), keeping the time under `frac` (0..1 across
    /// the plot) in place.
    pub fn zoom(&mut self, factor: f64, frac: f64) {
        let anchor = self.t0 as f64 + frac * self.span as f64;
        let span = (self.span as f64 * factor).round().clamp(1.0, u32::MAX as f64 / 2.0);
        self.span = span as u32;
        self.t0 = (anchor - frac * span).round() as i64;
    }

    /// Route a reader response to the image being built (stale generations are dropped).
    pub fn accept(&mut self, r: Resp) {
        let Some(f) = self.filling.as_mut() else { return };
        if r.generation != f.generation {
            return;
        }
        if let Some(e) = r.error {
            self.error = Some(e);
            self.filling = None;
            return;
        }
        self.error = None;
        let span = f.key.span.max(1) as i64;
        let pos0 = if f.append {
            // Right after the shown image, whose end sits at its own start position.
            self.shown.as_ref().map_or(0, |s| s.pos_t0)
        } else {
            // The lead-in (t0 - from samples) lands at the end of the window and is then
            // overwritten, so the visible stretch starts at window position 0.
            let lead = (f.key.t0 - r.from).max(0);
            ((span - lead % span) % span) as u32
        };
        f.loaded = Some(Loaded { data: std::sync::Arc::new(r.data), fed: 0, pos0, words: r.words });
        f.loaded_at = Some(std::time::Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_and_zoom_keep_the_view_inside_the_recording() {
        let mut v = ReviewView::new(500, 100);
        v.clamp(1000, 5000, 10, 1000);
        assert_eq!((v.t0, v.span), (1000, 100));
        v.t0 = 4990;
        v.clamp(1000, 5000, 10, 1000);
        assert_eq!(v.t0, 4900);
        // Zoom out 2x about the right edge: the right edge stays put.
        v.zoom(2.0, 1.0);
        assert_eq!((v.t0, v.span), (4800, 200));
        // Shorter recording than the span: the span shrinks to it, from the first sample.
        v.span = 1000;
        v.clamp(1000, 1500, 10, 1000);
        assert_eq!((v.t0, v.span), (1000, 500));
    }

    #[test]
    fn lead_in_ends_at_window_position_zero() {
        let key = Key { t0: 1000, span: 300, cols: 100, nch: 1, filters: vec![crate::config::ViewFilter::Band], band: (0, 0), car: None };
        let mut v = ReviewView::new(1000, 300);
        v.filling = Some(Filling { key, generation: 3, buf: 0, append: false, loaded: None, started: std::time::Instant::now(), loaded_at: None });
        // Read from 950: 50 lead-in samples, so the first one goes to position 250.
        v.accept(Resp { view: 0, generation: 3, from: 950, data: vec![0; 350], words: vec![], error: None });
        let l = v.filling.as_ref().unwrap().loaded.as_ref().unwrap();
        assert_eq!(l.pos0, 250);
        assert_eq!((l.pos0 as usize + 50) % 300, 0);
        // A stale response is ignored.
        v.filling.as_mut().unwrap().loaded = None;
        v.accept(Resp { view: 0, generation: 2, from: 950, data: vec![0; 350], words: vec![], error: None });
        assert!(v.filling.as_ref().unwrap().loaded.is_none());
    }

    #[test]
    fn ttl_words_follow_the_edges() {
        let t = TtlEdges { dir: PathBuf::new(), samples: vec![100, 105, 110], words: vec![1, 3, 2] };
        assert_eq!(t.words(98, 4), vec![0, 0, 1, 1]);
        assert_eq!(t.words(104, 8), vec![1, 3, 3, 3, 3, 3, 2, 2]);
        assert_eq!(t.words(200, 2), vec![2, 2]);
    }

    #[test]
    fn reads_a_growing_recording_back() {
        let dir = std::env::temp_dir().join(format!("oers-review-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dat = dir.join("continuous.dat");
        // 3 channels, 10 samples: value = 100 * sample + channel.
        let v: Vec<u8> = (0..10i16).flat_map(|s| (0..3i16).flat_map(move |c| (100 * s + c).to_le_bytes())).collect();
        std::fs::write(&dat, &v).unwrap();
        let src = RecSource { dat, ttl_dir: dir.clone(), nch: 3, first_sample: 1000, sample_rate: 30000.0 };
        assert_eq!(available(&src), 10);
        let (mut files, mut ttl) = (HashMap::new(), TtlEdges::default());
        // Asks for 1005..1020: only 1005..1010 exist.
        let r = serve(&Req { view: 1, generation: 7, src: src.clone(), from: 1005, n: 15, words_from: 0, words_n: 0 }, &mut files, &mut ttl);
        assert_eq!((r.generation, r.from, r.data.len()), (7, 1005, 15));
        assert_eq!(&r.data[..4], &[500, 501, 502, 600]);
        // Straddling the first sample: clamped to it.
        let r = serve(&Req { view: 1, generation: 8, src, from: 998, n: 4, words_from: 0, words_n: 0 }, &mut files, &mut ttl);
        assert_eq!((r.from, r.data.len()), (1000, 6));
        std::fs::remove_dir_all(&dir).ok();
    }
}
