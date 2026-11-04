//! Raw bindings to liboni 4.x (vendored in `liboni/`, MIT licensed).
//!
//! liboni loads the hardware driver (`libonidriver_ft600.so`) at runtime with
//! `dlopen`, so this crate builds and links without FTDI's proprietary library.
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
pub const ONI_EBADCONTROLLER: c_int = -29;

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
    pub fn oni_create_frame(ctx: oni_ctx, frame: *mut *mut oni_frame_t, dev: oni_dev_idx_t, data: *mut c_void, data_sz: usize) -> c_int;
    pub fn oni_write_frame(ctx: oni_ctx, frame: *const oni_frame_t) -> c_int;
    pub fn oni_destroy_frame(frame: *mut oni_frame_t);
    pub fn oni_version(major: *mut c_int, minor: *mut c_int, patch: *mut c_int);
    pub fn oni_get_driver_info(ctx: oni_ctx) -> *const oni_driver_info_t;
    pub fn oni_error_str(err: c_int) -> *const c_char;
    /// oers patch to the vendored loader: directory tried before the default dlopen path.
    pub fn oni_set_driver_search_dir(dir: *const c_char);
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
