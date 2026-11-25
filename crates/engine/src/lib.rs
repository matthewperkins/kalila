//! Acquisition engine: owns the board, runs the real-time acquisition thread and fans
//! blocks out to the recorder (lossless, back-pressured), DSP/spike detection, audio
//! monitor and display (all lossy, never able to stall acquisition).
//!
//! ```text
//!            ┌──────────── recorder  (blocking send: back-pressure into the board's DRAM)
//!  board ──► acq thread ─┼── dsp       (try_send) ──► spikes ──► GUI
//!  (RT prio)             ├── audio     (try_send) ──► cpal/PipeWire
//!                        └── display   (try_send) ──► GUI / GPU upload
//! ```

pub mod audio;
pub mod dsp;
pub mod pyramid;
pub mod record;
mod sched;

use anyhow::{Result, bail};
use audio::{AudioMonitor, AudioSettings, AudioStats};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};
use dsp::{BandSpec, Car, FilterBank, Spike, SpikeDetector, ThresholdMode};
use log::{error, info, warn};
use parking_lot::Mutex;
use record::{RecMsg, RecStatus};
pub use rhythm;
use rhythm::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct DspSettings {
    pub band: BandSpec,
    pub threshold: ThresholdMode,
    pub detect: bool,
    /// Common average reference before detection; set by the GUI, not saved here.
    #[serde(skip)]
    pub car: Option<Arc<Car>>,
}

impl Default for DspSettings {
    fn default() -> Self {
        Self {
            band: BandSpec::SPIKES,
            threshold: ThresholdMode::Sigma(4.5),
            detect: true,
            car: None,
        }
    }
}

/// Counters for the lossy consumers (display/dsp/audio drop blocks rather than stall).
#[derive(Default)]
pub struct PipelineStats {
    pub display_dropped: AtomicU64,
    pub dsp_dropped: AtomicU64,
    pub audio_dropped: AtomicU64,
    pub pyramid_dropped: AtomicU64,
    /// Time (µs) the acquisition thread spent blocked on the recorder queue, total.
    pub rec_backpressure_us: AtomicU64,
    pub rec_queue: AtomicU64,
    pub blocks: AtomicU64,
}

/// Noise σ and thresholds per electrode, published by the DSP thread.
#[derive(Default, Clone)]
pub struct ChannelStats {
    pub sigma_uv: Vec<f32>,
    pub threshold_uv: Vec<f32>,
    pub spike_counts: Vec<u64>,
}

pub enum BoardChoice {
    Hardware,
    Simulated(Vec<usize>),
}

enum Job {
    Open(JoinHandle<Result<Box<dyn Board>>>),
    Op(JoinHandle<(Box<dyn Board>, Result<()>)>, &'static str),
}

struct Running {
    stop: Arc<AtomicBool>,
    acq: JoinHandle<(Box<dyn Board>, Result<()>)>,
    workers: Vec<JoinHandle<()>>,
    started: Instant,
    /// The board's settings while it is owned by the acquisition thread (kept in step with
    /// live commands), so `board_settings()` still answers during acquisition.
    settings: BoardSettings,
    rates: Vec<u32>,
}

pub struct Engine {
    board: Option<Box<dyn Board>>,
    /// The last board chosen is the simulator (kept even if opening fails).
    simulated: bool,
    job: Option<Job>,
    running: Option<Running>,
    /// An acquisition thread that didn't stop in time (`stop_acquisition`): joined by `poll`
    /// if it ever ends, else left behind.
    stuck: Option<Running>,
    pub last_error: Option<String>,
    pub health: SharedHealth,
    pub pipeline: Arc<PipelineStats>,
    pub audio_settings: Arc<Mutex<AudioSettings>>,
    pub audio_stats: Arc<AudioStats>,
    pub dsp_settings: Arc<Mutex<DspSettings>>,
    pub channel_stats: Arc<Mutex<ChannelStats>>,
    pub rec_status: Arc<RecStatus>,
    /// Min/max pyramids of the recording being written (scroll-back review of long spans).
    pub pyramids: Arc<Mutex<pyramid::Store>>,
    rec_tx: Sender<RecMsg>,
    rec_thread: Option<JoinHandle<()>>,
    live_tx: Option<Sender<LiveCommand>>,
    display_rx: Option<Receiver<Arc<Block>>>,
    spike_rx: Option<Receiver<Spike>>,
    layout: Option<StreamLayout>,
    info: Option<BoardInfo>,
    rt_priority: bool,
}

impl Engine {
    pub fn new(record_root: std::path::PathBuf) -> Self {
        let (rec_tx, rec_rx) = unbounded();
        let rec_status = Arc::new(RecStatus::default());
        *rec_status.root.lock() = record_root;
        let st = rec_status.clone();
        let rec_thread = std::thread::Builder::new()
            .name("recorder".into())
            .spawn(move || record::run_recorder(rec_rx, st))
            .expect("spawn recorder");
        Self {
            board: None,
            simulated: false,
            job: None,
            running: None,
            stuck: None,
            last_error: None,
            health: Arc::new(Health::default()),
            pipeline: Arc::new(PipelineStats::default()),
            audio_settings: Arc::new(Mutex::new(AudioSettings::default())),
            audio_stats: Arc::new(AudioStats::default()),
            dsp_settings: Arc::new(Mutex::new(DspSettings::default())),
            channel_stats: Arc::new(Mutex::new(ChannelStats::default())),
            rec_status,
            pyramids: Arc::new(Mutex::new(pyramid::Store::default())),
            rec_tx,
            rec_thread: Some(rec_thread),
            live_tx: None,
            display_rx: None,
            spike_rx: None,
            layout: None,
            info: None,
            rt_priority: false,
        }
    }

    // -------------------------------------------------------------- board lifecycle
    pub fn open_board(&mut self, choice: BoardChoice, settings: BoardSettings) {
        if self.is_busy() || self.is_acquiring() {
            return;
        }
        self.board = None;
        self.layout = None;
        self.last_error = None;
        self.simulated = matches!(choice, BoardChoice::Simulated(_));
        let h = std::thread::Builder::new()
            .name("board-open".into())
            .spawn(move || -> Result<Box<dyn Board>> {
                let mut b: Box<dyn Board> = match choice {
                    BoardChoice::Hardware => Box::new(OniBoard::open()?),
                    BoardChoice::Simulated(hs) => Box::new(SimBoard::new(hs)),
                };
                if b.settings() != &settings {
                    b.apply_settings(&settings)?;
                }
                Ok(b)
            })
            .expect("spawn");
        self.job = Some(Job::Open(h));
    }

    /// Run a (possibly slow) operation on the idle board in the background.
    pub fn board_op(&mut self, what: &'static str, f: impl FnOnce(&mut dyn Board) -> Result<()> + Send + 'static) {
        if self.is_busy() || self.is_acquiring() {
            return;
        }
        let Some(mut b) = self.board.take() else { return };
        let h = std::thread::Builder::new()
            .name("board-op".into())
            .spawn(move || {
                let r = f(b.as_mut());
                (b, r)
            })
            .expect("spawn");
        self.job = Some(Job::Op(h, what));
    }

    pub fn apply_board_settings(&mut self, s: BoardSettings) {
        self.board_op("apply settings", move |b| b.apply_settings(&s));
    }
    pub fn rescan(&mut self) {
        self.board_op("scan ports", |b| b.scan_ports());
    }

    /// Call once per GUI frame.
    pub fn poll(&mut self) {
        let finished = match &self.job {
            Some(Job::Open(h)) => h.is_finished(),
            Some(Job::Op(h, _)) => h.is_finished(),
            None => false,
        };
        if finished {
            match self.job.take().unwrap() {
                Job::Open(h) => match h.join() {
                    Ok(Ok(b)) => self.set_board(b),
                    Ok(Err(e)) => self.last_error = Some(format!("{e:#}")),
                    Err(_) => self.last_error = Some("board open thread panicked".into()),
                },
                Job::Op(h, what) => match h.join() {
                    Ok((b, r)) => {
                        if let Err(e) = r {
                            self.last_error = Some(format!("{what}: {e:#}"));
                        }
                        self.set_board(b);
                    }
                    Err(_) => self.last_error = Some(format!("{what}: thread panicked (board lost; reopen)")),
                },
            }
        }
        // A thread left behind by `stop_acquisition` that has ended after all.
        if self.stuck.as_ref().is_some_and(|r| r.acq.is_finished()) {
            let r = self.stuck.take().unwrap();
            info!("the stuck acquisition thread has ended");
            if self.board.is_some() || self.running.is_some() || self.job.is_some() {
                // Another board is in use by now: close the old one.
                drop(r.acq.join());
                for w in r.workers {
                    let _ = w.join();
                }
            } else {
                self.finish_stopped(r);
            }
        }
        // Acquisition thread died on its own (e.g. USB unplugged)?
        if self.running.as_ref().is_some_and(|r| r.acq.is_finished()) {
            self.stop_acquisition();
        }
        // A worker that panicked while acquisition runs: report it once; recording carries
        // on. (A worker that returned normally saw the acquisition thread drop its sender,
        // which can be seen here just before `acq.is_finished()`.)
        if let Some(r) = &mut self.running
            && let Some(i) = r.workers.iter().position(|w| w.is_finished())
        {
            let w = r.workers.swap_remove(i);
            let name = w.thread().name().unwrap_or("worker").to_string();
            if w.join().is_err() {
                error!("{name} thread crashed");
                self.last_error = Some(format!(
                    "the {name} thread crashed: {}. Recording is not affected; stop and restart acquisition to recover.",
                    worker_role(&name)
                ));
            }
        }
        // The recorder thread ends only at shutdown; earlier means it crashed.
        if self.rec_thread.as_ref().is_some_and(|h| h.is_finished()) {
            let _ = self.rec_thread.take().map(|h| h.join());
            error!("recorder thread ended");
            self.last_error = Some("the recorder thread crashed: nothing more can be recorded in this session. Restart kalila to record again.".into());
        }
    }

    fn set_board(&mut self, b: Box<dyn Board>) {
        self.layout = Some(b.layout());
        self.info = Some(b.info());
        self.board = Some(b);
    }

    pub fn is_busy(&self) -> bool {
        self.job.is_some()
    }
    pub fn busy_label(&self) -> Option<&'static str> {
        match &self.job {
            Some(Job::Open(_)) => Some("opening board"),
            Some(Job::Op(_, w)) => Some(w),
            None => None,
        }
    }
    /// Whether the last board chosen (open, opening or failed) is the simulator.
    pub fn is_simulated(&self) -> bool {
        self.simulated
    }
    pub fn has_board(&self) -> bool {
        self.board.is_some() || self.running.is_some()
    }
    pub fn board_settings(&self) -> Option<BoardSettings> {
        self.board
            .as_ref()
            .map(|b| b.settings().clone())
            .or_else(|| self.running.as_ref().map(|r| r.settings.clone()))
    }
    pub fn available_sample_rates(&self) -> Vec<u32> {
        self.board
            .as_ref()
            .map(|b| b.available_sample_rates())
            .or_else(|| self.running.as_ref().map(|r| r.rates.clone()))
            .unwrap_or_default()
    }
    pub fn layout(&self) -> Option<&StreamLayout> {
        self.layout.as_ref()
    }
    pub fn info(&self) -> Option<&BoardInfo> {
        self.info.as_ref()
    }
    pub fn is_acquiring(&self) -> bool {
        self.running.is_some()
    }
    pub fn acquisition_time(&self) -> Option<Duration> {
        self.running.as_ref().map(|r| r.started.elapsed())
    }
    pub fn rt_priority(&self) -> bool {
        self.rt_priority
    }
    pub fn display_rx(&self) -> Option<&Receiver<Arc<Block>>> {
        self.display_rx.as_ref()
    }
    pub fn spike_rx(&self) -> Option<&Receiver<Spike>> {
        self.spike_rx.as_ref()
    }
    pub fn send_live(&mut self, c: LiveCommand) {
        if let Some(r) = &mut self.running {
            match &c {
                LiveCommand::SetLeds(on) => r.settings.leds = *on,
                LiveCommand::SetDacHpf { enabled, hz } => {
                    r.settings.dac_hpf_enabled = *enabled;
                    r.settings.dac_hpf_hz = *hz;
                }
                LiveCommand::SetNoiseSlicer(n) => r.settings.noise_slicer = *n,
                LiveCommand::SetTtlMode(on) => r.settings.ttl_output_mode = *on,
                LiveCommand::SetTtlOut(_) | LiveCommand::DacRoute { .. } => {}
            }
        }
        if let Some(tx) = &self.live_tx {
            let _ = tx.send(c);
        }
    }

    // ---------------------------------------------------------------- acquisition
    pub fn start_acquisition(&mut self) -> Result<()> {
        if self.running.is_some() {
            return Ok(());
        }
        if self.is_busy() {
            bail!("board is busy");
        }
        let Some(mut board) = self.board.take() else {
            bail!("no board open")
        };
        let layout = board.layout();
        let settings = board.settings().clone();
        let rates = board.available_sample_rates();
        if let Err(e) = board.start(self.health.clone()) {
            self.board = Some(board);
            return Err(e);
        }
        self.layout = Some(layout.clone());
        let fs = layout.sample_rate;
        let nch = layout.num_channels();
        let nel = layout.num_electrodes();
        let block_samples = board.settings().block_samples.max(1);
        let blocks_per_s = (fs / block_samples as f64).ceil() as usize;
        self.pipeline.display_dropped.store(0, Ordering::Relaxed);
        self.pipeline.dsp_dropped.store(0, Ordering::Relaxed);
        self.pipeline.audio_dropped.store(0, Ordering::Relaxed);
        self.pipeline.pyramid_dropped.store(0, Ordering::Relaxed);
        self.pipeline.rec_backpressure_us.store(0, Ordering::Relaxed);
        self.pipeline.blocks.store(0, Ordering::Relaxed);

        let stop = Arc::new(AtomicBool::new(false));
        let (live_tx, live_rx) = unbounded::<LiveCommand>();
        // Lossy queues: ~0.5 s for display, 1 s for DSP/audio.
        let (disp_tx, disp_rx) = bounded::<Arc<Block>>(blocks_per_s / 2 + 4);
        let (dsp_tx, dsp_rx) = bounded::<Arc<Block>>(blocks_per_s + 4);
        let (aud_tx, aud_rx) = bounded::<Arc<Block>>(blocks_per_s / 4 + 4);
        // The pyramid builder runs at idle priority, so it gets a longer queue (2 s).
        let (pyr_tx, pyr_rx) = bounded::<Arc<Block>>(blocks_per_s * 2 + 4);
        let (spk_tx, spk_rx) = bounded::<Spike>(20_000);
        let _ = self.rec_tx.send(RecMsg::AcquisitionStarted(layout.clone()));

        // Recorder queue limit by bytes (~20 s, ≤ 1 GiB) to bound memory.
        let block_bytes = (nch * block_samples * 2 + block_samples * 16).max(1);
        let rec_limit = ((fs as usize * 20 / block_samples).min((1usize << 30) / block_bytes)).max(64) as u64;

        let mut workers = Vec::new();
        // DSP / spike detection.
        {
            let rx = dsp_rx;
            let settings = self.dsp_settings.clone();
            let cs = self.channel_stats.clone();
            workers.push(
                std::thread::Builder::new()
                    .name("dsp".into())
                    .spawn(move || dsp_thread(rx, spk_tx, settings, cs, fs, nel))
                    .unwrap(),
            );
        }
        // Min/max pyramids of the recording.
        {
            let rx = pyr_rx;
            let store = self.pyramids.clone();
            let status = self.rec_status.clone();
            let pipe = self.pipeline.clone();
            let layout = layout.clone();
            workers.push(
                std::thread::Builder::new()
                    .name("pyramid".into())
                    .spawn(move || pyramid::pyramid_thread(rx, store, status, pipe, layout))
                    .unwrap(),
            );
        }
        // Audio feeder (owns the cpal stream).
        {
            let rx = aud_rx;
            let s = self.audio_settings.clone();
            let st = self.audio_stats.clone();
            workers.push(
                std::thread::Builder::new()
                    .name("audio-feed".into())
                    .spawn(move || {
                        sched::set_audio_feed();
                        let mut m = AudioMonitor::new(s, st, fs);
                        while let Ok(b) = rx.recv() {
                            m.process(&b);
                        }
                    })
                    .unwrap(),
            );
        }

        let health = self.health.clone();
        let pipe = self.pipeline.clone();
        let rec_tx = self.rec_tx.clone();
        let rec_status = self.rec_status.clone();
        let stop2 = stop.clone();
        let rt = Arc::new(AtomicBool::new(false));
        let rt2 = rt.clone();
        let acq = std::thread::Builder::new()
            .name("acquisition".into())
            .spawn(move || {
                // Sent however the thread ends (a panic included), so the recorder finalises
                // an open recording and clears `recording`; otherwise the GUI could neither
                // stop that recording nor close.
                let _stopped = AcqStopped(rec_tx.clone());
                let _awake = sched::KeepAwake::new();
                rt2.store(set_realtime(), Ordering::Relaxed);
                let r = acquisition_loop(
                    board.as_mut(),
                    &stop2,
                    &live_rx,
                    &disp_tx,
                    &dsp_tx,
                    &aud_tx,
                    &pyr_tx,
                    &rec_tx,
                    &rec_status,
                    rec_limit,
                    &health,
                    &pipe,
                );
                if let Err(e) = &r {
                    error!("acquisition loop ended with error: {e:#}");
                }
                if let Err(e) = board.stop() {
                    error!("board stop failed: {e:#}");
                }
                (board, r)
            })
            .unwrap();
        // Give the RT attempt a moment to report.
        std::thread::sleep(Duration::from_millis(5));
        self.rt_priority = rt.load(Ordering::Relaxed);
        if !self.rt_priority {
            warn!(
                "acquisition thread is not real-time{}. Board DRAM still buffers stalls.",
                if cfg!(target_os = "linux") {
                    "; see README (rtprio limits)"
                } else {
                    ""
                }
            );
        }
        self.live_tx = Some(live_tx);
        self.display_rx = Some(disp_rx);
        self.spike_rx = Some(spk_rx);
        self.running = Some(Running {
            stop,
            acq,
            workers,
            started: Instant::now(),
            settings,
            rates,
        });
        info!("acquisition running: {nch} channels @ {fs} Hz, {block_samples} samples/block");
        Ok(())
    }

    pub fn stop_acquisition(&mut self) {
        let Some(r) = self.running.take() else { return };
        r.stop.store(true, Ordering::SeqCst);
        self.live_tx = None;
        // The thread checks `stop` between blocks. A board that stops streaming without an
        // error could hold it in a read for good; don't freeze the GUI (or exit) on that.
        let t = Instant::now();
        while !r.acq.is_finished() && t.elapsed() < STOP_WAIT {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !r.acq.is_finished() {
            error!("acquisition thread did not stop within {STOP_WAIT:?}; leaving it");
            // Finish an open recording now: everything the board sent is already queued.
            let _ = self.rec_tx.send(RecMsg::Stop);
            self.last_error = Some("The board stopped responding, so acquisition could not stop cleanly (any recording was finished). Power-cycle the board or unplug its USB, then open it again.".into());
            self.stuck = Some(r);
            self.display_rx = None;
            self.spike_rx = None;
            return;
        }
        self.finish_stopped(r);
    }

    /// Join a stopped acquisition thread and its workers; the board comes back.
    fn finish_stopped(&mut self, r: Running) {
        match r.acq.join() {
            Ok((b, res)) => {
                if let Err(e) = res {
                    self.last_error = Some(format!("acquisition stopped: {e:#}"));
                }
                self.set_board(b);
            }
            Err(_) => self.last_error = Some("acquisition thread panicked; reopen the board".into()),
        }
        // Senders were dropped with the acquisition thread; workers drain and exit.
        for w in r.workers {
            let _ = w.join();
        }
        self.display_rx = None;
        self.spike_rx = None;
    }

    /// Write a typed note into the open recording (MessageCenter text event at `sample`,
    /// main-stream clock). Ignored when not recording.
    pub fn record_message(&self, sample: i64, text: String, keys: Vec<(i64, String)>) {
        let _ = self.rec_tx.send(RecMsg::Message { sample, text, keys });
    }

    pub fn start_recording(&self) {
        if self.rec_thread.as_ref().is_none_or(|h| h.is_finished()) || self.rec_tx.send(RecMsg::Start).is_err() {
            *self.rec_status.error.lock() =
                Some("cannot record: the recorder thread has crashed. Restart kalila.".into());
        }
    }
    pub fn stop_recording(&self) {
        let _ = self.rec_tx.send(RecMsg::Stop);
    }
    pub fn is_recording(&self) -> bool {
        self.rec_status.recording.load(Ordering::SeqCst)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop_acquisition();
        let _ = self.rec_tx.send(RecMsg::Shutdown);
        if let Some(h) = self.rec_thread.take() {
            let _ = h.join();
        }
    }
}

/// How long Stop waits for the acquisition thread (it normally takes one block, ~1 ms).
const STOP_WAIT: Duration = Duration::from_secs(3);

/// Tells the recorder that acquisition has ended, when dropped.
struct AcqStopped(Sender<RecMsg>);
impl Drop for AcqStopped {
    fn drop(&mut self) {
        let _ = self.0.send(RecMsg::AcquisitionStopped);
    }
}

/// What stops working when a worker thread dies (recording is never affected).
fn worker_role(name: &str) -> &'static str {
    match name {
        "dsp" => "spike detection and the spike tiles have stopped",
        "pyramid" => "the scroll-back overview no longer updates",
        "audio-feed" => "the audio monitor has stopped",
        _ => "part of the display has stopped",
    }
}

use sched::set_realtime;

#[allow(clippy::too_many_arguments)]
fn acquisition_loop(
    board: &mut dyn Board,
    stop: &AtomicBool,
    live_rx: &Receiver<LiveCommand>,
    disp: &Sender<Arc<Block>>,
    dsp: &Sender<Arc<Block>>,
    aud: &Sender<Arc<Block>>,
    pyr: &Sender<Arc<Block>>,
    rec: &Sender<RecMsg>,
    rec_status: &RecStatus,
    rec_limit: u64,
    _health: &Health,
    pipe: &PipelineStats,
) -> Result<()> {
    // Block pool: reuse blocks once every consumer has released them, so the acquisition
    // thread doesn't allocate in steady state. A block can sit in every lossy queue at once,
    // so the pool covers their combined depth (a few seconds; ~16 MB at 64 channels): then a
    // lagging consumer (the idle-priority pyramid builder, say) still doesn't cause
    // allocation. Only a recorder backlog beyond that does (a disk stall: one allocation per
    // block, sized up front).
    let (nch, bs) = (board.layout().num_channels(), board.settings().block_samples.max(1));
    let new_block = || Arc::new(Block::with_capacity(nch, bs));
    let pool_len = [disp, dsp, aud, pyr]
        .iter()
        .map(|tx| tx.capacity().unwrap_or(0))
        .sum::<usize>()
        + 16;
    let mut pool: Vec<Arc<Block>> = (0..pool_len).map(|_| new_block()).collect();
    let mut idx = 0;
    while !stop.load(Ordering::Relaxed) {
        while let Ok(c) = live_rx.try_recv() {
            if let Err(e) = board.live(&c) {
                warn!("live command {c:?} failed: {e:#}");
            }
        }
        let slot = &mut pool[idx];
        if Arc::get_mut(slot).is_none() {
            *slot = new_block(); // still in use downstream (recorder backlog); allocate
        }
        let b = Arc::get_mut(slot).unwrap();
        board.read_block(b)?;
        b.acquired_at = Some(Instant::now());
        let blk = slot.clone();
        idx = (idx + 1) % pool_len;
        pipe.blocks.fetch_add(1, Ordering::Relaxed);

        // Recorder: lossless while recording. If the disk falls behind we block here; the
        // board's DRAM absorbs the stall (and any overflow shows up as counted gaps).
        if rec_status.recording.load(Ordering::Relaxed) {
            let t = Instant::now();
            // `recording` also clears if the recorder thread dies, so this can't spin forever.
            while rec.len() as u64 >= rec_limit
                && !stop.load(Ordering::Relaxed)
                && rec_status.recording.load(Ordering::Relaxed)
            {
                std::thread::sleep(Duration::from_micros(200));
            }
            let w = t.elapsed().as_micros() as u64;
            if w > 0 {
                pipe.rec_backpressure_us.fetch_add(w, Ordering::Relaxed);
            }
            if rec.send(RecMsg::Data(blk.clone())).is_err() {
                // The recorder thread is gone (its exit guard reports it); never silently.
                rec_status.recording.store(false, Ordering::SeqCst);
            }
            pipe.rec_queue.store(rec.len() as u64, Ordering::Relaxed);
        }
        for (tx, ctr) in [
            (disp, &pipe.display_dropped),
            (dsp, &pipe.dsp_dropped),
            (aud, &pipe.audio_dropped),
            (pyr, &pipe.pyramid_dropped),
        ] {
            if let Err(TrySendError::Full(_)) = tx.try_send(blk.clone()) {
                ctr.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    Ok(())
}

fn dsp_thread(
    rx: Receiver<Arc<Block>>,
    spikes: Sender<Spike>,
    settings: Arc<Mutex<DspSettings>>,
    cs: Arc<Mutex<ChannelStats>>,
    fs: f64,
    nel: usize,
) {
    let mut cur = settings.lock().clone();
    let mut bank = FilterBank::new(cur.band.sections(fs), nel);
    let mut det = SpikeDetector::new(fs, nel, cur.threshold);
    let mut row = vec![0.0f32; nel];
    let mut out = Vec::new();
    let mut counts = vec![0u64; nel];
    let mut last_pub = Instant::now();
    let mut car_mask = cur.car.as_ref().map(|c| c.mask(nel));
    while let Ok(b) = rx.recv() {
        let s = settings.lock().clone();
        if s.band != cur.band {
            bank = FilterBank::new(s.band.sections(fs), nel);
        }
        if s.car != cur.car {
            car_mask = s.car.as_ref().map(|c| c.mask(nel));
        }
        det.mode = s.threshold;
        cur = s;
        for t in 0..b.len() {
            let r = b.sample(t);
            let (car, mask) = match (&cur.car, &car_mask) {
                (Some(c), Some(m)) => (c.mean(r), m.as_slice()),
                _ => (0.0, &[][..]),
            };
            for (ch, v) in row.iter_mut().enumerate() {
                let x = r[ch] as f32
                    - if mask.get(ch).copied().unwrap_or(false) {
                        car
                    } else {
                        0.0
                    };
                *v = bank.tick(ch, x * ELECTRODE_UV_PER_BIT);
            }
            if cur.detect {
                det.push(b.first_sample + t as i64, &row, &mut out);
            }
        }
        for sp in out.drain(..) {
            counts[sp.channel] += 1;
            let _ = spikes.try_send(sp);
        }
        if last_pub.elapsed() > Duration::from_millis(200) {
            last_pub = Instant::now();
            let mut c = cs.lock();
            c.sigma_uv = (0..nel).map(|i| det.sigma(i)).collect();
            c.threshold_uv = (0..nel).map(|i| det.threshold(i)).collect();
            c.spike_counts = counts.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The simulator, panicking in `read_block` after `left` blocks.
    struct PanicAfter {
        inner: SimBoard,
        left: usize,
    }
    impl Board for PanicAfter {
        fn info(&self) -> BoardInfo {
            self.inner.info()
        }
        fn layout(&self) -> StreamLayout {
            self.inner.layout()
        }
        fn settings(&self) -> &BoardSettings {
            self.inner.settings()
        }
        fn apply_settings(&mut self, s: &BoardSettings) -> Result<()> {
            self.inner.apply_settings(s)
        }
        fn available_sample_rates(&self) -> Vec<u32> {
            self.inner.available_sample_rates()
        }
        fn scan_ports(&mut self) -> Result<()> {
            self.inner.scan_ports()
        }
        fn start(&mut self, h: SharedHealth) -> Result<()> {
            self.inner.start(h)
        }
        fn read_block(&mut self, blk: &mut Block) -> Result<()> {
            if self.left == 0 {
                panic!("test: board panics mid-recording");
            }
            self.left -= 1;
            self.inner.read_block(blk)
        }
        fn live(&mut self, c: &LiveCommand) -> Result<()> {
            self.inner.live(c)
        }
        fn stop(&mut self) -> Result<()> {
            self.inner.stop()
        }
    }

    /// The simulator, hanging in `read_block` after `left` blocks until `release` fires (a
    /// board that stops streaming without an error).
    struct HangAfter {
        inner: SimBoard,
        left: usize,
        release: crossbeam_channel::Receiver<()>,
    }
    impl Board for HangAfter {
        fn info(&self) -> BoardInfo {
            self.inner.info()
        }
        fn layout(&self) -> StreamLayout {
            self.inner.layout()
        }
        fn settings(&self) -> &BoardSettings {
            self.inner.settings()
        }
        fn apply_settings(&mut self, s: &BoardSettings) -> Result<()> {
            self.inner.apply_settings(s)
        }
        fn available_sample_rates(&self) -> Vec<u32> {
            self.inner.available_sample_rates()
        }
        fn scan_ports(&mut self) -> Result<()> {
            self.inner.scan_ports()
        }
        fn start(&mut self, h: SharedHealth) -> Result<()> {
            self.inner.start(h)
        }
        fn read_block(&mut self, blk: &mut Block) -> Result<()> {
            if self.left == 0 {
                let _ = self.release.recv();
            } else {
                self.left -= 1;
            }
            self.inner.read_block(blk)
        }
        fn live(&mut self, c: &LiveCommand) -> Result<()> {
            self.inner.live(c)
        }
        fn stop(&mut self) -> Result<()> {
            self.inner.stop()
        }
    }

    #[test]
    fn stop_does_not_hang_on_a_silent_board() {
        let root = std::env::temp_dir().join(format!("kalila-hang-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut e = Engine::new(root.clone());
        e.audio_settings.lock().enabled = false;
        let (tx, rx) = crossbeam_channel::bounded(1);
        e.set_board(Box::new(HangAfter {
            inner: SimBoard::new(vec![16]),
            left: 300,
            release: rx,
        }));
        e.start_acquisition().unwrap();
        e.start_recording();
        wait_for("recording", || e.is_recording());
        std::thread::sleep(Duration::from_millis(400)); // the board has gone silent by now
        let t = Instant::now();
        e.stop_acquisition();
        assert!(t.elapsed() < STOP_WAIT + Duration::from_secs(1));
        assert!(!e.is_acquiring() && !e.has_board());
        assert!(
            e.last_error
                .as_deref()
                .is_some_and(|m| m.contains("stopped responding")),
            "{:?}",
            e.last_error
        );
        wait_for("the recording to be finished", || !e.is_recording());
        // The read returns after all (say, the board was power-cycled): the board comes back.
        tx.send(()).unwrap();
        wait_for("the board back", || {
            e.poll();
            e.has_board()
        });
        drop(e);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let t = Instant::now();
        while !f() {
            assert!(t.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The overview (pyramids) of a recording holds only what is in its file: not the blocks
    /// acquired after Stop, nor a later acquisition run's (whose sample numbers restart at 0).
    #[test]
    fn the_overview_stops_at_the_end_of_the_recording() {
        let root = std::env::temp_dir().join(format!("kalila-pyr-end-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut e = Engine::new(root.clone());
        e.audio_settings.lock().enabled = false;
        e.set_board(Box::new(SimBoard::new(vec![16])));
        e.start_acquisition().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        e.start_recording();
        wait_for("recording", || e.is_recording());
        std::thread::sleep(Duration::from_millis(700));
        e.stop_recording();
        wait_for("the recording to stop", || !e.is_recording());
        let src = e.rec_status.source.lock().clone().unwrap();
        let file_end = pyramid::available_samples(&src);
        let check = |e: &Engine, when: &str| {
            let st = e.pyramids.lock();
            assert_eq!(st.src.as_ref().map(|s| &s.dat), Some(&src.dat), "{when}");
            assert!(
                st.live_end > 0 && st.live_end <= file_end,
                "{when}: overview to {} > file {file_end}",
                st.live_end
            );
        };
        // Still acquiring, not recording.
        std::thread::sleep(Duration::from_millis(1000));
        check(&e, "after Stop");
        let cols = |e: &Engine| {
            e.pyramids
                .lock()
                .raw
                .as_ref()
                .and_then(|p| p.columns(0, file_end / 64 * 64, (file_end / 64) as u32, false))
                .unwrap()
                .0
        };
        let before = cols(&e);
        // A new acquisition run: its samples 0.. would otherwise land on the recording's.
        e.stop_acquisition();
        e.start_acquisition().unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        check(&e, "after a restart");
        assert!(
            before == cols(&e),
            "the new run's samples changed the recording's overview"
        );
        drop(e);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_panic_in_acquisition_finalises_the_recording() {
        let root = std::env::temp_dir().join(format!("kalila-acq-panic-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut e = Engine::new(root.clone());
        e.audio_settings.lock().enabled = false;
        // ~0.3 s of blocks: enough to start recording before the panic.
        e.set_board(Box::new(PanicAfter {
            inner: SimBoard::new(vec![32]),
            left: 300,
        }));
        e.start_acquisition().unwrap();
        e.start_recording();
        wait_for("recording", || e.is_recording());
        wait_for("the acquisition thread to end", || {
            e.poll();
            !e.is_acquiring()
        });
        wait_for("the recorder to finalise", || !e.is_recording());
        assert!(
            e.last_error.as_deref().is_some_and(|m| m.contains("panicked")),
            "{:?}",
            e.last_error
        );
        // The recorder finished the recording normally (no recorder error).
        assert!(e.rec_status.error.lock().is_none());
        drop(e);
        let _ = std::fs::remove_dir_all(&root);
    }
}
