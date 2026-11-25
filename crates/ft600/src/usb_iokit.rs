//! USB on macOS through IOKit's USB user client, with no library and no C in between: find a
//! device in the IORegistry, claim interfaces, control transfers, and bulk transfers.
//!
//! The same surface as `usbfs.rs` (Linux). A Rust port of dimna's `src/usb_iokit.c` and
//! `src/usb_darwin.zig`, which ran the board on a MacBook Air M4 (macOS 27) on 2026-11-24.
//! **This port has not yet been run with a board.**
//!
//! IOKit's USB interfaces are COM-style tables of function pointers behind CFPlugIn
//! (`<IOKit/usb/IOUSBLib.h>`). They are called here by slot number. The slot numbers, the
//! interface UUIDs, the struct layouts and the result codes below were printed by a C
//! program compiled against the macOS 27 SDK (`offsetof(IOUSBDeviceInterface942, …) / 8`
//! and so on); the interfaces are versioned by their UUID (…942), so a later system either
//! hands out this layout or refuses the UUID.
//!
//! Bulk transfers are asynchronous ones (`ReadPipeAsync`, `WritePipeAsync`) waited for on
//! the caller's thread and aborted (`AbortPipe`) when the time is up, because the FT600
//! layer needs the bytes that arrived before a timeout: the completion of an aborted
//! transfer carries that count. An interface's completions arrive as Mach messages on the
//! interface's own port, which `bulk` reads with a timeout. Nothing here starts a thread or
//! a run loop, and one transfer is in flight at a time per `Device`.
//!
//! The device itself is not opened (`USBDeviceOpen`): control requests do not need it, and
//! the interfaces are opened one by one in `claim_interface`. macOS asks for no permission
//! for a device no kernel driver has taken.

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_void};
use std::time::{Duration, Instant};

use log::debug;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsbError {
    /// No matching device in the IORegistry.
    NotFound,
    /// The device is there but may not be opened.
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
            UsbError::AccessDenied => "macOS refused access to the USB device",
            UsbError::Busy => "the USB interface is claimed by another program",
            UsbError::NoDevice => "the USB device went away",
            UsbError::Stall => "the USB endpoint stalled",
            UsbError::Io => "USB I/O error",
        })
    }
}

// ---- IOKit, CoreFoundation and Mach, as far as they are used ----

type IoObject = u32; // io_object_t, a Mach port name
type IoReturn = i32;
type CfRef = *const c_void;
/// A COM-style object: a pointer to a pointer to its table of functions.
type Com = *mut *mut c_void;
type Completion = extern "C" fn(refcon: *mut c_void, result: IoReturn, arg0: *mut c_void);

/// `CFUUIDBytes`, passed by value to `QueryInterface`.
#[repr(C)]
#[derive(Clone, Copy)]
struct UuidBytes([u8; 16]);

const USB_DEVICE_USER_CLIENT_TYPE: UuidBytes = UuidBytes([0x9d, 0xc7, 0xb7, 0x80, 0x9e, 0xc0, 0x11, 0xd4, 0xa5, 0x4f, 0x00, 0x0a, 0x27, 0x05, 0x28, 0x61]);
const USB_INTERFACE_USER_CLIENT_TYPE: UuidBytes = UuidBytes([0x2d, 0x97, 0x86, 0xc6, 0x9e, 0xf3, 0x11, 0xd4, 0xad, 0x51, 0x00, 0x0a, 0x27, 0x05, 0x28, 0x61]);
const CF_PLUGIN_INTERFACE: UuidBytes = UuidBytes([0xc2, 0x44, 0xe8, 0x58, 0x10, 0x9c, 0x11, 0xd4, 0x91, 0xd4, 0x00, 0x50, 0xe4, 0xc6, 0x42, 0x6f]);
const USB_DEVICE_INTERFACE_942: UuidBytes = UuidBytes([0x56, 0xad, 0x08, 0x9d, 0x87, 0x8d, 0x4b, 0xea, 0xa1, 0xf5, 0x2c, 0x8d, 0xc4, 0x3e, 0x8a, 0x98]);
const USB_INTERFACE_INTERFACE_942: UuidBytes = UuidBytes([0x87, 0x52, 0x66, 0x3b, 0xc0, 0x7b, 0x4b, 0xae, 0x95, 0x84, 0x22, 0x03, 0x2f, 0xab, 0x9c, 0x5a]);

// Slots (pointer-sized) in the function tables. IUnknown's three come first in every one.
const QUERY_INTERFACE: usize = 1;
const RELEASE: usize = 3;
// IOUSBDeviceInterface942
const DEV_CREATE_INTERFACE_ITERATOR: usize = 28;
const DEV_REQUEST_TO: usize = 30;
// IOUSBInterfaceInterface942
const INTF_CREATE_ASYNC_PORT: usize = 6;
const INTF_OPEN: usize = 8;
const INTF_CLOSE: usize = 9;
const INTF_GET_NUMBER: usize = 17;
const INTF_GET_NUM_ENDPOINTS: usize = 19;
const INTF_GET_PIPE_PROPERTIES: usize = 26;
const INTF_ABORT_PIPE: usize = 28;
const INTF_READ_PIPE_ASYNC: usize = 33;
const INTF_WRITE_PIPE_ASYNC: usize = 34;

/// `IOUSBDevRequestTO`.
#[repr(C)]
struct DevRequestTo {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    data: *mut c_void,
    len_done: u32,
    no_data_timeout: u32,
    completion_timeout: u32,
}

/// `IOUSBFindInterfaceRequest`; 0xffff in a field means "any".
#[repr(C)]
struct FindInterfaceRequest {
    class: u16,
    subclass: u16,
    protocol: u16,
    alternate: u16,
}

const _: () = {
    assert!(size_of::<DevRequestTo>() == 32);
    assert!(std::mem::offset_of!(DevRequestTo, data) == 8);
    assert!(std::mem::offset_of!(DevRequestTo, len_done) == 16);
    assert!(std::mem::offset_of!(DevRequestTo, completion_timeout) == 24);
    assert!(size_of::<FindInterfaceRequest>() == 8);
    assert!(size_of::<usize>() == 8);
};

const USB_IN: u8 = 1; // kUSBIn
const USB_OUT: u8 = 0; // kUSBOut
const UTF8: u32 = 0x0800_0100; // kCFStringEncodingUTF8
const CF_NUMBER_SINT32: isize = 3; // kCFNumberSInt32Type
const MACH_RCV_MSG: i32 = 0x2;
const MACH_RCV_TIMEOUT: i32 = 0x100;
const MACH_RCV_TIMED_OUT: i32 = 0x1000_4003;

// IOReturn values (<IOKit/IOReturn.h>, <IOKit/usb/USB.h>).
const fn io_return(code: u32) -> IoReturn {
    code as i32
}
const SUCCESS: IoReturn = 0;
const NO_RESOURCES: IoReturn = io_return(0xe000_02be);
const NO_DEVICE: IoReturn = io_return(0xe000_02c0);
const NOT_PRIVILEGED: IoReturn = io_return(0xe000_02c1);
const EXCLUSIVE_ACCESS: IoReturn = io_return(0xe000_02c5);
const UNSUPPORTED: IoReturn = io_return(0xe000_02c7);
const NOT_OPEN: IoReturn = io_return(0xe000_02cd);
const NOT_PERMITTED: IoReturn = io_return(0xe000_02e2);
const UNDERRUN: IoReturn = io_return(0xe000_02e7);
#[cfg(test)]
const ABORTED: IoReturn = io_return(0xe000_02eb);
const NOT_RESPONDING: IoReturn = io_return(0xe000_02ed);
const NOT_FOUND: IoReturn = io_return(0xe000_02f0);
const PIPE_STALLED: IoReturn = io_return(0xe000_404f);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(alloc: CfRef, s: *const c_char, encoding: u32) -> CfRef;
    fn CFRelease(cf: CfRef);
    fn CFGetTypeID(cf: CfRef) -> usize;
    fn CFNumberGetTypeID() -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFNumberGetValue(number: CfRef, kind: isize, out: *mut c_void) -> u8;
    fn CFStringGetCString(s: CfRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
    #[allow(clippy::too_many_arguments)]
    fn CFUUIDGetConstantUUIDWithBytes(alloc: CfRef, b0: u8, b1: u8, b2: u8, b3: u8, b4: u8, b5: u8, b6: u8, b7: u8, b8: u8, b9: u8, b10: u8, b11: u8, b12: u8, b13: u8, b14: u8, b15: u8) -> CfRef;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CfRef;
    fn IORegistryEntryIDMatching(entry_id: u64) -> CfRef;
    fn IOServiceGetMatchingServices(main_port: u32, matching: CfRef, existing: *mut IoObject) -> i32;
    fn IOServiceGetMatchingService(main_port: u32, matching: CfRef) -> IoObject;
    fn IOIteratorNext(iterator: IoObject) -> IoObject;
    fn IOObjectRelease(object: IoObject) -> i32;
    fn IORegistryEntryCreateCFProperty(entry: IoObject, key: CfRef, alloc: CfRef, options: u32) -> CfRef;
    fn IORegistryEntryGetRegistryEntryID(entry: IoObject, entry_id: *mut u64) -> i32;
    fn IOCreatePlugInInterfaceForService(service: IoObject, plugin_type: CfRef, interface_type: CfRef, interface: *mut Com, score: *mut i32) -> i32;
    fn IODispatchCalloutFromMessage(unused: *mut c_void, msg: *mut c_void, reference: *mut c_void);
}

unsafe extern "C" {
    fn mach_msg(msg: *mut c_void, option: i32, send_size: u32, rcv_size: u32, rcv_name: u32, timeout: u32, notify: u32) -> i32;
}

/// The function in slot `n` of a COM-style object's table, as the type `F`.
///
/// Safety: `obj` is a live object of the interface the slot number belongs to, and `F` is
/// that slot's `extern "C"` function type.
unsafe fn slot<F: Copy>(obj: Com, n: usize) -> F {
    const { assert!(size_of::<F>() == size_of::<usize>()) };
    unsafe {
        let table = *obj as *const *const c_void;
        std::mem::transmute_copy(&*table.add(n))
    }
}

unsafe fn com_release(obj: Com) {
    unsafe {
        let f: extern "C" fn(Com) -> u32 = slot(obj, RELEASE);
        f(obj);
    }
}

fn cf_uuid(u: UuidBytes) -> CfRef {
    let b = u.0;
    // A constant UUID object: owned by CoreFoundation, never released.
    unsafe { CFUUIDGetConstantUUIDWithBytes(std::ptr::null(), b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]) }
}

/// A CFString for a registry key, released when dropped.
struct CfString(CfRef);

impl CfString {
    fn new(s: &CStr) -> CfString {
        CfString(unsafe { CFStringCreateWithCString(std::ptr::null(), s.as_ptr(), UTF8) })
    }
}

impl Drop for CfString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

fn number_property(service: IoObject, key: &CStr) -> Option<u32> {
    let key = CfString::new(key);
    if key.0.is_null() {
        return None;
    }
    unsafe {
        let v = IORegistryEntryCreateCFProperty(service, key.0, std::ptr::null(), 0);
        if v.is_null() {
            return None;
        }
        let mut out: i32 = 0;
        let ok = CFGetTypeID(v) == CFNumberGetTypeID() && CFNumberGetValue(v, CF_NUMBER_SINT32, (&raw mut out).cast()) != 0;
        CFRelease(v);
        ok.then_some(out as u32)
    }
}

fn string_property(service: IoObject, key: &CStr) -> Option<String> {
    let key = CfString::new(key);
    if key.0.is_null() {
        return None;
    }
    unsafe {
        let v = IORegistryEntryCreateCFProperty(service, key.0, std::ptr::null(), 0);
        if v.is_null() {
            return None;
        }
        let mut buf = [0 as c_char; 256];
        let ok = CFGetTypeID(v) == CFStringGetTypeID() && CFStringGetCString(v, buf.as_mut_ptr(), buf.len() as isize, UTF8) != 0;
        CFRelease(v);
        ok.then(|| CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

/// What a failed IOKit call means in this module's terms. `NOT_RESPONDING` is what a
/// transfer reports when the device has been unplugged (to check), as `EPROTO` is on Linux.
fn map_error(r: IoReturn) -> UsbError {
    match r {
        NO_DEVICE | NOT_RESPONDING | NOT_OPEN => UsbError::NoDevice,
        EXCLUSIVE_ACCESS => UsbError::Busy,
        NOT_PRIVILEGED | NOT_PERMITTED => UsbError::AccessDenied,
        PIPE_STALLED => UsbError::Stall,
        _ => UsbError::Io,
    }
}

/// IOKit's device speed (`kUSBDeviceSpeed*`) in Mbit/s; 0 if unknown.
fn speed_mbit(speed: u32) -> u32 {
    match speed {
        0 => 2, // low speed is 1.5; callers only ask whether it is 480, or 5000 and up
        1 => 12,
        2 => 480,
        3 => 5000,
        4 => 10000,
        5 => 20000,
        _ => 0,
    }
}

/// Where a matching device sits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    /// The top byte of the location id: which controller (bus) the device hangs off.
    pub bus: u32,
    /// The device's USB address.
    pub dev: u32,
    /// Link speed in Mbit/s (480 = high speed, 5000 = SuperSpeed), 0 if unknown.
    pub speed: u32,
    /// Where the device is plugged in, the same from run to run for the same port. The
    /// order of `find`.
    location_id: u32,
    /// The IORegistry entry's id: what `Device::open` opens. New at each plugging in.
    entry_id: u64,
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

/// Every USB device with this vendor id, one of `products`, and exactly this product string,
/// in the order of their location ids (the same from run to run). Reads the IORegistry
/// only: no device is opened or sent anything.
pub fn find(vendor: u16, products: &[u16], product_string: &str) -> Vec<Location> {
    let mut out = Vec::new();
    unsafe {
        let mut it: IoObject = 0;
        // IOServiceGetMatchingServices consumes the matching dictionary. Main port 0 is
        // kIOMainPortDefault.
        if IOServiceGetMatchingServices(0, IOServiceMatching(c"IOUSBHostDevice".as_ptr()), &mut it) != 0 {
            return out;
        }
        loop {
            let s = IOIteratorNext(it);
            if s == 0 {
                break;
            }
            let matches = number_property(s, c"idVendor") == Some(vendor as u32)
                && number_property(s, c"idProduct").is_some_and(|p| products.iter().any(|&q| q as u32 == p))
                && string_property(s, c"USB Product Name").or_else(|| string_property(s, c"kUSBProductString")).as_deref() == Some(product_string);
            if matches {
                let mut entry_id = 0u64;
                IORegistryEntryGetRegistryEntryID(s, &mut entry_id);
                let location_id = number_property(s, c"locationID").unwrap_or(0);
                let address = number_property(s, c"USB Address").or_else(|| number_property(s, c"kUSBAddress")).unwrap_or(0);
                let speed = number_property(s, c"Device Speed").or_else(|| number_property(s, c"USBSpeed")).unwrap_or(u32::MAX);
                out.push(Location { bus: location_id >> 24, dev: address, speed: speed_mbit(speed), location_id, entry_id });
            }
            IOObjectRelease(s);
        }
        IOObjectRelease(it);
    }
    out.sort_by_key(|l| (l.location_id, l.entry_id));
    out
}

/// What one bulk transfer moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer {
    /// Bytes moved. Valid also when the transfer timed out.
    pub actual: usize,
    pub timed_out: bool,
}

/// The interface `iid` of the user client of type `kind` on `service`.
unsafe fn plugin_interface(service: IoObject, kind: UuidBytes, iid: UuidBytes) -> Result<Com, IoReturn> {
    unsafe {
        let mut plugin: Com = std::ptr::null_mut();
        let mut score = 0i32;
        let r = IOCreatePlugInInterfaceForService(service, cf_uuid(kind), cf_uuid(CF_PLUGIN_INTERFACE), &mut plugin, &mut score);
        if r != SUCCESS {
            return Err(r);
        }
        if plugin.is_null() {
            return Err(NO_RESOURCES);
        }
        let query: extern "C" fn(Com, UuidBytes, *mut Com) -> i32 = slot(plugin, QUERY_INTERFACE);
        let mut out: Com = std::ptr::null_mut();
        let h = query(plugin, iid, &mut out);
        com_release(plugin);
        if h == 0 && !out.is_null() { Ok(out) } else { Err(UNSUPPORTED) }
    }
}

/// A claimed interface.
struct Claimed {
    intf: Com,
    number: u32,
    /// Completions of this interface's asynchronous transfers arrive here.
    port: u32,
}

/// Where the one transfer in flight reports back. Boxed, so its address stays put.
#[derive(Default)]
struct Flight {
    done: Cell<bool>,
    result: Cell<IoReturn>,
    bytes: Cell<u32>,
}

extern "C" fn completed(refcon: *mut c_void, result: IoReturn, arg0: *mut c_void) {
    // Called from `pump` on the thread that started the transfer, with the `Flight` that
    // `bulk` passed; the device outlives every pump.
    let f = unsafe { &*(refcon as *const Flight) };
    f.result.set(result);
    f.bytes.set(arg0 as usize as u32);
    f.done.set(true);
}

/// Waits up to `ms` for a message on `port` and runs the completion it carries. False when
/// the wait timed out.
fn pump(port: u32, ms: u32) -> bool {
    let mut msg = [0u64; 128]; // 1024 bytes, aligned for a mach_msg_header_t
    unsafe {
        let k = mach_msg(msg.as_mut_ptr().cast(), MACH_RCV_MSG | MACH_RCV_TIMEOUT, 0, size_of_val(&msg) as u32, port, ms, 0);
        if k == MACH_RCV_TIMED_OUT {
            return false;
        }
        if k == 0 {
            IODispatchCalloutFromMessage(std::ptr::null_mut(), msg.as_mut_ptr().cast(), std::ptr::null_mut());
        }
    }
    true
}

/// How long an aborted transfer is given to report back before the device is given up.
const ABORT_WAIT: Duration = Duration::from_millis(2000);

/// Index into the endpoint table: the endpoint number, plus 16 for IN.
fn ep_index(endpoint: u8) -> usize {
    (endpoint & 0x0f) as usize | if endpoint & 0x80 != 0 { 16 } else { 0 }
}

pub struct Device {
    /// `IOUSBDeviceInterface942 **`; null on a device on which every call fails.
    dev: Com,
    ifs: RefCell<Vec<Claimed>>,
    /// Endpoint → (interface number, pipe reference), learned when an interface is claimed.
    pipes: RefCell<[Option<(u32, u8)>; 32]>,
    flight: Box<Flight>,
    /// Set when an aborted transfer never reported back: its completion may still arrive,
    /// so no further transfer is started.
    broken: Cell<bool>,
    /// For the test switch below: reads of more than 64 bytes so far.
    reads: Cell<u32>,
}

// The IOKit objects are not tied to a thread: completions arrive on a Mach port that
// whichever thread calls `bulk` reads. One thread uses a `Device` at a time (it is not Sync).
unsafe impl Send for Device {}

/// A test switch, as dimna's `DIMNA_USB_TEST_ABORT_EVERY`: `KALILA_USB_TEST_ABORT_EVERY=N`
/// gives up every Nth read of more than 64 bytes at once, as if its time were up, so that
/// reads are cut off part way while the board streams and the stream's own checks show
/// whether a byte was lost or repeated. Off unless set.
fn abort_every() -> Option<u32> {
    static N: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *N.get_or_init(|| std::env::var("KALILA_USB_TEST_ABORT_EVERY").ok().and_then(|v| v.parse().ok()).filter(|&n| n > 0))
}

impl Device {
    /// Opens nothing on the device itself; the interfaces are opened by `claim_interface`.
    pub fn open(loc: Location) -> Result<Device, UsbError> {
        unsafe {
            // IOServiceGetMatchingService consumes the matching dictionary.
            let s = IOServiceGetMatchingService(0, IORegistryEntryIDMatching(loc.entry_id));
            if s == 0 {
                return Err(UsbError::NoDevice);
            }
            let r = plugin_interface(s, USB_DEVICE_USER_CLIENT_TYPE, USB_DEVICE_INTERFACE_942);
            IOObjectRelease(s);
            match r {
                Ok(dev) => Ok(Device::with(dev)),
                Err(e) => {
                    debug!("open {:08x} failed: {:08x}", loc.location_id, e as u32);
                    Err(map_error(e))
                }
            }
        }
    }

    fn with(dev: Com) -> Device {
        Device { dev, ifs: RefCell::new(Vec::new()), pipes: RefCell::new([None; 32]), flight: Box::default(), broken: Cell::new(false), reads: Cell::new(0) }
    }

    /// A device on which every call fails (for tests).
    #[cfg(test)]
    pub(crate) fn invalid() -> Device {
        Device::with(std::ptr::null_mut())
    }

    /// Opens interface `interface` for this program alone and learns its endpoints.
    pub fn claim_interface(&self, interface: u32) -> Result<(), UsbError> {
        if self.dev.is_null() {
            return Err(UsbError::Io);
        }
        if self.ifs.borrow().iter().any(|c| c.number == interface) {
            return Ok(());
        }
        let r = unsafe { self.claim(interface) };
        r.map_err(|e| {
            debug!("claim interface {interface} failed: {:08x}", e as u32);
            map_error(e)
        })
    }

    unsafe fn claim(&self, interface: u32) -> Result<(), IoReturn> {
        unsafe {
            let mut req = FindInterfaceRequest { class: 0xffff, subclass: 0xffff, protocol: 0xffff, alternate: 0xffff };
            let mut it: IoObject = 0;
            let create: extern "C" fn(Com, *mut FindInterfaceRequest, *mut IoObject) -> IoReturn = slot(self.dev, DEV_CREATE_INTERFACE_ITERATOR);
            let r = create(self.dev, &mut req, &mut it);
            if r != SUCCESS {
                return Err(r);
            }
            let mut found: Com = std::ptr::null_mut();
            let mut err = NOT_FOUND;
            loop {
                let s = IOIteratorNext(it);
                if s == 0 {
                    break;
                }
                let cand = plugin_interface(s, USB_INTERFACE_USER_CLIENT_TYPE, USB_INTERFACE_INTERFACE_942);
                IOObjectRelease(s);
                let cand = match cand {
                    Ok(c) => c,
                    Err(e) => {
                        err = e;
                        continue;
                    }
                };
                let mut num = 0xffu8;
                let number: extern "C" fn(Com, *mut u8) -> IoReturn = slot(cand, INTF_GET_NUMBER);
                if number(cand, &mut num) == SUCCESS && num as u32 == interface {
                    found = cand;
                    break;
                }
                com_release(cand);
            }
            IOObjectRelease(it);
            if found.is_null() {
                return Err(err);
            }
            let intf = found;
            let open: extern "C" fn(Com) -> IoReturn = slot(intf, INTF_OPEN);
            let r = open(intf);
            if r != SUCCESS {
                com_release(intf);
                return Err(r);
            }
            let close: extern "C" fn(Com) -> IoReturn = slot(intf, INTF_CLOSE);
            let mut port = 0u32;
            let async_port: extern "C" fn(Com, *mut u32) -> IoReturn = slot(intf, INTF_CREATE_ASYNC_PORT);
            let mut r = async_port(intf, &mut port);
            let mut count = 0u8;
            if r == SUCCESS {
                let endpoints: extern "C" fn(Com, *mut u8) -> IoReturn = slot(intf, INTF_GET_NUM_ENDPOINTS);
                r = endpoints(intf, &mut count);
            }
            if r != SUCCESS {
                close(intf);
                com_release(intf);
                return Err(r);
            }
            // Pipe references run from 1 to the number of endpoints (0 is the control pipe).
            let properties: extern "C" fn(Com, u8, *mut u8, *mut u8, *mut u8, *mut u16, *mut u8) -> IoReturn = slot(intf, INTF_GET_PIPE_PROPERTIES);
            let mut pipes = self.pipes.borrow_mut();
            for pipe in 1..=count {
                let (mut direction, mut number, mut kind, mut interval, mut max_packet) = (0u8, 0u8, 0u8, 0u8, 0u16);
                if properties(intf, pipe, &mut direction, &mut number, &mut kind, &mut max_packet, &mut interval) != SUCCESS {
                    continue;
                }
                if direction != USB_IN && direction != USB_OUT {
                    continue;
                }
                let e = (number & 0x0f) as usize | if direction == USB_IN { 16 } else { 0 };
                pipes[e] = Some((interface, pipe));
            }
            self.ifs.borrow_mut().push(Claimed { intf, number: interface, port });
            Ok(())
        }
    }

    pub fn release_interface(&self, interface: u32) {
        for p in self.pipes.borrow_mut().iter_mut() {
            if p.is_some_and(|(n, _)| n == interface) {
                *p = None;
            }
        }
        let mut ifs = self.ifs.borrow_mut();
        if let Some(i) = ifs.iter().position(|c| c.number == interface) {
            drop_interface(ifs.remove(i));
        }
    }

    /// One control transfer on the default pipe (`request_type` bit 7 set = device to
    /// host). Returns bytes moved. The raw call is IOKit's `DeviceRequestTO`.
    /// Only `chip::Ft600::control_transfer` may call this, after checking the request.
    pub fn control(&self, request_type: u8, request: u8, value: u16, index: u16, data: &mut [u8], timeout_ms: u32) -> Result<usize, UsbError> {
        if self.dev.is_null() {
            return Err(UsbError::Io);
        }
        let mut req = DevRequestTo {
            request_type,
            request,
            value,
            index,
            length: data.len() as u16,
            data: data.as_mut_ptr().cast(),
            len_done: 0,
            no_data_timeout: timeout_ms,
            completion_timeout: timeout_ms,
        };
        let r = unsafe {
            let f: extern "C" fn(Com, *mut DevRequestTo) -> IoReturn = slot(self.dev, DEV_REQUEST_TO);
            f(self.dev, &mut req)
        };
        if r == SUCCESS {
            return Ok(req.len_done as usize);
        }
        debug!("control {request_type:02x} {request:02x} failed: {:08x}", r as u32);
        Err(map_error(r))
    }

    /// One bulk transfer on `endpoint` (bit 7 set = IN). Waits up to `timeout_ms`; on timeout
    /// the transfer is aborted and whatever arrived before is reported in `actual`. An OUT
    /// buffer is only read by the system. When this returns, nothing is still in flight
    /// (or the device is marked broken and takes no further transfer).
    pub fn bulk(&self, endpoint: u8, buf: *mut u8, len: usize, timeout_ms: u32) -> Result<Transfer, UsbError> {
        if self.dev.is_null() {
            return Err(UsbError::Io);
        }
        if self.broken.get() {
            return Err(UsbError::NoDevice);
        }
        let Some((number, pipe)) = self.pipes.borrow()[ep_index(endpoint)] else {
            return Err(UsbError::Io); // no claimed interface has the endpoint
        };
        let ifs = self.ifs.borrow();
        let Some(c) = ifs.iter().find(|c| c.number == number) else { return Err(UsbError::Io) };
        let is_in = endpoint & 0x80 != 0;

        let mut wait = Duration::from_millis(timeout_ms as u64);
        if is_in
            && len > 64
            && let Some(n) = abort_every()
        {
            self.reads.set(self.reads.get().wrapping_add(1));
            if self.reads.get().is_multiple_of(n) {
                wait = Duration::ZERO;
            }
        }

        let f = &*self.flight;
        f.done.set(false);
        let refcon = f as *const Flight as *mut c_void;
        let r = unsafe {
            let start: extern "C" fn(Com, u8, *mut c_void, u32, Completion, *mut c_void) -> IoReturn = slot(c.intf, if is_in { INTF_READ_PIPE_ASYNC } else { INTF_WRITE_PIPE_ASYNC });
            start(c.intf, pipe, buf.cast(), len as u32, completed, refcon)
        };
        if r != SUCCESS {
            debug!("ep {endpoint:02x} len {len}: starting the transfer failed: {:08x}", r as u32);
            return Err(if r == NOT_FOUND { UsbError::Io } else { map_error(r) });
        }
        // From here until the completion arrives the system owns `buf`.
        let deadline = Instant::now() + wait;
        while !f.done.get() {
            let now = Instant::now();
            if now >= deadline {
                // One last look without waiting: it may have completed in the meantime.
                pump(c.port, 0);
                break;
            }
            pump(c.port, (deadline - now).as_millis().clamp(1, u32::MAX as u128) as u32);
        }
        let mut timed_out = false;
        if !f.done.get() {
            // Give it up. The abort makes the transfer complete (ABORTED) with the bytes
            // that had arrived; wait for that, so nothing is in flight on return.
            timed_out = true;
            unsafe {
                let abort: extern "C" fn(Com, u8) -> IoReturn = slot(c.intf, INTF_ABORT_PIPE);
                abort(c.intf, pipe);
            }
            let give_up = Instant::now() + ABORT_WAIT;
            while !f.done.get() {
                let now = Instant::now();
                if now >= give_up {
                    self.broken.set(true);
                    return Err(UsbError::NoDevice);
                }
                pump(c.port, (give_up - now).as_millis().clamp(1, u32::MAX as u128) as u32);
            }
            // It completed on its own just before the abort took hold: not a timeout.
            if f.result.get() == SUCCESS {
                timed_out = false;
            }
        }
        let (r, actual) = (f.result.get(), f.bytes.get());
        bulk_result(r, actual, timed_out).inspect_err(|e| {
            if *e == UsbError::Io {
                debug!("ep {endpoint:02x} transfer failed: {:08x} after {actual} bytes", r as u32);
            }
        })
    }
}

/// What a finished bulk transfer means. A transfer this side gave up is a timeout whatever
/// IOKit calls its end (`ABORTED` is expected), unless the device has gone or the endpoint
/// stalled. A read that IOKit reports as an underrun (fewer bytes than asked; to check
/// whether it ever does for a bulk pipe) is a short read, not an error.
fn bulk_result(r: IoReturn, actual: u32, timed_out: bool) -> Result<Transfer, UsbError> {
    let actual = actual as usize;
    if r == SUCCESS || r == UNDERRUN {
        return Ok(Transfer { actual, timed_out: false });
    }
    if timed_out {
        return match map_error(r) {
            e @ (UsbError::NoDevice | UsbError::Stall) => Err(e),
            _ => Ok(Transfer { actual, timed_out: true }),
        };
    }
    Err(map_error(r))
}

fn drop_interface(c: Claimed) {
    // The port was made by CreateInterfaceAsyncPort and goes with the interface.
    unsafe {
        let close: extern "C" fn(Com) -> IoReturn = slot(c.intf, INTF_CLOSE);
        close(c.intf);
        com_release(c.intf);
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        for c in self.ifs.get_mut().drain(..) {
            drop_interface(c);
        }
        if !self.dev.is_null() {
            unsafe { com_release(self.dev) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iokit_results_map_to_this_modules_errors() {
        assert_eq!(map_error(NO_DEVICE), UsbError::NoDevice);
        assert_eq!(map_error(NOT_RESPONDING), UsbError::NoDevice);
        assert_eq!(map_error(EXCLUSIVE_ACCESS), UsbError::Busy);
        assert_eq!(map_error(NOT_PERMITTED), UsbError::AccessDenied);
        assert_eq!(map_error(PIPE_STALLED), UsbError::Stall);
        assert_eq!(map_error(ABORTED), UsbError::Io);
        assert_eq!(map_error(-1), UsbError::Io);
    }

    #[test]
    fn a_transfer_given_up_reports_its_bytes_and_is_not_an_error() {
        assert_eq!(bulk_result(ABORTED, 3072, true), Ok(Transfer { actual: 3072, timed_out: true }));
        assert_eq!(bulk_result(ABORTED, 0, true), Ok(Transfer { actual: 0, timed_out: true }));
        // Completed just as it was given up: complete, not a timeout (`bulk` clears the flag).
        assert_eq!(bulk_result(SUCCESS, 512, false), Ok(Transfer { actual: 512, timed_out: false }));
        // A short read is a read.
        assert_eq!(bulk_result(UNDERRUN, 20, false), Ok(Transfer { actual: 20, timed_out: false }));
        // An abort nobody here asked for is an error; so is a device that went away meanwhile.
        assert_eq!(bulk_result(ABORTED, 100, false), Err(UsbError::Io));
        assert_eq!(bulk_result(NO_DEVICE, 100, true), Err(UsbError::NoDevice));
        assert_eq!(bulk_result(PIPE_STALLED, 0, true), Err(UsbError::Stall));
    }

    #[test]
    fn packet_size_follows_link_speed() {
        let l = |speed| Location { bus: 1, dev: 1, speed: speed_mbit(speed), location_id: 0, entry_id: 0 };
        assert_eq!(l(3).bulk_packet_size(), 1024);
        assert_eq!(l(4).bulk_packet_size(), 1024);
        assert_eq!(l(2).bulk_packet_size(), 512);
        assert_eq!(l(1).bulk_packet_size(), 64);
        assert_eq!(l(99).bulk_packet_size(), 64);
    }

    #[test]
    fn endpoints_index_by_number_and_direction() {
        assert_eq!(ep_index(0x01), 1);
        assert_eq!(ep_index(0x82), 18);
        assert_eq!(ep_index(0x8f), 31);
    }

    #[test]
    fn every_call_fails_on_a_device_that_is_not_open() {
        let d = Device::invalid();
        assert_eq!(d.claim_interface(0), Err(UsbError::Io));
        let mut b = [0u8; 4];
        assert_eq!(d.bulk(0x82, b.as_mut_ptr(), b.len(), 10).unwrap_err(), UsbError::Io);
        d.release_interface(0);
    }

    #[test]
    fn the_registry_is_searched_without_finding_a_device_that_is_not_there() {
        // Walks the real IORegistry (read only): vendor 0xffff with this name does not exist.
        assert!(find(0xffff, &[0xffff], "no such kalila test device").is_empty());
    }
}
