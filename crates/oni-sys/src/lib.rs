//! Raw bindings to liboni 4.x (vendored in `liboni/`, MIT licensed).
//!
//! liboni loads a hardware driver (`libonidriver_ft600.so`) at runtime with `dlopen`, so this
//! crate builds and links without FTDI's proprietary library. A driver compiled into the
//! program (the `ft600` crate) is registered with `oni_register_builtin_driver` instead.
#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int, c_void};

pub type oni_size_t = u32;
pub type oni_dev_id_t = u32;
pub type oni_dev_idx_t = u32;
pub type oni_reg_addr_t = u32;
pub type oni_reg_val_t = u32;
pub type oni_fifo_dat_t = u32;
pub type oni_fifo_time_t = u64;

#[repr(C)]
pub struct oni_ctx_impl {
    _private: [u8; 0],
}
pub type oni_ctx = *mut oni_ctx_impl;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct oni_device_t {
    pub idx: oni_size_t,
    pub id: oni_dev_id_t,
    pub version: oni_size_t,
    pub read_size: oni_size_t,
    pub write_size: oni_size_t,
}

#[repr(C)]
pub struct oni_frame_t {
    pub time: oni_fifo_time_t,
    pub dev_idx: oni_fifo_dat_t,
    pub data_sz: oni_fifo_dat_t,
    pub data: *mut c_char,
}

#[repr(C)]
pub struct oni_driver_info_t {
    pub name: *const c_char,
    pub major: c_int,
    pub minor: c_int,
    pub patch: c_int,
    pub pre_release: *const c_char,
}

// Context options
pub const ONI_OPT_DEVICETABLE: c_int = 0;
pub const ONI_OPT_NUMDEVICES: c_int = 1;
pub const ONI_OPT_RUNNING: c_int = 2;
pub const ONI_OPT_RESET: c_int = 3;
pub const ONI_OPT_SYSCLKHZ: c_int = 4;
pub const ONI_OPT_ACQCLKHZ: c_int = 5;
pub const ONI_OPT_RESETACQCOUNTER: c_int = 6;
pub const ONI_OPT_HWADDRESS: c_int = 7;
pub const ONI_OPT_MAXREADFRAMESIZE: c_int = 8;
pub const ONI_OPT_MAXWRITEFRAMESIZE: c_int = 9;
pub const ONI_OPT_BLOCKREADSIZE: c_int = 10;
pub const ONI_OPT_BLOCKWRITESIZE: c_int = 11;

// Errors
pub const ONI_ESUCCESS: c_int = 0;
pub const ONI_EPATHINVALID: c_int = -1;
pub const ONI_EDEVID: c_int = -2;
pub const ONI_EDEVIDX: c_int = -3;
pub const ONI_EREADFAILURE: c_int = -5;
pub const ONI_EWRITEFAILURE: c_int = -6;
pub const ONI_ESEEKFAILURE: c_int = -8;
pub const ONI_EINVALSTATE: c_int = -9;
pub const ONI_EINVALOPT: c_int = -10;
pub const ONI_EINVALARG: c_int = -11;
pub const ONI_EBUFFERSIZE: c_int = -14;
pub const ONI_EINVALREADSIZE: c_int = -20;
pub const ONI_EINIT: c_int = -22;
pub const ONI_EBADCONTROLLER: c_int = -29;

// ---------------------------------------------------------------- driver side (onidriver.h)

/// `oni_config_t`: the controller's configuration registers.
pub type oni_config_t = c_int;
pub const ONI_CONFIG_DEV_IDX: oni_config_t = 0;
pub const ONI_CONFIG_REG_ADDR: oni_config_t = 1;
pub const ONI_CONFIG_REG_VALUE: oni_config_t = 2;
pub const ONI_CONFIG_RW: oni_config_t = 3;
pub const ONI_CONFIG_TRIG: oni_config_t = 4;
pub const ONI_CONFIG_RUNNING: oni_config_t = 5;
pub const ONI_CONFIG_RESET: oni_config_t = 6;
pub const ONI_CONFIG_SYSCLKHZ: oni_config_t = 7;
pub const ONI_CONFIG_ACQCLKHZ: oni_config_t = 8;
pub const ONI_CONFIG_RESETACQCOUNTER: oni_config_t = 9;
pub const ONI_CONFIG_HWADDRESS: oni_config_t = 10;

pub type oni_read_stream_t = c_int;
pub const ONI_READ_STREAM_DATA: oni_read_stream_t = 0;
pub const ONI_READ_STREAM_SIGNAL: oni_read_stream_t = 1;
pub type oni_write_stream_t = c_int;
pub const ONI_WRITE_STREAM_DATA: oni_write_stream_t = 0;

pub type oni_driver_ctx = *mut c_void;

/// `struct oni_driver` (onidriverloader.h): a driver's function table.
#[repr(C)]
pub struct oni_driver_t {
    pub handle: *mut c_void,
    pub ctx: oni_driver_ctx,
    pub create_ctx: Option<extern "C" fn() -> oni_driver_ctx>,
    pub destroy_ctx: Option<extern "C" fn(oni_driver_ctx) -> c_int>,
    pub init: Option<extern "C" fn(oni_driver_ctx, c_int) -> c_int>,
    pub read_stream: Option<extern "C" fn(oni_driver_ctx, oni_read_stream_t, *mut c_void, usize) -> c_int>,
    pub write_stream: Option<extern "C" fn(oni_driver_ctx, oni_write_stream_t, *const c_char, usize) -> c_int>,
    pub read_config: Option<extern "C" fn(oni_driver_ctx, oni_config_t, *mut oni_reg_val_t) -> c_int>,
    pub write_config: Option<extern "C" fn(oni_driver_ctx, oni_config_t, oni_reg_val_t) -> c_int>,
    pub set_opt_callback: Option<extern "C" fn(oni_driver_ctx, c_int, *const c_void, usize) -> c_int>,
    pub set_opt: Option<extern "C" fn(oni_driver_ctx, c_int, *const c_void, usize) -> c_int>,
    pub get_opt: Option<extern "C" fn(oni_driver_ctx, c_int, *mut c_void, *mut usize) -> c_int>,
    pub info: Option<extern "C" fn() -> *const oni_driver_info_t>,
}

unsafe extern "C" {
    pub fn oni_create_ctx(drv_name: *const c_char) -> oni_ctx;
    pub fn oni_init_ctx(ctx: oni_ctx, host_idx: c_int) -> c_int;
    pub fn oni_destroy_ctx(ctx: oni_ctx) -> c_int;
    pub fn oni_get_opt(ctx: oni_ctx, opt: c_int, value: *mut c_void, size: *mut usize) -> c_int;
    pub fn oni_set_opt(ctx: oni_ctx, opt: c_int, value: *const c_void, size: usize) -> c_int;
    pub fn oni_get_driver_opt(ctx: oni_ctx, opt: c_int, value: *mut c_void, size: *mut usize) -> c_int;
    pub fn oni_set_driver_opt(ctx: oni_ctx, opt: c_int, value: *const c_void, size: usize) -> c_int;
    pub fn oni_read_reg(ctx: oni_ctx, dev: oni_dev_idx_t, addr: oni_reg_addr_t, value: *mut oni_reg_val_t) -> c_int;
    pub fn oni_write_reg(ctx: oni_ctx, dev: oni_dev_idx_t, addr: oni_reg_addr_t, value: oni_reg_val_t) -> c_int;
    pub fn oni_read_frame(ctx: oni_ctx, frame: *mut *mut oni_frame_t) -> c_int;
    pub fn oni_create_frame(
        ctx: oni_ctx,
        frame: *mut *mut oni_frame_t,
        dev: oni_dev_idx_t,
        data: *mut c_void,
        data_sz: usize,
    ) -> c_int;
    pub fn oni_write_frame(ctx: oni_ctx, frame: *const oni_frame_t) -> c_int;
    pub fn oni_destroy_frame(frame: *mut oni_frame_t);
    pub fn oni_version(major: *mut c_int, minor: *mut c_int, patch: *mut c_int);
    pub fn oni_get_driver_info(ctx: oni_ctx) -> *const oni_driver_info_t;
    pub fn oni_error_str(err: c_int) -> *const c_char;
    /// kalila patch to the vendored loader: directory tried before the default dlopen path.
    pub fn oni_set_driver_search_dir(dir: *const c_char);
    /// kalila patch (as dimna's): when `name` is asked for, `functions` is used instead of
    /// loading `libonidriver_<name>.so`. `name` must stay valid for the program's life.
    pub fn oni_register_builtin_driver(name: *const c_char, functions: *const oni_driver_t);
}

#[cfg(test)]
mod tests {
    #[test]
    fn links_and_reports_version() {
        let (mut a, mut b, mut c) = (0, 0, 0);
        unsafe { super::oni_version(&mut a, &mut b, &mut c) };
        assert_eq!(a, 4);
    }
}
