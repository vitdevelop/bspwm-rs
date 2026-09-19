//! XWayland: running X11 programs (`xterm`, Steam, old toolkits) inside the
//! Wayland compositor, with `bspc` treating their windows like any other
//! (`docs/design.md`, roadmap the XWayland).
//!
//! bspwm *is* an X11 window manager, so X11 windows are its home turf —
//! including the `WM_CLASS` class/instance names its rules match on, which
//! [`crate::shell::map_new_window`] receives from here.
//!
//! **Lazy start.** X11 programs are rare on a Wayland desktop, so the real
//! `Xwayland` server (tens of MB, GL setup) is not started up front.
//! Smithay's `XWayland::spawn` claims the X display (`/tmp/.X<N>-lock`,
//! `/tmp/.X11-unix/X<N>`, abstract socket) and starts `Xwayland` at once, so
//! it is pointed at a *shim*: `PATH` gets a directory first whose `Xwayland`
//! is a symlink to this executable ([`shim_dir`]). Run under that name,
//! `main` calls [`run_shim_if_invoked`], which sleeps in `poll(2)` on the
//! `-listenfd` sockets and, when the first X11 client connects, `exec`s the
//! real `Xwayland` with the same arguments and inherited descriptors — the
//! waiting connection is served as soon as it is up. `DISPLAY=:<N>` is
//! exported at startup, before `bspwmrc`.
//!
//! **Lazy stop.** Once the last X11 window is gone (30 s grace, so a program
//! that is still starting up is not cut off) and no X11 client owns the
//! clipboard, `Xwayland` is shut down and its memory returned; the next X11
//! client starts it again through a fresh shim. (It cannot stop on its own,
//! `-terminate`: the window manager's own X connection counts as a client.)
//! An X11 client that has connected but shows no window and owns no selection
//! is not noticed and ends with the server. The window manager side is
//! Smithay's `X11Wm`.
//!
//! X11 windows that a window manager manages become nodes in the `bsp-core`
//! tree (rules, tiling, focus, `bspc node` all apply). *Override-redirect*
//! windows and menus/tooltips/notifications (`_NET_WM_WINDOW_TYPE`) are
//! placed where the program asked and stay outside the tree, as in bspwm
//! (`src/events.c` `map_request()`: override-redirect windows are never
//! managed). Clipboard and primary selection are bridged both ways.
//!
//! **Window types** (`_NET_WM_WINDOW_TYPE`) follow bspwm's `src/rule.c`
//! `apply_rules()`: dialogs float centred, toolbars and utility windows do not
//! take focus, docks, desktops and notifications are not managed; user rules
//! override all of it. **Struts:** a window's `_NET_WM_STRUT_PARTIAL` grows
//! the padding of the monitors it touches (`src/ewmh.c` `ewmh_handle_struts()`,
//! ported as `bsp_core::wm::Wm::apply_ewmh_struts`), unless `bspc config
//! ignore_ewmh_struts` is set. Smithay does not offer window properties it
//! does not know, so the property is read over a short-lived second X
//! connection when the window maps; a strut set or changed later is not seen.
//! `_NET_ACTIVE_WINDOW` on the root window is kept current by Smithay; *requests*
//! to activate a window (`wmctrl -a`) never reach an `XwmHandler`, so they are
//! ignored.
//!
//! **Hard-rule note:** `XwmHandler::xwm_state` must return an `&mut X11Wm`
//! unconditionally, so it panics if called with no window manager running;
//! Smithay only calls it from the WM's own event source, which exists only
//! while `X11Wm` does (the field is cleared in `disconnected`).

use std::os::fd::OwnedFd;
use std::path::PathBuf;

use smithay::desktop::Window;
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Rectangle};
use smithay::wayland::selection::data_device::{
    clear_data_device_selection, request_data_device_client_selection, set_data_device_selection,
};
use smithay::wayland::selection::primary_selection::{
    clear_primary_selection, request_primary_client_selection, set_primary_selection,
};
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::xwayland::xwm::{Reorder, ResizeEdge, WmWindowProperty, WmWindowType, X11Window, XwmId};
use smithay::xwayland::{X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler};

use crate::state::{Backend, State};

/// Everything XWayland-related that `State` carries.
#[derive(Default)]
pub struct XWaylandState {
    /// The claimed X display number (kept across restarts, so `DISPLAY`
    /// stays valid for programs started earlier).
    display: Option<u32>,
    /// The current (possibly still dormant) `Xwayland` instance's registration
    /// with the event loop, which owns the instance itself.
    token: Option<RegistrationToken>,
    /// The window manager connected to it.
    xwm: Option<X11Wm>,
    /// X11 windows shown but not managed (override-redirect, menus, …).
    unmanaged: Vec<Window>,
    /// `bspc config ignore_ewmh_struts`: do not reserve space for panels.
    ignore_struts: bool,
    /// `bspc config ignore_ewmh_focus`: accepted for compatibility; Smithay's
    /// X11 window manager does not pass `_NET_ACTIVE_WINDOW` requests on.
    ignore_focus: bool,
    /// An X11 client owns the clipboard or primary selection; its window may
    /// be gone but the server must stay up to serve it.
    selection_owned: bool,
    /// The pending "no X11 windows left" check (`schedule_idle_stop`).
    idle_timer: Option<RegistrationToken>,
}

/// How long `Xwayland` is kept after its last window went away before it is
/// stopped (the next X11 client starts it again).
const IDLE_STOP_AFTER: std::time::Duration = std::time::Duration::from_secs(30);

/// The name this program answers to when it is run as the lazy-start shim
/// (see [`shim_dir`] and [`run_shim_if_invoked`]).
const SHIM_NAME: &str = "Xwayland";

/// Directory holding a symlink named `Xwayland` to this very executable.
/// Smithay's `XWayland::spawn` runs `Xwayland` from `PATH` immediately; with
/// this directory first on that `PATH` it runs the shim instead, which waits
/// for the first X11 client before executing the real server.
fn shim_dir() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let dir = PathBuf::from(runtime).join("bspwm-rs-xwayland-shim");
    std::fs::create_dir_all(&dir).ok()?;
    let link = dir.join(SHIM_NAME);
    let exe = std::env::current_exe().ok()?;
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(exe, &link).ok()?;
    Some(dir)
}

/// If this process was started as `Xwayland` (the shim symlink), waits until
/// one of the `-listenfd` sockets has a pending connection and then replaces
/// itself with the real `Xwayland`, with the same arguments and inherited
/// descriptors (so the waiting connection is served at once). Never returns
/// in that case; returns normally when run as `bspwm-rs`. Call first in `main`.
pub fn run_shim_if_invoked() {
    use smithay::reexports::rustix::event::{poll, PollFd, PollFlags};
    use std::os::fd::{BorrowedFd, RawFd};
    use std::os::unix::process::CommandExt;

    let mut args = std::env::args_os();
    let invoked_as = args.next().map(PathBuf::from).and_then(|p| p.file_name().map(|n| n.to_owned()));
    if invoked_as.as_deref() != Some(std::ffi::OsStr::new(SHIM_NAME)) {
        return;
    }
    let args: Vec<std::ffi::OsString> = args.collect();
    // Die with the compositor (`KILL`: the compositor blocks `TERM` for its signal
    // handling and children inherit that mask, so `TERM` never arrived): the dormant shim polls forever and the real
    // server (which keeps the setting across `exec`) would otherwise outlive a
    // crashed or killed compositor, as stray `Xwayland` processes did.
    crate::spawn::unblock_signals();
    let parent = smithay::reexports::rustix::process::getppid();
    if smithay::reexports::rustix::process::set_parent_process_death_signal(Some(
        smithay::reexports::rustix::process::Signal::KILL,
    ))
    .is_err()
        || smithay::reexports::rustix::process::getppid() != parent
    {
        // No death signal, or the compositor is already gone.
        if smithay::reexports::rustix::process::getppid() != parent {
            std::process::exit(0);
        }
    }
    let listen_fds: Vec<RawFd> = args
        .windows(2)
        .filter(|w| w[0] == "-listenfd")
        .filter_map(|w| w[1].to_str().and_then(|s| s.parse().ok()))
        .collect();

    if !listen_fds.is_empty() {
        // SAFETY: these descriptors were inherited from the compositor and
        // stay open for this process's whole (short) life.
        let borrowed: Vec<BorrowedFd<'_>> = listen_fds.iter().map(|fd| unsafe { BorrowedFd::borrow_raw(*fd) }).collect();
        loop {
            let mut fds: Vec<PollFd<'_>> = borrowed.iter().map(|fd| PollFd::new(fd, PollFlags::IN)).collect();
            match poll(&mut fds, None) {
                Ok(_) if fds.iter().any(|f| f.revents().contains(PollFlags::IN)) => break,
                Ok(_) => continue,
                Err(err) if err == smithay::reexports::rustix::io::Errno::INTR => continue,
                Err(_) => break,
            }
        }
    }

    // The real server: the first `Xwayland` on PATH that is not this shim.
    let shim = shim_dir_path();
    let real = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .filter(|dir| Some(dir) != shim.as_ref())
        .map(|dir| dir.join(SHIM_NAME))
        .find(|candidate| candidate.is_file());
    let Some(real) = real else {
        eprintln!("bspwm-rs xwayland shim: no real Xwayland found on PATH");
        std::process::exit(127);
    };
    let err = std::process::Command::new(real).args(&args).exec();
    eprintln!("bspwm-rs xwayland shim: cannot run Xwayland: {err}");
    std::process::exit(126);
}

/// The shim directory's path without creating it (for the shim itself).
fn shim_dir_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|r| PathBuf::from(r).join("bspwm-rs-xwayland-shim"))
}

/// Starts the lazy `Xwayland` machinery: claims a display, exports `DISPLAY`
/// and leaves the dormant shim waiting. Call once at startup, before
/// `bspwmrc` runs (its children inherit `DISPLAY`). Logs and continues
/// without X11 support on failure.
pub fn init<Bd: Backend + 'static>(state: &mut State<Bd>) {
    let _ = spawn(state);
}

/// Spawns (another) dormant instance on the same display number. `false` if
/// that failed for a reason that may pass (the previous instance still holds
/// the display).
fn spawn<Bd: Backend + 'static>(state: &mut State<Bd>) -> bool {
    // The directory X11 sockets live in; Smithay does not create it, and
    // X clients require it to be world-writable with the sticky bit.
    if !std::path::Path::new("/tmp/.X11-unix").exists() {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::create_dir_all("/tmp/.X11-unix")
            .and_then(|()| std::fs::set_permissions("/tmp/.X11-unix", std::fs::Permissions::from_mode(0o1777)))
            .is_err()
        {
            tracing::warn!("cannot create /tmp/.X11-unix: X11 programs will not work");
            return true;
        }
    }
    let Some(dir) = shim_dir() else {
        tracing::warn!("cannot prepare the Xwayland lazy-start shim: X11 programs will not work");
        return true;
    };
    let path = {
        let mut dirs = vec![dir];
        if let Some(existing) = std::env::var_os("PATH") {
            dirs.extend(std::env::split_paths(&existing));
        }
        match std::env::join_paths(dirs) {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!("cannot build PATH for Xwayland: {err}");
                return true;
            }
        }
    };
    let spawned = XWayland::spawn(
        &state.display_handle,
        state.xwayland.display,
        [("PATH", path)],
        true,
        std::process::Stdio::null(),
        std::process::Stdio::null(),
        |_| {},
    );
    let (xwayland, client) = match spawned {
        Ok(pair) => pair,
        Err(err) => {
            tracing::warn!("cannot start Xwayland: {err}");
            return false;
        }
    };
    let number = xwayland.display_number();
    if state.xwayland.display.is_none() {
        tracing::info!(x_display = number, "XWayland will start on the first X11 connection");
        // SAFETY: `set_var` is unsound only if another thread reads or writes
        // the environment concurrently; this runs at startup before any
        // helper thread or child process exists.
        unsafe {
            std::env::set_var("DISPLAY", format!(":{number}"));
        }
    }
    state.xwayland.display = Some(number);
    let inserted = state.handle.insert_source(xwayland, move |event, _, state| match event {
        XWaylandEvent::Ready { x11_socket, display_number } => {
            tracing::info!(display_number, "Xwayland is ready; connecting the window manager");
            match X11Wm::start_wm(state.handle.clone(), x11_socket, client.clone()) {
                Ok(xwm) => {
                    state.xwayland.xwm = Some(xwm);
                    // A client that connects without ever showing a window must not keep it up forever.
                    schedule_idle_stop(state);
                }
                Err(err) => tracing::warn!("cannot start the X11 window manager: {err}"),
            }
        }
        XWaylandEvent::Error => {
            tracing::warn!("Xwayland failed to start");
            restart_later(state);
        }
    });
    match inserted {
        Ok(token) => state.xwayland.token = Some(token),
        Err(err) => tracing::warn!("cannot register Xwayland with the event loop: {err}"),
    }
    true
}

/// [`spawn`] after `delay`, retrying every 500 ms (up to 20 times) while the
/// display is still held by the instance that just went away.
fn spawn_after<Bd: Backend + 'static>(state: &mut State<Bd>, delay: std::time::Duration, attempts_left: u32) {
    use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
    let _ = state.handle.insert_source(Timer::from_duration(delay), move |_, _, state| {
        if state.xwayland.token.is_none() && !spawn(state) && attempts_left > 0 {
            spawn_after(state, std::time::Duration::from_millis(500), attempts_left - 1);
        }
        TimeoutAction::Drop
    });
}

/// Drops the finished instance and its windows and prepares the next
/// dormant one (`Xwayland` exits after its last client, `-terminate`). The
/// new instance is spawned a moment later: the old process still holds the
/// display until it has exited.
fn restart<Bd: Backend + 'static>(state: &mut State<Bd>) {
    tracing::info!("Xwayland finished; the next X11 client starts it again");
    state.xwayland.xwm = None;
    state.xwayland.selection_owned = false;
    if let Some(token) = state.xwayland.idle_timer.take() {
        state.handle.remove(token);
    }
    if let Some(token) = state.xwayland.token.take() {
        state.handle.remove(token);
    }
    // Removing the source above dropped the `XWayland` handle, which
    // released the display's lock file and sockets.
    let windows: Vec<Window> = state.space.elements().filter(|w| w.x11_surface().is_some()).cloned().collect();
    for window in windows {
        forget_window(state, &window);
    }
    spawn_after(state, std::time::Duration::from_millis(300), 20);
}

/// [`restart`], but not from inside the event source being replaced.
fn restart_later<Bd: Backend + 'static>(state: &mut State<Bd>) {
    use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
    let timer = Timer::from_duration(std::time::Duration::from_millis(200));
    let _ = state.handle.insert_source(timer, |_, _, state| {
        restart(state);
        TimeoutAction::Drop
    });
}

/// Removes an X11 window — managed or not — from wherever it lives.
fn forget_window<Bd: Backend + 'static>(state: &mut State<Bd>, window: &Window) {
    if state.adapter.id_of(window).is_some() {
        crate::shell::unmap_window(state, window);
    } else {
        state.space.unmap_elem(window);
    }
    state.xwayland.unmanaged.retain(|w| w != window);
    state.backend_data.queue_redraw();
}

fn window_of<Bd: Backend + 'static>(state: &State<Bd>, surface: &X11Surface) -> Option<Window> {
    state.space.elements().find(|w| w.x11_surface() == Some(surface)).cloned()
}

/// Whether a window should be left to place itself: menus, tooltips,
/// notifications, drop-downs and override-redirect windows.
fn is_unmanaged(window: &X11Surface) -> bool {
    window.is_override_redirect()
        || window.is_popup()
        || matches!(
            window.window_type(),
            Some(WmWindowType::DropdownMenu | WmWindowType::Menu | WmWindowType::PopupMenu | WmWindowType::Tooltip | WmWindowType::Notification)
        )
}

/// What Smithay's `X11Surface` does not tell us about a window, read over a
/// short-lived second connection to the X display.
#[derive(Default)]
struct X11Extra {
    /// `_NET_WM_STRUT_PARTIAL`.
    struts: Option<bsp_core::wm::EwmhStruts>,
    /// `_NET_WM_WINDOW_TYPE` includes `_NET_WM_WINDOW_TYPE_DOCK`.
    dock: bool,
    /// `_NET_WM_WINDOW_TYPE` includes `_NET_WM_WINDOW_TYPE_DESKTOP`.
    desktop: bool,
}

/// Reads [`X11Extra`] of X11 window `window` from display `display`
/// (all-default if the server does not answer).
fn read_extra(display: u32, window: u32) -> X11Extra {
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};
    fn read(display: u32, window: u32) -> Option<X11Extra> {
        let (conn, _) = x11rb::rust_connection::RustConnection::connect(Some(&format!(":{display}"))).ok()?;
        let atom = |name: &[u8]| conn.intern_atom(true, name).ok()?.reply().ok().map(|r| r.atom).filter(|a| *a != 0);
        let mut extra = X11Extra::default();
        if let Some(strut) = atom(b"_NET_WM_STRUT_PARTIAL") {
            if let Ok(reply) = conn.get_property(false, window, strut, AtomEnum::CARDINAL, 0, 12).ok()?.reply() {
                let values: Vec<u32> = reply.value32().map(|v| v.collect()).unwrap_or_default();
                extra.struts = bsp_core::wm::EwmhStruts::from_cardinals(&values);
            }
        }
        if let Some(kind) = atom(b"_NET_WM_WINDOW_TYPE") {
            if let Ok(reply) = conn.get_property(false, window, kind, AtomEnum::ATOM, 0, 32).ok()?.reply() {
                let types: Vec<u32> = reply.value32().map(|v| v.collect()).unwrap_or_default();
                extra.dock = atom(b"_NET_WM_WINDOW_TYPE_DOCK").is_some_and(|a| types.contains(&a));
                extra.desktop = atom(b"_NET_WM_WINDOW_TYPE_DESKTOP").is_some_and(|a| types.contains(&a));
            }
        }
        Some(extra)
    }
    read(display, window).unwrap_or_default()
}

/// What an X11 window's `_NET_WM_WINDOW_TYPE` asks for, before any rule.
///
/// bspwm: `src/rule.c` `apply_rules()`: toolbar and utility windows are not
/// focused, dialogs float centred, docks/desktops/notifications are not managed.
fn type_defaults(window: &X11Surface, extra: &X11Extra) -> bsp_core::rules::RuleConsequence {
    use bsp_core::node::ClientState;
    let mut consequence = bsp_core::rules::RuleConsequence::default();
    // A game that maps already fullscreen (`_NET_WM_STATE_FULLSCREEN` in its initial state).
    if window.is_fullscreen() {
        consequence.state = Some(ClientState::Fullscreen);
    }
    if extra.dock || extra.desktop {
        consequence.manage = Some(false);
        return consequence;
    }
    match window.window_type() {
        Some(WmWindowType::Toolbar | WmWindowType::Utility) => consequence.focus = Some(false),
        Some(WmWindowType::Dialog) => {
            consequence.state = Some(ClientState::Floating);
            consequence.center = true;
        }
        Some(WmWindowType::Notification) => consequence.manage = Some(false),
        _ => {}
    }
    consequence
}

/// Shows an X11 `window` where it asked to be, outside the tree (a rule said
/// `manage=off`, or its type is dock/desktop/notification). `false` for a
/// window that is not an X11 one.
pub(crate) fn map_unmanaged<Bd: Backend + 'static>(state: &mut State<Bd>, window: &Window) -> bool {
    let Some(x11) = window.x11_surface() else {
        return false;
    };
    let geometry = x11.geometry();
    state.space.map_element(window.clone(), geometry.loc, true);
    state.xwayland.unmanaged.push(window.clone());
    state.backend_data.queue_redraw();
    true
}

/// Reserves the space a newly mapped X11 window's strut asks for
/// (`bspc config ignore_ewmh_struts` off) and re-tiles every desktop.
///
/// bspwm: `src/window.c` `manage_window()` / `src/events.c` `property_notify()`
/// call `ewmh_handle_struts()` and then `arrange()` every desktop.
fn apply_struts<Bd: Backend + 'static>(state: &mut State<Bd>, struts: Option<bsp_core::wm::EwmhStruts>) {
    if state.xwayland.ignore_struts {
        return;
    }
    let Some(struts) = struts else {
        return;
    };
    let screen = state
        .wm
        .monitors
        .iter()
        .fold((0, 0), |(w, h), m| (w.max(m.rectangle.x + m.rectangle.width), h.max(m.rectangle.y + m.rectangle.height)));
    if state.wm.apply_ewmh_struts(&struts, screen) {
        tracing::debug!(?struts, "X11 panel struts reserved");
        let settings = state.wm.settings.clone();
        for mi in 0..state.wm.monitors.len() {
            for di in 0..state.wm.monitors[mi].desktops.len() {
                state.wm.monitors[mi].arrange(di, &settings);
            }
        }
        crate::shell::sync_wayland_from_core(state);
    }
}

/// Whether any X11 window is still shown.
fn has_x11_windows<Bd: Backend + 'static>(state: &State<Bd>) -> bool {
    state.space.elements().any(|w| w.x11_surface().is_some())
}

/// Arms the lazy stop: if, [`IDLE_STOP_AFTER`] from now, no X11 window is shown
/// and no X11 client owns a selection, `Xwayland` is shut down. Re-arming
/// replaces a pending check.
fn schedule_idle_stop<Bd: Backend + 'static>(state: &mut State<Bd>) {
    use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
    if state.xwayland.xwm.is_none() {
        return;
    }
    if let Some(token) = state.xwayland.idle_timer.take() {
        state.handle.remove(token);
    }
    let timer = Timer::from_duration(IDLE_STOP_AFTER);
    tracing::debug!("Xwayland idle check armed");
    state.xwayland.idle_timer = state
        .handle
        .insert_source(timer, |_, _, state| {
            state.xwayland.idle_timer = None;
            tracing::debug!(
                windows = has_x11_windows(state),
                selection_owned = state.xwayland.selection_owned,
                running = state.xwayland.xwm.is_some(),
                "Xwayland idle check"
            );
            if state.xwayland.xwm.is_some() && !has_x11_windows(state) && !state.xwayland.selection_owned {
                tracing::info!("no X11 windows left; stopping Xwayland");
                restart(state);
            }
            TimeoutAction::Drop
        })
        .ok();
}

/// `bspc config ignore_ewmh_struts` and `ignore_ewmh_focus` (compositor-side
/// settings `bsp_ipc::exec` does not know). `Some` if `command` named one.
pub fn try_config<Bd: Backend + 'static>(state: &mut State<Bd>, command: &bsp_ipc::command::Command) -> Option<bsp_ipc::wire::Reply> {
    use bsp_ipc::wire::Reply;
    let bsp_ipc::command::Command::Config(c) = command else {
        return None;
    };
    let slot = match c.name.as_str() {
        "ignore_ewmh_struts" => &mut state.xwayland.ignore_struts,
        "ignore_ewmh_focus" => &mut state.xwayland.ignore_focus,
        _ => return None,
    };
    Some(match c.value.as_deref() {
        None => Reply::Ok(format!("{}\n", slot)),
        Some(v) => match bsp_ipc::value::parse_bool(v) {
            Some(b) => {
                *slot = b;
                Reply::Ok(String::new())
            }
            None => Reply::Fail(format!("config: {}: Invalid value: '{v}'.\n", c.name)),
        },
    })
}

impl<Bd: Backend + 'static> XWaylandShellHandler for State<Bd> {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.protocols._xwayland_shell
    }

    /// An X11 window's `wl_surface` exists only once Xwayland has created
    /// and associated it — after the map request. Keyboard focus that was
    /// meant for the window (`focus_node` at map time found no surface yet)
    /// is completed here.
    fn surface_associated(&mut self, _xwm: XwmId, _wl_surface: WlSurface, _surface: X11Surface) {
        crate::input::sync_keyboard_focus(self);
        self.backend_data.queue_redraw();
    }
}
smithay::delegate_xwayland_shell!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> XwmHandler for State<Bd> {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        // See the module's hard-rule note.
        match self.xwayland.xwm.as_mut() {
            Some(xwm) => xwm,
            None => unreachable!("XwmHandler::xwm_state called with no X11 window manager"),
        }
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Err(err) = window.set_mapped(true) {
            tracing::warn!("cannot map an X11 window: {err}");
            return;
        }
        let element = Window::new_x11_window(window.clone());
        let extra = self.xwayland.display.map(|display| read_extra(display, window.window_id())).unwrap_or_default();
        apply_struts(self, extra.struts);
        if is_unmanaged(&window) {
            let geometry = window.geometry();
            self.space.map_element(element.clone(), geometry.loc, true);
            self.xwayland.unmanaged.push(element);
        } else {
            crate::shell::map_new_window(self, element, window.class(), window.instance(), window.title(), type_defaults(&window, &extra));
        }
        self.backend_data.queue_redraw();
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        let struts = self.xwayland.display.and_then(|display| read_extra(display, window.window_id()).struts);
        apply_struts(self, struts);
        let element = Window::new_x11_window(window.clone());
        self.space.map_element(element.clone(), window.geometry().loc, true);
        self.xwayland.unmanaged.push(element);
        self.backend_data.queue_redraw();
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(element) = window_of(self, &window) {
            forget_window(self, &element);
        }
        schedule_idle_stop(self);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        if let Some(element) = window_of(self, &window) {
            forget_window(self, &element);
        }
        schedule_idle_stop(self);
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // A managed window's geometry is the tree's decision: answer with it.
        // Anything else gets what it asked for.
        let managed = window_of(self, &window).and_then(|el| self.adapter.id_of(&el));
        let rect = match managed.and_then(|id| crate::input::locate_window(self, id)) {
            Some((mi, di, node)) => self.wm.monitors[mi].desktops[di].tree.node(node).client.as_ref().map(|c| {
                let r = if c.state.is_tiled() { c.tiled_rectangle } else { c.floating_rectangle };
                Rectangle::<i32, Logical>::new((r.x, r.y).into(), (r.width.max(1), r.height.max(1)).into())
            }),
            None => None,
        };
        let geometry = rect.unwrap_or_else(|| {
            let current = window.geometry();
            Rectangle::new(
                (x.unwrap_or(current.loc.x), y.unwrap_or(current.loc.y)).into(),
                (w.map_or(current.size.w, |w| w as i32), h.map_or(current.size.h, |h| h as i32)).into(),
            )
        });
        if let Err(err) = window.configure(geometry) {
            tracing::debug!("cannot answer an X11 configure request: {err}");
        }
    }

    fn configure_notify(&mut self, _xwm: XwmId, window: X11Surface, geometry: Rectangle<i32, Logical>, _above: Option<X11Window>) {
        // Unmanaged windows move themselves.
        if let Some(element) = window_of(self, &window) {
            if self.xwayland.unmanaged.contains(&element) {
                self.space.map_element(element, geometry.loc, false);
                self.backend_data.queue_redraw();
            }
        }
    }

    fn property_notify(&mut self, _xwm: XwmId, window: X11Surface, property: WmWindowProperty) {
        if matches!(property, WmWindowProperty::Class) {
            if let Some(id) = window_of(self, &window).and_then(|el| self.adapter.id_of(&el)) {
                self.adapter.set_class(id, &window.class(), &window.instance());
            }
        }
        // Title changes reach taskbars through `crate::taskbar::sync`'s diffing.
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_state_request(&window, "fullscreen");
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        self.x11_state_request(&window, "tiled");
    }

    fn resize_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32, _edge: ResizeEdge) {}

    fn move_request(&mut self, _xwm: XwmId, _window: X11Surface, _button: u32) {}

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        true
    }

    /// An X11 client wants the Wayland selection's contents.
    fn send_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_type: String, fd: OwnedFd) {
        let seat = self.seat.clone();
        let result = match selection {
            SelectionTarget::Clipboard => request_data_device_client_selection(&seat, mime_type, fd).map_err(|e| e.to_string()),
            SelectionTarget::Primary => request_primary_client_selection(&seat, mime_type, fd).map_err(|e| e.to_string()),
        };
        if let Err(err) = result {
            tracing::debug!("X11 client asked for a Wayland selection that is not available: {err}");
        }
    }

    /// An X11 client set a selection: offer it to Wayland clients.
    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        self.xwayland.selection_owned = true;
        let (dh, seat) = (self.display_handle.clone(), self.seat.clone());
        match selection {
            SelectionTarget::Clipboard => set_data_device_selection(&dh, &seat, mime_types, ()),
            SelectionTarget::Primary => set_primary_selection(&dh, &seat, mime_types, ()),
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        self.xwayland.selection_owned = false;
        schedule_idle_stop(self);
        let (dh, seat) = (self.display_handle.clone(), self.seat.clone());
        match selection {
            SelectionTarget::Clipboard => clear_data_device_selection(&dh, &seat),
            SelectionTarget::Primary => clear_primary_selection(&dh, &seat),
        }
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        // Already stopped on purpose (`restart`): nothing to recover.
        if self.xwayland.xwm.is_none() {
            return;
        }
        restart_later(self);
    }
}

impl<Bd: Backend + 'static> State<Bd> {
    /// Runs `bspc node ID -t STATE` for a managed X11 window's request.
    fn x11_state_request(&mut self, window: &X11Surface, node_state: &str) {
        tracing::debug!(class = window.class(), node_state, "X11 window state request");
        let Some(id) = window_of(self, window).and_then(|el| self.adapter.id_of(&el)) else {
            return;
        };
        let Some((mi, di, node)) = crate::input::locate_window(self, id) else {
            return;
        };
        let desktop = self.wm.monitors[mi].desktops[di].id;
        let Some(wire_id) = self.registry.id_of(desktop, node) else {
            return;
        };
        let argv = vec!["node".to_string(), format!("0x{wire_id:08X}"), "-t".to_string(), node_state.to_string()];
        crate::protocols::run_bspc(self, &argv);
    }

    /// Raises every unmanaged X11 window (menus, tooltips) above the tiled
    /// windows: reconciling tiled windows must never bury a menu.
    pub fn raise_unmanaged_x11(&mut self) {
        let windows = self.xwayland.unmanaged.clone();
        for window in windows {
            self.space.raise_element(&window, false);
        }
    }

    /// Forwards a Wayland-side selection change to X11 clients.
    pub fn selection_to_xwm(&mut self, target: SelectionTarget, mime_types: Option<Vec<String>>) {
        if let Some(xwm) = self.xwayland.xwm.as_mut() {
            if let Err(err) = xwm.new_selection(target, mime_types) {
                tracing::debug!("cannot forward a selection to X11: {err}");
            }
        }
    }

    /// An asks-for-data request from a Wayland client for an X11 selection.
    pub fn selection_from_xwm(&mut self, target: SelectionTarget, mime_type: String, fd: OwnedFd) {
        let handle = self.handle.clone();
        if let Some(xwm) = self.xwayland.xwm.as_mut() {
            if let Err(err) = xwm.send_selection(target, mime_type, fd, handle) {
                tracing::debug!("cannot fetch an X11 selection: {err}");
            }
        }
    }

    /// Tells X11 windows which one has keyboard focus (`_NET_WM_STATE_FOCUSED`
    /// and input focus).
    pub fn activate_x11_focus(&mut self, focused: Option<&smithay::reexports::wayland_server::protocol::wl_surface::WlSurface>) {
        for window in self.space.elements() {
            if let Some(x11) = window.x11_surface() {
                let active = focused.is_some() && x11.wl_surface().as_ref() == focused;
                if x11.is_activated() != active {
                    let _ = x11.set_activated(active);
                }
            }
        }
    }
}
