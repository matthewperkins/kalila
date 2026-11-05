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
- **First real-board runs passed (rig, 2026-11-05).** Init, port scan and cable delay
  (`crates/rhythm/src/oni_board.rs`) worked first time: A1 RHD2132 + B1 RHD2216 = 48 ch.
  600 s soak with recording: `dropped=0 gaps=0 bad=0`, `hwbuf` 0.0 %, recorder stall 5 ms
  in total, 18,000,096 contiguous samples (exactly 30 kHz). Display latency about 5 ms
  steady state, measured headless. (The `samples` counter in the headless log rises about
  30,080 per tick only because the ticker fires slightly after each 1 s.)
- **FT600 driver: D3XX must be 1.0.x.** With D3XX 1.1.8 (FTDI's C/libusb rewrite),
  `oni_init_ctx` hangs forever in `_oni_reset_routine` → `oni_ft600_update_control` →
  `FT_ReadPipeEx(timeout 0)`, which now means "block forever". Built against **D3XX 1.0.14**
  (FTDI's own file, from the Wayback Machine; URL and sha256 in README), the driver
  (ft600 1.0.6) works. The stock plugin's prebuilt driver (ft600 1.0.3, D3XX 1.0.16) also
  works via `ONI_DRIVER_PATH=~/acquisition-board/libONI/linux`.
  `scripts/build-ft600-driver.sh` refuses 1.1.x.
- Second soak, 120 s with our own driver: also 0/0/0, 3,600,096 contiguous samples.
  `--aux --adc` gives 62 ch (48 + 6 AUX + 8 ADC), also clean.
- To check against the stock GUI: AUX is recorded as `raw − 32768` × 37.4 µV, which is the
  true voltage minus 1.2255 V. On port A, AUX1–3 sit at raw 65535 (top of the input
  range). With the I/O board unconnected, all 8 ADCs read about −4.35 V. This board is
  ID 0x100, so it's not a v3 and the ADC is not inverted.
- Not yet compared against a stock-GUI recording (rig plan step 5), and the GUI hasn't
  been run on hardware.
- Clippy on the rig's Rust 1.95 shows 4 newer lints (`dsp.rs`, `parser.rs`, `traces.rs`)
  that the dev box toolchain didn't flag.
- Hardware-safety audit: no flash, EEPROM or FTDI chip-config writes; all registers are
  volatile. The worst case is power-cycling the board. Disconnect I/O-board outputs
  (DAC/TTL out) for the first tests.

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

Impedance measurement, IMU display (IMU data *is* recorded), DAC/TTL-output UI (the engine
supports it via `LiveCommand`), processor graph and plugins, spike file writing.

## Working conventions

- Commit/push to `matthewperkins/oers` (private) on `main` when asked.
- Hardware-touching changes: say what registers or devices they write.
