//! USB from user space through the kernel's usbdevfs (`/dev/bus/usb/BBB/DDD`), with no library
//! in between: find a device in sysfs, claim interfaces, control transfers, and bulk transfers
//! as asynchronous URBs.
//!
//! Port of dimna's `src/usb_linux.zig`. Bulk transfers use submit / poll / reap rather than the
//! synchronous USBDEVFS_BULK ioctl, because that one throws away partial data when it times
//! out. A reaped URB always reports how many bytes arrived, also when it was cancelled.
//!
//! One transfer is in flight at a time per `Device`, made and finished on the caller's
//! thread; nothing here starts a thread.

use std::ffi::CString;
use std::os::raw::{c_int, c_ulong, c_void};
use std::time::{Duration, Instant};

use log::debug;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbError {
    /// No matching device in sysfs.
    NotFound,
    /// The device node cannot be opened read/write (udev rule missing?).
    AccessDenied,
    /// Another program has claimed the interface.
    Busy,
    /// The device went away.
    NoDevice,
    /// The endpoint stalled.
    Stall,
    /// Any other transfer or system error.
    Io,
}

impl std::fmt::Display for UsbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            UsbError::NotFound => "no matching USB device",
            UsbError::AccessDenied => "permission denied on the USB device node (udev rule installed?)",
            UsbError::Busy => "the USB interface is claimed by another program",
            UsbError::NoDevice => "the USB device went away",
            UsbError::Stall => "the USB endpoint stalled",
            UsbError::Io => "USB I/O error",
        })
    }
}

// From <linux/usbdevice_fs.h> (x86-64 and aarch64; checked by the test below).
const fn ioc(dir: c_ulong, nr: c_ulong, size: usize) -> c_ulong {
    (dir << 30) | ((size as c_ulong) << 16) | ((b'U' as c_ulong) << 8) | nr
}
const IOC_NONE: c_ulong = 0;
const IOC_WRITE: c_ulong = 1;
const IOC_READ: c_ulong = 2;
const USBDEVFS_CONTROL: c_ulong = ioc(IOC_READ | IOC_WRITE, 0, std::mem::size_of::<CtrlTransfer>());
const USBDEVFS_SUBMITURB: c_ulong = ioc(IOC_READ, 10, std::mem::size_of::<Urb>());
const USBDEVFS_DISCARDURB: c_ulong = ioc(IOC_NONE, 11, 0);
const USBDEVFS_REAPURB: c_ulong = ioc(IOC_WRITE, 12, std::mem::size_of::<*mut c_void>());
const USBDEVFS_REAPURBNDELAY: c_ulong = ioc(IOC_WRITE, 13, std::mem::size_of::<*mut c_void>());
const USBDEVFS_CLAIMINTERFACE: c_ulong = ioc(IOC_READ, 15, std::mem::size_of::<u32>());
const USBDEVFS_RELEASEINTERFACE: c_ulong = ioc(IOC_READ, 16, std::mem::size_of::<u32>());
const USBDEVFS_URB_TYPE_BULK: u8 = 3;

/// `struct usbdevfs_urb` without its trailing (empty) iso frame array.
#[repr(C)]
struct Urb {
    kind: u8,
    endpoint: u8,
    status: c_int,
    flags: u32,
    buffer: *mut c_void,
    buffer_length: c_int,
    actual_length: c_int,
    start_frame: c_int,
    number_of_packets: c_int,
    error_count: c_int,
    signr: u32,
    usercontext: *mut c_void,
}

/// `struct usbdevfs_ctrltransfer`.
#[repr(C)]
struct CtrlTransfer {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    timeout: u32,
    data: *mut c_void,
}

const _: () = {
    assert!(std::mem::size_of::<Urb>() == 56);
    assert!(std::mem::offset_of!(Urb, buffer) == 16);
    assert!(std::mem::offset_of!(Urb, actual_length) == 28);
    assert!(std::mem::offset_of!(Urb, usercontext) == 48);
    assert!(std::mem::size_of::<CtrlTransfer>() == 24);
    assert!(std::mem::offset_of!(CtrlTransfer, data) == 16);
};

/// Where a matching device sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub bus: u32,
    pub dev: u32,
    /// Link speed in Mbit/s (480 = high speed, 5000 = SuperSpeed), 0 if unknown.
    pub speed: u32,
}

impl Location {
    /// Largest bulk packet at this speed; a shorter packet ends a transfer.
    pub fn bulk_packet_size(&self) -> usize {
        if self.speed >= 5000 {
            1024
        } else if self.speed >= 480 {
            512
        } else {
            64
        }
    }
}

fn sysfs(dir: &std::path::Path, attr: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(attr))
        .ok()
        .map(|s| s.trim_end().to_string())
}

/// Every USB device with this vendor id, one of `products`, and exactly this product string,
/// in bus then device order (the same from run to run).
pub fn find(vendor: u16, products: &[u16], product_string: &str) -> Vec<Location> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/bus/usb/devices") else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        // Devices are "1-2", "2-1.3"; skip root hubs ("usb1") and interfaces ("1-2:1.0").
        if name.is_empty() || name.starts_with('.') || name.starts_with("usb") || name.contains(':') {
            continue;
        }
        let d = e.path();
        let hex = |a: &str| sysfs(&d, a).and_then(|s| u16::from_str_radix(&s, 16).ok());
        let dec = |a: &str| sysfs(&d, a).and_then(|s| s.parse::<u32>().ok());
        if hex("idVendor") != Some(vendor) {
            continue;
        }
        match hex("idProduct") {
            Some(p) if products.contains(&p) => {}
            _ => continue,
        }
        if sysfs(&d, "product").as_deref() != Some(product_string) {
            continue;
        }
        let (Some(bus), Some(dev)) = (dec("busnum"), dec("devnum")) else {
            continue;
        };
        out.push(Location {
            bus,
            dev,
            speed: dec("speed").unwrap_or(0),
        });
    }
    out.sort_by_key(|l| (l.bus, l.dev));
    out
}

/// What one bulk transfer moved.
#[derive(Debug, Clone, Copy)]
pub struct Transfer {
    /// Bytes moved. Valid also when the transfer timed out.
    pub actual: usize,
    pub timed_out: bool,
}

fn errno() -> c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

pub struct Device {
    fd: c_int,
}

impl Device {
    pub fn open(loc: Location) -> Result<Device, UsbError> {
        let path = CString::new(format!("/dev/bus/usb/{:03}/{:03}", loc.bus, loc.dev)).unwrap();
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd >= 0 {
            return Ok(Device { fd });
        }
        Err(match errno() {
            libc::EACCES | libc::EPERM => UsbError::AccessDenied,
            libc::ENOENT | libc::ENODEV => UsbError::NoDevice,
            _ => UsbError::Io,
        })
    }

    /// A device on which every call fails (for tests).
    #[cfg(test)]
    pub(crate) fn invalid() -> Device {
        Device { fd: -1 }
    }

    /// An ioctl, retried on EINTR. Returns the result or the errno.
    fn ioctl(&self, req: c_ulong, arg: *mut c_void) -> Result<c_int, c_int> {
        loop {
            let r = unsafe { libc::ioctl(self.fd, req as _, arg) };
            if r >= 0 {
                return Ok(r);
            }
            let e = errno();
            if e != libc::EINTR {
                return Err(e);
            }
        }
    }

    pub fn claim_interface(&self, interface: u32) -> Result<(), UsbError> {
        let mut n = interface;
        self.ioctl(USBDEVFS_CLAIMINTERFACE, &mut n as *mut u32 as *mut c_void)
            .map(|_| ())
            .map_err(|e| match e {
                libc::EBUSY => UsbError::Busy,
                libc::ENODEV => UsbError::NoDevice,
                _ => UsbError::Io,
            })
    }

    pub fn release_interface(&self, interface: u32) {
        let mut n = interface;
        let _ = self.ioctl(USBDEVFS_RELEASEINTERFACE, &mut n as *mut u32 as *mut c_void);
    }

    /// One control transfer (`request_type` bit 7 set = device to host). Returns bytes moved.
    /// Only `chip::Ft600::control_transfer` may call this, after checking the request.
    pub fn control(
        &self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
        timeout_ms: u32,
    ) -> Result<usize, UsbError> {
        let mut ct = CtrlTransfer {
            request_type,
            request,
            value,
            index,
            length: data.len() as u16,
            timeout: timeout_ms,
            data: data.as_mut_ptr() as *mut c_void,
        };
        match self.ioctl(USBDEVFS_CONTROL, &mut ct as *mut CtrlTransfer as *mut c_void) {
            Ok(n) => Ok(n as usize),
            Err(libc::ENODEV | libc::ESHUTDOWN) => Err(UsbError::NoDevice),
            Err(libc::EPIPE) => Err(UsbError::Stall),
            Err(e) => {
                debug!("control {request_type:02x} {request:02x} failed: errno {e}");
                Err(UsbError::Io)
            }
        }
    }

    /// One bulk transfer on `endpoint` (bit 7 set = IN). Waits up to `timeout_ms`; on timeout
    /// the transfer is cancelled and whatever arrived before is reported in `actual`. An OUT
    /// buffer is only read by the kernel. When this returns, nothing is still in flight.
    pub fn bulk(&self, endpoint: u8, buf: *mut u8, len: usize, timeout_ms: u32) -> Result<Transfer, UsbError> {
        let mut urb = Urb {
            kind: USBDEVFS_URB_TYPE_BULK,
            endpoint,
            status: 0,
            flags: 0,
            buffer: buf as *mut c_void,
            buffer_length: len as c_int,
            actual_length: 0,
            start_frame: 0,
            number_of_packets: 0,
            error_count: 0,
            signr: 0,
            usercontext: std::ptr::null_mut(),
        };
        let urb_ptr: *mut Urb = &mut urb;
        match self.ioctl(USBDEVFS_SUBMITURB, urb_ptr as *mut c_void) {
            Ok(_) => {}
            Err(libc::ENODEV | libc::ESHUTDOWN) => return Err(UsbError::NoDevice),
            Err(e) => {
                debug!("submit ep {endpoint:02x} len {len} failed: errno {e}");
                return Err(UsbError::Io);
            }
        }
        // From here until the URB is reaped the kernel owns `urb` and `buf`; every path below
        // reaps it before returning (after a discard the URB always completes, so the blocking
        // reap cannot hang).
        let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
        let mut cancelled = false;
        loop {
            if !cancelled {
                let now = Instant::now();
                if now >= deadline {
                    // EINVAL here means it completed in the meantime; the reap then returns it.
                    let _ = self.ioctl(USBDEVFS_DISCARDURB, urb_ptr as *mut c_void);
                    cancelled = true;
                } else {
                    // Wait first: usbdevfs reports a reapable URB as writable.
                    let left = (deadline - now).as_millis().clamp(1, i32::MAX as u128) as c_int;
                    let mut pfd = libc::pollfd {
                        fd: self.fd,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    let pr = unsafe { libc::poll(&mut pfd, 1, left) };
                    if pr > 0 && pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0 && pfd.revents & libc::POLLOUT == 0
                    {
                        // The device is gone; its URBs are already given back by the kernel.
                        let mut done: *mut Urb = std::ptr::null_mut();
                        let _ = self.ioctl(USBDEVFS_REAPURBNDELAY, &mut done as *mut *mut Urb as *mut c_void);
                        return Err(UsbError::NoDevice);
                    }
                    if pr == 0 {
                        continue; // timed out: cancel above
                    }
                }
            }
            let mut done: *mut Urb = std::ptr::null_mut();
            let req = if cancelled {
                USBDEVFS_REAPURB
            } else {
                USBDEVFS_REAPURBNDELAY
            };
            match self.ioctl(req, &mut done as *mut *mut Urb as *mut c_void) {
                Ok(_) => {
                    if done == urb_ptr {
                        break;
                    }
                    // Any other cannot happen with one in flight.
                }
                Err(libc::EAGAIN) => {}
                Err(libc::ENODEV | libc::ESHUTDOWN) => return Err(UsbError::NoDevice),
                Err(_) => return Err(UsbError::Io),
            }
        }
        let actual = urb.actual_length.max(0) as usize;
        match -urb.status {
            0 => Ok(Transfer {
                actual,
                timed_out: false,
            }),
            // Cancelled by us (ENOENT), or the kernel's own unlink (ECONNRESET).
            libc::ENOENT | libc::ECONNRESET => Ok(Transfer {
                actual,
                timed_out: true,
            }),
            libc::ENODEV | libc::ESHUTDOWN | libc::EPROTO => Err(UsbError::NoDevice),
            libc::EPIPE => Err(UsbError::Stall),
            e => {
                debug!("ep {endpoint:02x} transfer failed: errno {e} after {actual} bytes");
                Err(UsbError::Io)
            }
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usbdevfs_ioctl_numbers_match_the_kernel_header() {
        // Values from <linux/usbdevice_fs.h> on x86-64, as dimna checks them.
        assert_eq!(USBDEVFS_CONTROL, 0xc018_5500);
        assert_eq!(USBDEVFS_SUBMITURB, 0x8038_550a);
        assert_eq!(USBDEVFS_DISCARDURB, 0x550b);
        assert_eq!(USBDEVFS_REAPURB, 0x4008_550c);
        assert_eq!(USBDEVFS_REAPURBNDELAY, 0x4008_550d);
        assert_eq!(USBDEVFS_CLAIMINTERFACE, 0x8004_550f);
        assert_eq!(USBDEVFS_RELEASEINTERFACE, 0x8004_5510);
    }

    #[test]
    fn packet_size_follows_link_speed() {
        let l = |speed| Location { bus: 1, dev: 1, speed };
        assert_eq!(l(5000).bulk_packet_size(), 1024);
        assert_eq!(l(480).bulk_packet_size(), 512);
        assert_eq!(l(12).bulk_packet_size(), 64);
    }
}
