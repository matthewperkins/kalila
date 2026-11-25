//! SIGTERM and SIGINT as a request to stop cleanly.
//!
//! A logout, `systemctl stop`, `kill` or Ctrl+C would otherwise end the process without the
//! normal exit path, leaving a recording with stale `.npy` headers and its buffered tail
//! lost. [`install`] makes those signals set a flag; the main loop (the GUI's
//! `App::logic`, the headless loop) polls [`requested`] and stops the recording and
//! acquisition as it would on a normal exit.
//!
//! The handler does only async-signal-safe things: an atomic store, and `signal(2)`
//! (restoring the default action). A *second* signal therefore kills a process that is hung
//! in its clean-up, as the default action would.

use std::sync::atomic::{AtomicI32, Ordering};

/// The first signal received, 0 for none.
static RECEIVED: AtomicI32 = AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn handler(sig: libc::c_int) {
    // Only the first caller records; the defaults are restored either way so that the next
    // signal (of either kind) is not caught.
    let _ = RECEIVED.compare_exchange(0, sig, Ordering::SeqCst, Ordering::SeqCst);
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
        libc::signal(libc::SIGINT, libc::SIG_DFL);
    }
}

/// Installs the SIGTERM and SIGINT handlers (`SA_RESTART`, so reads and sleeps in other
/// code are not interrupted with EINTR). Call once, early in `main`.
#[cfg(unix)]
pub fn install() {
    for sig in [libc::SIGTERM, libc::SIGINT] {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = handler as *const () as libc::sighandler_t;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
}

#[cfg(not(unix))]
pub fn install() {}

/// True once SIGTERM or SIGINT has arrived.
pub fn requested() -> bool {
    RECEIVED.load(Ordering::SeqCst) != 0
}

/// Name of the signal that arrived, for the log.
pub fn name() -> &'static str {
    #[cfg(unix)]
    match RECEIVED.load(Ordering::SeqCst) {
        0 => "none",
        s if s == libc::SIGTERM => "SIGTERM",
        s if s == libc::SIGINT => "SIGINT",
        _ => "signal",
    }
    #[cfg(not(unix))]
    "none"
}
