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
- Dev box: NVIDIA T400, **no board attached**. Rig: NVIDIA RTX **A2000**, board attached.

## Goal and priorities (in order)

Rewrite the Open Ephys GUI and acquisition-board plugin in Rust; C only where unavoidable
(vendored liboni, FT600 ONI driver wrapping FTDI's closed-source D3XX).

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

- Workspace builds; 18 tests pass; clippy clean.
- GUI verified on the simulator: 0 lost blocks, audio ≈ 7 ms, display ≈ 0.5 ms.
- The FT600 driver builds from D3XX 1.1.8 (`scripts/build-ft600-driver.sh`) and loads;
  with no board it reports "no ONI acquisition board found (ONI error -2)".
- **Never run against the real board yet.** The init, scan and cable-delay path in
  `crates/rhythm/src/oni_board.rs` is a line-by-line port of AcqBoardONI/Rhd2000ONIBoard;
  that is where hardware bugs are most likely.
- Hardware-safety audit: no flash, EEPROM or FTDI chip-config writes; all registers are
  volatile. The worst case is power-cycling the board. Disconnect I/O-board outputs
  (DAC/TTL out) for the first tests.

## Rig test plan

1. Close the stock Open Ephys GUI (only one program can open the board).
2. `cargo build --release`
3. `scripts/build-ft600-driver.sh <extracted D3XX dir>` → `~/.local/lib`. This takes
   precedence over a driver from an existing Open Ephys install. The startup log line
   `using libonidriver_ft600.so from …` shows which copy loaded. Existing Open Ephys udev
   rules on the same VID/PIDs are fine; ours is redundant but harmless.
4. Soak: `cargo run --release -p engine --example headless -- --hw --seconds 600 --record /tmp/oers-test`.
   Expect `dropped=0 gaps=0 bad=0`, `hwbuf` near 0 %, `bp=0ms`.
5. Compare against a short stock-GUI recording with the same headstages: channel count,
   µV scale (0.195 µV/bit), ADC scale (v3 boards invert), TTL timing.
6. `./target/release/oers` for the GUI.

When debugging hardware, compare against the original sources: the acquisition-board
plugin (`Source/devices/oni/`) and liboni 4.6 (vendored in `crates/oni-sys/liboni`). On the
dev box these live in `~/OErewrite/ext`; on the rig, clone them from
github.com/open-ephys-plugins/acquisition-board and github.com/open-ephys/liboni as needed.

## Not yet ported

Impedance measurement, IMU display (IMU data *is* recorded), DAC/TTL-output UI (the engine
supports it via `LiveCommand`), processor graph and plugins, spike file writing.

## Working conventions

- Commit/push to `matthewperkins/oers` (private) on `main` when asked.
- Hardware-touching changes: say what registers or devices they write.
