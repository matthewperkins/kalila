//! macOS: when a frame of live traces reached the screen, as the system reports it.
//!
//! The "display" figure elsewhere stops at the hand-over to the GPU. On Linux with tearing
//! that is nearly the whole story; on macOS the compositor adds most of the delay (5 to
//! 30 ms, by mode), so the figure has to come from the frame's own drawable:
//! `MTLDrawable.presentedTime`.
//!
//! eframe owns the surface, so the drawable is not reachable through wgpu. Instead
//! `-[CAMetalLayer nextDrawable]` is replaced by a wrapper that calls the original and, when
//! a trace window has just drawn (`frame_shows`), adds a presented handler to the drawable it
//! returns. That is the only thing changed: the drawable, its timing and wgpu's use of it
//! are as before. A window's UI runs just before its surface is acquired, on the same
//! thread, so the drawable taken next is that window's.
//!
//! The figure is the system's own report, not a photodiode's.

use block2::RcBlock;
use objc2::runtime::{AnyClass, AnyObject, Imp, Sel};
use objc2::{msg_send, sel};
use parking_lot::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[link(name = "QuartzCore", kind = "framework")]
unsafe extern "C" {
    /// Seconds on the clock `presentedTime` is given in.
    fn CACurrentMediaTime() -> f64;
}

type NextDrawable = unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> *mut AnyObject;

static ORIGINAL: OnceLock<Imp> = OnceLock::new();
/// When the newest sample in the frame about to be shown was acquired.
static PENDING: Mutex<Option<Instant>> = Mutex::new(None);
static STATS: Mutex<Stats> = Mutex::new(Stats { mean_ms: 0.0, max_ms: 0.0, frames: 0, dropped: 0, since: None });

struct Stats {
    mean_ms: f32,
    max_ms: f32,
    frames: u32,
    /// Frames the system never showed (presented time 0).
    dropped: u32,
    since: Option<Instant>,
}

/// Replaces `nextDrawable` once, before any window exists. Without it (the class or method
/// missing) nothing is measured and `latency` stays None.
pub fn install() {
    if ORIGINAL.get().is_some() {
        return;
    }
    let Some(method) = AnyClass::get(c"CAMetalLayer").and_then(|c| c.instance_method(sel!(nextDrawable))) else {
        log::warn!("CAMetalLayer.nextDrawable not found: no time-to-screen figure");
        return;
    };
    // The original is stored before the wrapper can be called.
    let _ = ORIGINAL.set(method.implementation());
    let wrapper: NextDrawable = next_drawable;
    unsafe { method.set_implementation(std::mem::transmute::<NextDrawable, Imp>(wrapper)) };
}

unsafe extern "C-unwind" fn next_drawable(layer: *mut AnyObject, cmd: Sel) -> *mut AnyObject {
    let original: NextDrawable = unsafe { std::mem::transmute::<Imp, NextDrawable>(*ORIGINAL.get().unwrap()) };
    let drawable = unsafe { original(layer, cmd) };
    let Some(acquired) = PENDING.lock().take() else { return drawable };
    if drawable.is_null() {
        return drawable;
    }
    let handler = RcBlock::new(move |d: *mut AnyObject| {
        let presented: f64 = unsafe { msg_send![d, presentedTime] };
        let mut st = STATS.lock();
        if presented == 0.0 {
            st.dropped += 1;
            return;
        }
        // The handler runs some time after the frame was shown: take that time off.
        let since_shown = (unsafe { CACurrentMediaTime() } - presented).max(0.0);
        let ms = (acquired.elapsed().as_secs_f64() - since_shown).max(0.0) as f32 * 1e3;
        st.mean_ms = if st.frames == 0 { ms } else { 0.9 * st.mean_ms + 0.1 * ms };
        st.max_ms = st.max_ms.max(ms);
        st.frames += 1;
    });
    let _: () = unsafe { msg_send![drawable, addPresentedHandler: &*handler] };
    drawable
}

/// A trace window has drawn live traces whose newest sample was acquired at `acquired`; its
/// frame is the next one handed to the system.
pub fn frame_shows(acquired: Instant) {
    if ORIGINAL.get().is_some() {
        *PENDING.lock() = Some(acquired);
    }
}

/// Sample acquired → shown: a running mean and the worst of the last two seconds, in ms.
/// None until a frame has been reported shown.
pub fn latency() -> Option<(f32, f32)> {
    let mut st = STATS.lock();
    if st.frames == 0 {
        return None;
    }
    let out = (st.mean_ms, st.max_ms);
    let since = *st.since.get_or_insert_with(Instant::now);
    if since.elapsed() > Duration::from_secs(2) {
        log::debug!("to screen: mean {:.1} ms, worst {:.1} ms, {} frames not shown", st.mean_ms, st.max_ms, st.dropped);
        st.since = Some(Instant::now());
        st.max_ms = 0.0;
        st.dropped = 0;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wrapper_passes_next_drawable_through() {
        install();
        install(); // a second call changes nothing
        assert!(ORIGINAL.get().is_some());
        unsafe {
            // A layer with no device and no window: the original answers (with no drawable),
            // through the wrapper, with and without a frame announced.
            let layer: *mut AnyObject = msg_send![AnyClass::get(c"CAMetalLayer").unwrap(), new];
            assert!(!layer.is_null());
            let _: *mut AnyObject = msg_send![layer, nextDrawable];
            frame_shows(Instant::now());
            let d: *mut AnyObject = msg_send![layer, nextDrawable];
            if d.is_null() {
                // Nothing was shown, so nothing is reported.
                assert!(latency().is_none());
            }
            let _: () = msg_send![layer, release];
        }
    }
}
