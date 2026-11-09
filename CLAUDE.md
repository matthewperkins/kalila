# Context for Claude sessions on this project

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
(vendored liboni, FT600 ONI driver wrapping FTDI's closed-source D3XX). **Linux only**:
Windows and macOS are not a consideration (no cross-platform shims or testing).

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
- Desktop launcher: `packaging/install-desktop.sh` (log in `~/.local/state/oers/oers.log`).
- Simulator: a common-mode artefact burst (30 ms every 1.7 s) for testing CAR; on by
  default, `OERS_SIM_ARTEFACT=0` turns it off.

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
  300–6k 0.05 s, LFP 1.5 s. Two GPU states per view (`review_key`) are double-buffered; the back
  one is fed in chunks without painting and swapped in when complete.
- **Controls.**
  - Drag, Shift+wheel or ←/→ pan; panning back or Home enters review; End or "LIVE ▸" leaves.
  - Alt+wheel and `+`/`-` set the time base in both modes, like typing into the time box. Live
    allows 0.05–20 s (one window of samples is kept in RAM); review 5 ms–60 s, clamped on
    return to live.
  - A badge shows how far behind live the view is; an overview strip below jumps on click or drag.
- Not yet: a min/max pyramid for spans over 60 s, typed notes, signal-level triggers.

## Per-row display filters (simulator, 2026-11-08)

- **Presets:** Raw, HP, 300–6k and LFP. HP is a 2nd-order high-pass, 10 Hz by default, adjustable;
  it shows raw minus drift and movement, for the Myomatrix EMG on Port B.
- **Per channel:** each electrode can have its own preset (`TraceViewSettings::channel_filter`,
  keyed `"<headstage>:<index>"`). All presets share one GPU filter bank (`FilterBank`, at most
  8 sections), and `chinfo` flag bits 3–4 pick the preset per channel.
- **Controls:**
  - the R·H·S·L chips beside each row set that row's filter;
  - a "Filtering" button in the toolbar opens a palette with one column per preset: "all rows",
    its µV/row (`uv_by_filter`), the HP corner and CAR. The button reads "mixed", hatched, when
    rows differ;
  - Ctrl+wheel over a row scales that row's filter.
- **CAR** uses and corrects only the 300–6k channels.
- Display only; recordings, spike detection and audio are untouched.

## Scripted GUI tests (`OERS_UI_SCRIPT`)

No input-injection tools are installed on the rig. To click through the GUI, run the simulator
with `OERS_UI_SCRIPT=<file>`.
- **Steps:** window, wait, move, click, drag, scroll, key, text, shot, log, quit. The format
  is at the top of `app/src/uiscript.rs`.
- **Input path:** `App::raw_input_hook` feeds the steps into one oers window's own egui input,
  never the system pointer.
- **Screenshots:** `shot` saves that window as a PNG, to `OERS_UI_SHOTS` or the script's folder.
- **Coordinates** are window points (= pixels on the rig).
- **Example run:** `OERS_UI_SCRIPT=s.txt XDG_CONFIG_HOME=<scratch cfg> ./target/release/oers --sim --acquire`
- Ask before launching it while the user's own oers is open: the window rules place the
  simulator's windows over theirs, and it plays audio.

## CPU comparison with the stock GUI (rig, 2026-11-05)

`scripts/cpuload.py` (per process and thread, % of one core; the rig has 4 cores, no SMT).
Same board, 32 channels, full screen, 60 s per condition; CSVs in `~/oers-test/cpu/`:

| | stock 1.1 | oers |
|---|---|---|
| acquiring | 82 % (GUI thread 50) | 59 % (GUI thread 11) |
| recording | 178 %, one core pinned at 100 % | 53 % |
| recording + both cameras, system | 60 % of 4 cores | 26 % of 4 cores |

- The stock "Record Thread" busy-polls (`perf`: `DataQueue::startRead`,
  `AbstractFifo::getNumReady`), holding one core for as long as it records.
- Both cameras together (`~/gst-record-scripts/cam_record`) take about 16 % of a core plus
  NVENC (12 %). No camera frames were dropped with either program.
- oers's acquisition + FTDI driver threads (30–40 %) cost more than stock's (~30 %).
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
6. `./target/release/oers` for the GUI.

When debugging hardware, compare against the original sources: the acquisition-board
plugin (`Source/devices/oni/`) and liboni 4.6 (vendored in `crates/oni-sys/liboni`). On the
dev box these live in `~/OErewrite/ext`; on the rig, clone them from
github.com/open-ephys-plugins/acquisition-board and github.com/open-ephys/liboni as needed.

## Not yet ported

Impedance measurement (the user doesn't use it), IMU display (IMU data *is* recorded; the
rig's headstages have no IMU), processor graph and plugins (deferred), spike file writing.

## Working conventions

- Commit/push to `matthewperkins/oers` (private) on `main` when asked.
- Hardware-touching changes: say what registers or devices they write.
