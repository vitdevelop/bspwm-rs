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

/// Runs `external_rules_command` for a window that is about to be managed, without
/// waiting for it: the command's standard output is read as it arrives (a
/// calloop source), and `done` runs with the effects it printed once the command
/// closes it, or with `None` if it cannot be started or does not answer within
/// [`EXTERNAL_RULES_TIMEOUT`] (it is killed then). The command may call `bspc`
/// (most do), which only works because the compositor is free to answer.
///
/// It gets bspwm's four arguments: the window id (decimal; 0 for a Wayland
/// window, which has none), its class, its instance and the consequence so far as
/// `key=value` words.
///
/// bspwm: `src/rule.c` `schedule_rules()`, `print_rule_consequence()`; the window
/// stays pending until the answer (`pending_rule_t`, `src/bspwm.c`).
pub fn start_external_rules<Bd: crate::state::Backend + 'static>(
    state: &mut crate::state::State<Bd>,
    command: &str,
    wid: u32,
    class: &str,
    instance: &str,
    consequence: &bsp_core::rules::RuleConsequence,
    done: ExternalRulesDone<Bd>,
) {
    use smithay::reexports::calloop::generic::Generic;
    use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
    use smithay::reexports::calloop::{Interest, Mode, PostAction};
    use std::cell::{Cell, RefCell};
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::process::Stdio;
    use std::rc::Rc;

    let mut child = match clean_signal_mask(Command::new(command).args([&wid.to_string(), class, instance, &describe_consequence(consequence)]))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(command, "cannot run external_rules_command: {err}");
            done(state, None);
            return;
        }
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        done(state, None);
        return;
    };
    // SAFETY: `fcntl(F_GETFL/F_SETFL)` on the pipe's own, open descriptor only
    // sets `O_NONBLOCK`; no memory is touched.
    unsafe {
        let fd = stdout.as_raw_fd();
        let flags = libc::fcntl(fd, libc::F_GETFL);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    // Whichever of "the command finished" and "it took too long" comes first
    // takes `done` and the child; the other finds nothing left to do.
    let done = Rc::new(RefCell::new(Some(done)));
    let child = Rc::new(RefCell::new(Some(child)));
    let token = Rc::new(Cell::new(None));

    let (done_r, child_r) = (done.clone(), child.clone());
    let mut output = Vec::new();
    let source = state.handle.insert_source(Generic::new(stdout, Interest::READ, Mode::Level), move |_, pipe, state| {
        // SAFETY: the pipe is only read here, never dropped through this reference.
        let pipe = unsafe { pipe.get_mut() };
        let mut buf = [0u8; 4096];
        let finished = loop {
            match pipe.read(&mut buf) {
                Ok(0) => break Some(true),
                Ok(n) => output.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break None,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break Some(false),
            }
        };
        let Some(ok) = finished else {
            return Ok(PostAction::Continue);
        };
        if let Some(mut c) = child_r.borrow_mut().take() {
            let _ = c.wait();
        }
        if let Some(done) = done_r.borrow_mut().take() {
            let effects = ok.then(|| bsp_ipc::command::parse_rule_effects(&String::from_utf8_lossy(&output)));
            done(state, effects);
        }
        Ok(PostAction::Remove)
    });
    match source {
        Ok(t) => token.set(Some(t)),
        Err(err) => {
            tracing::warn!(command, "cannot watch external_rules_command: {err}");
            if let Some(mut c) = child.borrow_mut().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            if let Some(done) = done.borrow_mut().take() {
                done(state, None);
            }
            return;
        }
    }
    let command = command.to_string();
    let _ = state.handle.insert_source(Timer::from_duration(EXTERNAL_RULES_TIMEOUT), move |_, _, state| {
        if let Some(done) = done.borrow_mut().take() {
            tracing::warn!(command, "external_rules_command did not answer in time; killed");
            if let Some(mut c) = child.borrow_mut().take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            if let Some(t) = token.take() {
                state.handle.remove(t);
            }
            done(state, None);
        }
        TimeoutAction::Drop
    });
}

/// What runs when `external_rules_command` has answered (or failed).
pub type ExternalRulesDone<Bd> = Box<dyn FnOnce(&mut crate::state::State<Bd>, Option<bsp_core::rules::RuleConsequence>)>;

/// How long a window waits for `external_rules_command`.
pub const EXTERNAL_RULES_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// The consequence as `key=value` words, the shape bspwm hands its rules command.
fn describe_consequence(c: &bsp_core::rules::RuleConsequence) -> String {
    use bsp_core::node::{ClientState, Layer};
    let onoff = |b: bool| if b { "on" } else { "off" };
    let mut words = Vec::new();
    if let Some(s) = c.state {
        words.push(format!(
            "state={}",
            match s {
                ClientState::Tiled => "tiled",
                ClientState::PseudoTiled => "pseudo_tiled",
                ClientState::Floating => "floating",
                ClientState::Fullscreen => "fullscreen",
            }
        ));
    }
    if let Some(l) = c.layer {
        words.push(format!("layer={}", match l { Layer::Below => "below", Layer::Normal => "normal", Layer::Above => "above" }));
    }
    for (key, value) in [("hidden", c.hidden), ("sticky", c.sticky), ("private", c.private), ("locked", c.locked), ("marked", c.marked)] {
        words.push(format!("{key}={}", onoff(value.unwrap_or(false))));
    }
    words.push(format!("center={}", onoff(c.should_center())));
    words.push(format!("follow={}", onoff(c.should_follow())));
    words.push(format!("manage={}", onoff(c.should_manage())));
    words.push(format!("focus={}", onoff(c.should_focus())));
    words.push(format!("border={}", onoff(c.should_border())));
    words.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_consequence_is_described_as_bspwm_prints_it() {
        let c = bsp_core::rules::RuleConsequence { state: Some(bsp_core::node::ClientState::Floating), hidden: Some(true), ..Default::default() };
        assert_eq!(
            describe_consequence(&c),
            "state=floating hidden=on sticky=off private=off locked=off marked=off center=off follow=off manage=on focus=on border=on"
        );
    }
}
