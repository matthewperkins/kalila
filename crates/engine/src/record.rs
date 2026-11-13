//! Open Ephys Binary format writer (compatible with open-ephys-python-tools, SpikeInterface/neo,
//! and the stock GUI's file reader).
//!
//! ```text
//! <root>/<YYYY-MM-DD_HH-MM-SS>/Record Node 101/experimentE/recordingR/
//!     structure.oebin, sync_messages.txt
//!     continuous/Acquisition_Board-100.<stream>/{continuous.dat, sample_numbers.npy, timestamps.npy}
//!     events/Acquisition_Board-100.<stream>/TTL/{states,sample_numbers,timestamps,full_words}.npy
//!     events/MessageCenter/{text,sample_numbers,timestamps}.npy
//!     note_keystrokes.tsv   (oers only: every keypress of each typed note)
//! ```
//!
//! Messages follow the stock GUI 1.x (`RecordNode::handleBroadcastMessage`): sample numbers in
//! the main stream's clock, timestamps in its seconds, text as null-padded `|S513`.
//!
//! The recorder thread receives every block while acquiring (recording or not). Data reaches
//! disk through large buffered writes; `.npy` headers are rewritten every second so a crash
//! leaves readable files. Any I/O error stops the recording and is reported loudly.

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use log::{error, info};
use parking_lot::Mutex;
use rhythm::{Block, ChannelKind, StreamLayout};
use serde_json::json;
use std::fs::{self, File};
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SOURCE_NAME: &str = "Acquisition Board";
const SOURCE_ID: u32 = 100;
const RECORD_NODE_ID: u32 = 101;
const GUI_VERSION: &str = "1.0.0";

#[cfg(not(target_endian = "little"))]
compile_error!("the Open Ephys binary format is little-endian");

/// Incrementally written `.npy` file (1-D, or 1-D of fixed-size records).
struct NpyFile {
    w: BufWriter<File>,
    descr: &'static str,
    count: u64,
    header_len: u64,
    path: PathBuf,
}

/// A new file; never truncates one that exists (an earlier recording's).
fn create_new(path: &Path) -> Result<File> {
    fs::OpenOptions::new().write(true).create_new(true).open(path).with_context(|| format!("creating {}", path.display()))
}

/// Keeps the first error and carries on, so every file that can be finished is.
fn keep_first(first: &mut Option<anyhow::Error>, r: Result<()>) {
    if let Err(e) = r {
        first.get_or_insert(e);
    }
}

impl NpyFile {
    fn create(path: &Path, descr: &'static str) -> Result<Self> {
        let f = create_new(path)?;
        let mut n = Self { w: BufWriter::with_capacity(1 << 16, f), descr, count: 0, header_len: 0, path: path.to_owned() };
        let h = n.header();
        n.header_len = h.len() as u64;
        n.w.write_all(&h)?;
        Ok(n)
    }
    fn header(&self) -> Vec<u8> {
        // Shape field is padded so the header length never changes as the count grows.
        let dict = format!("{{'descr': '{}', 'fortran_order': False, 'shape': ({},), }}", self.descr, self.count);
        let mut dict = format!("{dict:<width$}", width = dict.len() + (24 - self.count.to_string().len()));
        let base = 10; // magic(6) + version(2) + len(2)
        let total = (base + dict.len() + 1).div_ceil(64) * 64;
        while base + dict.len() + 1 < total {
            dict.push(' ');
        }
        dict.push('\n');
        let mut h = Vec::with_capacity(total);
        h.extend_from_slice(b"\x93NUMPY\x01\x00");
        h.extend_from_slice(&(dict.len() as u16).to_le_bytes());
        h.extend_from_slice(dict.as_bytes());
        h
    }
    fn write<T: Copy>(&mut self, v: &[T]) -> Result<()> {
        let bytes = unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) };
        self.w.write_all(bytes)?;
        self.count += v.len() as u64;
        Ok(())
    }
    fn update_header(&mut self) -> Result<()> {
        self.w.flush()?;
        let h = self.header();
        if h.len() as u64 != self.header_len {
            bail!("npy header size changed for {}", self.path.display());
        }
        let f = self.w.get_mut();
        let pos = f.stream_position()?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&h)?;
        f.seek(SeekFrom::Start(pos))?;
        Ok(())
    }
    fn finish(mut self) -> Result<()> {
        self.update_header()?;
        self.w.get_ref().sync_all()?;
        Ok(())
    }
}

struct StreamFiles {
    dat: BufWriter<File>,
    samples: NpyFile,
    timestamps: NpyFile,
    tmp_i64: Vec<i64>,
    channels: usize,
}

struct TtlFiles {
    states: NpyFile,
    samples: NpyFile,
    timestamps: NpyFile,
    words: NpyFile,
    last: Option<u16>,
    initial: u16,
}

/// Stock `MAX_MSG_LENGTH`; records are this plus a terminating null.
pub const MAX_MESSAGE_BYTES: usize = 512;

struct MessageFiles {
    text: NpyFile,
    samples: NpyFile,
    timestamps: NpyFile,
    /// Keystroke log (tab-separated), created with the first note that has keys.
    keys: Option<BufWriter<File>>,
}

/// A message's text as one stock record: UTF-8 cut to 512 bytes at a character boundary,
/// null-padded to 513.
fn message_record(text: &str) -> [u8; MAX_MESSAGE_BYTES + 1] {
    let mut end = text.len().min(MAX_MESSAGE_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut r = [0u8; MAX_MESSAGE_BYTES + 1];
    r[..end].copy_from_slice(&text.as_bytes()[..end]);
    r
}

struct Recording {
    dir: PathBuf,
    layout: StreamLayout,
    main: StreamFiles,
    side: Vec<StreamFiles>,
    side_tmp: Vec<i16>,
    ttl: TtlFiles,
    messages: MessageFiles,
    /// (first sample, its timestamp) of the latest block: messages are timestamped from it.
    clock: Option<(i64, f64)>,
    first_sample: Option<i64>,
    /// Samples written to the main stream, and the sample number the next block should have.
    written: u64,
    next: Option<i64>,
    /// See `RecSource::gaps`.
    gaps: Vec<(i64, u64)>,
    last_header_update: Instant,
    oebin: serde_json::Value,
    dat_path: PathBuf,
    ttl_dir: PathBuf,
}

impl Recording {
    /// Read-back description, once the first sample is known.
    fn source(&self) -> Option<RecSource> {
        Some(RecSource { dat: self.dat_path.clone(), ttl_dir: self.ttl_dir.clone(), nch: self.main.channels, first_sample: self.first_sample?, sample_rate: self.layout.sample_rate, gaps: Arc::new(self.gaps.clone()) })
    }
}

fn stream_folder(stream: &str) -> String {
    format!("{}-{}.{}", SOURCE_NAME.replace([' ', '@'], "_"), SOURCE_ID, stream)
}

impl Recording {
    fn open(dir: &Path, layout: &StreamLayout) -> Result<Self> {
        let cont = dir.join("continuous");
        let mk_stream = |name: &str, nch: usize| -> Result<StreamFiles> {
            let d = cont.join(stream_folder(name));
            fs::create_dir_all(&d)?;
            Ok(StreamFiles {
                dat: BufWriter::with_capacity(8 << 20, create_new(&d.join("continuous.dat"))?),
                samples: NpyFile::create(&d.join("sample_numbers.npy"), "<i8")?,
                timestamps: NpyFile::create(&d.join("timestamps.npy"), "<f8")?,
                tmp_i64: Vec::new(),
                channels: nch,
            })
        };
        let main = mk_stream(&layout.name, layout.num_channels())?;
        let side = layout.side_streams.iter().map(|s| mk_stream(&s.name, s.channel_names.len())).collect::<Result<Vec<_>>>()?;
        let ed = dir.join("events").join(stream_folder(&layout.name)).join("TTL");
        let dat_path = cont.join(stream_folder(&layout.name)).join("continuous.dat");
        fs::create_dir_all(&ed)?;
        let ttl = TtlFiles {
            states: NpyFile::create(&ed.join("states.npy"), "<i2")?,
            samples: NpyFile::create(&ed.join("sample_numbers.npy"), "<i8")?,
            timestamps: NpyFile::create(&ed.join("timestamps.npy"), "<f8")?,
            words: NpyFile::create(&ed.join("full_words.npy"), "<u8")?,
            last: None,
            initial: 0,
        };
        let md = dir.join("events").join("MessageCenter");
        fs::create_dir_all(&md)?;
        let messages = MessageFiles {
            text: NpyFile::create(&md.join("text.npy"), "|S513")?,
            samples: NpyFile::create(&md.join("sample_numbers.npy"), "<i8")?,
            timestamps: NpyFile::create(&md.join("timestamps.npy"), "<f8")?,
            keys: None,
        };
        let mut r = Self { dir: dir.to_owned(), layout: layout.clone(), main, side, side_tmp: Vec::new(), ttl, messages, clock: None, first_sample: None, written: 0, next: None, gaps: Vec::new(), last_header_update: Instant::now(), oebin: json!({}), dat_path, ttl_dir: ed };
        r.oebin = r.build_oebin();
        fs::write(dir.join("structure.oebin"), serde_json::to_string_pretty(&r.oebin)?)?;
        Ok(r)
    }

    fn build_oebin(&self) -> serde_json::Value {
        let l = &self.layout;
        let ty = |k: ChannelKind| match k {
            ChannelKind::Electrode => 0,
            ChannelKind::Aux => 1,
            ChannelKind::Adc => 2,
        };
        let main_channels: Vec<_> = l
            .channels
            .iter()
            .map(|c| {
                json!({
                    "channel_name": c.name, "description": match c.kind { ChannelKind::Electrode => "Headstage data channel", ChannelKind::Aux => "Auxiliary input channel", ChannelKind::Adc => "ADC input channel" },
                    "identifier": match c.kind { ChannelKind::Electrode => "genericdata.continuous", ChannelKind::Aux => "acq-board.aux", ChannelKind::Adc => "acq-board.adc" },
                    "history": SOURCE_NAME, "bit_volts": c.bit_volts, "units": c.units, "type": ty(c.kind)
                })
            })
            .collect();
        let stream_json = |name: &str, rate: f64, chans: Vec<serde_json::Value>| {
            json!({
                "folder_name": format!("{}/", stream_folder(name)), "sample_rate": rate,
                "source_processor_name": SOURCE_NAME, "source_processor_id": SOURCE_ID, "stream_name": name,
                "recorded_processor": "Record Node", "recorded_processor_id": RECORD_NODE_ID,
                "num_channels": chans.len(), "channels": chans
            })
        };
        let mut continuous = vec![stream_json(&l.name, l.sample_rate, main_channels)];
        for s in &l.side_streams {
            let chans = s.channel_names.iter().zip(&s.units).zip(&s.bit_volts).map(|((n, u), b)| json!({"channel_name": n, "description": "Auxiliary data", "identifier": "acq-board.aux", "history": SOURCE_NAME, "bit_volts": b, "units": u, "type": 1})).collect();
            continuous.push(stream_json(&s.name, s.sample_rate, chans));
        }
        json!({
            "GUI version": GUI_VERSION,
            "continuous": continuous,
            "events": [{
                "folder_name": format!("{}/TTL/", stream_folder(&l.name)),
                "channel_name": "Acquisition Board TTL Input", "description": "TTL events from the acquisition board digital inputs",
                "identifier": "acq-board.ttl", "sample_rate": l.sample_rate, "type": "int16",
                "source_processor": SOURCE_NAME, "stream_name": l.name, "initial_state": self.ttl.initial
            }, {
                // As stock, except sample_rate: stock gives its 1 kHz message stream's rate,
                // but the sample numbers are in the main stream, so we give that one's.
                "folder_name": "MessageCenter/",
                "channel_name": "Messages", "description": "Broadcasts messages from the MessageCenter",
                "identifier": "messagecenter.events", "sample_rate": l.sample_rate, "type": "string",
                "source_processor": "Message Center", "stream_name": "MessageCenter stream"
            }],
            "spikes": []
        })
    }

    fn write_block(&mut self, b: &Block) -> Result<()> {
        if b.num_channels != self.main.channels {
            bail!("channel count changed during recording ({} -> {})", self.main.channels, b.num_channels);
        }
        // A malformed block would shift every channel of continuous.dat from here on, and a
        // block behind the last would make sample_numbers.npy go backwards: stop recording
        // (loudly, with every file finished) rather than write either.
        if b.data.len() != b.len() * b.num_channels || b.ttl.len() != b.len() {
            bail!("malformed block at sample {}: {} samples, {} values for {} channels, {} TTL words", b.first_sample, b.len(), b.data.len(), b.num_channels, b.ttl.len());
        }
        if let Some(n) = self.next
            && b.first_sample < n
        {
            bail!("block at sample {} is behind the last one written (next was {n})", b.first_sample);
        }
        if !b.is_empty() {
            if self.first_sample.is_none() {
                self.first_sample = Some(b.first_sample);
                let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
                let txt = format!(
                    "Software Time (milliseconds since midnight Jan 1st 1970 UTC): {ms}\r\nStart Time for {SOURCE_NAME} ({SOURCE_ID}) - {} @ {} Hz: {}\r\n",
                    self.layout.name, self.layout.sample_rate, b.first_sample
                );
                fs::write(self.dir.join("sync_messages.txt"), txt)?;
            }
            if let Some(&t0) = b.timestamps.first() {
                self.clock = Some((b.first_sample, t0));
            }
            // continuous.dat has no holes: after a gap, record where the file picks up again.
            if self.next.is_some_and(|n| n != b.first_sample) {
                self.gaps.push((b.first_sample, self.written));
            }
            self.written += b.len() as u64;
            self.next = Some(b.first_sample + b.len() as i64);
            let bytes = unsafe { std::slice::from_raw_parts(b.data.as_ptr() as *const u8, b.data.len() * 2) };
            self.main.dat.write_all(bytes)?;
            let m = &mut self.main;
            m.tmp_i64.clear();
            m.tmp_i64.extend((0..b.len() as i64).map(|i| b.first_sample + i));
            m.samples.write(&m.tmp_i64)?;
            self.main.timestamps.write(&b.timestamps)?;
            // TTL edges.
            for (i, &w) in b.ttl.iter().enumerate() {
                let last = match self.ttl.last {
                    None => {
                        self.ttl.initial = w;
                        self.ttl.last = Some(w);
                        continue;
                    }
                    Some(l) => l,
                };
                let changed = last ^ w;
                if changed != 0 {
                    let n = b.first_sample + i as i64;
                    for line in 0..16 {
                        if changed & (1 << line) != 0 {
                            let on = w & (1 << line) != 0;
                            let state: i16 = (line + 1) * if on { 1 } else { -1 };
                            self.ttl.states.write(&[state])?;
                            self.ttl.samples.write(&[n])?;
                            self.ttl.timestamps.write(&[b.timestamps[i]])?;
                            self.ttl.words.write(&[w as u64])?;
                        }
                    }
                    self.ttl.last = Some(w);
                }
            }
        }
        for s in &b.side {
            let Some(f) = self.side.get_mut(s.stream) else { continue };
            let bv = &self.layout.side_streams[s.stream].bit_volts;
            self.side_tmp.clear();
            self.side_tmp.extend(s.values.iter().zip(bv).map(|(v, b)| (v / b).round().clamp(-32768.0, 32767.0) as i16));
            let bytes = unsafe { std::slice::from_raw_parts(self.side_tmp.as_ptr() as *const u8, self.side_tmp.len() * 2) };
            f.dat.write_all(bytes)?;
            f.samples.write(&[s.sample_number])?;
            f.timestamps.write(&[s.timestamp])?;
        }
        if self.last_header_update.elapsed() > Duration::from_secs(1) {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// A typed note: text stamped at `sample` (main-stream clock), and its keystrokes
    /// (sample, key) for the sidecar log.
    fn write_message(&mut self, sample: i64, text: &str, keys: &[(i64, String)]) -> Result<()> {
        let ts = match self.clock {
            Some((s0, t0)) => t0 + (sample - s0) as f64 / self.layout.sample_rate,
            None => sample as f64 / self.layout.sample_rate,
        };
        self.messages.text.write(&[message_record(text)])?;
        self.messages.samples.write(&[sample])?;
        self.messages.timestamps.write(&[ts])?;
        if !keys.is_empty() {
            if self.messages.keys.is_none() {
                let mut w = BufWriter::new(create_new(&self.dir.join("note_keystrokes.tsv"))?);
                writeln!(w, "note_sample\tkey_sample\tkey")?;
                self.messages.keys = Some(w);
            }
            let w = self.messages.keys.as_mut().unwrap();
            for (k, key) in keys {
                writeln!(w, "{sample}\t{k}\t{}", key.replace(['\t', '\n'], " "))?;
            }
            w.flush()?;
        }
        // Headers now: notes are rare and should survive a crash right away.
        for n in [&mut self.messages.text, &mut self.messages.samples, &mut self.messages.timestamps] {
            n.update_header()?;
        }
        Ok(())
    }

    fn checkpoint(&mut self) -> Result<()> {
        self.last_header_update = Instant::now();
        for f in std::iter::once(&mut self.main).chain(self.side.iter_mut()) {
            f.dat.flush()?;
            f.samples.update_header()?;
            f.timestamps.update_header()?;
        }
        for n in [&mut self.ttl.states, &mut self.ttl.samples, &mut self.ttl.timestamps, &mut self.ttl.words] {
            n.update_header()?;
        }
        Ok(())
    }

    /// Flush, sync and close every file, then write the final metadata. Also used after a
    /// write error: each file is finished on its own, so one failing file doesn't leave the
    /// others with stale headers. Returns the first error.
    fn finish(self) -> Result<()> {
        // The initial TTL state is known now; written last, once the data is on disk.
        let oebin = self.build_oebin();
        let oebin_path = self.dir.join("structure.oebin");
        let mut first = None;
        for f in std::iter::once(self.main).chain(self.side) {
            let mut dat = f.dat;
            keep_first(&mut first, dat.flush().map_err(Into::into));
            keep_first(&mut first, dat.get_ref().sync_all().map_err(Into::into));
            keep_first(&mut first, f.samples.finish());
            keep_first(&mut first, f.timestamps.finish());
        }
        let t = self.ttl;
        for n in [t.states, t.samples, t.timestamps, t.words] {
            keep_first(&mut first, n.finish());
        }
        let m = self.messages;
        for n in [m.text, m.samples, m.timestamps] {
            keep_first(&mut first, n.finish());
        }
        if let Some(mut k) = m.keys {
            keep_first(&mut first, k.flush().map_err(Into::into));
        }
        keep_first(&mut first, serde_json::to_string_pretty(&oebin).map_err(Into::into).and_then(|j| fs::write(oebin_path, j).map_err(Into::into)));
        first.map_or(Ok(()), Err)
    }
}

pub enum RecMsg {
    Data(Arc<Block>),
    /// A new acquisition run starts (new experiment number on next recording).
    AcquisitionStarted(StreamLayout),
    AcquisitionStopped,
    Start,
    Stop,
    /// A typed note for the open recording (ignored when not recording).
    Message { sample: i64, text: String, keys: Vec<(i64, String)> },
    Shutdown,
}

#[derive(Default)]
pub struct RecStatus {
    pub recording: AtomicBool,
    pub bytes: AtomicU64,
    pub samples: AtomicU64,
    pub error: Mutex<Option<String>>,
    pub current_dir: Mutex<Option<PathBuf>>,
    pub root: Mutex<PathBuf>,
    /// Force a new date-time session directory on the next recording.
    pub new_session: AtomicBool,
    /// The recording being written (or the last one), for reading back while it grows
    /// (scroll-back review in the GUI). Set once its first sample is on its way to disk.
    pub source: Mutex<Option<RecSource>>,
}

/// Where a recording's files are, for reading them back while they are written.
#[derive(Clone, Debug, PartialEq)]
pub struct RecSource {
    /// Main stream's continuous.dat: int16, `nch` channels interleaved, sample `first_sample` first.
    pub dat: PathBuf,
    /// Its TTL folder (sample_numbers.npy, full_words.npy).
    pub ttl_dir: PathBuf,
    pub nch: usize,
    pub first_sample: i64,
    pub sample_rate: f64,
    /// The file has no holes, so after a gap in the sample numbers (dropped samples) file
    /// index ≠ sample − `first_sample`. Each entry is (sample number, file index) of a block
    /// that didn't follow on from the one before, oldest first; empty without gaps.
    pub gaps: Arc<Vec<(i64, u64)>>,
}

impl RecSource {
    /// Contiguous stretches: (first sample number, file index, length in samples), for a
    /// file holding `file_len` samples.
    fn runs(&self, file_len: u64) -> impl Iterator<Item = (i64, u64, u64)> + '_ {
        let starts: Vec<(i64, u64)> = std::iter::once((self.first_sample, 0)).chain(self.gaps.iter().copied().filter(|&(_, f)| f < file_len)).collect();
        (0..starts.len()).map(move |k| {
            let (s, f) = starts[k];
            let to = starts.get(k + 1).map_or(file_len, |&(_, f1)| f1);
            (s, f, to.saturating_sub(f))
        })
    }

    /// One past the last sample number in a file holding `file_len` samples.
    pub fn end_sample(&self, file_len: u64) -> i64 {
        self.runs(file_len).last().map_or(self.first_sample, |(s, _, n)| s + n as i64)
    }

    /// Samples in the file now, as a span of sample numbers from `first_sample` (dropped
    /// samples included, so it lines up with sample numbers).
    pub fn available(&self) -> u64 {
        let len = fs::metadata(&self.dat).map(|m| m.len() / (2 * self.nch.max(1) as u64)).unwrap_or(0);
        (self.end_sample(len) - self.first_sample) as u64
    }

    /// Samples `s0 .. s0 + n` (sample numbers) of `f`, the recording's continuous.dat, as
    /// `n × nch` int16. Dropped samples, and anything past the end of the file, read as 0.
    pub fn read(&self, f: &File, s0: i64, n: usize) -> std::io::Result<Vec<i16>> {
        use std::os::unix::fs::FileExt;
        let nch = self.nch.max(1);
        let len = f.metadata()?.len() / (2 * nch as u64);
        let mut data = vec![0i16; n * nch];
        let s1 = s0 + n as i64;
        for (s, fi, m) in self.runs(len) {
            let (a, b) = (s.max(s0), (s + m as i64).min(s1));
            if a < b {
                let out = &mut data[(a - s0) as usize * nch..(b - s0) as usize * nch];
                f.read_exact_at(bytemuck::cast_slice_mut(out), (fi + (a - s) as u64) * nch as u64 * 2)?;
            }
        }
        Ok(data)
    }
}

/// Whether the next recording starts a new date-time session directory: the first recording,
/// when asked ("New session directory"), or when the date has changed since the session began,
/// so a program left open overnight doesn't file today's recordings under yesterday's date.
fn needs_new_session(session_date: Option<chrono::NaiveDate>, today: chrono::NaiveDate, forced: bool) -> bool {
    forced || session_date != Some(today)
}

pub fn run_recorder(rx: Receiver<RecMsg>, status: Arc<RecStatus>) {
    // However this thread ends (a panic included), clear `recording` so the acquisition
    // thread stops sending and the GUI stops saying "Recording", and say why.
    struct Exit<'a>(&'a RecStatus);
    impl Drop for Exit<'_> {
        fn drop(&mut self) {
            if std::thread::panicking() {
                *self.0.error.lock() = Some("the recorder thread crashed: this recording ends at that point (files are readable up to the last second). Restart oers to record again.".into());
            }
            self.0.recording.store(false, Ordering::SeqCst);
        }
    }
    let _exit = Exit(&status);
    let mut layout: Option<StreamLayout> = None;
    let mut session: Option<PathBuf> = None;
    let mut session_date: Option<chrono::NaiveDate> = None;
    let mut session_root: Option<PathBuf> = None;
    let mut experiment = 0u32;
    let mut recording_idx = 0u32;
    let mut acq_run_seen = false;
    let mut rec: Option<Recording> = None;
    // After a write error: finish what can be finished (the error is already reported).
    let abandon = |r: Option<Recording>| {
        if let Some(Err(e)) = r.map(Recording::finish) {
            error!("finishing the recording after the error: {e:#}");
        }
    };
    let fail = |status: &RecStatus, e: anyhow::Error| {
        error!("recording error: {e:#}");
        *status.error.lock() = Some(format!("{e:#}"));
        status.recording.store(false, Ordering::SeqCst);
    };
    loop {
        let msg = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => RecMsg::Shutdown,
        };
        match msg {
            RecMsg::Data(b) => {
                if let Some(r) = rec.as_mut() {
                    if let Err(e) = r.write_block(&b) {
                        fail(&status, e);
                        abandon(rec.take());
                    } else {
                        if status.source.lock().as_ref().is_none_or(|s| s.dat != r.dat_path || s.gaps.len() != r.gaps.len()) {
                            *status.source.lock() = r.source();
                        }
                        status.bytes.fetch_add(b.data.len() as u64 * 2, Ordering::Relaxed);
                        status.samples.fetch_add(b.len() as u64, Ordering::Relaxed);
                    }
                }
            }
            RecMsg::AcquisitionStarted(l) => {
                layout = Some(l);
                acq_run_seen = true;
            }
            RecMsg::AcquisitionStopped => {
                if let Some(r) = rec.take()
                    && let Err(e) = r.finish()
                {
                    fail(&status, e);
                }
                status.recording.store(false, Ordering::SeqCst);
                layout = None;
            }
            RecMsg::Start => {
                if rec.is_some() {
                    continue;
                }
                let Some(l) = layout.clone() else {
                    fail(&status, anyhow::anyhow!("cannot record: acquisition is not running"));
                    continue;
                };
                let now = chrono::Local::now();
                let forced = status.new_session.swap(false, Ordering::SeqCst);
                let root = status.root.lock().clone();
                // A changed root directory (say, the next animal) also starts a new session.
                if needs_new_session(session_date, now.date_naive(), forced) || session_root.as_ref() != Some(&root) {
                    // Never reuse a session folder (two sessions within one second).
                    let stem = now.format("%Y-%m-%d_%H-%M-%S").to_string();
                    let name = (1..).map(|k| if k == 1 { stem.clone() } else { format!("{stem}_{k}") }).find(|n| !root.join(n).exists()).unwrap();
                    session_date = Some(now.date_naive());
                    session = Some(root.join(name).join(format!("Record Node {RECORD_NODE_ID}")));
                    session_root = Some(root);
                    experiment = 0;
                    acq_run_seen = true;
                }
                if acq_run_seen {
                    experiment += 1;
                    recording_idx = 0;
                    acq_run_seen = false;
                }
                recording_idx += 1;
                let rec_dir = |i: u32| session.as_ref().unwrap().join(format!("experiment{experiment}")).join(format!("recording{i}"));
                // Never into an existing recording folder (its files would be refused anyway).
                while rec_dir(recording_idx).exists() {
                    recording_idx += 1;
                }
                let dir = rec_dir(recording_idx);
                match fs::create_dir_all(&dir).map_err(anyhow::Error::from).and_then(|_| Recording::open(&dir, &l)) {
                    Ok(r) => {
                        info!("recording to {}", dir.display());
                        *status.error.lock() = None;
                        *status.current_dir.lock() = Some(dir);
                        status.bytes.store(0, Ordering::Relaxed);
                        status.samples.store(0, Ordering::Relaxed);
                        status.recording.store(true, Ordering::SeqCst);
                        rec = Some(r);
                    }
                    Err(e) => fail(&status, e),
                }
            }
            RecMsg::Stop => {
                if let Some(r) = rec.take() {
                    if let Err(e) = r.finish() {
                        fail(&status, e);
                    }
                    info!("recording stopped");
                }
                status.recording.store(false, Ordering::SeqCst);
            }
            RecMsg::Message { sample, text, keys } => {
                if let Some(r) = rec.as_mut()
                    && let Err(e) = r.write_message(sample, &text, &keys)
                {
                    fail(&status, e);
                    abandon(rec.take());
                }
            }
            RecMsg::Shutdown => {
                if let Some(r) = rec.take()
                    && let Err(e) = r.finish()
                {
                    fail(&status, e);
                }
                status.recording.store(false, Ordering::SeqCst);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rhythm::{ChannelInfo, SideStreamInfo};

    #[test]
    fn new_session_on_first_recording_when_forced_or_on_a_new_day() {
        let d = |day| chrono::NaiveDate::from_ymd_opt(2026, 11, day).unwrap();
        assert!(needs_new_session(None, d(5), false));
        assert!(!needs_new_session(Some(d(5)), d(5), false));
        assert!(needs_new_session(Some(d(5)), d(5), true));
        assert!(needs_new_session(Some(d(5)), d(6), false));
    }

    fn read_npy(path: &Path) -> (String, Vec<u8>) {
        let b = fs::read(path).unwrap();
        assert_eq!(&b[..8], b"\x93NUMPY\x01\x00");
        let hl = u16::from_le_bytes([b[8], b[9]]) as usize;
        assert_eq!((10 + hl) % 64, 0);
        (String::from_utf8(b[10..10 + hl].to_vec()).unwrap(), b[10 + hl..].to_vec())
    }

    #[test]
    fn message_records_are_cut_at_a_character_boundary() {
        let long = "é".repeat(300); // 600 bytes
        let r = message_record(&long);
        assert_eq!(r.len(), 513);
        assert_eq!(std::str::from_utf8(&r[..512]).unwrap(), "é".repeat(256));
        assert_eq!(r[512], 0);
    }

    /// After dropped samples the file skips ahead; reading back by sample number must
    /// put every sample at its own number (review and the overview depend on it).
    #[test]
    fn reads_back_by_sample_number_across_gaps() {
        let root = std::env::temp_dir().join(format!("oers-rec-gap-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let layout = StreamLayout {
            name: "acquisition_board".into(),
            sample_rate: 30000.0,
            channels: (0..2).map(|i| ChannelInfo { name: format!("CH{}", i + 1), kind: ChannelKind::Electrode, bit_volts: 0.195, units: "uV".into(), headstage: Some(0) }).collect(),
            headstages: vec![],
            ttl_lines: 8,
            side_streams: vec![],
            hw_clock_hz: 0.0,
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        let st = Arc::new(RecStatus::default());
        *st.root.lock() = root.clone();
        let st2 = st.clone();
        let h = std::thread::spawn(move || run_recorder(rx, st2));
        tx.send(RecMsg::AcquisitionStarted(layout)).unwrap();
        tx.send(RecMsg::Start).unwrap();
        // Blocks at 1000, 1032, then a gap to 1200, 1232, then a gap to 1300.
        for first in [1000i64, 1032, 1200, 1232, 1300] {
            let mut b = Block::with_capacity(2, 32);
            b.first_sample = first;
            for s in first..first + 32 {
                b.data.extend([s as i16, -(s as i16)]);
                b.timestamps.push(s as f64 / 30000.0);
                b.ttl.push(0);
            }
            tx.send(RecMsg::Data(Arc::new(b))).unwrap();
        }
        tx.send(RecMsg::Stop).unwrap();
        tx.send(RecMsg::Shutdown).unwrap();
        h.join().unwrap();
        let src = st.source.lock().clone().unwrap();
        assert_eq!(*src.gaps, vec![(1200, 64), (1300, 128)]);
        assert_eq!(src.available(), 332, "the span runs to sample 1332");
        let f = File::open(&src.dat).unwrap();
        let data = src.read(&f, 1050, 300).unwrap();
        for (i, s) in (1050i64..1350).enumerate() {
            let present = (1000..1064).contains(&s) || (1200..1264).contains(&s) || (1300..1332).contains(&s);
            let want = if present { [s as i16, -(s as i16)] } else { [0, 0] };
            assert_eq!([data[2 * i], data[2 * i + 1]], want, "sample {s}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_malformed_block_stops_the_recording_loudly() {
        let root = std::env::temp_dir().join(format!("oers-rec-bad-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let layout = StreamLayout {
            name: "acquisition_board".into(),
            sample_rate: 30000.0,
            channels: (0..2).map(|i| ChannelInfo { name: format!("CH{}", i + 1), kind: ChannelKind::Electrode, bit_volts: 0.195, units: "uV".into(), headstage: Some(0) }).collect(),
            headstages: vec![],
            ttl_lines: 8,
            side_streams: vec![],
            hw_clock_hz: 0.0,
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        let st = Arc::new(RecStatus::default());
        *st.root.lock() = root.clone();
        let st2 = st.clone();
        let h = std::thread::spawn(move || run_recorder(rx, st2));
        tx.send(RecMsg::AcquisitionStarted(layout)).unwrap();
        tx.send(RecMsg::Start).unwrap();
        let block = |first: i64, short: bool| {
            let mut b = Block::with_capacity(2, 32);
            b.first_sample = first;
            for s in first..first + 32 {
                b.data.extend([s as i16, 0]);
                b.timestamps.push(0.0);
                b.ttl.push(0);
            }
            if short {
                b.data.pop();
            }
            Arc::new(b)
        };
        tx.send(RecMsg::Data(block(0, false))).unwrap();
        tx.send(RecMsg::Data(block(32, true))).unwrap();
        tx.send(RecMsg::Data(block(64, false))).unwrap();
        tx.send(RecMsg::Shutdown).unwrap();
        h.join().unwrap();
        assert!(st.error.lock().as_deref().is_some_and(|e| e.contains("malformed block at sample 32")), "{:?}", st.error.lock());
        assert!(!st.recording.load(Ordering::SeqCst));
        // Finished up to the good block: the npy header says 32 samples, the file holds them.
        let dir = st.current_dir.lock().clone().unwrap();
        let cont = fs::read_dir(dir.join("continuous")).unwrap().next().unwrap().unwrap().path();
        assert_eq!(fs::metadata(cont.join("continuous.dat")).unwrap().len(), 32 * 2 * 2);
        let (hdr, body) = read_npy(&cont.join("sample_numbers.npy"));
        assert!(hdr.contains("(32,)"), "{hdr}");
        assert_eq!(body.len(), 32 * 8);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_session_in_the_same_second_never_reuses_a_folder() {
        let root = std::env::temp_dir().join(format!("oers-rec-collide-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let layout = StreamLayout { name: "acquisition_board".into(), sample_rate: 30000.0, channels: vec![ChannelInfo { name: "CH1".into(), kind: ChannelKind::Electrode, bit_volts: 0.195, units: "uV".into(), headstage: Some(0) }], headstages: vec![], ttl_lines: 8, side_streams: vec![], hw_clock_hz: 0.0 };
        let (tx, rx) = crossbeam_channel::unbounded();
        let st = Arc::new(RecStatus::default());
        *st.root.lock() = root.clone();
        let st2 = st.clone();
        let h = std::thread::spawn(move || run_recorder(rx, st2));
        tx.send(RecMsg::AcquisitionStarted(layout)).unwrap();
        let mut dirs = Vec::new();
        for _ in 0..3 {
            // "New session directory" each time, well within one second.
            st.new_session.store(true, Ordering::SeqCst);
            tx.send(RecMsg::Start).unwrap();
            let mut b = Block::with_capacity(1, 1);
            b.data.push(7);
            b.timestamps.push(0.0);
            b.ttl.push(0);
            tx.send(RecMsg::Data(Arc::new(b))).unwrap();
            tx.send(RecMsg::Stop).unwrap();
            let t = Instant::now();
            while st.current_dir.lock().as_ref() == dirs.last() || st.recording.load(Ordering::SeqCst) {
                assert!(t.elapsed() < Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(5));
            }
            dirs.push(st.current_dir.lock().clone().unwrap());
        }
        // A changed root starts a new session there.
        let root2 = root.join("animal2");
        *st.root.lock() = root2.clone();
        tx.send(RecMsg::Start).unwrap();
        tx.send(RecMsg::Stop).unwrap();
        tx.send(RecMsg::Shutdown).unwrap();
        h.join().unwrap();
        assert!(st.error.lock().is_none(), "{:?}", st.error.lock());
        dirs.sort();
        dirs.dedup();
        assert_eq!(dirs.len(), 3, "{dirs:?}");
        for d in &dirs {
            assert!(d.join("structure.oebin").exists());
        }
        assert!(st.current_dir.lock().as_ref().unwrap().starts_with(&root2));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn writes_valid_open_ephys_binary() {
        let root = std::env::temp_dir().join(format!("oers-rec-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let layout = StreamLayout {
            name: "acquisition_board".into(),
            sample_rate: 30000.0,
            channels: (0..4).map(|i| ChannelInfo { name: format!("CH{}", i + 1), kind: ChannelKind::Electrode, bit_volts: 0.195, units: "uV".into(), headstage: Some(0) }).collect(),
            headstages: vec![],
            ttl_lines: 8,
            side_streams: vec![SideStreamInfo { name: "memory_usage".into(), sample_rate: 100.0, channel_names: vec!["MEM".into()], units: vec!["%".into()], bit_volts: vec![1.0] }],
            hw_clock_hz: 250e6,
        };
        let (tx, rx) = crossbeam_channel::unbounded();
        let st = Arc::new(RecStatus::default());
        *st.root.lock() = root.clone();
        let st2 = st.clone();
        let h = std::thread::spawn(move || run_recorder(rx, st2));
        tx.send(RecMsg::AcquisitionStarted(layout)).unwrap();
        tx.send(RecMsg::Start).unwrap();
        for k in 0..10i64 {
            let mut b = Block::with_capacity(4, 32);
            b.first_sample = 1000 + 32 * k;
            for s in 0..32 {
                for c in 0..4 {
                    b.data.push((c * 100 + s) as i16);
                }
                b.timestamps.push((1000 + 32 * k + s as i64) as f64 / 30000.0);
                b.ttl.push(if (k * 32 + s as i64) >= 50 && (k * 32 + s as i64) < 100 { 0b101 } else { 0 });
            }
            b.side.push(rhythm::SideSample { stream: 0, sample_number: k, timestamp: 0.0, values: vec![12.4] });
            tx.send(RecMsg::Data(Arc::new(b))).unwrap();
        }
        tx.send(RecMsg::Message { sample: 1100, text: "drinking ☕".into(), keys: vec![(1100, "/".into()), (1130, "d".into())] }).unwrap();
        tx.send(RecMsg::Stop).unwrap();
        tx.send(RecMsg::Shutdown).unwrap();
        h.join().unwrap();
        assert!(st.error.lock().is_none(), "{:?}", st.error.lock());

        let dir = st.current_dir.lock().clone().unwrap();
        assert!(dir.ends_with("Record Node 101/experiment1/recording1"));
        let oebin: serde_json::Value = serde_json::from_str(&fs::read_to_string(dir.join("structure.oebin")).unwrap()).unwrap();
        assert_eq!(oebin["continuous"][0]["num_channels"], 4);
        assert_eq!(oebin["continuous"][0]["folder_name"], "Acquisition_Board-100.acquisition_board/");
        let cd = dir.join("continuous/Acquisition_Board-100.acquisition_board");
        let dat = fs::read(cd.join("continuous.dat")).unwrap();
        assert_eq!(dat.len(), 320 * 4 * 2);
        assert_eq!(i16::from_le_bytes([dat[2 * 5], dat[2 * 5 + 1]]), 101); // sample 1, ch 1
        let (h, d) = read_npy(&cd.join("sample_numbers.npy"));
        assert!(h.contains("'shape': (320,)"), "{h}");
        assert_eq!(i64::from_le_bytes(d[..8].try_into().unwrap()), 1000);
        let ed = dir.join("events/Acquisition_Board-100.acquisition_board/TTL");
        let (h, d) = read_npy(&ed.join("states.npy"));
        assert!(h.contains("'shape': (4,)"), "{h}");
        let states: Vec<i16> = d.chunks(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(states, vec![1, 3, -1, -3]);
        let (_, d) = read_npy(&ed.join("sample_numbers.npy"));
        assert_eq!(i64::from_le_bytes(d[..8].try_into().unwrap()), 1050);
        let (h, _) = read_npy(&dir.join("continuous/Acquisition_Board-100.memory_usage/timestamps.npy"));
        assert!(h.contains("'shape': (10,)"));
        assert!(fs::read_to_string(dir.join("sync_messages.txt")).unwrap().contains("@ 30000 Hz: 1000"));
        // Typed note: stock MessageCenter layout, main-stream sample number and seconds.
        assert_eq!(oebin["events"][1]["folder_name"], "MessageCenter/");
        let md = dir.join("events/MessageCenter");
        let (h, d) = read_npy(&md.join("text.npy"));
        assert!(h.contains("'descr': '|S513'") && h.contains("'shape': (1,)"), "{h}");
        assert_eq!(d.len(), 513);
        assert_eq!(&d[..12], "drinking ☕".as_bytes());
        assert!(d[12..].iter().all(|&b| b == 0));
        let (_, d) = read_npy(&md.join("sample_numbers.npy"));
        assert_eq!(i64::from_le_bytes(d[..8].try_into().unwrap()), 1100);
        let (_, d) = read_npy(&md.join("timestamps.npy"));
        assert!((f64::from_le_bytes(d[..8].try_into().unwrap()) - 1100.0 / 30000.0).abs() < 1e-9);
        let keys = fs::read_to_string(dir.join("note_keystrokes.tsv")).unwrap();
        assert_eq!(keys, "note_sample\tkey_sample\tkey\n1100\t1100\t/\n1100\t1130\td\n");
        let _ = fs::remove_dir_all(&root);
    }
}
