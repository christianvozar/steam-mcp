//! Minimal termination-signal handling (SIGINT, SIGTERM) without external
//! crates. The handler only stores the signal number in an atomic, which is
//! async-signal-safe; a normal thread polls [`received`] and performs the
//! actual shutdown.

use std::sync::atomic::{AtomicI32, Ordering};

static RECEIVED: AtomicI32 = AtomicI32::new(0);

pub const SIGINT: i32 = 2;
pub const SIGTERM: i32 = 15;

#[cfg(unix)]
mod imp {
    use super::*;

    unsafe extern "C" {
        /// `sighandler_t signal(int signum, sighandler_t handler)` from libc,
        /// which std already links on every Unix target.
        fn signal(signum: i32, handler: usize) -> usize;
    }

    extern "C" fn on_signal(sig: i32) {
        RECEIVED.store(sig, Ordering::SeqCst);
    }

    pub fn install() {
        let handler = on_signal as extern "C" fn(i32) as usize;
        // SAFETY: `on_signal` only performs an atomic store, which is
        // async-signal-safe, and the signal numbers are valid on Linux and macOS.
        unsafe {
            signal(SIGINT, handler);
            signal(SIGTERM, handler);
        }
    }
}

#[cfg(not(unix))]
mod imp {
    pub fn install() {}
}

/// Installs handlers for SIGINT and SIGTERM.
pub fn install() {
    imp::install();
}

/// The most recent termination signal received, if any.
pub fn received() -> Option<i32> {
    match RECEIVED.load(Ordering::SeqCst) {
        0 => None,
        s => Some(s),
    }
}

/// Clears the recorded signal (used to detect a second signal).
pub fn reset() {
    RECEIVED.store(0, Ordering::SeqCst);
}

pub fn name(sig: i32) -> &'static str {
    match sig {
        SIGINT => "SIGINT",
        SIGTERM => "SIGTERM",
        _ => "signal",
    }
}
