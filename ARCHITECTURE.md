# kalila: architecture

This is the map of the code, laid out by structure. `CLAUDE.md` holds the history and the
reasons, laid out by date. Every statement below names a file and a function or type, not a
line number. Paths are from the repository root.

## 1. Bird's-eye view

kalila acquires from the Open Ephys acquisition board (ECP5 gateware, FT600 USB 3 bridge, ONI
protocol), records every sample losslessly in Open Ephys Binary format, and shows, detects
and plays the same data live. Display, spike detection and audio can lose data; the recording
must not. The simulator (`SimBoard`) implements the same `Board` trait as the hardware
(`OniBoard`).

```text
USB ─► ft600 driver ─► liboni ─► OniBoard::read_block ─► acquisition thread ─┬─► recorder ─► disk
       (chip.rs,       (C,       (rhythm::assemble:                          │   (blocks while recording)
        usbfs.rs)      oni-sys)   frames → contiguous Block)                 ├─► display ─► GPU (egui/wgpu)
                                                                             ├─► dsp ─► spikes ─► spike tiles
                                                                             ├─► audio-feed ─► cpal ─► loudspeaker
                                                                             └─► pyramid ─► RAM (review overview)
```

The acquisition thread owns the board. It reads frames, assembles `Block`s (contiguous
samples, one `Arc<Block>` each) and hands the same `Arc` to five consumers. Only the recorder
may make it wait (`acquisition_loop` in `crates/engine/src/lib.rs`). Review (scroll-back)
reads the recording's own files back (`app/src/review.rs`, `RecSource::read`).

## 2. Code map

Dependencies point down only: `app` → `engine` → `rhythm` → `ft600`, `oni-sys` (`ft600` also
uses `oni-sys`). The `Cargo.toml` files are the check; `app` depends only on `engine`, and
reaches the board types through `engine::rhythm` (`pub use rhythm` in `engine/src/lib.rs`).

| Crate | Module | What it holds |
|---|---|---|
| `crates/oni-sys` | `src/lib.rs`, `build.rs`, `liboni/` | FFI to the vendored C liboni, compiled by `cc`; `oni_register_builtin_driver` lets Rust register a driver. |
| `crates/ft600` | `chip.rs` | FT600 wire protocol (`Ft600`); the only control-transfer site, behind `ALLOWED_REQUESTS`. |
| | `usbfs.rs` / `usb_iokit.rs` | USB without a library: Linux usbdevfs with async URBs; macOS IOKit. One is `usb` (`lib.rs`, by `cfg`). |
| | `driver.rs` | Port of liboni's FT600 driver; `register()` installs it as `DRIVER_NAME`. |
| `crates/rhythm` | `oni.rs` | Safe wrapper of liboni (`Context`, `Frame`); D3XX driver search. |
| | `oni_board.rs` | `OniBoard`: init, port scan, cable delay, registers, `read_block`; `Driver::from_env` (`KALILA_DRIVER`). |
| | `registers.rs` | Rhythm chip command lists and register values (literal port). |
| | `parser.rs` | `RhythmParser`: one frame to samples; counter checks; `encode_payload` (tests). |
| | `assemble.rs` | `assemble`: frames to contiguous blocks, shared by `OniBoard` and the simulation test. |
| | `sim_board.rs`, `types.rs` | `SimBoard`; `Block`, `StreamLayout`, `Health`, `BoardSettings`, `LiveCommand`, `Board`. |
| `crates/engine` | `lib.rs` | `Engine`: board lifecycle, thread start/stop, `acquisition_loop`, `dsp_thread`, `PipelineStats`. |
| | `record.rs` | `run_recorder`, `Recording` (Open Ephys Binary writer), `RecStatus`, `RecSource`. |
| | `pyramid.rs` | Min/max pyramids (raw and per-window band) in RAM, `pyramid_thread`, backfill from the file. |
| | `dsp.rs`, `audio.rs` | Filters, noise estimate, `SpikeDetector`, `Car`; the audio monitor and its cpal stream. |
| | `sched.rs` | Thread scheduling and keep-awake, per platform. |
| `app` (`kalila`) | `main.rs`, `app.rs` | Window setup; `App` (main window: controls, spike tiles, config, output sync). |
| | `app/views.rs`, `app/views/frame.rs` | Trace windows: `ViewsShared`, `ViewState`, one `ViewFrame` per window per frame. |
| | `traces.rs`, `spikes.rs`, `traces.wgsl`, `spikes.wgsl` | GPU renderers (`TraceGpus`, `TraceFrame`). |
| | `review.rs`, `sweep.rs`, `slope.rs`, `ttl.rs`, `notes.rs` | Review reader; triggered sweeps; slope trigger; TTL overlay; typed notes. |
| | `config.rs`, `uiscript.rs`, `presented.rs` | `Config` (JSON, migrations); scripted GUI input; macOS present timing. |

What each layer must not know:
- `ft600` and `oni-sys` know nothing of Rust threads, blocks, or recording.
- `rhythm` knows nothing of threads (other than the caller's), channels, files or the GUI;
  `parser.rs` and `assemble.rs` are pure functions of bytes.
- `engine` knows nothing of egui, wgpu or window layouts; `Config` is the app's, the engine
  gets `DspSettings` and `AudioSettings`.
- The GUI never touches the board: it reaches it through `Engine` (`send_live`,
  `apply_board_settings`, `board_op`). The recorder never reads display, DSP or audio
  settings.

## 3. Threads and queues

Threads, as named in the `thread::Builder` that spawns them:

| Name | Spawned in | What it does | Scheduling |
|---|---|---|---|
| (main) | `app/src/main.rs` | eframe event loop; `App::logic` polls the engine and runs while the root window is hidden; trace windows are deferred viewports drawn from `ViewsShared::window`. | normal |
| `acquisition` | `Engine::start_acquisition` | Owns the board; reads, assembles, fans out; applies `LiveCommand`s; stops the board on exit. | `sched::set_realtime`: `SCHED_FIFO` 70 on Linux, Mach time-constraint on macOS; falls back to max ordinary priority (`Engine::rt_priority` reports which) |
| `recorder` | `Engine::new` | `run_recorder`: files, checkpoints, `RecStatus`. Lives for the whole session. | normal (no `sched` call) |
| `dsp` | `start_acquisition` | `dsp_thread`: band-pass, CAR, `SpikeDetector`, publishes `ChannelStats`. | normal |
| `pyramid` | `start_acquisition` | `pyramid::pyramid_thread`: folds blocks into pyramids, backfills holes from the file. | `sched::set_idle`: `SCHED_IDLE` / macOS background QoS |
| `audio-feed` | `start_acquisition` | `AudioMonitor::process`: chain, resampler, writes the ring. | `sched::set_audio_feed`: max ordinary priority; macOS time-constraint |
| `review-reader` | `review::Reader::spawn` | Serves read requests from the recording's files. | normal |
| `board-open`, `board-op` | `Engine::open_board`, `board_op` | Slow board work (open, scan, apply settings) while idle. | normal |
| cpal callback | cpal (not spawned by kalila) | Pulls from the audio ring; `try_lock`s the consumer and outputs silence if it is held. | the audio backend's |

`ft600` starts no threads (`usbfs.rs`, `usb_iokit.rs`); liboni calls the driver on the
thread that owns the context, the acquisition thread.

Channels and rings (sizes in blocks; `blocks_per_s` = sample rate / `block_samples`):

| Queue | Bound | When full | Counted by |
|---|---|---|---|
| to recorder (`RecMsg`, `Engine::rec_tx`) | `unbounded()`, held to `rec_limit` by the producer: at most ~20 s of blocks or 1 GiB, at least 64 blocks | the acquisition thread waits in 200 µs sleeps (it sends only while `RecStatus::recording`) | `PipelineStats::rec_backpressure_us`, `rec_queue`; board DRAM overflow shows as `Health::dropped_samples` |
| display | `blocks_per_s / 2 + 4` | block dropped (`try_send`) | `PipelineStats::display_dropped` |
| dsp | `blocks_per_s + 4` | dropped | `dsp_dropped` |
| audio | `blocks_per_s / 4 + 4` | dropped | `audio_dropped` |
| pyramid | `blocks_per_s * 2 + 4` | dropped; backfill later fills the hole | `pyramid_dropped` |
| spikes (`dsp` to GUI) | `bounded(20_000)` | spike dropped (`try_send` in `dsp_thread`) | not counted |
| live commands | `unbounded()` | n/a (one human) | n/a |
| audio ring (`rtrb`) | 0.2 s of stereo output (`(fs_out / 5) * 2` f32 values) | `AudioMonitor::process` drops the chunk and counts | `AudioStats::overruns`, `underruns` |
| review requests, responses | `unbounded()` | the reader keeps the newest request per view | n/a |
| per-view staging (`ViewState::pending`) | one window of samples | oldest samples trimmed | n/a |

Other bounds: the block pool in `acquisition_loop` holds the sum of the four lossy queues'
capacities plus 16 blocks, so a lagging consumer does not make it allocate; only a recorder
backlog beyond that does (`Arc::get_mut` fails, a block is allocated). `App::spike_new` is
compacted to the newest per tile slot (`compact_newest_per_slot`). Pyramids stop at
`Store::budget` (`enforce_budget`). Stop waits `STOP_WAIT` (3 s) for the acquisition thread
(`Engine::stop_acquisition`, then `Engine::stuck`).

## 4. Invariants

Each: the rule; where it is kept; the test.

1. **The recorder is the only consumer that may block acquisition.** The other four use
   `try_send` and count drops. `acquisition_loop` (`crates/engine/src/lib.rs`). Not tested.
2. **A `Block` is contiguous: sample `k` is `first_sample + k`; a gap starts a new block.**
   `RhythmParser::parse` returns `Discontinuous`, `assemble` keeps that frame in `pending`
   for the next block (`crates/rhythm/src/parser.rs`, `assemble.rs`). Tests:
   `detects_gaps_without_double_counting`; `acquisition_to_recorder_under_seeded_faults`
   (`crates/engine/tests/simulation.rs`).
3. **A bad frame never becomes a gap.** A frame whose counter repeats or goes backwards is
   skipped and counted in `Health::bad_frames`; after `RESYNC_AFTER` such frames the counter
   is taken to have restarted (`RhythmParser::parse`). Tests:
   `skips_repeated_or_backwards_counters_and_resyncs_after_a_restart`, the simulation.
4. **`data.len() == len() * num_channels` and `ttl.len() == len()`, and sample numbers never
   go backwards within a recording.** `Recording::write_block` refuses anything else, the
   recording stops with a REC ERROR and every file is finished (`record.rs`). Test:
   `a_malformed_block_stops_the_recording_loudly`. The producer side (`parse`) does not
   assert it.
5. **`RecStatus::recording` is set only by the recorder (on a successful open).** It is
   cleared by the recorder on stop, error and exit (`Exit` guard in `run_recorder`) and by the
   acquisition thread if the send fails. The acquisition thread sends `Data` only while it
   is set. Test for the acquisition-side exit: `a_panic_in_acquisition_finalises_the_recording`
   (`engine/src/lib.rs`). Recorder death: not tested.
6. **Display, DSP, audio and review settings never reach a recording.** The recorder gets
   the raw `Arc<Block>`, which every consumer holds read-only; display filters run on the GPU
   (`traces.wgsl`), spike and audio filters on the consumers' own copies (`dsp_thread`,
   `AudioMonitor`). Structural; not tested.
7. **File index to sample number goes through `RecSource::gaps`.** `continuous.dat` has no
   holes. `Recording::write_block` records each restart (sample number, file index);
   `RecSource::read`, `available` and `end_sample` map through it; dropped samples read as 0.
   Tests: `reads_back_by_sample_number_across_gaps`, the simulation.
8. **Recordings are never overwritten or truncated.** Files use `create_new`; session and
   recording folders skip names that exist (`run_recorder`, `Recording::open`). Test:
   `a_second_session_in_the_same_second_never_reuses_a_folder`.
9. **What is on disk is a correct prefix, also when the disk is full.** Each file is
   finished one by one, data synced before metadata, `structure.oebin` written to a
   temporary file and renamed last (`Recording::finish`, `write_replace`); `.npy` headers
   are rewritten each second (`checkpoint`). Test: the simulation, seeds from 1000
   (`RLIMIT_FSIZE`). Crash truncation: not tested.
10. **Every thread death is loud.** `AcqStopped` sends `AcquisitionStopped` when the
    acquisition thread ends, however it ends; `Engine::poll` reports a crashed worker or
    recorder in `last_error`. Tests: `a_panic_in_acquisition_finalises_the_recording`,
    `stop_does_not_hang_on_a_silent_board`. A crashed worker or recorder: not tested.
11. **The overview holds only the file.** `pyramid_thread` folds live blocks in only while
    the recording is being written in this run (`earlier`, `this_run`); otherwise
    `Store::live_end` is the file's end. Test: `the_overview_stops_at_the_end_of_the_recording`.
12. **Pyramids and review are read-only consumers.** They open `continuous.dat` with
    `File::open` and use their own queue or request channel (`pyramid.rs`, `review.rs`). The
    builder yields to the recorder: backfill pauses on backpressure. Not tested.
13. **A trace-window frame reads one settings snapshot and queues edits.** `ViewFrame`
    never writes settings; `Edit`s are applied by `ViewsShared::apply_edits`
    (`app/src/app/views/frame.rs`, `views.rs`). Not tested.
14. **Electrode-indexed settings follow their electrodes.** DAC routes, CAR sets and audio
    channels are global electrode indices; `Config::electrode_keys` and
    `App::remap_electrodes` (with `ElectrodeMap`) move them when the layout changes. Test:
    `electrode_settings_follow_their_electrodes_across_a_layout_change` (`config.rs`).
15. **The audio chain keeps texture.** Held gain, no adapting AGC, a limiter linear below
    0.5 FS, a flat resampler (`audio.rs`). Tests in `audio.rs`: `limiter_is_monotone_bounded_and_transparent_for_spikes`, `held_gain_does_not_track_firing_rate`,
    `expander_preserves_relative_unit_amplitudes`, `hash_remains_audible_at_default_depth`,
    `resampler_rate_and_flatness`, `reference_mode_is_linear`; and `dsp.rs`:
    `noise_estimator_is_robust`.
16. **Nothing blocks the GUI on the board.** Slow work runs in `board-open` / `board-op`;
    Stop gives up after `STOP_WAIT`. Test: `stop_does_not_hang_on_a_silent_board`.

## 5. Hardware safety

- **USB control requests.** Every control transfer goes through `Ft600::control_transfer`
  (`crates/ft600/src/chip.rs`), which refuses any (request type, request) not in
  `ALLOWED_REQUESTS`: `(0xc0, 0xf1)` read firmware version, `(0xc0, 0xcf)` read chip
  configuration, `(0x40, 0x02)` GPIO. The chip-configuration write (`0x40 0xcf`) is not in
  the list and must never be. Tests in `chip.rs`:
  `the_control_requests_the_driver_uses_are_allowed`,
  `writing_the_chip_configuration_is_refused`,
  `a_forbidden_request_is_refused_before_it_reaches_the_kernel`,
  `this_file_makes_control_transfers_only_in_control_transfer`,
  `no_other_file_in_the_workspace_calls_or_defines_a_usb_control_transfer` (scans `crates/`
  and `app/` for other control-transfer calls and for the raw ioctl and IOKit request).
- **What else goes to the board.** Bulk transfers (announcements, aborts, ONI commands and
  data) in `chip.rs` and `driver.rs`; the ONI register writes and frames of `OniBoard`
  (`write_reg`, `write_reg_mask`, `write_frame` for TTL and DAC out). `OniBoard` reads the
  headstage EEPROM only (`i2c_read_byte` is a register read).
- **Nothing persistent.** The code has no flash, EEPROM or chip-configuration write path;
  the registers written are the board's own volatile ones. The worst case is a power cycle.
- **With `KALILA_DRIVER=d3xx`** the control traffic is FTDI's library's, outside these
  guards. `KALILA_USB_TRACE=1` logs the built-in driver's USB commands.
- **Tests never open a board.** `liboni_finds_the_registered_driver_and_init_fails_cleanly_without_a_board` (`driver.rs`) checks registration and a clean failure.

## 6. Platform seams

Linux is the first target; macOS is behind `cfg(target_os = "macos")` seams, not yet run
with a board.
- `crates/ft600/src/lib.rs`: `usbfs.rs` (usbdevfs) or `usb_iokit.rs` (IOKit) as `usb`.
- `crates/engine/src/sched.rs`: `SCHED_FIFO` / `SCHED_IDLE` or Mach time-constraint /
  background QoS; `KeepAwake` (power assertion) on macOS only.
- `crates/rhythm/src/oni.rs`: `DRIVER_FILE` (`.so` or `.dylib`) for the D3XX search.
- `app/src/main.rs`: wgpu backend (Vulkan and GL, or Metal), logging to a file when started
  from Finder; `app/src/presented.rs` (macOS only): present-time measurement.
- `app/src/app/views.rs` (`MOD`: Ctrl or Cmd), `views/frame.rs` (zoom as Cmd+wheel),
  `app.rs` (RT chip text), `uiscript.rs`.
- `crates/oni-sys/build.rs`: an empty `<execinfo.h>` on musl.
- `review.rs` and `RecSource::read` use `std::os::unix`, so they are Unix-only.

## 7. Deliberately absent

- **Windows.** Not a target.
- **Adaptive audio gain.** No AGC; gain comes from a held noise calibration (`audio.rs`).
- **Pyramids on disk.** They live in RAM under a budget (`Store::budget`), so they cannot
  compete with the recorder for the disk.
- **Display, CAR and detection settings in recordings.** Recordings are raw (invariant 6).
- **Not yet ported** (`CLAUDE.md`): impedance measurement (some register code in
  `registers.rs` exists), the IMU display (IMU data is recorded),
  a processor graph and plugins, spike file writing.
- **A guard against a second program on the board.** Only the usual "interface claimed"
  error (`UsbError::Busy`); nothing in kalila arbitrates.
