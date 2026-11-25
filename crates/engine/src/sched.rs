//! Thread scheduling and power, per platform. Linux: SCHED_FIFO for acquisition, SCHED_IDLE
//! for the overview builder. macOS: a Mach time-constraint policy for acquisition and the
//! audio feeder, the background QoS class for the builder, and a power assertion that keeps
//! the machine awake while acquiring.

/// Raise the calling (acquisition) thread. True only when it got real-time scheduling.
pub fn set_realtime() -> bool {
    use thread_priority::*;
    #[cfg(target_os = "linux")]
    {
        let p = ThreadPriority::from_posix(ScheduleParams { sched_priority: 70 });
        if set_thread_priority_and_policy(thread_native_id(), p, ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo)).is_ok() {
            return true;
        }
    }
    #[cfg(target_os = "macos")]
    if mac::time_constraint() {
        return true;
    }
    let _ = set_current_thread_priority(ThreadPriority::Max);
    false
}

/// Make the calling thread yield to everything else on the machine.
pub fn set_idle() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        use thread_priority::*;
        set_thread_priority_and_policy(thread_native_id(), ThreadPriority::Min, ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Idle)).map_err(|e| format!("{e:?}"))
    }
    #[cfg(target_os = "macos")]
    {
        // Background QoS also throttles this thread's disk reads, which suits the backfill.
        match unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0) } {
            0 => Ok(()),
            e => Err(format!("pthread_set_qos_class_self_np: {e}")),
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Min).map_err(|e| format!("{e:?}"))
    }
}

/// The audio feeder's scheduling. Linux: the highest ordinary priority, as before. macOS:
/// the acquisition thread's time-constraint policy (in dimna's port an ordinary feeder
/// thread woke 5 to 58 ms late while the screen was busy; with this policy it did not).
pub fn set_audio_feed() {
    #[cfg(target_os = "macos")]
    if mac::time_constraint() {
        return;
    }
    let _ = thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max);
}

/// Held while acquiring: the machine does not go to sleep for being idle (macOS; a closed
/// lid or the user choosing Sleep still do it). Nothing on Linux, where the rig never sleeps.
pub struct KeepAwake {
    #[cfg(target_os = "macos")]
    id: u32,
}

impl KeepAwake {
    pub fn new() -> Self {
        #[cfg(target_os = "macos")]
        {
            let id = mac::keep_awake();
            if id == 0 {
                log::warn!("could not keep the machine awake (power assertion refused): it may sleep when idle");
            }
            Self { id }
        }
        #[cfg(not(target_os = "macos"))]
        Self {}
    }
}

impl Default for KeepAwake {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        mac::allow_sleep(self.id);
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{c_char, c_int, c_uint, c_void};

    /// `thread_time_constraint_policy` of <mach/thread_policy.h>, in Mach time units.
    #[repr(C)]
    struct TimeConstraint {
        period: u32,
        computation: u32,
        constraint: u32,
        preemptible: c_int,
    }
    const THREAD_TIME_CONSTRAINT_POLICY: c_uint = 2;
    const COUNT: c_uint = (size_of::<TimeConstraint>() / size_of::<c_uint>()) as c_uint;

    #[repr(C)]
    struct Timebase {
        numer: u32,
        denom: u32,
    }

    unsafe extern "C" {
        fn mach_timebase_info(info: *mut Timebase) -> c_int;
        fn thread_policy_set(thread: libc::mach_port_t, flavor: c_uint, policy: *mut c_void, count: c_uint) -> c_int;
    }

    // The figures are dimna's (os_darwin.zig), found on the same Mac and board: a block
    // every 1 to 2 ms, a fraction of a millisecond of work on it. The kernel raises a
    // computation below half the constraint to half of it, so 0.5 ms is what the thread
    // would get anyway; preemptible, since most of a block's time is spent waiting.
    const PERIOD_US: u64 = 1000;
    const COMPUTATION_US: u64 = 500;
    const CONSTRAINT_US: u64 = 1000;

    fn mach_units(us: u64) -> u32 {
        let mut tb = Timebase { numer: 0, denom: 0 };
        unsafe { mach_timebase_info(&mut tb) };
        if tb.numer == 0 {
            return (us * 1000) as u32;
        }
        (us * 1000 * tb.denom as u64 / tb.numer as u64) as u32
    }

    /// User-interactive QoS (off the efficiency cores) and the time-constraint policy for
    /// the calling thread. True when the policy was set.
    pub fn time_constraint() -> bool {
        unsafe {
            if libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0) != 0 {
                return false;
            }
            let mut p = TimeConstraint { period: mach_units(PERIOD_US), computation: mach_units(COMPUTATION_US), constraint: mach_units(CONSTRAINT_US), preemptible: 1 };
            thread_policy_set(libc::pthread_mach_thread_np(libc::pthread_self()), THREAD_TIME_CONSTRAINT_POLICY, (&raw mut p).cast(), COUNT) == 0
        }
    }

    // IOKit's power assertions (<IOKit/pwr_mgt/IOPMLib.h>).
    const UTF8: u32 = 0x0800_0100;
    const LEVEL_ON: u32 = 255;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, encoding: u32) -> *const c_void;
        fn CFRelease(cf: *const c_void);
    }
    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOPMAssertionCreateWithName(kind: *const c_void, level: u32, name: *const c_void, id: *mut u32) -> c_int;
        fn IOPMAssertionRelease(id: u32) -> c_int;
    }

    /// `PreventUserIdleSystemSleep`; the system drops it by itself when the process ends.
    /// Returns the assertion's id, 0 when it was refused.
    pub fn keep_awake() -> u32 {
        unsafe {
            let kind = CFStringCreateWithCString(std::ptr::null(), c"PreventUserIdleSystemSleep".as_ptr(), UTF8);
            let name = CFStringCreateWithCString(std::ptr::null(), c"kalila is acquiring".as_ptr(), UTF8);
            let mut id = 0u32;
            if !kind.is_null() && !name.is_null() && IOPMAssertionCreateWithName(kind, LEVEL_ON, name, &mut id) != 0 {
                id = 0;
            }
            for s in [kind, name] {
                if !s.is_null() {
                    CFRelease(s);
                }
            }
            id
        }
    }

    pub fn allow_sleep(id: u32) {
        if id != 0 {
            unsafe { IOPMAssertionRelease(id) };
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn the_time_constraint_policy_is_accepted() {
        std::thread::spawn(|| assert!(super::mac::time_constraint())).join().unwrap();
    }

    #[test]
    fn the_power_assertion_is_given_and_released() {
        let id = super::mac::keep_awake();
        assert_ne!(id, 0);
        super::mac::allow_sleep(id);
    }
}
