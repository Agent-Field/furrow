//! Crash points for the crash-safety tests. A build with the `fault-injection`
//! feature kills the process, without unwinding, at the point named by
//! `FURROW_FAULT`; every other build compiles the points away.

#[cfg(feature = "fault-injection")]
pub fn point(name: &str) {
    if std::env::var("FURROW_FAULT").is_ok_and(|armed| armed == name) {
        // C28 requires death without cleanup. SIGKILL also bypasses host
        // crash-report collection, which can otherwise stall every fault test.
        #[cfg(unix)]
        // SAFETY: this process owns its PID; the fallback exits without atexit handlers.
        unsafe {
            libc::kill(libc::getpid(), libc::SIGKILL);
            libc::_exit(137);
        }
        #[cfg(windows)]
        std::process::abort();
    }
}

#[cfg(not(feature = "fault-injection"))]
#[inline(always)]
pub fn point(_name: &str) {}
