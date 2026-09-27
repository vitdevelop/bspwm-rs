//! Wires `bsp-ipc`'s control socket into the compositor's calloop event
//! loop: binds the socket, accepts connections, and for each request runs
//! it through `bsp_ipc::exec::execute` (or handles `subscribe`/`quit`
//! directly, which `execute` does not), reconciling the Wayland-visible
//! state afterward via `crate::shell::sync_wayland_from_core` (deferred to the
//! end of the event-loop turn, `State::request_sync`).
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
    let explicit = std::env::var_os(wire::SOCKET_ENV_VAR).is_some();
    let bound = match Listener::bind(&path) {
        // Another bspwm-rs (another VT) answers at the default path: take a
        // path of our own instead of taking over (or, on exit, deleting) its
        // socket, which left the first session's `bspc` and hotkeys dead.
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse && !explicit => {
            let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| std::process::id().to_string());
            let own = path.with_file_name(format!("bspwm-rs-{display}-socket"));
            tracing::info!(taken = %path.display(), path = %own.display(), "another instance owns the control socket; using one of our own");
            Listener::bind(&own)
        }
        other => other,
    };
    let listener = match bound {
        Ok(l) => l,
        Err(err) => {
            tracing::warn!(path = %path.display(), "failed to bind the control socket: {err}");
            return;
        }
    };
    tracing::info!(path = %listener.path().display(), "control socket listening");
    // Our children (`bspwmrc`, hotkey commands, terminals) find this instance.
    // SAFETY: process environment, set at startup before `bspwmrc` or any
    // hotkey command is spawned and before the X worker thread exists, like
    // `WAYLAND_DISPLAY` just before.
    unsafe {
        std::env::set_var(wire::SOCKET_ENV_VAR, listener.path());
    }

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
            reply_and_close(state, slot, Reply::Fail(e.message));
            return PostAction::Remove;
        }
    };

    match command {
        Command::Quit(status) => {
            crate::state::EXIT_STATUS.store(status.unwrap_or(0), std::sync::atomic::Ordering::Relaxed);
            state.running = false;
            reply_and_close(state, slot, Reply::Ok(String::new()));
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
            reply_and_close(state, slot, reply);
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
    let focus_before = crate::input::focus_key(state);
    // What `pointed` refers to for this command.
    let location = state.pointer.current_location();
    state.adapter.pointer = (Some((location.x as i32, location.y as i32)), crate::input::window_under(state, location));
    let (mut reply, mut events) = {
        let mut ctx = ExecCtx {
            wm: &mut state.wm,
            registry: &mut state.registry,
            adapter: &mut state.adapter,
        };
        exec::execute(&mut ctx, command)
    };
    apply_pending_kills(state);
    // `bspc output`/`bspc input` only validate and queue (`crate::hardware`);
    // the real change happens here, where `Output`s and the backend live.
    if let Err(msg) = apply_hardware_changes(state, &mut events) {
        if matches!(reply, Reply::Ok(_)) {
            reply = Reply::Fail(msg);
        }
    }
    // A command that only reads (`query`, `config KEY`) changed nothing: no
    // reconcile, no report, no redraw. Status bars poll `query` often.
    if command.is_read_only() && events.is_empty() {
        return reply;
    }
    // The windows follow the tree, and the pointer the focus, in the one sync
    // at the end of this event-loop turn (or before the next input event), so
    // a burst of commands in one turn reconciles once.
    state.request_warp(focus_before);
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

/// Sends `events` to every `subscribe`d connection, then a fresh report line
/// (bspwm puts a report after every change that alters what a bar shows).
/// Does nothing for an empty list.
pub(crate) fn broadcast_events<Bd: Backend + 'static>(state: &mut State<Bd>, events: &[bsp_ipc::report::Event]) {
    if events.is_empty() {
        return;
    }
    for event in events {
        state.subscribers.broadcast_event(event);
    }
    let report = build_report(state);
    state.subscribers.broadcast_report(&report);
}

/// Runs one of `bsp_ipc::exec`'s bspwm-shaped operations (`focus_node`,
/// `activate_node`, `transfer_node` ...) on the live state, so a focus, map or
/// unmap the compositor itself causes changes the tree exactly as the matching
/// `bspc` command would, and reports what bspwm reports.
pub(crate) fn with_ops<Bd: Backend + 'static, R>(
    state: &mut State<Bd>,
    f: impl FnOnce(&mut ExecCtx<crate::adapter::WindowAdapter>, &mut Vec<bsp_ipc::report::Event>) -> R,
) -> R {
    state.wm.sync_history();
    let layouts = bsp_ipc::exec::layout_snapshot(&state.wm);
    let mut events = Vec::new();
    let result = {
        let mut ctx = ExecCtx { wm: &mut state.wm, registry: &mut state.registry, adapter: &mut state.adapter };
        f(&mut ctx, &mut events)
    };
    state.wm.sync_history();
    state.registry.sync_with(&state.wm);
    bsp_ipc::exec::push_layout_changes(&state.wm, &layouts, &mut events);
    bsp_ipc::exec::push_geometry_changes(&mut state.wm, &state.registry, &mut events);
    broadcast_events(state, &events);
    // The operation may have raised a window (focus, state, layer): the
    // stacking is applied by the one sync at the end of the event-loop turn.
    state.request_sync();
    result
}

/// Kills the clients of the windows `bspc node -k` queued.
///
/// bspwm: `xcb_kill_client()`. An X11 window's client is killed through the X
/// server (never the whole Xwayland, which every X11 client shares). A Wayland
/// window's client is disconnected, which destroys all its windows, so a client
/// with several toplevels loses them all, as an X11 client would. The nodes
/// leave the tree when the surfaces are destroyed (`shell::unmap_window`).
fn apply_pending_kills<Bd: Backend + 'static>(state: &mut State<Bd>) {
    use smithay::reexports::wayland_server::backend::DisconnectReason;
    use smithay::reexports::wayland_server::Resource;
    use smithay::wayland::seat::WaylandFocus;
    for window in std::mem::take(&mut state.adapter.pending_kills) {
        if let Some(x11) = window.x11_surface() {
            tracing::debug!(window = x11.window_id(), display = ?state.xwayland.display, "killing an X11 client");
            if !crate::xwayland::kill_client(state, x11.window_id()) {
                let _ = x11.close();
            }
        } else if let Some(client) = window.wl_surface().and_then(|s| s.client()) {
            tracing::debug!("killing a Wayland client");
            state.display_handle.backend_handle().kill_client(client.id(), DisconnectReason::ConnectionClosed);
        }
    }
}

/// Applies the output/input changes `crate::hardware::HwModel` queued
/// while `bsp_ipc::exec` ran: mode/scale/position on the Smithay
/// `Output` (and the backend, for a mode), then the matching `bsp-core`
/// monitor's rectangle via `exec::set_monitor_rectangle` (the same path
/// `bspc monitor -g` takes); keyboard repeat on the seat, pointer accel
/// on the backend's libinput device. Returns the first failure's message
/// (later changes are still attempted).
/// The Wayland transform for `t`.
fn wl_transform(t: bsp_ipc::command::OutputTransform) -> smithay::utils::Transform {
    use bsp_ipc::command::OutputTransform as T;
    use smithay::utils::Transform;
    match t {
        T::Normal => Transform::Normal,
        T::Rotate90 => Transform::_90,
        T::Rotate180 => Transform::_180,
        T::Rotate270 => Transform::_270,
        T::Flipped => Transform::Flipped,
        T::Flipped90 => Transform::Flipped90,
        T::Flipped180 => Transform::Flipped180,
        T::Flipped270 => Transform::Flipped270,
    }
}

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
        // Virtual outputs come and go here; nothing else applies to a name that
        // does not exist yet.
        match &action {
            OutputAction::CreateHeadless(mode) => {
                crate::headless::create(state, *mode, events);
                continue;
            }
            OutputAction::Remove => {
                if let Err(msg) = crate::headless::remove(state, &name, events) {
                    first_error.get_or_insert(msg);
                }
                continue;
            }
            _ => {}
        }
        let Some(output) = state.space.outputs().find(|o| o.name() == name).cloned() else {
            continue;
        };
        match action {
            // A virtual output has no hardware to program: the mode is the state.
            OutputAction::SetMode(mode) if crate::headless::is_headless(&output) => {
                let wl_mode = smithay::output::Mode { size: (mode.width, mode.height).into(), refresh: mode.refresh_mhz };
                output.add_mode(wl_mode);
                output.set_preferred(wl_mode);
                output.change_current_state(Some(wl_mode), None, None, None);
            }
            OutputAction::CreateHeadless(_) | OutputAction::Remove => {}
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
            OutputAction::SetTransform(transform) => {
                output.change_current_state(None, Some(wl_transform(transform)), None, None);
                // The frame is drawn rotated from now on: everything is stale.
                state.backend_data.reset_buffers(&output);
                state.backend_data.queue_redraw();
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
    state.request_sync();
    for event in &events {
        state.subscribers.broadcast_event(event);
    }
    let report = build_report(state);
    state.subscribers.broadcast_report(&report);
    state.backend_data.queue_redraw();
    result
}

/// Whether `command` is `wm -r`.
fn requests_restart(command: &Command) -> bool {
    matches!(command, Command::Wm(actions) if actions.contains(&bsp_ipc::command::WmAction::Restart))
}

/// Sends `reply` but deliberately leaves the connection in `slot`: every
/// caller returns `PostAction::Remove` next, and calloop must deregister
/// the fd from epoll *before* it is closed — closing it here (dropping
/// the taken `Connection`) made that deregistration fail with EBADF, a
/// warning per `bspc` call. Dropping the source afterward closes it.
fn reply_and_close<Bd: Backend + 'static>(state: &mut State<Bd>, slot: &mut ConnSlot, reply: Reply) {
    let Some(mut connection) = slot.conn.take() else {
        return;
    };
    let sent = connection.send_reply(reply);
    if sent.is_ok() && connection.has_queued() {
        // The reader is slow: the rest is written as the socket takes it
        // (`flush_ipc`), not waited for here. The connection moves out of the
        // slot but stays open, so the source is still deregistered before the fd closes.
        state.pending_replies.push(PendingReply { connection, since: std::time::Instant::now() });
    } else {
        slot.conn = Some(connection);
    }
}

/// A reply the socket has not taken completely yet.
pub struct PendingReply {
    connection: Connection,
    since: std::time::Instant,
}

/// Writes what the sockets of slow `bspc` and `subscribe` readers did not take
/// yet, drops a connection that has been stuck for [`bsp_ipc::server::REPLY_WRITE_TIMEOUT`]
/// or died, and, while anything is still queued, arms a timer so this runs
/// again soon (a queue no event ever touches would otherwise stay stuck).
pub fn flush_ipc<Bd: Backend + 'static>(state: &mut State<Bd>) {
    use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
    state.subscribers.flush_all();
    state.pending_replies.retain_mut(|p| {
        matches!(p.connection.flush(), Ok(false)) && p.since.elapsed() < bsp_ipc::server::REPLY_WRITE_TIMEOUT
    });
    if (state.subscribers.has_queued() || !state.pending_replies.is_empty()) && !state.ipc_flush_armed {
        state.ipc_flush_armed = true;
        let armed = state.handle.insert_source(Timer::from_duration(std::time::Duration::from_millis(20)), |_, _, state| {
            state.ipc_flush_armed = false;
            TimeoutAction::Drop
        });
        if armed.is_err() {
            state.ipc_flush_armed = false;
        }
    }
}

fn build_report<Bd: Backend + 'static>(state: &State<Bd>) -> bsp_ipc::report::Report {
    exec::build_report(&state.wm)
}

fn build_report_line<Bd: Backend + 'static>(state: &State<Bd>) -> String {
    build_report(state).to_string()
}
