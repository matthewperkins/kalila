//! The FTDI FT600 (USB 3 FIFO bridge) wire protocol, as far as the ONI acquisition board needs
//! it, without FTDI's D3XX library.
//!
//! Port of dimna's `src/ft600.zig`, where the protocol was learned by logging what D3XX 1.0.14
//! sends for the calls liboni's ONI driver makes, on the rig's board. The chip has two
//! interfaces:
//!
//! ```text
//!   interface 0: EP 0x01 OUT (commands), EP 0x81 IN (notifications, unused here)
//!   interface 1: EP 0x02/0x82 = channel 0 (ONI data), EP 0x03/0x83 = channel 1 (ONI control)
//! ```
//!
//! Every transfer on a channel is first announced with a 20-byte command on EP 0x01:
//!
//! ```text
//!   index u32 | pipe u8 | command u8 | 0 u16 | length u32 | 0 u32 | 0 u32      (little-endian)
//!
//!   command 1 = "a transfer of `length` bytes on `pipe` follows" (IN and OUT alike)
//!   command 3 = abort the pipe
//! ```
//!
//! (Commands 0 and 2, the chip's stream mode, are not used: measured by dimna on the rig, it
//! costs as much CPU as announced reads once reads are paced, and delivers later.)
//!
//! An announced IN transfer ends when `length` bytes have been sent or the chip's FIFO runs
//! empty (a short packet), whichever comes first; the next read needs a new announcement.
//!
//! GPIO and configuration are vendor control requests:
//!
//! ```text
//!   0x40 0x02 wValue=1  u16 (mask << 2) | level       write GPIO
//!   0x40 0x02 wValue=2  u16 (mask << 2) | direction   enable GPIO (1 = output)
//!   0x40 0x02 wValue=3  u16                           GPIO pulls
//!   0xc0 0xcf wValue=1  152 bytes                     read chip configuration
//!   0xc0 0xf1           u32                           firmware version
//! ```
//!
//! **Hardware safety:** nothing here writes the chip configuration (that would be 0x40 0xcf).
//! Everything sent is what D3XX sends on every open. This is enforced, not only intended:
//! every control transfer goes through `Ft600::control_transfer`, which refuses any request
//! not in `ALLOWED_REQUESTS` before it reaches the kernel. The tests at the end of this file
//! check the list, that 0x40 0xcf is refused, and that no other code in the workspace makes a
//! control transfer.

use std::time::Instant;

use log::info;

use crate::usb::{self, Device, UsbError};

pub const VENDOR_ID: u16 = 0x0403;
/// FT600 (16-bit FIFO) and FT601 (32-bit FIFO).
pub const PRODUCT_IDS: [u16; 2] = [0x601e, 0x601f];

const EP_COMMAND: u8 = 0x01;
const CMD_TRANSFER: u8 = 1;
const CMD_ABORT: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Data = 0,
    Control = 1,
}

impl Channel {
    pub fn pipe_in(self) -> u8 {
        0x82 + self as u8
    }
    pub fn pipe_out(self) -> u8 {
        0x02 + self as u8
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ChipConfig {
    pub vendor_id: u16,
    pub product_id: u16,
    pub fifo_clock: u8,
    /// 0 = "245" mode, 1 = "600" (multi-channel) mode.
    pub fifo_mode: u8,
    pub channel_config: u8,
    pub optional_features: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Usb(UsbError),
    /// A 20-byte command was not sent whole.
    ShortCommand,
    /// A control request not in `ALLOWED_REQUESTS`.
    ForbiddenRequest,
}

impl From<UsbError> for Error {
    fn from(e: UsbError) -> Self {
        Error::Usb(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Usb(e) => e.fmt(f),
            Error::ShortCommand => f.write_str("FT600 command not sent whole"),
            Error::ForbiddenRequest => f.write_str("control request not allowed"),
        }
    }
}

/// The only control requests (bmRequestType, bRequest) the driver may send. Nothing that
/// writes the chip configuration (0x40 0xcf) may ever be added here.
const ALLOWED_REQUESTS: [(u8, u8); 3] = [
    (0xc0, 0xf1), // read firmware version
    (0xc0, 0xcf), // read (device to host) chip configuration
    (0x40, 0x02), // GPIO write / enable / pulls
];

/// Whether a control request may be sent to the chip.
fn allowed(request_type: u8, request: u8) -> bool {
    ALLOWED_REQUESTS.contains(&(request_type, request))
}

/// `KALILA_USB_TRACE=1` logs every command and transfer to stderr.
fn tracing() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("KALILA_USB_TRACE").is_some())
}

macro_rules! trace {
    ($($arg:tt)*) => {
        if tracing() {
            static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
            let t = T0.get_or_init(Instant::now).elapsed();
            eprintln!("[usb {}.{:06}] {}", t.as_secs(), t.subsec_micros(), format_args!($($arg)*));
        }
    };
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub struct Ft600 {
    dev: Device,
    pub firmware: u32,
    /// Bytes announced on each channel's IN pipe that the chip has not delivered yet.
    outstanding: [usize; 2],
    /// Bulk packet size at the link's speed (1024 at SuperSpeed).
    packet_size: usize,
    drain: Vec<u8>,
    /// True when the last data-channel read emptied the chip's FIFO (its transfer ended
    /// short): the reader has caught up with the board.
    pub caught_up: bool,
    /// Bulk IN transfers done on the data channel, and the bytes they carried.
    pub data_transfers: u64,
    pub data_bytes: u64,
}

impl Ft600 {
    /// Opens the `index`-th board (bus/device order) whose product string is `product`; a
    /// negative index means the first. Claims both interfaces.
    pub fn open(product: &str, index: i32) -> Result<Ft600, Error> {
        let locs = usb::find(VENDOR_ID, &PRODUCT_IDS, product);
        let want = index.max(0) as usize;
        let loc = *locs.get(want).ok_or(UsbError::NotFound)?;
        let dev = Device::open(loc)?;
        dev.claim_interface(0)?;
        if let Err(e) = dev.claim_interface(1) {
            dev.release_interface(0);
            return Err(e.into());
        }
        let mut ft = Ft600 {
            dev,
            firmware: 0,
            outstanding: [0; 2],
            packet_size: loc.bulk_packet_size(),
            drain: vec![0; 65536],
            caught_up: false,
            data_transfers: 0,
            data_bytes: 0,
        };
        let mut v = [0u8; 4];
        if let Ok(4) = ft.control_transfer(0xc0, 0xf1, 0, 0, &mut v, 1000) {
            ft.firmware = u32::from_le_bytes(v);
        }
        info!(
            "FT60x at bus {} device {} ({} Mbit/s), firmware {:x}.{:02x}, opened without D3XX",
            loc.bus,
            loc.dev,
            loc.speed,
            (ft.firmware >> 8) & 0xff,
            ft.firmware & 0xff
        );
        // As D3XX does on open: both IN pipes start from a known state.
        let r = ft.command(Channel::Data.pipe_in(), CMD_ABORT, 0).and_then(|_| ft.command(Channel::Control.pipe_in(), CMD_ABORT, 0));
        if let Err(e) = r {
            ft.release();
            return Err(e);
        }
        Ok(ft)
    }

    fn release(&self) {
        self.dev.release_interface(1);
        self.dev.release_interface(0);
    }

    /// Reads (never writes) the chip configuration.
    pub fn chip_config(&self) -> Result<ChipConfig, Error> {
        let mut b = [0u8; 152];
        let n = self.control_transfer(0xc0, 0xcf, 1, 0, &mut b, 1000)?;
        if n != b.len() {
            return Err(UsbError::Io.into());
        }
        Ok(ChipConfig {
            vendor_id: u16::from_le_bytes([b[0], b[1]]),
            product_id: u16::from_le_bytes([b[2], b[3]]),
            fifo_clock: b[137],
            fifo_mode: b[138],
            channel_config: b[139],
            optional_features: u16::from_le_bytes([b[140], b[141]]),
        })
    }

    /// The one place this driver makes a control transfer. Requests not in
    /// `ALLOWED_REQUESTS` are refused before anything reaches the kernel.
    fn control_transfer(&self, request_type: u8, request: u8, value: u16, index: u16, data: &mut [u8], timeout_ms: u32) -> Result<usize, Error> {
        if !allowed(request_type, request) {
            info!("refused control request {request_type:02x} {request:02x}");
            return Err(Error::ForbiddenRequest);
        }
        Ok(self.dev.control(request_type, request, value, index, data, timeout_ms)?)
    }

    fn gpio_request(&self, what: u16, value: u16) -> Result<(), Error> {
        let mut b = value.to_le_bytes();
        self.control_transfer(0x40, 0x02, what, 0, &mut b, 1000).map(|_| ())
    }
    /// Drives the GPIOs in `mask` (bits 0..1) to `level`. D3XX: FT_WriteGPIO.
    pub fn gpio_write(&self, mask: u8, level: u8) -> Result<(), Error> {
        self.gpio_request(1, ((mask as u16 & 3) << 2) | (level as u16 & 3))
    }
    /// Sets the direction of the GPIOs in `mask` (1 = output). D3XX: FT_EnableGPIO.
    pub fn gpio_enable(&self, mask: u8, direction: u8) -> Result<(), Error> {
        self.gpio_request(2, ((mask as u16 & 3) << 2) | (direction as u16 & 3))
    }
    /// The pull setting the ONI driver uses, `FT_SetGPIOPull(0x3, 0x05)`, sent as the value
    /// D3XX puts on the wire for it. (The general encoding was not worked out.)
    pub fn gpio_pull_oni(&self) -> Result<(), Error> {
        self.gpio_request(3, 0x002d)
    }

    fn command(&self, pipe: u8, cmd: u8, len: u32) -> Result<(), Error> {
        let mut b = [0u8; 20];
        b[4] = pipe;
        b[5] = cmd;
        b[8..12].copy_from_slice(&len.to_le_bytes());
        let t = self.dev.bulk(EP_COMMAND, b.as_mut_ptr(), b.len(), 1000)?;
        let ok = !t.timed_out && t.actual == b.len();
        trace!("cmd {cmd} pipe {pipe:02x} len {len}{}", if ok { "" } else { " FAILED" });
        if ok { Ok(()) } else { Err(Error::ShortCommand) }
    }

    /// Reads up to `buf.len()` bytes from a channel: returns as soon as the chip ends the
    /// transfer (its FIFO ran empty), so often fewer. Zero means nothing arrived within
    /// `timeout_ms`; the announcement then stays pending and the next call continues it.
    pub fn read(&mut self, ch: Channel, buf: &mut [u8], timeout_ms: u32) -> Result<usize, Error> {
        let i = ch as usize;
        if self.outstanding[i] == 0 {
            self.command(ch.pipe_in(), CMD_TRANSFER, buf.len() as u32)?;
            self.outstanding[i] = buf.len();
        }
        let want = buf.len().min(self.outstanding[i]);
        let t = self.dev.bulk(ch.pipe_in(), buf.as_mut_ptr(), want, timeout_ms)?;
        if ch == Channel::Data {
            self.data_transfers += 1;
            self.data_bytes += t.actual as u64;
            self.caught_up = t.actual < want;
        }
        if ch == Channel::Control || t.timed_out {
            trace!(
                "read {:02x} want {want} (announced {}) -> {}{} {}",
                ch.pipe_in(),
                self.outstanding[i],
                t.actual,
                if t.timed_out { " TIMEOUT" } else { "" },
                hex(&buf[..t.actual.min(24)])
            );
        }
        if t.timed_out {
            // Cancelled. If the last packet received was a short one, the chip has ended the
            // transfer (the data arrived just as the read was given up) and owes nothing.
            // Otherwise it still owes the rest. A cancel that lands exactly on a packet
            // boundary is ambiguous; if the transfer had in fact ended, the next read
            // completes at once with nothing, which clears the announcement below.
            if t.actual % self.packet_size != 0 {
                self.outstanding[i] = 0;
            } else {
                self.outstanding[i] -= t.actual;
            }
        } else if t.actual == want && want < self.outstanding[i] {
            // Our buffer was smaller than what is announced: the chip still owes the rest.
            self.outstanding[i] -= t.actual;
        } else {
            // Complete, or ended early by the chip (short packet): the announcement is used up.
            self.outstanding[i] = 0;
        }
        Ok(t.actual)
    }

    /// Writes all of `data` to a channel. Returns the bytes written (less only on timeout).
    pub fn write(&self, ch: Channel, data: &[u8], timeout_ms: u32) -> Result<usize, Error> {
        self.command(ch.pipe_out(), CMD_TRANSFER, data.len() as u32)?;
        // The kernel only reads from an OUT buffer.
        let t = self.dev.bulk(ch.pipe_out(), data.as_ptr() as *mut u8, data.len(), timeout_ms)?;
        trace!(
            "write {:02x} {} -> {}{} {}",
            ch.pipe_out(),
            data.len(),
            t.actual,
            if t.timed_out { " TIMEOUT" } else { "" },
            hex(&data[..data.len().min(45)])
        );
        Ok(t.actual)
    }

    /// Aborts a pipe. For an IN pipe with an announcement still pending, first collects and
    /// discards what is still arriving for up to 200 ms, then sends the abort command.
    /// Unlike D3XX's FT_AbortPipe, which does that 200 ms read on every call: with nothing
    /// announced the chip has nothing it may send (232 of 232 such reads returned no data on
    /// the rig, per dimna), so it is skipped; that takes about 6 s off opening the board.
    pub fn abort(&mut self, pipe: u8) -> Result<(), Error> {
        if pipe & 0x80 != 0 {
            let i = ((pipe & 0x7f) - 2) as usize;
            if self.outstanding[i] != 0 {
                let t = self.dev.bulk(pipe, self.drain.as_mut_ptr(), self.drain.len(), 200)?;
                trace!("drain {pipe:02x} -> {} bytes (announced {})", t.actual, self.outstanding[i]);
                self.outstanding[i] = 0;
            }
        }
        self.command(pipe, CMD_ABORT, 0)
    }
}

impl Drop for Ft600 {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_requests_the_driver_uses_are_allowed() {
        assert!(allowed(0xc0, 0xf1));
        assert!(allowed(0xc0, 0xcf));
        assert!(allowed(0x40, 0x02));
    }

    #[test]
    fn writing_the_chip_configuration_is_refused() {
        assert!(!allowed(0x40, 0xcf));
        assert!(!allowed(0x40, 0xf1));
        assert!(!allowed(0xc0, 0x02));
        assert!(!allowed(0x00, 0x09)); // standard SET_CONFIGURATION
        assert!(!allowed(0x41, 0xcf));
        assert!(!allowed(0x42, 0xcf));
        // No host-to-device request is allowed except GPIO.
        for rt in 0..=255u8 {
            for rq in 0..=255u8 {
                if rt & 0x80 == 0 && !(rt == 0x40 && rq == 0x02) {
                    assert!(!allowed(rt, rq), "{rt:02x} {rq:02x}");
                }
            }
        }
    }

    #[test]
    fn a_forbidden_request_is_refused_before_it_reaches_the_kernel() {
        // fd -1: had the request got as far as the ioctl, the error would be Usb(Io).
        let ft = Ft600 {
            dev: Device::invalid(),
            firmware: 0,
            outstanding: [0; 2],
            packet_size: 1024,
            drain: Vec::new(),
            caught_up: false,
            data_transfers: 0,
            data_bytes: 0,
        };
        let mut b = [0u8; 152];
        assert_eq!(ft.control_transfer(0x40, 0xcf, 1, 0, &mut b, 1000), Err(Error::ForbiddenRequest));
        assert_eq!(ft.control_transfer(0xc0, 0xcf, 1, 0, &mut b, 1000), Err(Error::Usb(UsbError::Io)));
        std::mem::forget(ft); // nothing to release on fd -1
    }

    #[test]
    fn this_file_makes_control_transfers_only_in_control_transfer() {
        let src = include_str!("chip.rs");
        // The spellings are split so that this test does not contain them.
        let call = concat!(".", "control(");
        assert_eq!(src.matches(call).count(), 1);
        let wrapper = src.find(concat!("fn ", "control_transfer(")).unwrap();
        let check = wrapper + src[wrapper..].find("if !allowed(request_type, request)").unwrap();
        let at = src.find(call).unwrap();
        let end = wrapper + src[wrapper..].find("\n    }\n").unwrap();
        assert!(wrapper < check && check < at && at < end);
    }

    #[test]
    fn no_other_file_in_the_workspace_calls_or_defines_a_usb_control_transfer() {
        // Only the two backends (usbfs.rs, usb_iokit.rs) may define the function and only
        // chip.rs may call it. Text is searched line by line, comments included. Not seen:
        // calls through a function pointer or alias. The raw calls are looked for too: the
        // ioctl number (only usbfs.rs may use it) and IOKit's request (only usb_iokit.rs).
        let call = concat!(".", "control(");
        let def = concat!("fn ", "control(");
        let ioctl = concat!("USBDEVFS_", "CONTROL");
        let iokit = concat!("Device", "Request");
        let iokit_slot = concat!("DEV_", "REQUEST_TO");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut files = 0;
        let mut bad = Vec::new();
        let mut stack = vec![root.join("crates"), root.join("app")];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n != "target" && n != "liboni") {
                        stack.push(p);
                    }
                    continue;
                }
                if p.extension().is_none_or(|x| x != "rs") {
                    continue;
                }
                files += 1;
                let name = p.file_name().unwrap().to_string_lossy().into_owned();
                let in_ft600 = p.components().any(|c| c.as_os_str() == "ft600");
                let text = std::fs::read_to_string(&p).unwrap();
                for (n, line) in text.lines().enumerate() {
                    let here = || format!("{}:{}: {}", p.display(), n + 1, line.trim());
                    if line.contains(def) && !(in_ft600 && (name == "usbfs.rs" || name == "usb_iokit.rs")) {
                        bad.push(here());
                    }
                    if (line.contains(iokit) || line.contains(iokit_slot)) && !(in_ft600 && name == "usb_iokit.rs") {
                        bad.push(here());
                    }
                    if line.contains(call) && !(in_ft600 && name == "chip.rs") {
                        bad.push(here());
                    }
                    if line.contains(ioctl) && !(in_ft600 && name == "usbfs.rs") {
                        bad.push(here());
                    }
                }
            }
        }
        assert!(files > 30, "walked only {files} files");
        assert!(bad.is_empty(), "control transfers outside the FT600 layer:\n{}", bad.join("\n"));
    }
}
