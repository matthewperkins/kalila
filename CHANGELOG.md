# Changelog

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## Unreleased

## 0.1.0 — 2026-11-25

The first release. The program was called oers until 2026-11-13.

### Acquisition
- Tested with gateware v2.0.0 and board ID 0x100, through vendored liboni 4.6.
- A hardware counter checks every sample; gaps are counted and show in the sample numbers.

### Recording
- Open Ephys Binary, read by open-ephys-python-tools and neo; `.npy` headers rewritten every second.
- Lossless or loudly not: a full queue blocks acquisition; a write error stops with REC ERROR.

### Display
- GPU traces (wgpu; Vulkan on Linux, Metal on macOS) per port, with Raw or Band filters,
  common-average reference and per-row scaling.
- Spike tiles with persistence, TTL lanes, and sweeps triggered on a TTL edge or a level crossing.

### Review
- Scroll-back review of the ongoing recording, with min/max pyramids and a RAM budget (1.5 GB default).

### Audio
- Spike monitor with a held noise calibration (no AGC), a gentle expander, a linear limiter and
  a reference A/B mode.

### Board driver
- FT600 USB driver in Rust (`crates/ft600`) is the default; it sends the chip only what D3XX sends.
- `KALILA_DRIVER=d3xx` uses FTDI's D3XX, which must be 1.0.x (1.1.x hangs on open).

### Packaging
- Packages: .deb (Debian 12+, Ubuntu 24.04+), .rpm (RHEL, Alma, Rocky 9+, Fedora), and tarballs
  for glibc 2.34+ and musl.
- The release workflow installs and runs each package on the simulator; a desktop launcher is included.

### macOS
- Apple silicon, shipped as `kalila.app`. Tried on one MacBook Air M4 (macOS 27), simulated board only.
- Display, audio, recording and review ran there. The real board has not been run from a Mac;
  record on Linux until it has.
