//! Minimal safe wrapper over liboni.

use oni_sys as sys;
use std::ffi::{CStr, CString};
use std::os::raw::{c_int, c_void};
use std::ptr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OniError(pub c_int);

impl std::fmt::Display for OniError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = unsafe { CStr::from_ptr(sys::oni_error_str(self.0)) };
        write!(f, "ONI error {}: {}", self.0, s.to_string_lossy())
    }
}
impl std::error::Error for OniError {}

pub type OniResult<T> = Result<T, OniError>;

fn check(rc: c_int) -> OniResult<c_int> {
    if rc < 0 { Err(OniError(rc)) } else { Ok(rc) }
}

pub struct Context {
    ctx: sys::oni_ctx,
}

// liboni contexts may be used from one thread at a time; we move it into the
// acquisition thread and never share it.
unsafe impl Send for Context {}

#[derive(Debug)]
pub enum OpenError {
    /// The driver shared library (libonidriver_ft600.so) could not be loaded.
    DriverNotFound,
    /// No board answered.
    NoBoard(OniError),
    /// Board gateware is too old for this ONI version.
    BadController,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::DriverNotFound => write!(
                f,
                "{DRIVER_FILE} not found (install it or set ONI_DRIVER_PATH; see README)"
            ),
            OpenError::NoBoard(e) => write!(f, "no ONI acquisition board found ({e})"),
            OpenError::BadController => write!(f, "board gateware is incompatible; update to >= 2.0.0"),
        }
    }
}
impl std::error::Error for OpenError {}

pub struct DriverInfo {
    pub name: String,
    pub version: (i32, i32, i32),
    pub pre_release: Option<String>,
}

impl Context {
    pub fn open(driver: &str, host_idx: i32) -> Result<Self, OpenError> {
        let name = CString::new(driver).unwrap();
        let ctx = unsafe { sys::oni_create_ctx(name.as_ptr()) };
        if ctx.is_null() {
            return Err(OpenError::DriverNotFound);
        }
        let rc = unsafe { sys::oni_init_ctx(ctx, host_idx) };
        if rc != sys::ONI_ESUCCESS {
            unsafe { sys::oni_destroy_ctx(ctx) };
            return Err(if rc == sys::ONI_EBADCONTROLLER { OpenError::BadController } else { OpenError::NoBoard(OniError(rc)) });
        }
        Ok(Self { ctx })
    }

    pub fn driver_info(&self) -> Option<DriverInfo> {
        let p = unsafe { sys::oni_get_driver_info(self.ctx) };
        if p.is_null() {
            return None;
        }
        let i = unsafe { &*p };
        let s = |c: *const std::os::raw::c_char| if c.is_null() { None } else { Some(unsafe { CStr::from_ptr(c) }.to_string_lossy().into_owned()) };
        Some(DriverInfo { name: s(i.name).unwrap_or_default(), version: (i.major, i.minor, i.patch), pre_release: s(i.pre_release) })
    }

    pub fn library_version() -> (i32, i32, i32) {
        let (mut a, mut b, mut c) = (0, 0, 0);
        unsafe { sys::oni_version(&mut a, &mut b, &mut c) };
        (a, b, c)
    }

    pub fn read_reg(&self, dev: u32, addr: u32) -> OniResult<u32> {
        let mut v = 0u32;
        check(unsafe { sys::oni_read_reg(self.ctx, dev, addr, &mut v) })?;
        Ok(v)
    }
    pub fn write_reg(&self, dev: u32, addr: u32, val: u32) -> OniResult<()> {
        check(unsafe { sys::oni_write_reg(self.ctx, dev, addr, val) }).map(|_| ())
    }
    pub fn write_reg_mask(&self, dev: u32, addr: u32, val: u32, mask: u32) -> OniResult<()> {
        let cur = self.read_reg(dev, addr)?;
        self.write_reg(dev, addr, (cur & !mask) | (val & mask))
    }

    pub fn get_opt_u32(&self, opt: c_int) -> OniResult<u32> {
        let mut v = 0u32;
        let mut sz = std::mem::size_of::<u32>();
        check(unsafe { sys::oni_get_opt(self.ctx, opt, &mut v as *mut u32 as *mut c_void, &mut sz) })?;
        Ok(v)
    }
    pub fn set_opt_u32(&self, opt: c_int, v: u32) -> OniResult<()> {
        check(unsafe { sys::oni_set_opt(self.ctx, opt, &v as *const u32 as *const c_void, std::mem::size_of::<u32>()) }).map(|_| ())
    }
    pub fn get_driver_opt_u32(&self, opt: c_int) -> OniResult<u32> {
        let mut v = 0u32;
        let mut sz = std::mem::size_of::<u32>();
        check(unsafe { sys::oni_get_driver_opt(self.ctx, opt, &mut v as *mut u32 as *mut c_void, &mut sz) })?;
        Ok(v)
    }

    pub fn device_table(&self) -> OniResult<Vec<sys::oni_device_t>> {
        let n = self.get_opt_u32(sys::ONI_OPT_NUMDEVICES)? as usize;
        let mut v = vec![sys::oni_device_t::default(); n];
        let mut sz = n * std::mem::size_of::<sys::oni_device_t>();
        check(unsafe { sys::oni_get_opt(self.ctx, sys::ONI_OPT_DEVICETABLE, v.as_mut_ptr() as *mut c_void, &mut sz) })?;
        Ok(v)
    }

    /// Blocking read of the next frame from the data stream.
    pub fn read_frame(&self) -> OniResult<Frame> {
        let mut f: *mut sys::oni_frame_t = ptr::null_mut();
        check(unsafe { sys::oni_read_frame(self.ctx, &mut f) })?;
        Ok(Frame { f })
    }

    pub fn write_frame(&self, dev: u32, data: &mut [u8]) -> OniResult<()> {
        let mut f: *mut sys::oni_frame_t = ptr::null_mut();
        let rc = unsafe { sys::oni_create_frame(self.ctx, &mut f, dev, data.as_mut_ptr() as *mut c_void, data.len()) };
        if rc <= 0 {
            return Err(OniError(rc.min(-1)));
        }
        let rc = unsafe { sys::oni_write_frame(self.ctx, f) };
        unsafe { sys::oni_destroy_frame(f) };
        check(rc).map(|_| ())
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { sys::oni_destroy_ctx(self.ctx) };
    }
}

/// RAII frame: memory is a view into liboni's shared read buffer and is released on drop.
pub struct Frame {
    f: *mut sys::oni_frame_t,
}

// A frame references its context's read buffer; it is only ever moved together
// with the owning `Context` (inside `OniBoard`), never shared.
unsafe impl Send for Frame {}

impl Frame {
    #[inline]
    pub fn time(&self) -> u64 {
        unsafe { (*self.f).time }
    }
    #[inline]
    pub fn dev_idx(&self) -> u32 {
        unsafe { (*self.f).dev_idx }
    }
    #[inline]
    pub fn data(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts((*self.f).data as *const u8, (*self.f).data_sz as usize) }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { sys::oni_destroy_frame(self.f) };
    }
}

/// Directories searched (in order) for `libonidriver_ft600.so` before the default
/// library path: `$ONI_DRIVER_PATH`, next to the executable, `../lib`, common prefixes and
/// existing Open Ephys GUI installs (so the driver they ship can be reused).
/// File name liboni's loader opens for the FT600 driver on this platform.
pub const DRIVER_FILE: &str = if cfg!(target_os = "macos") { "libonidriver_ft600.dylib" } else { "libonidriver_ft600.so" };

pub fn driver_search_candidates() -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let mut v: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("ONI_DRIVER_PATH") {
        v.extend(std::env::split_paths(&p));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(d) = exe.parent()
    {
        v.push(d.to_path_buf());
        v.push(d.join("../lib"));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(h) = &home {
        v.push(h.join(".local/lib"));
    }
    v.push("/usr/local/lib".into());
    // Open Ephys GUI installs keep plugin shared libraries in "shared*" folders.
    let mut roots: Vec<PathBuf> = vec!["/usr/local/bin/open-ephys-gui".into(), "/usr/local/bin/open-ephys".into(), "/opt/open-ephys".into()];
    if let Some(h) = &home {
        roots.push(h.join(".local/share/open-ephys"));
        roots.push(h.join(".config/open-ephys"));
    }
    for r in roots {
        if let Ok(rd) = std::fs::read_dir(&r) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("shared")) {
                    v.push(p);
                }
            }
            v.push(r.join("shared"));
        }
    }
    v
}

/// Point liboni at the first directory containing the FT600 driver. Returns it, if any.
pub fn configure_driver_search() -> Option<std::path::PathBuf> {
    let found = driver_search_candidates().into_iter().find(|d| d.join(DRIVER_FILE).exists())?;
    let c = CString::new(found.to_string_lossy().as_bytes()).ok()?;
    unsafe { sys::oni_set_driver_search_dir(c.as_ptr()) };
    Some(found)
}
