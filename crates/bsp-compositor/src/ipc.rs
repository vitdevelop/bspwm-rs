//! Wires `bsp-ipc`'s control socket into the compositor's calloop event
//! loop: binds the socket, accepts connections, and for each request runs
//! it through `bsp_ipc::exec::execute` (or handles `subscribe`/`quit`
//! directly, which `execute` does not), reconciling the Wayland-visible
//! state afterward via `crate::shell::sync_wayland_from_core`.
//!
//! bspwm's socket handling (`src/bspwm.c` `main()`) is a plain `select()`
//! loop; this is the calloop equivalent, using `Generic` to drive
//! `bsp-ipc`'s own non-blocking `Listener`/`Connection` types
//! (`docs/bsp-ipc.md`) from inside Smithay's event loop.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};

use smithay::reexports::calloop::generic::{FdWrapper, Generic};
use smithay::reexports::calloop::{Interest, Mode, PostAction};

use bsp_ipc::command::Command;
use bsp_ipc::exec::{self, ExecCtx};
use bsp_ipc::server::{Connection, Listener};
use bsp_ipc::wire::{self, Reply};

use crate::state::State;

/// Binds the control socket and registers it with the event loop. Logs a
/// warning and leaves IPC disabled if the socket path cannot be resolved
/// or bound — `bspc-rs` then cannot reach this compositor, but nothing
/// else is affected.
pub fn init(state: &mut State) {
    let Some(path) = wire::socket_path() else {
        tracing::warn!("no BSPWM_SOCKET/XDG_RUNTIME_DIR: control socket disabled");
        return;
    };
    let listener = match Listener::bind(&path) {
        Ok(l) => l,
        Err(err) => {
            tracing::warn!(path = %path.display(), "failed to bind the control socket: {err}");
            return;
        }
    };
    tracing::info!(path = %path.display(), "control socket listening");

    // SAFETY: `FdWrapper::new()` requires the wrapped value's `AsRawFd`
    // impl to always return a valid fd for the wrapper's lifetime; a
    // `Listener`'s underlying `UnixListener` is only closed when the
    // `Listener` itself (and so this `FdWrapper`) is dropped.
    let wrapped = unsafe { FdWrapper::new(listener) };
    let source = Generic::new(wrapped, Interest::READ, Mode::Level);
    state
        .handle
        .insert_source(source, |_, metadata, state| {
            // SAFETY: the `Listener` inside is not dropped through this
            // reference — only read via `&Listener` methods.
            let listener = unsafe { metadata.get_mut() };
            accept_new_connections(state, listener);
            Ok(PostAction::Continue)
        })
        .expect("failed to register the control socket with the event loop");
}

fn accept_new_connections(state: &mut State, listener: &mut FdWrapper<Listener>) {
    loop {
        match listener.accept() {
            Ok(Some(connection)) => register_connection(state, connection),
            Ok(None) => break,
            Err(err) => {
                tracing::warn!("failed to accept a control socket connection: {err}");
                break;
            }
        }
    }
}

/// Holds an accepted connection until it is either replied to (a normal
/// command) or handed to `Subscribers` (a `subscribe` request), at which
/// point [`ConnSlot::conn`] is emptied and the calloop source removed.
/// Wraps a [`Connection`] instead of using calloop's own `FdWrapper`
/// because `FdWrapper<T>` requires `T: AsRawFd`, and `Option<Connection>`
/// cannot implement it here (`AsRawFd` and `Option` are both foreign to
/// this crate) — this local type can implement [`AsFd`] directly instead,
/// backed by the fd captured at construction time so it stays valid to
/// query even after `conn` is taken (this source is always removed from
/// the event loop, via [`PostAction::Remove`], in the same step that
/// empties it, so the cached fd is never queried once stale).
struct ConnSlot {
    conn: Option<Connection>,
    fd: RawFd,
}

impl ConnSlot {
    fn new(conn: Connection) -> Self {
        let fd = conn.as_raw_fd();
        Self {
            conn: Some(conn),
            fd,
        }
    }
}

impl AsFd for ConnSlot {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: `self.fd` is `self.conn`'s file descriptor for as long
        // as `self.conn` is `Some`, which covers every point this is
        // called from calloop (see this type's doc comment).
        unsafe { BorrowedFd::borrow_raw(self.fd) }
    }
}

fn register_connection(state: &mut State, connection: Connection) {
    let source = Generic::new(ConnSlot::new(connection), Interest::READ, Mode::Level);
    let result = state.handle.insert_source(source, |_, metadata, state| {
        // SAFETY: `on_readable` only ever empties `conn` via `Option::take`
        // (transferring ownership out, not dropping it here) before
        // returning `PostAction::Remove`, which is `register_connection`'s
        // caller's cue that this source's own drop is next and safe.
        let slot = unsafe { metadata.get_mut() };
        Ok(on_readable(state, slot))
    });
    if let Err(err) = result {
        tracing::warn!("failed to register a control socket connection: {err}");
    }
}

fn on_readable(state: &mut State, slot: &mut ConnSlot) -> PostAction {
    let Some(connection) = slot.conn.as_mut() else {
        return PostAction::Remove;
    };
    let args = match connection.try_read_request() {
        Ok(Some(args)) => args,
        Ok(None) => return PostAction::Continue,
        Err(_) => return PostAction::Remove,
    };

    let command = match bsp_ipc::command::parse(&args) {
        Ok(c) => c,
        Err(e) => {
            reply_and_close(slot, Reply::Fail(e.message));
            return PostAction::Remove;
        }
    };

    match command {
        Command::Quit(_status) => {
            // The exit status argument is not threaded through to the
            // process's own exit code yet (`docs/bsp-compositor.md`,
            // scope) — stopping the main loop is what matters for
            // a nested development backend.
            state.running = false;
            reply_and_close(slot, Reply::Ok(String::new()));
            PostAction::Remove
        }
        Command::Subscribe { count, masks, .. } => {
            let Some(connection) = slot.conn.take() else {
                return PostAction::Remove;
            };
            let report = build_report_line(state);
            state.subscribers.add(connection, masks, count, &report);
            PostAction::Remove
        }
        other => {
            let reply = execute_and_broadcast(state, &other);
            reply_and_close(slot, reply);
            PostAction::Remove
        }
    }
}

/// Runs any `Command` but `Subscribe`/`Quit` (the caller handles those)
/// through `bsp_ipc::exec::execute`, reconciles the Wayland-visible
/// state, and broadcasts the resulting events/report to every
/// `subscribe`d connection — every side effect a socket request has,
/// factored out so `crate::hotkeys`' in-process `bspc` dispatch path
/// gets exactly the same behavior (`docs/bsp-hotkeys.md`'s "Binding
/// execution" equivalence requirement).
pub(crate) fn execute_and_broadcast(state: &mut State, command: &Command) -> Reply {
    let (reply, events) = {
        let mut ctx = ExecCtx {
            wm: &mut state.wm,
            registry: &mut state.registry,
            adapter: &mut state.adapter,
        };
        exec::execute(&mut ctx, command)
    };
    crate::shell::sync_wayland_from_core(state);
    for event in &events {
        state.subscribers.broadcast_event(event);
    }
    let report = build_report(state);
    state.subscribers.broadcast_report(&report);
    reply
}

fn reply_and_close(slot: &mut ConnSlot, reply: Reply) {
    if let Some(mut connection) = slot.conn.take() {
        let _ = connection.send_reply(reply);
    }
}

fn build_report(state: &State) -> bsp_ipc::report::Report {
    exec::build_report(&state.wm)
}

fn build_report_line(state: &State) -> String {
    build_report(state).to_string()
}
