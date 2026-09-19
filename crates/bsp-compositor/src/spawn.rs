//! Starting child processes without the compositor's signal mask.
//!
//! The event loop blocks `SIGINT`, `SIGHUP`, `SIGTERM` and `SIGUSR1` so it can
//! read them as events, and a child inherits that mask across `fork` and
//! `exec`: a `waybar` started from `bspwmrc` could then not be stopped with
//! `pkill` nor reloaded with `SIGUSR2`-style signals, and the dormant
//! `Xwayland` never received `SIGTERM`.

use std::os::unix::process::CommandExt;
use std::process::Command;

/// Clears the calling thread's signal mask. Async-signal-safe.
pub fn unblock_signals() {
    // SAFETY: `sigemptyset` initialises `set` and `pthread_sigmask` only reads
    // it; neither allocates, so this is safe between `fork` and `exec`.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
    }
}

/// Makes `command` start with an empty signal mask.
pub fn clean_signal_mask(command: &mut Command) -> &mut Command {
    // SAFETY: the closure only calls `unblock_signals`, which is
    // async-signal-safe and touches no memory shared with the parent.
    unsafe { command.pre_exec(|| { unblock_signals(); Ok(()) }) }
}
