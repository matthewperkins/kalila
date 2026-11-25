//! Thread scheduling, per platform. Linux: SCHED_FIFO for acquisition, SCHED_IDLE for the
//! overview builder. macOS: QoS classes (user-interactive / background).

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
    {
        // Not real time (that would be a Mach time-constraint policy), so this reports false.
        if unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0) } == 0 {
            return false;
        }
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
