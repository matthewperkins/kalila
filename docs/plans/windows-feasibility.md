# Windows support: feasibility (2026-11-26)

What it would take to run kalila on Windows while keeping what it does on Linux today. It
would reverse a standing decision (CLAUDE.md: "Windows is not a consideration"), so this is
written to help decide, not as a plan.

**Basis.** A read of the tree at v0.2.0 for everything tied to the operating system, and
general knowledge of the Windows APIs involved. Nothing was built or run on Windows: there
is no Windows machine in this project. Statements about Windows behaviour that I could not
check are marked *(unverified)*; they are the first things to test.

## Summary

- **Most of the program is portable as it stands**: the parser, block assembly, recorder
  format, pyramids, DSP, review, notes, configuration, and the whole GUI (egui, wgpu, WGSL
  shaders). The macOS port already put the OS-specific parts behind small seams, and
  Windows would use the same seams.
- **The simulator on Windows is a small job.** Building, passing most tests, and running
  the GUI against the simulated board is days of work, most of it mechanical.
- **The board is where the cost and the risk are**, and it forces a choice that cannot be
  avoided (see "The driver choice"): either FTDI's closed driver, as the stock GUI uses,
  or kalila's own driver at the price of replacing the device's Windows driver.
- **Real-time behaviour can be approximated, not matched.** Windows has no equivalent of
  `SCHED_FIFO`; audio latency through the default sound path is likely worse than Linux.
- **Nothing can be called tested without a Windows PC attached to a board.** That machine
  and the time on it are the real price, more than the code.

## What depends on the operating system today

Found by searching for `cfg(...)` and Unix-only calls (10 files):

| Where | What it does | Windows needs |
|---|---|---|
| `crates/ft600/src/usbfs.rs`, `usb_iokit.rs` | raw USB (Linux usbdevfs, macOS IOKit) | a third backend, or the D3XX route |
| `crates/engine/src/sched.rs` | `SCHED_FIFO`/`SCHED_IDLE`, Mach policies, keep-awake | thread priorities, MMCSS, a power request |
| `crates/engine/src/signals.rs` | SIGTERM/SIGINT finalise a recording | console control handler, session-end message |
| `crates/engine/src/record.rs`, `app/src/review.rs` | `FileExt::read_at` | `seek_read` (one line each) |
| `crates/engine/src/pyramid.rs` | idle priority for backfill | background thread mode |
| `app/src/main.rs` | GPU backend choice; stderr to a log file when not on a terminal | DX12/Vulkan; log file for a windowed program |
| `app/src/config.rs`, `app.rs`, `rhythm/src/oni.rs` | `$HOME`, `$XDG_CONFIG_HOME`, `$XDG_STATE_HOME` | `%APPDATA%`, `%LOCALAPPDATA%` |
| `crates/oni-sys/build.rs` | compiles vendored liboni | liboni already has `_WIN32` code; MSVC build to try |
| tests | `RLIMIT_FSIZE` (disk-full seeds), `kill` (signals), the fake chip hooks `usbfs` | other mechanisms, or these tests stay Unix-only |
| `packaging/`, `scripts/` | .deb/.rpm, udev rule, real-time limits, desktop entry | an installer or zip; none of the rest applies |

## Easy (mechanical, low risk)

- **Build and CI.** A `windows-latest` job beside the Linux and macOS ones. `cpal` is asked
  for with its `pipewire` feature unconditionally: make that Linux-only. `libc` is already
  under `cfg(unix)`.
- **Paths.** Config in `%APPDATA%\kalila`, notes log and app log in `%LOCALAPPDATA%\kalila`.
- **File access.** `read_at` → `seek_read`. Rust opens files on Windows so that another
  handle may read a file being written, which review and the pyramid backfill rely on.
  `create_new` (never overwrite a recording) and rename-into-place (`structure.oebin`,
  the config) work the same.
- **Recording format.** Byte-identical: the code already refuses big-endian targets and
  writes explicit little-endian `.npy`. Folder names contain nothing Windows rejects.
- **GUI.** eframe's deferred viewports (the separate trace windows) are supported on
  Windows. Shaders are WGSL through wgpu, as on Metal. Keyboard: Ctrl is already the
  modifier everywhere it would be on Windows.
- **Simulator, notes, review, sweeps, slope trigger, UI scripts.** No OS dependence.
- **Keep-awake while acquiring.** One call (`SetThreadExecutionState` or a power request),
  in the place the macOS assertion sits.

## Moderate (clear how, needs care and measurement)

- **Thread scheduling.** `sched.rs` gets a Windows arm: time-critical priority for the
  acquisition thread, the multimedia class scheduler ("Pro Audio") for the audio feeder,
  background mode for the pyramid builder. This is a best effort: see "Cannot be matched".
- **Timers.** The driver's read pacing sleeps until ~100 µs before a block is due, and the
  simulator paces itself with sleeps. Windows sleeps were historically quantised to
  ~15.6 ms; recent Rust and Windows use high-resolution timers *(unverified for this
  toolchain)*. Pacing is already off by default on macOS for the same kind of reason and
  would start off on Windows, at the CPU cost measured on Linux (about 8 s against 5 s per
  20 s run at 4 KiB).
- **Stopping cleanly.** There are no signals. A console program gets a control handler
  (Ctrl+C, window close, logoff; a few seconds to finish). The GUI must act on the
  session-ending message so that a logoff or shutdown finalises an open recording; whether
  eframe passes that through to `on_exit` is *(unverified)*. A forced kill cannot be
  caught, as with SIGKILL.
- **Durability.** `sync_data` maps to `FlushFileBuffers`. Its cost every 5 s must be
  measured on NTFS before it is enabled (macOS has it off because it was slow there).
- **Audio device.** cpal uses WASAPI. It will work; the latency is the open question
  (see below).
- **Tests that are Unix-only.** The disk-full seeds of the simulation test use a per-process
  file-size limit, which Windows lacks. Either those 60 seeds do not run on Windows, or the
  recorder gets a small write seam so the limit is simulated on every platform (worth
  doing anyway).
- **Packaging.** A zip with `kalila.exe` and `kalila-headless.exe` is easy. An installer
  with a Start-menu entry is moderate. Unsigned programs get a SmartScreen warning; a
  code-signing certificate costs money and identity checks, and is yours to decide.
- **Antivirus and indexing.** Defender scans files as they are written and can hold a
  rename briefly. Recording folders should be excluded, and the rename given a short retry.
  Whether it costs samples must be measured: this touches priority 1.

## Difficult (substantial work, or outcome uncertain)

### The driver choice

On Windows exactly one kernel driver is bound to the FT600, and what kalila can do follows
from which:

| | A: FTDI's driver, through D3XX | B: WinUSB, with kalila's own driver |
|---|---|---|
| How | liboni's `onidriver_ft600` built against FTDI's `FTD3XX.dll`, as the stock GUI does on Windows; kalila's `KALILA_DRIVER=d3xx` path already drives liboni this way | a new `usb_winusb.rs` behind the same surface as `usbfs.rs`; `chip.rs` and `driver.rs` unchanged |
| User setup | FTDI's driver, usually installed by Windows itself | the board's driver replaced by WinUSB (Zadig, or a signed driver package) |
| Stock Open Ephys GUI on the same PC | keeps working | cannot open the board until the driver is switched back |
| Closed-source code in the path | yes (FTDI's DLL) | no |
| Control-transfer allowlist | does not apply: the traffic is FTDI's, as with `d3xx` on Linux | applies unchanged |
| Read pacing, 3 s silence limit, 2 s open (vs 7 s) | no: those live in kalila's driver | yes |
| Work | moderate: build, find the DLL, check which D3XX versions behave | large: device discovery, overlapped bulk I/O where a timed-out read must keep its bytes, aborts; then the whole board test series again |
| Main risk | a D3XX version that blocks forever, as 1.1.8 did on Linux *(unverified on Windows)*; redistribution terms of the DLL | whether the FT600 behaves under WinUSB exactly as under usbdevfs *(unverified)*; a user who switches drivers and loses the stock GUI |

**Decided by the user, 2026-11-26: route A** (FTDI's driver through D3XX) is how the
drivers would be handled.

Route A is the realistic first step, and it is how the stock plugin reaches the same board
on Windows. Route B would restore what makes kalila's driver worth having, at a cost the
user of a shared rig may not accept. They are not exclusive: A first, B later behind the
existing `KALILA_DRIVER` switch.

### Real-time and latency

- **Acquisition under load.** On Linux the acquisition thread runs `SCHED_FIFO` and the
  board's DRAM absorbs stalls; the rig soaks show 0 lost samples with cameras recording.
  Windows offers high priorities but no fixed-priority real-time class for ordinary
  programs, and driver interrupt work (DPCs) can hold any thread. The board's buffer gives
  margin, so lossless recording is plausible, but it has to be shown by soaks on real
  Windows hardware, with the usual background services running.
- **Audio latency.** Linux: 7–9 ms with a 128-frame buffer, about one underrun in 30 s.
  Shared-mode WASAPI typically runs a 10 ms period with extra buffering; getting under
  ~20 ms may need exclusive mode or ASIO, and what cpal offers there is *(unverified)*. The
  drift servo and its 12 ms guard would need retuning. Texture (the audio rule) is
  unaffected: the DSP chain is the same code.
- **Display latency.** ~5 ms on Linux measured headless. Windows composites every window
  through DWM, which usually adds a frame unless the window is fullscreen. Measurable with
  the same method macOS now uses (presented time), which would need a Windows equivalent.

### Verification

Everything in "What has been tested" in the README was earned on the rig: comparison with
a stock recording, soaks, eleven open/close runs, latency tables. None of it transfers.
Windows needs its own series on a PC with the board: first open, register dump comparison,
a 600 s soak, a recording compared with the Linux rig's, unplug and power-off behaviour,
the stop and Apply paths. CI can only cover the simulator. Without that PC the honest label
is "builds and runs on the simulator", as macOS is labelled now.

## Cannot be matched

- **No vendor driver, and coexistence with the stock GUI, together.** Linux has both today
  (usbdevfs needs no driver; the stock GUI is unaffected). Windows allows one or the other
  (route A or B above).
- **The hardware-safety guarantee on route A.** "Every control transfer goes through the
  allowlist" cannot be claimed when FTDI's DLL owns the device. kalila still would not call
  the chip-configuration write, but could no longer prove what is sent.
- **`SCHED_FIFO` and `SCHED_IDLE` as such.** The closest Windows settings are weaker in
  both directions. "RT" in the status chip would mean something less.
- **The Linux packaging pieces**: the udev rule, the real-time group and limits. They have
  no Windows counterpart and are not needed there; the requirement they serve (priority
  without administrator rights) is met differently.

Nothing in the recording, display, review or analysis-compatibility features is impossible.

## Cost beyond the port

- A third platform in every hardware-facing change: three USB backends (or two plus D3XX),
  three scheduling arms, three sets of rig checks. The macOS port already shows the shape:
  its board path is "written, never run with a board".
- CI time roughly up by half; a Windows runner cannot open windows or a board either.
- Support questions about drivers, antivirus and sound devices, which Linux users of a
  rig rarely raise.

## If you go ahead: an order that fails early

1. **Simulator only.** Windows CI job, paths, `seek_read`, scheduling stubs, log file. Run
   the GUI on the simulator on a Windows PC; measure display and audio latency there.
   *Stop here if audio latency or dropped display frames are unacceptable.*
2. **Board through D3XX (route A)** on a PC with the board. First open, register dump,
   soak, comparison with a Linux recording. *Stop here if a soak loses samples that tuning
   cannot remove.*
3. **Stopping, durability, packaging.** Session-end handling, the sync cost on NTFS, a zip
   release from CI, with the README saying exactly what was tried.
4. **Optional: WinUSB backend (route B)**, behind `KALILA_DRIVER`, for those who want
   kalila's own driver and accept the driver swap.

Steps 1 and 3 suit sub-agents (mechanical, checked by CI). Steps 2 and 4 need you at a
Windows PC with the board, as the rig sessions did.

## What I would want to know first

- Is there a Windows PC that can have the board for test sessions? Without it, only step 1
  is possible.
- Who is it for: your own rigs, or others who cannot run Linux? For others, route A's
  "works beside the stock GUI" matters more than kalila's own driver.
- Is a signed installer wanted, or is a zip with a SmartScreen warning acceptable?
