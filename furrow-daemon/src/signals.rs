//! Termination signals as a flag the serving loop polls.

use std::sync::atomic::{AtomicBool, Ordering};

static RECEIVED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn note(_: libc::c_int) {
    RECEIVED.store(true, Ordering::SeqCst);
}

/// From now on SIGTERM and SIGINT set [`received`] instead of killing the
/// process, so the daemon can finish what it is doing.
#[cfg(unix)]
pub fn install() {
    let handler = note as extern "C" fn(libc::c_int) as libc::sighandler_t;
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler only stores to an atomic.
        unsafe { libc::signal(signal, handler) };
    }
}

pub fn received() -> bool {
    RECEIVED.load(Ordering::SeqCst)
}

// Windows sends console-close notifications on a separate system thread. Keep
// that callback alive until the serving loop has closed repositories, subject
// to the OS console-close deadline.
#[cfg(windows)]
static CLOSED: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
pub fn finished() {
    CLOSED.store(true, Ordering::SeqCst);
}

#[cfg(windows)]
unsafe extern "system" fn console_event(event: u32) -> i32 {
    if !matches!(event, 0 | 1 | 2 | 5 | 6) {
        return 0;
    }
    RECEIVED.store(true, Ordering::SeqCst);
    if event >= 2 {
        for _ in 0..90 {
            if CLOSED.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    1
}

#[cfg(windows)]
pub fn install() {
    #[link(name = "kernel32")]
    extern "system" {
        fn SetConsoleCtrlHandler(
            handler: Option<unsafe extern "system" fn(u32) -> i32>,
            add: i32,
        ) -> i32;
    }
    // SAFETY: the handler has static lifetime and shares only atomic flags.
    unsafe {
        SetConsoleCtrlHandler(Some(console_event), 1);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn w2_console_control_events_request_a_drained_shutdown() {
        // W2: Ctrl-C, Ctrl-Break and console-close events all request the
        // serving loop's normal shutdown; unrelated events remain unhandled.
        finished();
        for event in [0, 1, 2, 5, 6] {
            RECEIVED.store(false, Ordering::SeqCst);
            assert_eq!(unsafe { console_event(event) }, 1);
            assert!(received());
        }
        RECEIVED.store(false, Ordering::SeqCst);
        assert_eq!(unsafe { console_event(99) }, 0);
        assert!(!received());
    }
}
