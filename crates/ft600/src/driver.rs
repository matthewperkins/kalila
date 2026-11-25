//! An ONI driver for the FT600 acquisition board that talks to the chip directly (`chip.rs`)
//! instead of through FTDI's D3XX library. It is compiled into the program and registered with
//! liboni under the name `DRIVER_NAME`.
//!
//! Port of dimna's `src/oni_usb.zig`, itself a port of liboni's
//! `drivers/ft600/onidriver_ft600.c` (MIT), Linux path (control stream polled, synchronous
//! reads). The ONI side is unchanged: commands and register access go over channel 1, sample
//! frames over channel 0. Differences from upstream are marked `DIFF:`.
//!
//! liboni calls every function here from the thread that owns the `oni_ctx` (kalila's
//! acquisition thread); nothing is shared between threads.

use std::os::raw::{c_int, c_void};
use std::time::{Duration, Instant};

use log::{error, info};
use oni_sys as sys;

use crate::chip::{Channel, Error, Ft600};
use crate::usb::UsbError;

/// The name to pass to `oni_create_ctx` once `register` has run.
pub const DRIVER_NAME: &str = "kalilausb";
const USB_DESCRIPTION: &str = "Open Ephys FT600 USB board";

const CMD_WRITEREG: u8 = 0x01;
const CMD_READREG: u8 = 0x02;
const CMD_SETPRIO: u8 = 0x03;
const SIGRES_SIGNAL: u8 = 0x01;
const SIGRES_REG: u8 = 0x02;
const DEFAULT_PRIO: u32 = 0x0A0A_0000;
const AUX_SIZE: usize = 8192;

/// Read pacing (see `read_stream`): `KALILA_USB_PACE=0` turns it off, any other value on.
/// Unset, it is on, except on macOS: there the pacing sleep wakes 2 to 5 ms late while the
/// screen is busy (measured in dimna's port, blocks up to 21 ms late), and waiting in the
/// USB transfer instead cost no more CPU.
fn pacing() -> bool {
    std::env::var("KALILA_USB_PACE").map_or(cfg!(not(target_os = "macos")), |v| !v.starts_with('0'))
}

/// A byte ring of fixed capacity (one slot is kept free, as upstream's circBuffer).
struct Ring {
    buf: Vec<u8>,
    r: usize,
    w: usize,
}

impl Ring {
    fn new(size: usize) -> Self {
        Ring { buf: vec![0; size], r: 0, w: 0 }
    }
    fn len(&self) -> usize {
        let n = self.buf.len();
        (self.w + n - self.r) % n
    }
    fn can_write(&self, n: usize) -> bool {
        self.buf.len() - self.len() > n
    }
    fn write(&mut self, src: &[u8]) {
        for &b in src {
            self.buf[self.w] = b;
            self.w = (self.w + 1) % self.buf.len();
        }
    }
    fn read(&mut self, dst: &mut [u8]) {
        for b in dst {
            *b = self.buf[self.r];
            self.r = (self.r + 1) % self.buf.len();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    NoInit,
    Init,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SigState {
    Cmd,
    Signal,
    Reg,
}

#[derive(Default)]
struct RegOp {
    dev_idx: u32,
    dev_addr: u32,
    value: u32,
    rw: u32,
}

struct Ctx {
    ft: Option<Ft600>,
    in_block: u32,
    out_block: u32,
    signal: Ring,
    reg: Ring,
    aux: Vec<u8>,
    state: State,
    sig_state: SigState,
    sig_offset: usize,
    sig_error: bool,
    reg_op: RegOp,
    /// Read pacing: how long the board takes to produce one byte (running estimate, 0 =
    /// unknown), and when the last block was completed.
    ns_per_byte: f64,
    last_block: Option<Instant>,
    pace: bool,
}

impl Ctx {
    fn new() -> Self {
        Ctx {
            ft: None,
            in_block: 4096,
            out_block: 4096,
            signal: Ring::new(8192),
            reg: Ring::new(512),
            aux: vec![0; AUX_SIZE],
            state: State::NoInit,
            sig_state: SigState::Cmd,
            sig_offset: 0,
            sig_error: false,
            reg_op: RegOp::default(),
            ns_per_byte: 0.0,
            last_block: None,
            pace: true,
        }
    }

    fn reset_ctx(&mut self) {
        self.sig_state = SigState::Cmd;
        self.sig_offset = 0;
        self.sig_error = false;
        self.state = State::Init;
    }

    /// Sorts bytes from the control channel into the signal and register-reply buffers.
    fn fill_control_buffers(&mut self, data: &[u8]) {
        let mut index = 0;
        while index < data.len() {
            match self.sig_state {
                SigState::Cmd => {
                    if data[index] == SIGRES_SIGNAL {
                        self.sig_state = SigState::Signal;
                    } else if data[index] == SIGRES_REG {
                        self.sig_state = SigState::Reg;
                    }
                    index += 1;
                }
                SigState::Signal => {
                    let first = index;
                    while index < data.len() && data[index] != 0 {
                        index += 1;
                    }
                    if index >= data.len() {
                        index -= 1;
                    }
                    let part = &data[first..=index];
                    // DIFF: upstream spins until there is room, which cannot happen on one
                    // thread; report it instead.
                    if !self.signal.can_write(part.len()) {
                        self.sig_error = true;
                        return;
                    }
                    self.signal.write(part);
                    if data[index] == 0 {
                        self.sig_state = SigState::Cmd;
                    }
                    index += 1;
                }
                SigState::Reg => {
                    // DIFF: upstream takes min(4, left) even after a split reply; take the
                    // rest of the 4-byte value.
                    let n = (4 - self.sig_offset).min(data.len() - index);
                    if !self.reg.can_write(n) {
                        self.sig_error = true;
                        return;
                    }
                    self.reg.write(&data[index..index + n]);
                    self.sig_offset = (self.sig_offset + n) % 4;
                    index += n;
                    if self.sig_offset == 0 {
                        self.sig_state = SigState::Cmd;
                    }
                }
            }
        }
    }

    /// Reads whatever the control channel has (waiting up to 100 ms) into the buffers.
    fn update_control(&mut self) {
        let Some(ft) = self.ft.as_mut() else {
            self.sig_error = true;
            return;
        };
        let mut aux = std::mem::take(&mut self.aux);
        match ft.read(Channel::Control, &mut aux, 100) {
            Ok(n) => {
                if n > 0 {
                    self.fill_control_buffers(&aux[..n]);
                }
            }
            Err(_) => self.sig_error = true,
        }
        self.aux = aux;
    }

    fn send_cmd(&mut self, data: &[u8]) -> c_int {
        let Some(ft) = self.ft.as_ref() else { return sys::ONI_EINVALSTATE };
        let mut total = 0;
        while total < data.len() {
            match ft.write(Channel::Control, &data[total..], 1000) {
                Ok(n) => total += n,
                Err(_) => return sys::ONI_ESEEKFAILURE,
            }
        }
        sys::ONI_ESUCCESS
    }

    fn reset_acq(&mut self, hard: bool) {
        if let Some(ft) = self.ft.as_mut() {
            if hard {
                let _ = ft.gpio_write(1, 1);
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = ft.abort(Channel::Data.pipe_in());
            let _ = ft.abort(Channel::Data.pipe_out());
        }
        self.reset_ctx();
        if let Some(ft) = self.ft.as_ref() {
            let _ = ft.gpio_write(1, 0);
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    fn stop_acq(&mut self) {
        std::thread::sleep(Duration::from_millis(10));
        let running = self.state == State::Running;
        if let Some(ft) = self.ft.as_mut() {
            if running && ft.data_transfers > 1000 {
                info!(
                    "data channel: {} transfers, {} bytes ({} bytes per transfer)",
                    ft.data_transfers,
                    ft.data_bytes,
                    ft.data_bytes / ft.data_transfers
                );
            }
            let _ = ft.abort(Channel::Data.pipe_in());
        }
        self.state = State::Init;
    }

    fn start_acq(&mut self) {
        if let Some(ft) = self.ft.as_mut() {
            ft.data_transfers = 0;
            ft.data_bytes = 0;
        }
        self.ns_per_byte = 0.0;
        self.last_block = None;
        self.pace = pacing();
        self.state = State::Running;
    }

    fn close_device(&mut self) {
        if let Some(mut ft) = self.ft.take() {
            let _ = ft.abort(Channel::Data.pipe_in());
            let _ = ft.abort(Channel::Data.pipe_out());
            let _ = ft.abort(Channel::Control.pipe_out());
            let _ = ft.abort(Channel::Control.pipe_in());
            // Dropping releases the interfaces and closes the device node.
        }
    }

    fn init(&mut self, host_idx: c_int) -> c_int {
        let ft = match Ft600::open(USB_DESCRIPTION, host_idx) {
            Ok(ft) => ft,
            Err(e) => {
                error!("could not open the FT600 without D3XX: {e}");
                return match e {
                    Error::Usb(UsbError::NotFound) => {
                        if host_idx > 0 {
                            sys::ONI_EDEVIDX
                        } else {
                            sys::ONI_EDEVID
                        }
                    }
                    _ => sys::ONI_EINIT,
                };
            }
        };
        let cfg = match ft.chip_config() {
            Ok(c) => c,
            Err(e) => {
                error!("could not read the FT600 configuration: {e}");
                self.ft = Some(ft);
                self.close_device();
                return sys::ONI_EINIT;
            }
        };
        info!(
            "FT600 configuration: {:04x}:{:04x}, FIFO clock {}, FIFO mode {}, channels {}, optional features {:#x}",
            cfg.vendor_id, cfg.product_id, cfg.fifo_clock, cfg.fifo_mode, cfg.channel_config, cfg.optional_features
        );
        self.ft = Some(ft);
        // FIFO mode "600" with all optional features (notifications and the like) off.
        if cfg.fifo_mode != 1 || cfg.optional_features != 0 {
            error!(
                "FT600 configuration not supported: FIFO mode {}, optional features {:#x}",
                cfg.fifo_mode, cfg.optional_features
            );
            self.close_device();
            return sys::ONI_EBADCONTROLLER;
        }
        // GPIO 0 is the board's FIFO reset.
        let ft = self.ft.as_ref().unwrap();
        let gpio = ft.gpio_pull_oni().and_then(|_| ft.gpio_write(1, 0)).and_then(|_| ft.gpio_enable(1, 1));
        if gpio.is_err() {
            self.close_device();
            return sys::ONI_EINIT;
        }
        self.reset_acq(true);
        let mut buf = [0u8; 5];
        buf[0] = CMD_SETPRIO;
        buf[1..5].copy_from_slice(&DEFAULT_PRIO.to_le_bytes());
        let res = self.send_cmd(&buf);
        if res != sys::ONI_ESUCCESS {
            self.close_device();
            return res;
        }
        self.state = State::Init;
        sys::ONI_ESUCCESS
    }

    fn read_data(&mut self, dst: &mut [u8]) -> c_int {
        // DIFF: upstream spins until another thread starts acquisition; here that is an error.
        if self.state != State::Running {
            return sys::ONI_EINVALSTATE;
        }
        let want = dst.len() & !3; // whole 32-bit words
        if want < self.in_block as usize {
            return sys::ONI_EINVALREADSIZE;
        }
        let Some(ft) = self.ft.as_mut() else { return sys::ONI_EINVALSTATE };
        // The chip ends a transfer whenever its FIFO runs empty, so reading as fast as
        // possible fetches a block in dozens of pieces of one sample each (about 12,000
        // transfers a second, each with its announcement). liboni needs the whole block
        // before it hands anything on, so nothing is gained by that. Pacing: after a short
        // read (we have caught up), sleep until the rest of the block should have been
        // produced, less a margin, and fetch it in one transfer. When we are behind, the
        // chip's FIFO does not run empty (`caught_up` is false) and there is no sleep.
        const MARGIN_NS: f64 = 100_000.0;
        let mut got = 0;
        while got < want {
            match ft.read(Channel::Data, &mut dst[got..want], 1000) {
                Ok(n) => got += n,
                Err(e) => {
                    error!("data read failed: {e}");
                    return sys::ONI_EREADFAILURE;
                }
            }
            if got < want && ft.caught_up && self.pace && self.ns_per_byte > 0.0 {
                let wait = (want - got) as f64 * self.ns_per_byte - MARGIN_NS;
                if wait > 50_000.0 {
                    std::thread::sleep(Duration::from_nanos(wait.min(5_000_000.0) as u64));
                }
            }
        }
        let now = Instant::now();
        if let Some(last) = self.last_block {
            let mut per_byte = (now - last).as_nanos() as f64 / want as f64;
            if self.ns_per_byte == 0.0 {
                self.ns_per_byte = per_byte;
            } else {
                // A stall must not teach us to sleep long: limit each step, then smooth.
                per_byte = per_byte.clamp(0.5 * self.ns_per_byte, 2.0 * self.ns_per_byte);
                self.ns_per_byte = 0.9 * self.ns_per_byte + 0.1 * per_byte;
            }
        }
        self.last_block = Some(now);
        got as c_int
    }
}

fn reg_offset(reg: sys::oni_config_t) -> u32 {
    match reg {
        sys::ONI_CONFIG_DEV_IDX => 0,
        sys::ONI_CONFIG_REG_ADDR => 1,
        sys::ONI_CONFIG_REG_VALUE => 2,
        sys::ONI_CONFIG_RW => 3,
        sys::ONI_CONFIG_TRIG => 4,
        sys::ONI_CONFIG_RUNNING => 5,
        sys::ONI_CONFIG_RESET => 6,
        sys::ONI_CONFIG_SYSCLKHZ => 7,
        sys::ONI_CONFIG_ACQCLKHZ => 8,
        sys::ONI_CONFIG_RESETACQCOUNTER => 9,
        sys::ONI_CONFIG_HWADDRESS => 10,
        _ => 0,
    }
}

fn put_reg_write(buf: &mut [u8; 45], i: &mut usize, reg: u32, value: u32) {
    let o = 9 * *i;
    buf[o] = CMD_WRITEREG;
    buf[o + 1..o + 5].copy_from_slice(&reg.to_le_bytes());
    buf[o + 5..o + 9].copy_from_slice(&value.to_le_bytes());
    *i += 1;
}

// ------------------------------------------------------------------ the C function table

/// # Safety
/// `p` must be a pointer returned by `create_ctx` and not yet destroyed.
unsafe fn cast<'a>(p: sys::oni_driver_ctx) -> &'a mut Ctx {
    unsafe { &mut *(p as *mut Ctx) }
}

/// Runs `f`, turning a panic into an error code: unwinding into liboni's C is undefined.
fn guard(f: impl FnOnce() -> c_int) -> c_int {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
        error!("panic inside the FT600 driver");
        sys::ONI_EINIT
    })
}

extern "C" fn create_ctx() -> sys::oni_driver_ctx {
    Box::into_raw(Box::new(Ctx::new())) as sys::oni_driver_ctx
}

extern "C" fn destroy_ctx(p: sys::oni_driver_ctx) -> c_int {
    if p.is_null() {
        return sys::ONI_ESUCCESS;
    }
    guard(|| {
        let mut ctx = unsafe { Box::from_raw(p as *mut Ctx) };
        // Leave the board's FIFO in reset, as upstream does.
        if let Some(ft) = ctx.ft.as_ref() {
            let _ = ft.gpio_write(1, 1);
        }
        ctx.close_device();
        sys::ONI_ESUCCESS
    })
}

extern "C" fn init(p: sys::oni_driver_ctx, host_idx: c_int) -> c_int {
    guard(|| unsafe { cast(p) }.init(host_idx))
}

extern "C" fn read_stream(p: sys::oni_driver_ctx, stream: sys::oni_read_stream_t, data: *mut c_void, size: usize) -> c_int {
    guard(|| {
        let ctx = unsafe { cast(p) };
        if data.is_null() {
            return sys::ONI_EINVALARG;
        }
        let dst = unsafe { std::slice::from_raw_parts_mut(data as *mut u8, size) };
        match stream {
            sys::ONI_READ_STREAM_SIGNAL => {
                if size >= ctx.signal.buf.len() {
                    return sys::ONI_EBUFFERSIZE;
                }
                while ctx.signal.len() < size {
                    ctx.update_control();
                    if ctx.sig_error {
                        ctx.sig_error = false;
                        return sys::ONI_ESEEKFAILURE;
                    }
                }
                ctx.signal.read(dst);
                size as c_int
            }
            sys::ONI_READ_STREAM_DATA => ctx.read_data(dst),
            _ => sys::ONI_EPATHINVALID,
        }
    })
}

extern "C" fn write_stream(p: sys::oni_driver_ctx, stream: sys::oni_write_stream_t, data: *const std::os::raw::c_char, size: usize) -> c_int {
    guard(|| {
        let ctx = unsafe { cast(p) };
        if stream != sys::ONI_WRITE_STREAM_DATA {
            return sys::ONI_EPATHINVALID;
        }
        let Some(ft) = ctx.ft.as_ref() else { return sys::ONI_EINVALSTATE };
        let src = unsafe { std::slice::from_raw_parts(data as *const u8, size) };
        let total = size & !3;
        let mut done = 0;
        while done < total {
            let n = (total - done).min(ctx.out_block as usize);
            match ft.write(Channel::Data, &src[done..done + n], 1000) {
                Ok(k) => done += k,
                Err(_) => return sys::ONI_EWRITEFAILURE,
            }
        }
        size as c_int
    })
}

extern "C" fn write_config(p: sys::oni_driver_ctx, reg: sys::oni_config_t, value: sys::oni_reg_val_t) -> c_int {
    guard(|| {
        let ctx = unsafe { cast(p) };
        // The steps of a device register access are latched and sent as one USB transfer.
        match reg {
            sys::ONI_CONFIG_DEV_IDX => {
                ctx.reg_op.dev_idx = value;
                return sys::ONI_ESUCCESS;
            }
            sys::ONI_CONFIG_REG_ADDR => {
                ctx.reg_op.dev_addr = value;
                return sys::ONI_ESUCCESS;
            }
            sys::ONI_CONFIG_REG_VALUE => {
                ctx.reg_op.value = value;
                return sys::ONI_ESUCCESS;
            }
            sys::ONI_CONFIG_RW => {
                ctx.reg_op.rw = value;
                return sys::ONI_ESUCCESS;
            }
            _ => {}
        }
        let mut buf = [0u8; 45];
        let mut i = 0;
        if reg == sys::ONI_CONFIG_TRIG {
            put_reg_write(&mut buf, &mut i, reg_offset(sys::ONI_CONFIG_DEV_IDX), ctx.reg_op.dev_idx);
            put_reg_write(&mut buf, &mut i, reg_offset(sys::ONI_CONFIG_REG_ADDR), ctx.reg_op.dev_addr);
            put_reg_write(&mut buf, &mut i, reg_offset(sys::ONI_CONFIG_RW), ctx.reg_op.rw);
            if ctx.reg_op.rw != 0 {
                put_reg_write(&mut buf, &mut i, reg_offset(sys::ONI_CONFIG_REG_VALUE), ctx.reg_op.value);
            }
            put_reg_write(&mut buf, &mut i, reg_offset(sys::ONI_CONFIG_TRIG), 1);
        } else {
            put_reg_write(&mut buf, &mut i, reg_offset(reg), value);
        }
        if reg == sys::ONI_CONFIG_RESET && value != 0 {
            ctx.stop_acq();
        }
        ctx.send_cmd(&buf[..9 * i])
    })
}

extern "C" fn read_config(p: sys::oni_driver_ctx, reg: sys::oni_config_t, value: *mut sys::oni_reg_val_t) -> c_int {
    guard(|| {
        let ctx = unsafe { cast(p) };
        let mut buf = [0u8; 5];
        buf[0] = CMD_READREG;
        buf[1..5].copy_from_slice(&reg_offset(reg).to_le_bytes());
        let res = ctx.send_cmd(&buf);
        if res != sys::ONI_ESUCCESS {
            return res;
        }
        while ctx.reg.len() < 4 {
            ctx.update_control();
            if ctx.sig_error {
                ctx.sig_error = false;
                return sys::ONI_ESEEKFAILURE;
            }
        }
        let mut v = [0u8; 4];
        ctx.reg.read(&mut v);
        unsafe { *value = u32::from_le_bytes(v) };
        sys::ONI_ESUCCESS
    })
}

extern "C" fn set_opt_callback(p: sys::oni_driver_ctx, option: c_int, value: *const c_void, len: usize) -> c_int {
    guard(|| {
        let ctx = unsafe { cast(p) };
        if value.is_null() || len < 4 {
            return sys::ONI_EINVALARG;
        }
        let v = unsafe { (value as *const u32).read_unaligned() };
        match option {
            sys::ONI_OPT_RUNNING => {
                if v == 0 {
                    ctx.stop_acq()
                } else {
                    ctx.start_acq()
                }
            }
            sys::ONI_OPT_RESETACQCOUNTER if v > 1 => ctx.start_acq(),
            sys::ONI_OPT_BLOCKREADSIZE => {
                if ctx.state == State::Running {
                    return sys::ONI_EINVALSTATE;
                }
                if v % 4 != 0 {
                    return sys::ONI_EINVALREADSIZE;
                }
                ctx.in_block = v;
            }
            _ => {}
        }
        sys::ONI_ESUCCESS
    })
}

extern "C" fn set_opt(_: sys::oni_driver_ctx, _: c_int, _: *const c_void, _: usize) -> c_int {
    sys::ONI_EINVALOPT
}

/// Option 0: the FT60x firmware version. (Upstream: D3XX driver version; option 1, the D3XX
/// library version, has no meaning here.)
extern "C" fn get_opt(p: sys::oni_driver_ctx, option: c_int, value: *mut c_void, len: *mut usize) -> c_int {
    guard(|| {
        let ctx = unsafe { cast(p) };
        let Some(ft) = ctx.ft.as_ref() else { return sys::ONI_EINVALSTATE };
        if option != 0 {
            return sys::ONI_EINVALOPT;
        }
        if value.is_null() || len.is_null() || unsafe { *len } < 4 {
            return sys::ONI_EBUFFERSIZE;
        }
        unsafe { (value as *mut u32).write_unaligned(ft.firmware) };
        sys::ONI_ESUCCESS
    })
}

/// The driver's name and version, read-only for the program's life.
struct Info(sys::oni_driver_info_t);
// SAFETY: it holds pointers to static string literals only, and is never written.
unsafe impl Sync for Info {}

static DRIVER_INFO: Info = Info(sys::oni_driver_info_t {
    name: c"kalila-ft600-usb".as_ptr(),
    major: 0,
    minor: 1,
    patch: 0,
    pre_release: std::ptr::null(),
});

extern "C" fn info() -> *const sys::oni_driver_info_t {
    &DRIVER_INFO.0
}

/// Makes liboni use this driver for `oni_create_ctx(DRIVER_NAME)`. Idempotent.
pub fn register() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let table = sys::oni_driver_t {
            handle: std::ptr::null_mut(),
            ctx: std::ptr::null_mut(),
            create_ctx: Some(create_ctx),
            destroy_ctx: Some(destroy_ctx),
            init: Some(init),
            read_stream: Some(read_stream),
            write_stream: Some(write_stream),
            read_config: Some(read_config),
            write_config: Some(write_config),
            set_opt_callback: Some(set_opt_callback),
            set_opt: Some(set_opt),
            get_opt: Some(get_opt),
            info: Some(info),
        };
        let name = std::ffi::CString::new(DRIVER_NAME).unwrap();
        // liboni keeps the name pointer, so it lives for the program.
        unsafe { sys::oni_register_builtin_driver(name.into_raw(), &table) };
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_stream_parser_splits_signals_and_register_replies() {
        let mut ctx = Ctx::new();
        // A register reply, then a signal packet ending in 0, then a reply split over two reads.
        ctx.fill_control_buffers(&[SIGRES_REG, 0x78, 0x56, 0x34, 0x12, SIGRES_SIGNAL, 5, 6, 7, 0, SIGRES_REG, 0xaa, 0xbb]);
        ctx.fill_control_buffers(&[0xcc, 0xdd, SIGRES_SIGNAL, 9]);
        ctx.fill_control_buffers(&[0]);
        assert_eq!(ctx.reg.len(), 8);
        let mut v = [0u8; 4];
        ctx.reg.read(&mut v);
        assert_eq!(u32::from_le_bytes(v), 0x1234_5678);
        ctx.reg.read(&mut v);
        assert_eq!(u32::from_le_bytes(v), 0xddcc_bbaa);
        let mut s = [0u8; 6];
        assert_eq!(ctx.signal.len(), 6);
        ctx.signal.read(&mut s);
        assert_eq!(s, [5, 6, 7, 0, 9, 0]);
        assert!(!ctx.sig_error);
    }

    #[test]
    fn ring_wraps() {
        let mut r = Ring::new(8);
        let mut out = [0u8; 5];
        for k in 0..4u8 {
            assert!(r.can_write(5));
            r.write(&[k, 1, 2, 3, 4]);
            r.read(&mut out);
            assert_eq!(out[0], k);
        }
        assert_eq!(r.len(), 0);
        assert!(!r.can_write(8));
    }

    #[test]
    fn a_register_access_is_latched_into_one_command() {
        let mut buf = [0u8; 45];
        let mut i = 0;
        put_reg_write(&mut buf, &mut i, reg_offset(sys::ONI_CONFIG_DEV_IDX), 0x1234);
        put_reg_write(&mut buf, &mut i, reg_offset(sys::ONI_CONFIG_TRIG), 1);
        assert_eq!(i, 2);
        assert_eq!(&buf[..9], &[CMD_WRITEREG, 0, 0, 0, 0, 0x34, 0x12, 0, 0]);
        assert_eq!(&buf[9..18], &[CMD_WRITEREG, 4, 0, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn liboni_finds_the_registered_driver_and_init_fails_cleanly_without_a_board() {
        // No board on the dev box (and none must be touched by tests): only meaningful
        // where no FT600 is attached, which is checked first.
        if !crate::usb::find(crate::chip::VENDOR_ID, &crate::chip::PRODUCT_IDS, USB_DESCRIPTION).is_empty() {
            return;
        }
        register();
        let name = std::ffi::CString::new(DRIVER_NAME).unwrap();
        let ctx = unsafe { sys::oni_create_ctx(name.as_ptr()) };
        assert!(!ctx.is_null(), "liboni did not find the built-in driver");
        let info = unsafe { &*sys::oni_get_driver_info(ctx) };
        assert_eq!(unsafe { std::ffi::CStr::from_ptr(info.name) }.to_str().unwrap(), "kalila-ft600-usb");
        let rc = unsafe { sys::oni_init_ctx(ctx, -1) };
        assert_eq!(rc, sys::ONI_EDEVID);
        unsafe { sys::oni_destroy_ctx(ctx) };
    }
}
