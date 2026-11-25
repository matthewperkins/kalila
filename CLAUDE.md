# Context for Claude sessions on this project

The program is **kalila** (lower case everywhere; environment variables `KALILA_*`). It was
called **oers** until 2026-11-13: commit messages, `docs/reviews/` and older notes use that
name. Its twin, the Zig program in `~/oEZ/oez` (oEZ, being renamed dimna), shares the board.
Before any board run, check nothing else holds it:
`pgrep -x oers || pgrep -x kalila || pgrep -x kalila-headless || pgrep -x oez || pgrep -x oez-headless || pgrep -x dimna || pgrep -x dimna-headless || pgrep -x headless || pgrep -f '[o]pen-ephys'`

Carried over from the development session on the dev box, so a session on the rig starts
with the same background. See `README.md` for build and setup; this file holds the *why*
and the current state.

## User

- Electrophysiologist. Records single units (e.g. globus pallidus, barrel cortex) with the
  Open Ephys acquisition board: the white box with the newer ECP5/FT600 **ONI** board,
  **not** Opal Kelly. Knows the hardware and neuroscience deeply; expects
  psychophysics-informed design.
- Linux + Hyprland (finds the JUCE GUI annoying there).
- Dev box: NVIDIA T400, **no board attached**. Rig: NVIDIA **RTX 2000 Ada**, board attached
  (USB 3, 5 Gbps), ECP5-ONI gateware v2.0.0, board ID 0x100. Headstages on the rig:
  RHD2132 on port A, RHD2216 on port B.

## Goal and priorities (in order)

Rewrite the Open Ephys GUI and acquisition-board plugin in Rust; C only where unavoidable
(vendored liboni, FT600 ONI driver wrapping FTDI's closed-source D3XX). **Linux first**; a macOS port (Apple silicon) is in progress on the branch `macos-port`,
see "macOS port" below. Windows is not a consideration.

1. Reliable acquisition (MUST).
2. GPU drawing as much as possible, minimal CPU↔GPU transfer.
3. Minimal display latency.
4. Vulkan via wgpu. The user questioned wgpu vs raw Vulkan (ash); the decision was to stay on
   wgpu, keep the renderer isolated so it can be ported, and only reach for
   present-wait/as_hal if *measured* latency warrants it.
5. egui look.
6. Spike audio, biased toward single-unit frequencies and noise floor for auditory salience.

## Audio rule: preserve "texture"

GP spikes sound very different from barrel cortex, and that difference matters as much as
hearing spikes at all. The audio chain must not flatten it:

- No continuously adapting AGC; gain comes from a **held** noise calibration.
- Expander is gentle, keyed on the held σ; multi-unit hash stays audible by default.
- Noise σ uses a spike-excluding two-pass median, so dense firing doesn't inflate it.
- The limiter is exactly linear in the spike range (below 0.5 FS).
- Resampling is a flat windowed-sinc.
- A linear "reference" A/B mode is kept.
- Changes are backed by tests: amplitude ratios, rate-invariance, hash level.

## Recording

Open Ephys Binary format, for compatibility with existing analysis tools
(open-ephys-python-tools, neo/SpikeInterface). Verified with both on the simulator.

## Status (2026-11-05)

- Workspace builds; 19 tests pass. Clippy shows 4 lints that predate today's work
  (`dsp.rs`, `parser.rs` tests, `traces.rs`); the rig's Rust 1.95 flags them, the dev box's
  older toolchain didn't.
- GUI verified on the simulator: 0 lost blocks, audio ≈ 7 ms, display ≈ 0.5 ms.
- **GUI runs on hardware** (Vulkan on the RTX 2000 Ada); its recordings are valid.
  - Board settings can be applied while acquiring: Apply stops, applies and restarts
    (`App::autostart`), and is locked while recording. LEDs switch live. While acquiring,
    the engine keeps a snapshot of the board settings (`Running::settings`), because the
    board itself is owned by the acquisition thread.
  - On the rig: applying 16-ch A1 mid-acquisition restarted it at 32 channels.
  - The trace display scales per channel (`chinfo` in `traces.wgsl`). Electrodes use µV/row
    and the display filter; AUX (2.5 V/row) and ADC (adjustable V/row, centred on 0 V) are
    drawn raw and never filtered, after the electrodes in the row order. Only checked on the
    simulator so far; not yet viewed with ADC on the board.
  - Display and spike-detection settings never affect recordings: the recorder takes the
    raw blocks from the acquisition thread.
- **First real-board runs passed (rig, 2026-11-05).** Init, port scan and cable delay
  (`crates/rhythm/src/oni_board.rs`) worked first time. 600 s soak with recording:
  `dropped=0 gaps=0 bad=0`, `hwbuf` 0.0 %, recorder stall 5 ms in total, 18,000,096
  contiguous samples (exactly 30 kHz). Display latency about 5 ms steady state, measured
  headless. (The `samples` counter in the headless log rises about 30,080 per tick only
  because the ticker fires slightly after each 1 s.) A second soak (120 s, our own driver)
  was also clean.
- **FT600 driver: D3XX must be 1.0.x.** With D3XX 1.1.8 (FTDI's C/libusb rewrite),
  `oni_init_ctx` hangs forever in `_oni_reset_routine` → `oni_ft600_update_control` →
  `FT_ReadPipeEx(timeout 0)`, which now means "block forever". Built against **D3XX 1.0.14**
  (FTDI's own file, from the Wayback Machine; URL and sha256 in README), the driver
  (ft600 1.0.6) works. The stock plugin's prebuilt driver (ft600 1.0.3, D3XX 1.0.16) also
  works via `ONI_DRIVER_PATH=~/acquisition-board/libONI/linux`.
  `scripts/build-ft600-driver.sh` refuses 1.1.x.
- **Matches a stock-GUI recording** (stock GUI 1.1.0, plugin 1.1.6, same headstages):
  - Neural channels match channel for channel: the per-channel noise profile correlates at
    1.000 (0.95 in 16-ch mode, with room drift). 0.195 µV/bit is confirmed.
  - AUX uses the same `raw − 32768` × 37.4 µV convention, so values are the true voltage
    minus 1.2255 V. Stock labels AUX "mV" although its bit_volts is in V; ours says V.
  - ADC scale and offset match (board ID 0x100, not v3, not inverted); pulse high level
    +2.227 V in both.
  - TTL: same line number and `full_words`; 250.018 ms period and 20.000 ms width in both;
    the ADC edge lands +1 sample after the TTL edge in both.
- **Rig headstage A is a 16-channel board on an RHD2132**, so it needs 16-ch mode
  (`BoardSettings::rhd2132_16ch`, GUI "16-ch RHD2132", headless `--16ch A1`). As in stock,
  that mode keeps chip channels 8–23 (`RHD2132_16CH_OFFSET`); the unwired inputs read about
  1.5 µV. Parsing and layout only. DAC routing adds the +8 offset to reach the physical
  channel. Stock doesn't, so stock's DAC outputs an unwired channel in this mode. Our DAC
  routing in 16-ch mode is untested on hardware (I/O outputs are disconnected).
- **LEDs off by default.** The LED driver puts a ~1.06 kHz tone on every analog input:
  floating ADC σ was 38.6 counts with LEDs on vs 4.4 off, in an alternating test, and it
  shows on driven inputs too. Neural channels are unaffected. The stock settings here also
  had LEDs off. Headless `--leds` turns them on.
- Hardware-safety audit: no flash, EEPROM or FTDI chip-config writes; all registers are
  volatile. The worst case is power-cycling the board. Disconnect I/O-board outputs
  (DAC/TTL out) for the first tests.

## Added on the rig (2026-11-05), committed; some checks still pending

Committed per feature where the code separates; the GUI features share one commit, since
`app.rs`/`traces.rs` interleave them.
- TTL-input overlay (`app/src/ttl.rs`): bands plus per-line lanes. Verified on the board.
  A "lines" pop-out beside the toggle hides single lines (`ttl_hidden`), live and in review,
  e.g. the camera trigger (2026-11-10, checked on the sim live and in review).
- Outputs panel (DAC routing, DAC 1 follows audio, manual TTL out, threshold → TTL out,
  DAC high-pass, noise slicer). Sent live and at every acquisition start (`sync_outputs`);
  new `LiveCommand::SetTtlMode`. **Not yet checked on a scope or the Grass AM10.**
- Triggered sweeps (`app/src/sweep.rs`, `cut` pass in `traces.wgsl`): TTL2 laser trigger by
  default, −10/+40 ms, sweeps overlaid with fading, laser-on shading. Channel overlay:
  1/2/4/8 electrodes per row. For opto-tagging; laser TTL on line 2, RZ5 drive on ADC1.
- Audio underrun fix (`audio.rs`): the servo now measures the fill left *after* each
  callback, and the guard gets 12 ms of slack for bursty USB delivery. On the board:
  ~13/s → 1 in 30 s, ~9 ms latency. `headless --mute` and callback-size stats were added.
- Display CAR in the 300–6k view (`filt`/`carmean`/`fold` passes), with a palette for the
  reference and subtract sets (`CarSettings`, shared with spikes/audio). Simulator check
  passed; not yet tried on real data.
- CAR is also optional for spike detection and the audio monitor (`engine::dsp::Car`,
  subtracted from raw samples before filtering; same sets as the display; recordings never).
  Headless `--car` applies it to all electrodes. The Spike detection / Audio monitor
  headings show a "CAR" badge when it applies there.
- Spike tiles: click monitors that channel, Ctrl+click adds/removes it from the audio mix.
  Monitored channels are coloured by stereo position (amber left → yellow → lime right).
- Desktop launcher: `packaging/install-desktop.sh` (log in `~/.local/state/kalila/kalila.log`).
- Simulator: a common-mode artefact burst (30 ms every 1.7 s) for testing CAR; on by
  default, `KALILA_SIM_ARTEFACT=0` turns it off.

## Thread liveness and electrode identity (2026-11-13, from `docs/reviews/`)

- **Thread deaths are loud.** The acquisition thread sends `AcquisitionStopped` from a drop
  guard (`AcqStopped`), so a panic there still finalises an open recording and clears
  `recording` (test `a_panic_in_acquisition_finalises_the_recording`). The recorder clears
  `recording` and sets a REC ERROR on any exit (`Exit` guard in `run_recorder`); the
  back-pressure wait also stops on it. `Engine::poll` reports a crashed recorder or worker
  (dsp, pyramid, audio-feed) in the error bar. The Record button can stop a recording even
  when acquisition has ended.
- **Electrode-indexed settings follow their electrodes.** DAC routes (and so threshold → TTL
  out), the spike/audio CAR sets, each window's CAR sets and the audio channels are global
  electrode indices. `Config::electrode_keys` records which electrode ("A1:5") each index
  meant; when the layout changes, `App::remap_electrodes` moves them (`ElectrodeMap`) and
  drops those whose headstage is gone, with a toast. Checked on the sim (32+16 → 16+16).
  Old configs without keys are taken to mean the first layout they see.
- **The overview holds only the file.** The pyramid builder folds live blocks into a
  recording only while it is being written in this acquisition run (`earlier`/`this_run`
  in `pyramid_thread`: sample numbers restart at 0 every run). Once it is not being written,
  the live edge is the file's end (`live_end = available`), so blocks taken between Stop
  and the recorder acting on it fall outside it. Test
  `the_overview_stops_at_the_end_of_the_recording` (after Stop, and after a restart).
- **Review reads by sample number, across gaps.** `continuous.dat` has no holes, so after
  dropped samples file index ≠ sample − first. The recorder publishes each restart point in
  `RecSource::gaps` (sample number, file index); `RecSource::read` (review reader and pyramid
  backfill) and `RecSource::available` (the span in sample numbers) map through it, and
  dropped samples read as 0 (a flat stretch). Test `reads_back_by_sample_number_across_gaps`.
  The simulator can't drop samples, so on the GUI this was only checked without gaps.
- **Recorder hardening (group 1 of the reviews).**
  - Files are created with `create_new`, so an existing recording is never truncated;
    session folders get `_2`, `_3` within one second, and a taken `recordingN` is skipped.
  - Changing the root directory starts a new session there (it used to take effect only
    at the next session).
  - `write_block` refuses a malformed block (values ≠ samples × channels, TTL count) or one
    behind the last written: the recording stops with a REC ERROR. After any write error the
    recording is finished file by file (`finish` keeps going past errors, syncs data first,
    writes `structure.oebin` last).
  - The parser skips a frame whose counter repeats or goes backwards (a bad frame; it used
    to count ~4.3e9 lost samples and shift later sample numbers by 2^32). After 3000 such
    frames in a row the counter is taken to have restarted (`RESYNC_AFTER`).
  - A config that fails to parse is renamed `config.json.bad-<time>`, and the error bar
    says so (the defaults used to be saved over it).
- **Bounds (group 2 of the reviews).**
  - The acquisition thread's block pool covers the combined depth of the lossy queues
    (~3500 blocks, ~16 MB at 64 ch, allocated with capacity at start), so a lagging
    consumer no longer makes it allocate; only a recorder backlog beyond that does.
  - Spikes waiting for the main window (`App::spike_new`) are compacted to the newest per
    tile slot while it's hidden (`compact_newest_per_slot`); they used to grow ~1 GB/h.
  - Stop waits at most `STOP_WAIT` (3 s) for the acquisition thread. If a silent board
    holds it in a read, the thread is left in `Engine::stuck`, an open recording is
    finished, and the error bar asks for a power-cycle; `poll` takes the board back if the
    read ever returns (test `stop_does_not_hang_on_a_silent_board`).
  - Port scan gives up after 2 s without enough headstage frames (upstream waits forever)
    and skips short frames instead of panicking. Not yet run on the board.
- **Deterministic simulation test (group 3).** `crates/engine/tests/simulation.rs`: per
  seed (200 by default; `KALILA_SIM_SEED=<n>` reruns one), a frame stream like the board's
  with seeded faults (dropped frames, duplicates, stale and corrupted and truncated frames,
  other devices' frames, a counter wrapping at 2^32) runs through the board's own block
  assembly (`rhythm::assemble`, now shared with `OniBoard::read_block`) and the real
  recorder, with recordings opened and closed at seeded blocks. A checker proves from the
  files that each recording holds exactly the samples accepted while it was open (every
  row at its own sample number), TTL edges and `initial_state` replay from the samples,
  the health counters equal the injected faults, and `RecSource::read` maps across gaps.
  Mutation-checked: disabling the counter check, the recorder's gap list or TTL edges at
  block starts each fails most seeds. Runs in ~6 s (release).
  - **Disk full:** seeds 1000–1059 cap every file's size (`RLIMIT_FSIZE`, so writes fail
    with EFBIG mid-recording). The recorder must count an error (`RecStatus::errors`, never
    reset; the GUI shows "rec errors n" once the message has cleared), every header must
    count each whole record on disk, and what's on disk must be a correct prefix. This found
    and fixed: a cut-off `structure.oebin` (now written to a temporary file and renamed),
    headers left at the last checkpoint when the final flush failed (0 records claimed,
    3479 on disk), and an error forgotten when the next recording started.
  - Not covered: crash truncation, the GUI.
- **Backfill filters outside the store lock**, as the live path does; only `ingest` runs
  under it (every GUI frame takes that lock, and the builder runs at idle priority).

## FT600 without D3XX (default since 2026-11-24)

`crates/ft600`, a Rust port of dimna's USB driver (`src/usb_linux.zig`, `ft600.zig`,
`oni_usb.zig`), registered with liboni as a built-in driver (`oni_register_builtin_driver`,
the same 11-line loader patch as dimna's). **The default; `KALILA_DRIVER=d3xx`** selects
liboni's D3XX driver. Made the default by the user on 2026-11-24 after the rig checks below,
before the GUI Stop/Start and Apply check.
- Layers: `usbfs` (usbdevfs ioctls, async URBs so a timed-out read keeps its bytes), `chip`
  (FT600 protocol: 20-byte announcements on EP 0x01, GPIO and config as vendor requests),
  `driver` (port of liboni's `onidriver_ft600.c`, with dimna's read pacing and no 200 ms
  abort waits when nothing is announced).
- **Hardware safety:** sends only what D3XX sends on every open (GPIO 0 = FIFO reset,
  announcements, aborts; reads of firmware version and chip config). Every control transfer
  goes through `Ft600::control_transfer`, which refuses anything outside `ALLOWED_REQUESTS`
  (never 0x40 0xcf, the chip-config write). Tests check the list, the refusal, and scan the
  workspace for any other control transfer; both were mutation-checked.
- dimna's measurements (46 ch): CPU 2.2 s vs 8.7 s per 20 s, open + scan 2.5 s vs 8.6 s.
- `KALILA_USB_TRACE=1` logs USB commands; `KALILA_USB_PACE=0` turns pacing off.
- **USB block default 8 KiB** (was 24 KiB, stock's), from the delay table below. Saved configs
  without `defaults_rev` that hold exactly 24 KiB move to 8 KiB once, with a toast
  (`Config::parse`); any other value is the user's choice and stays.
- **On the rig (2026-11-24), 46 ch, 30 kHz:** 600 s soak with recording `dropped=0 gaps=0
  bad=0`, hwbuf 0.0 %, 17,999,968 contiguous samples; 20 s recordings back to back with D3XX
  agree (noise profile r = 1.000, AUX within 0.24 mV, ADC within 0.18 mV; TTL not compared,
  no input connected); CPU for a 20 s recording run 3.3 s vs 9.8 s; open to acquiring ~2 s vs
  ~7 s. Eleven open/acquire/close runs in a row, all clean.
- **Delivery delay** (headless `--latency`: each 32-sample block's newest sample vs the
  quickest block of its second; ms, mean / p99 / max; CPU user+sys per 20 s):

  | | 24 KiB | 8 KiB | 4 KiB |
  |---|---|---|---|
  | usb, paced | 2.07 / 4.05 / 4.5, 3.1 s | 0.75 / 1.44 / 2.0, 4.3 s | 0.38 / 0.75 / 1.5, 5.1 s |
  | usb, unpaced | | | 0.37 / 0.72 / 1.6, 8.2 s |
  | D3XX | 2.05 / 4.04 / 5.0, 8.3 s | 0.71 / 1.39 / 5.5, 10.3 s | 0.39 / 1.11 / 5.0, 10.1 s |

  The mean is half the block's fill time, so block size sets the delay; pacing adds ~0.01 ms
  and saves ~40 % CPU at 4 KiB; D3XX has the longer tail (p99.9 2.6–2.8 ms at 8/4 KiB vs 1.5/0.8).
- Still to do: Stop/Start and Apply-while-acquiring in the GUI with the new driver.

## Releases (2026-11-24)

`.github/workflows/release.yml`, on a `v*` tag or by hand (workflow_dispatch). Scripts in
`packaging/`: `ci-build.sh gnu|musl` (build, test, release folder, tarball; for gnu also
.deb/.rpm with nfpm, `nfpm.yaml`), `ci-test-install.sh` (install and run on a distribution),
`collect-licenses.py` (licence files of every crate compiled in).
- **glibc** built on Rocky 9 (glibc 2.34; `ci-build.sh` fails if a newer symbol is needed),
  Rust pinned there (`RUST_VERSION`). Tested as packages on Ubuntu 22.04/24.04, Debian
  12/13, Rocky 9, Alma 9, Fedora, and as a tarball on Arch.
- **musl** built on Alpine 3.22, dynamically linked (`-C target-feature=-crt-static`, so
  Vulkan/Wayland/X11 can be dlopen()ed); liboni gets an empty `<execinfo.h>` there
  (`oni-sys/build.rs`). Tested on Alpine and Void musl.
- The test (no board, no display): every symbol resolves, the dlopen()ed libraries are
  installed, `kalila-headless` runs 3 s on the simulator with nothing lost.
- A tag only **drafts** the release; the user reads and publishes it. The tag must be
  `v<workspace version>`.
- Shipped binaries: `kalila` and `kalila-headless` (the engine's headless example).

## macOS port (branch `macos-port`, started 2026-11-24)

Apple silicon only, macOS 27 (the user's MacBook Air M4). The Linux code paths must stay as
they are; platform code sits behind small `cfg` seams.
- **Runs on the simulator** (2026-11-24): Metal through wgpu (`main.rs` picks the backend per
  OS), Core Audio through cpal, all tests pass, and a scripted run recorded and reviewed.
  The WGSL shaders run unchanged.
- **Seams so far:** `engine::sched` (SCHED_FIFO / SCHED_IDLE on Linux, QoS classes on
  macOS), `oni::DRIVER_FILE` (`.so` / `.dylib`), `oni-sys/build.rs` links `libdl` on Linux only.
- **The board on macOS: written, never run with a board** (2026-11-24).
  `crates/ft600/src/usb_iokit.rs`, the macOS backend behind the same surface as `usbfs.rs`
  (`crate::usb` picks one per OS). Decided with the user: **pure Rust**, no C shim, so it
  is a port of dimna's `usb_iokit.c` that calls IOKit's COM-style tables by slot number.
  The slots, UUIDs, struct layouts and result codes were printed by a throwaway C program
  against the macOS 27 SDK (`offsetof(IOUSBDeviceInterface942, …) / 8`).
  - Sends the board nothing the Linux backend does not: the same `chip.rs` and `driver.rs`
    run above it, and every control transfer still goes through `Ft600::control_transfer`
    and its allowlist. The workspace scan now also allows `fn control(` in `usb_iokit.rs`
    and looks for IOKit's request call outside it (mutation-checked).
  - Bulk transfers are async reads waited for on the caller's thread and aborted at the
    timeout, so a timed-out read keeps its bytes. `KALILA_USB_TEST_ABORT_EVERY=N` gives up
    every Nth read at once, to prove that with the board streaming.
  - Read pacing is **off by default on macOS** (`KALILA_USB_PACE=1` turns it on), from
    dimna's finding below.
  - Without a board `headless --hw` ends with "no ONI acquisition board found".
  - **First board sessions need the user present**, in dimna's order
    (`~/dimna/docs/plans/macos-step8-usb.md`): registry and claim; 10 s traced; the same
    with `KALILA_USB_TEST_ABORT_EVERY=3`; open and close twenty times; 10 min recorded;
    unplug; then a recording compared with the rig's.
- **From dimna's port** (`~/dimna/docs/plans/macos-port.md`, `macos-step8-usb.md`): timers
  wake 2–5 ms late while the screen is busy, so no timed pacing sleep on the acquisition
  path (the simulator's pacing sleep is the likely cause of audio underruns on the sim);
  the audio feeder needed a Mach time-constraint policy; display latency as macOS reports
  it is ~5–7 ms without sync, 16–32 ms with. kalila's "display ms" read-out is not
  time-to-screen on macOS until it uses the drawable's presented time.
- `sync_all` is `F_FULLFSYNC` on macOS (slow); the recorder only syncs when a recording
  finishes. The simulation test takes ~135 s there against ~6 s on Linux.
- Scripted GUI runs on the Mac: window points are half the pixels (Retina).
- **Scheduling and power** (`engine::sched`, 2026-11-24): the acquisition and audio-feed
  threads take the user-interactive QoS class and a Mach time-constraint policy (dimna's
  figures: period 1 ms, computation 0.5 ms, constraint 1 ms, preemptible), so the status
  chip reads RT; `KeepAwake` holds a `PreventUserIdleSystemSleep` assertion for the
  acquisition thread's lifetime (seen in `pmset -g assertions`).
- **Open: audio underruns on the simulator**, about 2 per second (45 in 19 s, buffer 128 at
  48 kHz, latency 7 ms), the same with `present_mode` "vsync" and with the policy above, so
  neither the redraw rate (it free-runs at ~3000 fps with "auto") nor the feeder's
  priority. Not yet tried: the simulator's pacing sleep, the Core Audio callback sizes.
- Still to do: the board sessions on the Mac, Cmd equivalents for the Ctrl gestures, a macOS CI job, an `.app`.

## Trace windows (rig, 2026-11-06)

- **Layout.** The main window holds the controls and the spike grid. Traces live in separate
  windows (`app/src/app/views.rs`), configured in `Config::views`; an old `display` config
  migrates into view 0. Each window has its own control strip:
  - channels (headstages, AUX, ADC);
  - filter, CAR, scaling;
  - triggered sweeps.

  "One window per port" makes the Port A / Port B pair the user records with: Myomatrix on B,
  single units on A.
- **Deferred viewports.** Trace windows are eframe *deferred* viewports. Immediate ones are drawn
  inside the root's pass, and Hyprland withholds frames from a hidden root window.
  - Settings and per-view state live in `ViewsShared` behind `Arc<parking_lot::Mutex>`.
  - Whichever window draws first drains the display channel for all views.
  - Board polling, outputs and spike intake run in `App::logic`, which eframe keeps running
    while the root is hidden.
  - GPU state is one `TraceGpu` per view in `TraceGpus` (egui-wgpu shares one renderer, and so
    the callback resources, across viewports).
- **One frame of a window** (`app/src/app/views/frame.rs`, since 2026-11-11): `ViewFrame`
  holds the window's settings read-only, its `ViewState`, and borrowed handles to the rest
  of `ViewsShared`. It never writes settings; wheel scaling, row scrolling, zoom, the trigger
  level, row chips and monitor clicks are queued as `Edit`s and applied by
  `ViewsShared::apply_edits` after the frame, so one frame draws with one set of settings.
- **Staging buffers.** Each view keeps up to one window of undrawn samples in `VecDeque`s and
  uploads a backlog in chunks of ≤ 32768 samples per frame.
  - The earlier `Vec` + `drain(..)` shifted the whole window for every block: ~74 MB × 940/s
    for 20 s × 62 ch. Whenever a window missed frames, that froze all windows for 5–100 s.
  - A one-shot backlog upload also needed a buffer over the 128 MB storage-binding limit. That
    crashed on the simulator; the rig's 46 ch sat exactly at the limit.
- **Stall warning.** "trace window '…' stalled" in the log means an open window went over 2 s
  without drawing or consuming samples while acquiring. It's expected while that window itself
  is hidden.

## Scroll-back review, phase 1 (simulator, 2026-11-07)

Spike2-style scrubbing through the ongoing recording, per trace window (`app/src/review.rs`,
`review_view` in `app/src/app/views.rs`). Plan and later phases: memory `review-scrollback-plan`.
- **Source.** It reads the recording's `continuous.dat` and TTL `.npy` files, which the recorder
  publishes in `RecStatus.source`. A "review-reader" thread does the reading, only up to the
  flushed length, and never touches acquisition or the recorder. Without a recording there is
  nothing to review; the window shows a note instead.
- **Filtering.** Each read includes a lead-in so the display filter settles: Raw 0 s,
  Band max(0.05, 3 / f_low) s. Two GPU states per view (`review_key`) are double-buffered; the back
  one is fed in chunks without painting and swapped in when complete.
- **Snappiness (2026-11-10).** While a fill is in flight the last complete image is drawn
  shifted and zoomed to where its stretch now falls (`RenderU` `x_shift`/`x_scale`/`rot`, and
  the same in `TtlGeom`), so a drag moves at once. A fill is never cancelled; when it completes,
  the newest stretch is requested. A pan forward by less than a window *appends* just the new
  samples to the shown state (filter state continues, no lead-in). The reader reads straight
  into the `i16` buffer. `RUST_LOG=kalila=debug` logs each fill with its read time.
  - Measured on the sim (62 ch, Raw): 3 s fills in 11–27 ms; 33 s in 0.85–0.98 s. The file
    read is 20–80 ms of that; the rest is the GPU `filt` pass, which runs one serial thread
    per channel (an IIR is sequential), about 57 ms per 8 MB chunk. Raw is no faster because
    it goes through the same loop. The fix for long spans is the min/max pyramid (phase 2).
- **Controls.**
  - Drag, Shift+wheel or ←/→ pan; panning back or Home enters review; End or "LIVE ▸" leaves.
  - Alt+wheel and `+`/`-` set the time base in both modes, like typing into the time box. Live
    allows 0.05–20 s (one window of samples is kept in RAM); review 5 ms–60 s, clamped on
    return to live.
  - A badge shows how far behind live the view is; an overview strip below jumps on click or drag.
- Later: the min/max pyramid (phase 2, below), typed notes and the slope trigger.

## Scroll-back review, phase 2: min/max pyramids (simulator, 2026-11-10)

`crates/engine/src/pyramid.rs`; GUI side in `review_view` (`pyramid_columns`, `band_key`).
Spans of at least 64 samples per column (about 1.7 s at 814 columns) are drawn from pyramids
instead of reading and filtering samples, so panning a 60 s or 30 min window costs one
column pass (a few ms) and the image follows the pointer. The review span limit is now 1 h,
clamped to the recording's length; spans over 60 s never fall back to direct reads.
- **Pyramids.** Per bucket of 64 samples, then ×4 per level (8 levels), `[min, max]` as i16
  per channel; coarser buckets merge incrementally from finer ones, so the live edge is
  right at every level. Two kinds, built live from the acquisition blocks by the "pyramid"
  thread (its own lossy queue, `pyramid_dropped` in `PipelineStats`):
  - **raw:** every channel, plus the OR of the TTL words per bucket (the review TTL lanes);
  - **band:** one per trace window (`Store::want`, set each frame in `ViewsShared::drain`):
    the electrodes of the window's headstages, band-passed with its corners and its display
    CAR exactly as the shader applies it. Stored in raw LSB units.
- **Backfill.** Coverage holes (the recording's first block, dropped blocks, a band filter's
  warm-up, or a whole new band pyramid after the corners or CAR change) are filled from
  `continuous.dat`, newest first, 2 s per chunk with a 10 ms pause, at `SCHED_IDLE`, and
  paused while the recorder reports backpressure. A band change waits 2 s before the
  rebuild starts. On the sim it ran at about 45× real time (a 2 h recording in under 3 min).
  Backfilled raw holes have no TTL words (the reader's edge files aren't consulted there).
- **Safety.** The builder is a consumer like the display: it never touches acquisition or
  the recorder, and reads the file read-only. The only coupling is CPU/disk, hence the idle
  priority and the backpressure check.
- **RAM.** About 0.56 GB/h for 62 raw channels plus 0.29 GB/h per 32-channel band pyramid;
  the rig's configuration is about 0.7 GB/h. Nothing is written to disk (ruled out: it would
  compete with the recorder).
- **RAM budget (2026-11-11).** `review_ram_gb` in the config, default 1.5 GB for all
  pyramids together; the status line shows "review x / y GB". Levels are stored in chunks
  of 2^24 samples (9.3 min at 30 kHz), aligned across levels. Over the budget,
  `Store::enforce_budget` releases the oldest chunks of level 0 in every pyramid, then of
  level 1 (`RELEASABLE`), never the chunk at the live edge or coarser levels; released
  chunks are not written again by backfill. `Pyramid::columns` returns None where the
  level a span needs is released, and the GUI reads the file directly (measured on the sim,
  64 ch Raw: 6 s in 0.2 s, 18 s in 0.5 s; the shifted image follows the pointer meanwhile).
  Checked in a 21 min sim run with a 0.15 GB budget: releases at each 9.3 min boundary. Only spans too long to read (> 60 s, possible at level 1 on windows over
  ~2000 px) fall back to the finest held level, a little blocky. The user chose no blocky
  preview otherwise. At 1.5 GB the rig keeps full detail for ~2 h, then for the newest
  stretch only.
- **GUI.** `TraceFrame::columns` uploads a whole column buffer (physical units) without
  ingest. While a pyramid is incomplete the badge says "building the overview" and the
  image is redrawn every 300 ms. `RUST_LOG=engine=debug` logs backfill progress and RAM.
- Scripted runs that enter or leave review unprompted: most likely real pointer input reaching
  the simulator's windows, which sit over the user's own kalila (on 2026-11-11 the user was
  moving a window at the time). `leave_review` and `reset_all` log at debug level.

## Per-row display filters (simulator, 2026-11-08; Raw/Band since 2026-11-10)

- **Two filters per window:** Raw, and **Band**, a bandpass whose corners belong to the window
  (`TraceViewSettings::band_low_hz`/`band_high_hz`, 4th-order Butterworth each side, 0 = that
  side off, so "10 Hz–" is the Myomatrix high-pass and "300–6k" the spike band). Port A and
  Port B windows can have different bands. There is no fixed corner for speed reasons: an IIR
  costs the same whatever its corners; only the review lead-in depends on the low corner
  (`review::lead_in_s`, 3 / f_low). The spike detector, audio monitor and slope trigger keep
  their own fixed 300–6k band in the engine.
  - Before 2026-11-10 there were four presets (Raw, HP, 300–6k, LFP). `Config::parse`
    migrates: a view's band becomes the preset most of its rows used, with that preset's
    µV/row, and every non-raw row becomes Band (`migrate_view_presets`).
- **Per channel:** each electrode is Raw or Band (`TraceViewSettings::channel_filter`, keyed
  `"<headstage>:<index>"`). The GPU filter bank (`FilterBank`, at most 8 sections) holds the
  band; `chinfo` flag bits 3–4 pick raw or band per channel.
- **Controls:**
  - the R·B chips beside each row set that row's filter;
  - a "Filter settings" button in the toolbar (always that label, a plain button) opens a
    palette with a column per filter: "all rows", its µV/row (`uv_by_filter`), the band
    corners and CAR. Its "all rows" cells are hatched when rows differ;
  - **Y range per row** (checked on the sim): Ctrl+wheel over a row scales that row alone
    (`TraceViewSettings::channel_uv`, keyed like `channel_filter`, sent per channel in
    `chinfo`); Ctrl+Shift+wheel scales the row's whole filter, overrides included; off the
    rows, every filter in use. Setting a filter's µV/row in the palette clears the overrides
    on that filter. The status line adds "· per-row" while any override is set. egui turns
    Ctrl+wheel into `zoom_delta()` with no scroll delta, so that is what is read; before
    2026-11-10 the code read the scroll delta and Ctrl+wheel did nothing.
- **CAR** uses and corrects only the Band channels.
- Display only; recordings, spike detection and audio are untouched.

## Typed notes (simulator, 2026-11-09)

`app/src/notes.rs`. Keys work in any kalila window while no text box has focus:
- **Typed notes:** `/` opens a note stamped at that sample; Enter or `\` saves it, Esc cancels.
  While a note is open, every key goes into it, so no shortcuts fire.
- **One-key stamps:** `` ` `` stamps the next draft (queued in the Notes panel); 1–9 stamp the
  presets.
- **Stamp accuracy:** the newest block's sample plus the time since that block was acquired, so
  the display latency is excluded. Keys are read once per frame, so the stamp can be up to one
  frame late.
- **Recording format:** as stock 1.x (`RecordNode::handleBroadcastMessage`), in
  `events/MessageCenter/`:
  - `text.npy`: `|S513`, null-padded, cut at 512 bytes;
  - `sample_numbers.npy`: main-stream clock;
  - `timestamps.npy`: offset from the latest block's timestamp.

  In `structure.oebin` the entry's `sample_rate` is the main stream's; stock writes 1000 there.
  Verified with open-ephys-python-tools and neo 0.14.3.
- **Keystroke log:** every keypress, sample-stamped, goes to `note_keystrokes.tsv` in the
  recording folder.
- **Notes log:** every note, recorded or not, is also appended to
  `$XDG_STATE_HOME/kalila/notes.log` (default `~/.local/state`). Point `XDG_STATE_HOME` elsewhere
  for simulator tests.
- **Markers:** notes are drawn on the live traces and in review.

## Slope trigger for sweeps (simulator, 2026-11-10)

`app/src/slope.rs`; settings in `TraceViewSettings` (`trig_source`, `trig_channel`, …). A trace
window's sweeps can start at a level crossing on one of its electrodes instead of a TTL edge.
- **Detection** runs per window on the CPU, over the samples it uploads: the channel is
  band-passed with the spike detector's band (300–6k) and CAR, so the sweeps match the spike
  tiles. It does not use the detector's event stream, because each window needs its own
  level, slope and alignment.
- **Level:** the spike detector's threshold for that channel by default (so it follows σ), or a
  fixed µV: drag the dashed line on the channel's row, or click the "level" button. "detector"
  goes back. The line is drawn in the row's own µV scale, so it only lines up with the trace
  when that row's band is near 300–6k.
- **Slope** falling (negative level) or rising; **t0** at the trough (the extreme within 0.5 ms
  of the crossing, as the detector aligns) or at the crossing; **holdoff** on top of the 1 ms
  dead time; **window** discriminator (reject troughs beyond a second level), off by default.
- **Window:** its own −1/+2 ms defaults in 0.1 ms steps (`spk_pre_ms`/`spk_post_ms`); the TTL
  window keeps −10/+40. With fewer samples than columns, the `cut` pass interpolates linearly
  instead of taking per-column min/max.
- **Picking the channel:** the trigger combo lists the TTL lines and the window's electrodes;
  Alt+click on a row label picks that row's first channel and switches to triggered mode.
- Changing the level does not clear the sweeps (it moves while dragging and as σ updates);
  every other trigger setting does.
- Not yet: triggering in review; a combined spike/laser latency view.

## Scripted GUI tests (`KALILA_UI_SCRIPT`)

No input-injection tools are installed on the rig. To click through the GUI, run the simulator
with `KALILA_UI_SCRIPT=<file>`.
- **Steps:** window, wait, move, click, drag, scroll, key, text, shot, log, quit. The format
  is at the top of `app/src/uiscript.rs`.
- **Input path:** `App::raw_input_hook` feeds the steps into one kalila window's own egui input,
  never the system pointer.
- **Screenshots:** `shot` saves that window as a PNG, to `KALILA_UI_SHOTS` or the script's folder.
- **Coordinates** are window points (= pixels on the rig).
- **Example run:** `KALILA_UI_SCRIPT=s.txt XDG_CONFIG_HOME=<scratch cfg> ./target/release/kalila --sim --acquire`
- Ask before launching it while the user's own kalila is open: the window rules place the
  simulator's windows over theirs, and it plays audio.

## CPU comparison with the stock GUI (rig, 2026-11-05)

`scripts/cpuload.py` (per process and thread, % of one core; the rig has 4 cores, no SMT).
Same board, 32 channels, full screen, 60 s per condition; CSVs in `~/oers-test/cpu/`:

| | stock 1.1 | kalila |
|---|---|---|
| acquiring | 82 % (GUI thread 50) | 59 % (GUI thread 11) |
| recording | 178 %, one core pinned at 100 % | 53 % |
| recording + both cameras, system | 60 % of 4 cores | 26 % of 4 cores |

- The stock "Record Thread" busy-polls (`perf`: `DataQueue::startRead`,
  `AbstractFifo::getNumReady`), holding one core for as long as it records.
- Both cameras together (`~/gst-record-scripts/cam_record`) take about 16 % of a core plus
  NVENC (12 %). No camera frames were dropped with either program.
- kalila's acquisition + FTDI driver threads (30–40 %) cost more than stock's (~30 %).
  Unconfirmed guess: smaller USB reads. A possible trim if headroom ever matters.
- The FTDI driver's threads show up as "board-open" (they inherit that thread's name).

## Rig test plan

1. Close the stock Open Ephys GUI (only one program can open the board).
2. `cargo build --release`
3. `scripts/build-ft600-driver.sh <extracted D3XX 1.0.x dir>` → `~/.local/lib`. This takes
   precedence over a driver from an existing Open Ephys install. The startup log line
   `using libonidriver_ft600.so from …` shows which copy loaded. Existing Open Ephys udev
   rules on the same VID/PIDs are fine; ours is redundant but harmless.
4. Soak: `cargo run --release -p engine --example headless -- --hw --seconds 600 --no-audio --record ~/oers-test`.
   Expect `dropped=0 gaps=0 bad=0` and `hwbuf` near 0 %. `bp` is the cumulative recorder
   stall since start; a few ms is fine. On the rig `/tmp` is a 3.9 GB tmpfs, so record to
   disk instead.
5. Compare against a short stock-GUI recording with the same headstages: channel count,
   µV scale (0.195 µV/bit), ADC scale (v3 boards invert), TTL timing. Add `--aux --adc` to
   headless to include those channels.
6. `./target/release/kalila` for the GUI.

When debugging hardware, compare against the original sources: the acquisition-board
plugin (`Source/devices/oni/`) and liboni 4.6 (vendored in `crates/oni-sys/liboni`). On the
dev box these live in `~/OErewrite/ext`; on the rig, clone them from
github.com/open-ephys-plugins/acquisition-board and github.com/open-ephys/liboni as needed.

## Not yet ported

Impedance measurement (the user doesn't use it), IMU display (IMU data *is* recorded; the
rig's headstages have no IMU), processor graph and plugins (deferred), spike file writing.

## Working conventions

- Commit/push to `matthewperkins/kalila` (public since 2026-11-24; renamed from `oers`) on
  `main` when asked. Being public, nothing private (recordings, personal details) goes in.
- Hardware-touching changes: say what registers or devices they write.
