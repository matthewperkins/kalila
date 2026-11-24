//! The Open Ephys acquisition board's FT600 USB bridge, driven without FTDI's D3XX library.
//!
//! Ported from dimna (<https://github.com/matthewperkins/dimna>: `src/usb_linux.zig`,
//! `src/ft600.zig`, `src/oni_usb.zig`),
//! where the FT600 protocol was learned from D3XX 1.0.14 and measured on the rig. Three
//! layers:
//!
//! - `usbfs`: USB through the kernel's usbdevfs, no libusb;
//! - `chip`: the FT600 wire protocol; every control request is checked against an allowlist
//!   that excludes writing the chip configuration;
//! - `driver`: an ONI driver (a port of liboni's `onidriver_ft600.c`) registered with the
//!   vendored liboni under `DRIVER_NAME`, so liboni and everything above it are unchanged.
//!
//! What it sends the board: what D3XX sends on every open (GPIO requests, GPIO 0 being the
//! board's FIFO reset; transfer announcements; pipe aborts; two read-only requests, firmware
//! version and chip configuration) and the ONI command and register traffic liboni asks for.
//! Linux only. Used when `KALILA_DRIVER=usb`; not yet verified on the board from kalila.

mod chip;
mod driver;
mod usbfs;

pub use driver::{DRIVER_NAME, register};
