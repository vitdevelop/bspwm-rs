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

use crate::state::{Backend, State};

/// Binds the control socket and registers it with the event loop. Logs a
/// warning and leaves IPC disabled if the socket path cannot be resolved
/// or bound — `bspc-rs` then cannot reach this compositor, but nothing
/// else is affected.
pub fn init<Bd: Backend + 'static>(state: &mut State<Bd>) {
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
    let registered = state.handle.insert_source(source, |_, metadata, state| {
        // SAFETY: the `Listener` inside is not dropped through this
        // reference — only read via `&Listener` methods.
        let listener = unsafe { metadata.get_mut() };
        accept_new_connections(state, listener);
        Ok(PostAction::Continue)
    });
    if let Err(err) = registered {
        tracing::warn!("failed to register the control socket with the event loop: {err}");
    }
}

fn accept_new_connections<Bd: Backend + 'static>(state: &mut State<Bd>, listener: &mut FdWrapper<Listener>) {
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

fn register_connection<Bd: Backend + 'static>(state: &mut State<Bd>, connection: Connection) {
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

fn on_readable<Bd: Backend + 'static>(state: &mut State<Bd>, slot: &mut ConnSlot) -> PostAction {
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
            let reply = try_hotkeys_inline_bspc(state, &other)
                .or_else(|| crate::pointer_action::try_config(state, &other))
                .or_else(|| crate::xwayland::try_config(state, &other))
                .unwrap_or_else(|| execute_and_broadcast(state, &other));
            reply_and_close(slot, reply);
            PostAction::Remove
        }
    }
}

/// `bspc config hotkeys_inline_bspc` (`docs/bsp-hotkeys.md`'s "Binding
/// execution" off switch): a compositor-only setting
/// (`crate::state::State::hotkeys_inline_bspc`'s doc comment explains
/// why it isn't part of `bsp_core::wm::Wm::settings`), so `bsp_ipc::exec`
/// has never heard of it and would otherwise reply with its "Unknown
/// setting" fallback. Intercepted here, before a command would otherwise
/// reach `execute_and_broadcast`, and reused as-is by
/// `crate::hotkeys::run_inline` so a hotkey bound to this exact command
/// behaves identically to the socket path (the same "Binding execution"
/// equivalence requirement [`execute_and_broadcast`] documents).
///
/// `Some` when `command` was this setting (handled, whether get or set);
/// `None` for anything else, meaning the caller should fall through to
/// its usual handling.
pub(crate) fn try_hotkeys_inline_bspc<Bd: Backend + 'static>(state: &mut State<Bd>, command: &Command) -> Option<Reply> {
    let Command::Config(c) = command else {
        return None;
    };
    if c.name != "hotkeys_inline_bspc" {
        return None;
    }
    Some(match &c.value {
        None => Reply::Ok(format!("{}\n", bool_str(state.hotkeys_inline_bspc))),
        Some(value) => match bsp_ipc::value::parse_bool(value) {
            Some(b) => {
                state.hotkeys_inline_bspc = b;
                Reply::Ok(String::new())
            }
            None => Reply::Fail(format!(
                "config: hotkeys_inline_bspc: Invalid value: '{value}'.\n"
            )),
        },
    })
}

fn bool_str(b: bool) -> String {
    (if b { "true" } else { "false" }).to_string()
}

/// Runs any `Command` but `Subscribe`/`Quit` (the caller handles those)
/// through `bsp_ipc::exec::execute`, reconciles the Wayland-visible
/// state, and broadcasts the resulting events/report to every
/// `subscribe`d connection — every side effect a socket request has,
/// factored out so `crate::hotkeys`' in-process `bspc` dispatch path
/// gets exactly the same behavior (`docs/bsp-hotkeys.md`'s "Binding
/// execution" equivalence requirement).
///
/// `bspc wm -r` (`WmAction::Restart`) is also handled here rather than
/// in `bsp_ipc::exec`: `exec_wm` already produces the right `Reply` for
/// it (`Reply::Ok`, no `Event`s), but a restart is a compositor-only
/// side effect `bsp-ipc` has no way to perform itself. `docs/design.md`
/// Compatibility: restarting a Wayland compositor kills every client,
/// so unlike bspwm's own `wm -r` (which re-execs the whole process),
/// this is a live reload — re-running `bspwmrc` and re-reading sxhkdrc
/// (`crate::bspwmrc::run`, `crate::hotkeys::reload`, the same as
/// `SIGUSR1` does for the latter) — without disturbing any mapped
/// client or `bsp-core` state.
pub(crate) fn execute_and_broadcast<Bd: Backend + 'static>(state: &mut State<Bd>, command: &Command) -> Reply {
    let (mut reply, mut events) = {
        let mut ctx = ExecCtx {
            wm: &mut state.wm,
            registry: &mut state.registry,
            adapter: &mut state.adapter,
        };
        exec::execute(&mut ctx, command)
    };
    // `bspc output`/`bspc input` only validate and queue (`crate::hardware`);
    // the real change happens here, where `Output`s and the backend live.
    if let Err(msg) = apply_hardware_changes(state, &mut events) {
        if matches!(reply, Reply::Ok(_)) {
            reply = Reply::Fail(msg);
        }
    }
    crate::shell::sync_wayland_from_core(state);
    for event in &events {
        state.subscribers.broadcast_event(event);
    }
    let report = build_report(state);
    state.subscribers.broadcast_report(&report);
    state.backend_data.queue_redraw();

    if matches!(reply, Reply::Ok(_)) && requests_restart(command) {
        crate::bspwmrc::run();
        crate::hotkeys::reload(state);
    }

    reply
}

/// Applies the output/input changes `crate::hardware::HwModel` queued
/// while `bsp_ipc::exec` ran: mode/scale/position on the Smithay
/// `Output` (and the backend, for a mode), then the matching `bsp-core`
/// monitor's rectangle via `exec::set_monitor_rectangle` (the same path
/// `bspc monitor -g` takes); keyboard repeat on the seat, pointer accel
/// on the backend's libinput device. Returns the first failure's message
/// (later changes are still attempted).
fn apply_hardware_changes<Bd: Backend + 'static>(
    state: &mut State<Bd>,
    events: &mut Vec<bsp_ipc::report::Event>,
) -> Result<(), String> {
    use bsp_ipc::command::{InputAction, OutputAction};
    use smithay::output::Scale;

    let outputs = std::mem::take(&mut state.adapter.hw.pending_outputs);
    let inputs = std::mem::take(&mut state.adapter.hw.pending_inputs);
    let mut first_error: Option<String> = None;

    for (name, action) in outputs {
        let Some(output) = state.space.outputs().find(|o| o.name() == name).cloned() else {
            continue;
        };
        match action {
            OutputAction::SetMode(mode) => match state.backend_data.set_output_mode(&output, mode) {
                Ok(wl_mode) => output.change_current_state(Some(wl_mode), None, None, None),
                Err(msg) => {
                    first_error.get_or_insert(msg);
                    continue;
                }
            },
            OutputAction::SetScale(scale) => {
                output.change_current_state(None, None, Some(Scale::Fractional(scale)), None)
            }
            OutputAction::SetPosition(x, y) => {
                output.change_current_state(None, None, None, Some((x, y).into()));
                state.space.map_output(&output, (x, y));
            }
        }
        // Mode and scale change the output's logical size; a moved output
        // also changed its origin: re-map at the (possibly new) position
        // so the space's cached geometry follows, then mirror it into
        // `bsp-core`.
        let position = output.current_location();
        state.space.map_output(&output, position);
        let Some(geo) = state.space.output_geometry(&output) else {
            continue;
        };
        if let Some(monitor) = state.wm.monitors.iter().position(|m| m.name == name) {
            let mut ctx = ExecCtx {
                wm: &mut state.wm,
                registry: &mut state.registry,
                adapter: &mut state.adapter,
            };
            exec::set_monitor_rectangle(
                &mut ctx,
                monitor,
                bsp_core::geometry::Rect::new(geo.loc.x, geo.loc.y, geo.size.w, geo.size.h),
                events,
            );
        }
        // Panels are laid out against the output's size: re-arrange them
        // and refresh the monitor's struts for the new geometry.
        state.rearrange_layers(&output);
    }

    for (device, action) in inputs {
        match action {
            InputAction::SetRepeat { rate, delay } => {
                if let Some(keyboard) = state.seat.get_keyboard() {
                    keyboard.change_repeat_info(rate, delay);
                }
            }
            InputAction::SetAccel(accel) => {
                if let Err(msg) = state.backend_data.set_pointer_accel(&device, accel) {
                    first_error.get_or_insert(msg);
                }
            }
        }
    }

    first_error.map_or(Ok(()), Err)
}

/// Applies the output/input changes queued in the hardware model right
/// now, outside a `bspc` command: for protocols that reconfigure outputs
/// (`wlr-output-management`). Broadcasts the resulting `bspc subscribe`
/// events like a command would.
pub(crate) fn apply_hardware_now<Bd: Backend + 'static>(state: &mut State<Bd>) -> Result<(), String> {
    let mut events = Vec::new();
    let result = apply_hardware_changes(state, &mut events);
    crate::shell::sync_wayland_from_core(state);
    for event in &events {
        state.subscribers.broadcast_event(event);
    }
    let report = build_report(state);
    state.subscribers.broadcast_report(&report);
    state.backend_data.queue_redraw();
    result
}

fn requests_restart(command: &Command) -> bool {
    matches!(command, Command::Wm(actions) if actions.contains(&bsp_ipc::command::WmAction::Restart))
}

/// Sends `reply` but deliberately leaves the connection in `slot`: every
/// caller returns `PostAction::Remove` next, and calloop must deregister
/// the fd from epoll *before* it is closed — closing it here (dropping
/// the taken `Connection`) made that deregistration fail with EBADF, a
/// warning per `bspc` call. Dropping the source afterward closes it.
fn reply_and_close(slot: &mut ConnSlot, reply: Reply) {
    if let Some(connection) = slot.conn.as_mut() {
        let _ = connection.send_reply(reply);
    }
}

fn build_report<Bd: Backend + 'static>(state: &State<Bd>) -> bsp_ipc::report::Report {
    exec::build_report(&state.wm)
}

fn build_report_line<Bd: Backend + 'static>(state: &State<Bd>) -> String {
    build_report(state).to_string()
}
