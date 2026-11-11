# oers — Open Ephys acquisition in Rust

A rewrite of the Open Ephys GUI's acquisition path for the **ONI/FT600 acquisition board**
(ECP5 gateware, not Opal Kelly), with a GPU-drawn egui interface and a spike-oriented audio
monitor. Rust everywhere except the vendored C `liboni` and the FT600 ONI driver, which
wraps FTDI's closed-source D3XX library.

```
crates/oni-sys   liboni 4.6 (C, vendored, statically linked) + FFI
crates/rhythm    board driver (port of AcqBoardONI/Rhd2000ONIBoard), frame parser, simulator
crates/engine    real-time acquisition thread, recorder (Open Ephys Binary), DSP, audio
app/             egui + wgpu (Vulkan) GUI: GPU traces, GPU spike persistence view
scripts/         FT600 driver build, udev rule, realtime limits
```

## Build

```sh
cargo build --release            # needs Rust ≥ 1.85, a C compiler, libpipewire/alsa headers
./target/release/oers --sim      # simulator (no hardware needed)
./target/release/oers            # hardware
```

## Rig setup (one time)

1. **FT600 ONI driver.** Needs FTDI's *D3XX Linux x86_64* package from the **1.0.x** line;
   tested on hardware with **1.0.14**. D3XX 1.1.x (FTDI's libusb-based rewrite) does *not*
   work: `oni_init_ctx` hangs forever, because a 0 ms pipe timeout became "block forever".
   FTDI's page (<https://ftdichip.com/drivers/d3xx-drivers/>) now lists only 1.1.x for Linux;
   FTDI's 1.0.14 upload is archived at
   <https://web.archive.org/web/20240519150808id_/https://ftdichip.com/wp-content/uploads/2023/06/libftd3xx-linux-x86_64-1.0.14.tgz>
   (sha256 `ca71469fc2267ba655c0a9af9aa4cb853e4adad1dfd438df83f72d0b6b7ac265`).
   Extract it, then run `scripts/build-ft600-driver.sh <extracted>/linux-x86_64`. That
   installs `~/.local/lib/libonidriver_ft600.so` (FTDI's library linked statically, no
   runtime dependency on it). The script refuses 1.1.x. A driver shipped with an existing
   Open Ephys install is also picked up automatically from `~/.local/share/open-ephys/shared*`,
   or set `ONI_DRIVER_PATH=<dir>`.
2. **USB permissions:** `sudo install -m644 scripts/60-open-ephys-ft600.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger`
3. **Real-time priority (recommended):** copy `scripts/99-oers-realtime.conf` to
   `/etc/security/limits.d/`, add yourself to the `realtime` group, then re-login. The top
   bar shows `RT` (green) once the acquisition thread runs under SCHED_FIFO. Without it,
   acquisition still works; the board's DRAM buffer absorbs scheduling stalls.
4. **Hyprland / NVIDIA:** use NVIDIA driver ≥ 555 (explicit sync). For the absolute lowest
   display latency, allow tearing for this window:
   `windowrulev2 = immediate, class:^(oers)$` and `general:allow_tearing = true`.

## First check on the rig (before an experiment)

```sh
cargo run --release -p engine --example headless -- --hw --seconds 600 --record /tmp/oers-test
```

It prints once per second. Expect `dropped=0 gaps=0 bad=0`, `hwbuf` near 0 %, and
recorder back-pressure `bp=0ms`. Then open `/tmp/oers-test` with
`open_ephys.analysis.Session` and compare against a short recording from the stock GUI with
the same headstages: channel count, µV scale, and TTL timing. The register command lists
are verified byte-identical to Intan's C++, and the frame layout and init/scan sequence are
ported line by line from the stock plugin. See "What has been tested" below for what has
run on a real board.

## Reliability design

* The acquisition thread does only USB reads and parsing; one pooled `Arc<Block>` per
  32 samples, with no allocation in steady state.
* **Recording is lossless or loudly not.** The recorder queue is the only blocking consumer.
  If the disk stalls, acquisition waits and the board's DRAM buffers the data (`hw buf %`
  rises). Display, DSP and audio drop blocks instead, and are counted, never stalling.
* Every sample's hardware counter is checked. Gaps split blocks, are counted
  (`lost`/`gaps`), and show up as non-contiguous `sample_numbers.npy`; they are never
  silently papered over. (The stock plugin numbers frames as received.)
* Bad frames, read errors, PLL timeouts and recorder I/O errors surface in the UI. Stopping
  acquisition while recording asks for confirmation.
* Files: Open Ephys Binary (`structure.oebin`, `continuous.dat`, `sample_numbers.npy`,
  `timestamps.npy`, TTL `states/full_words`). `.npy` headers are rewritten every second, so
  a crash leaves readable files. Verified with `open-ephys-python-tools` and `neo`
  (SpikeInterface).

## GPU path

Raw int16 samples go to the GPU exactly as they arrive from USB (one `write_buffer` per
frame, about 64 KB at 64 channels). A compute shader runs the display filter (IIR, per
channel) and folds samples into per-pixel min/max columns. The vertex shader builds
geometry from those columns. Spike waveforms are uploaded once each and drawn as instanced
thick lines with age-faded persistence. The CPU does no per-sample drawing work.
Present mode defaults to no-vsync with one queued frame (`present_mode` in the config:
`auto`, `vsync`, `mailbox`, `immediate`, `fifo`), and frames are paced by `max fps`.

## Audio

See the module docs in `crates/engine/src/audio.rs`. In short: band-limit 300–7k → gain
from a **held** noise calibration (never an AGC, so dense GP firing and quiet cortex keep
their character) → gentle look-ahead expander keyed on the held σ (hash stays audible;
depth adjustable; 0 = off) → +3 dB presence at 2.8 kHz → limiter that is exactly linear
below 0.5 FS → stereo placement → windowed-sinc resampling with a clock-drift servo.
**A/B: reference** bypasses the expander and EQ. Noise σ uses a two-pass median that excludes
spikes, so high-rate regions don't inflate it (tested at 80 Hz tonic firing). Click a
channel label to listen; Ctrl+click or Shift+click to add up to 8 channels.

## Config

`~/.config/oers/config.json` (board, display, audio, spike, record root). Command line:
`--sim`, `--acquire`.

## Desktop launcher

`packaging/install-desktop.sh` adds oers to your application launcher (with a "simulated
board" action). Run it again if you move the checkout. A desktop launch has no terminal, so
the log goes to `~/.local/state/oers/oers.log`.

## What has been tested

oers has run on **one rig** so far. Until it has run on others, make your first
recordings alongside the stock GUI (or compare a short recording from each with the same
headstages), as below.

**Tested on hardware** (ECP5/FT600 ONI board, gateware v2.0.0, board ID 0x100; RHD2132
in 16-channel mode on port A, RHD2216 on port B; NVIDIA RTX 2000 Ada, Hyprland):
* Board init, port scan and cable delay.
* Recording: 600 s soaks with no dropped samples, no gaps and no bad frames; 18,000,096
  contiguous samples at exactly 30 kHz.
* Against a stock GUI 1.1.0 / plugin 1.1.6 recording with the same headstages: neural
  channels match channel for channel (0.195 µV/bit), AUX and ADC scale and offset match,
  TTL line numbers, timing and `full_words` match.
* The GUI on the board: traces, TTL overlay, spike tiles, audio monitor, scroll-back review
  of the ongoing recording. Triggered sweeps, the slope trigger and typed notes have been
  checked on the simulator only.
* CPU while recording: about a third of the stock GUI's (stock's record thread busy-polls).

**Not yet tested:**
* Any other headstage (RHD2164, RHD2132 in 32-channel mode, an RHD2216 on its own, more
  than one per port), other board IDs or gateware versions, and v3 boards (whose ADC is
  inverted; the code handles it, untested).
* The ADC inputs with real signals beyond a pulse; DAC and TTL outputs (not yet checked on
  a scope); DAC routing in 16-channel mode.
* Other GPUs, AMD/Intel drivers, X11 and Wayland compositors other than Hyprland.
* Sessions longer than a few hours. Scroll-back review keeps an overview of the recording
  in RAM (about 0.7 GB per hour on the rig, with no cap yet), so a very long session on
  a machine with little memory could run short.

Only Linux is supported.

## Not yet ported

Impedance measurement, BNO/IMU display (IMU data *is* recorded), the general processor graph
and other plugins, and a spike recording format.

## License

GPL-3.0-or-later (see [LICENSE](LICENSE)), as for the Open Ephys GUI and acquisition-board
plugin this is ported from. The vendored `liboni` (`crates/oni-sys/liboni`) keeps its own MIT
licence. FTDI's D3XX library is closed source and not included; see "Rig setup".

## Icon

The icon shows Bastet and Sekhmet, adapted from drawings by Gunawan Kartapranata
([CC BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/)) and Jeff Dahl
([CC BY-SA 4.0](https://creativecommons.org/licenses/by-sa/4.0/)) on Wikimedia Commons. The
icon is CC BY-SA 4.0; full credits and changes are in
[packaging/icon/ICON-CREDITS.md](packaging/icon/ICON-CREDITS.md). It doesn't affect the
licence of the code.
