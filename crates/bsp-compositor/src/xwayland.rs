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

use crate::xworker::{Job, Reply, X11Extra, XWorker};

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
    pub(crate) display: Option<u32>,
    /// The current (possibly still dormant) `Xwayland` instance's registration
    /// with the event loop, which owns the instance itself.
    token: Option<RegistrationToken>,
    /// The window manager connected to it.
    xwm: Option<X11Wm>,
    /// The size Xwayland emulates per window, as the X worker last reported it
    /// (asked again when a window reconfigures); `shell::sync_one_window` reads it
    /// on every sync.
    emulated: std::collections::HashMap<u32, Option<(i32, i32)>>,
    /// Windows whose emulated size is being asked for.
    emulated_asked: std::collections::HashSet<u32>,
    /// The thread that talks to the X server (`crate::xworker`), started with the
    /// first job, and where its answers arrive.
    worker: Option<XWorker>,
    replies: Option<smithay::reexports::calloop::channel::Sender<Reply>>,
    /// X11 windows waiting for their properties before they are managed, and
    /// override-redirect ones waiting to have their struts applied.
    pending: std::collections::HashMap<u32, Pending>,
    /// Windows managed without their properties because the X worker was too slow;
    /// a late answer only applies their struts.
    timed_out: std::collections::HashSet<u32>,
    /// X11 windows shown but not managed (override-redirect, menus, …).
    unmanaged: Vec<Window>,
    /// The desktop list last written on the root window (`publish_ewmh`).
    published_desktops: Option<(Vec<String>, u32)>,
    /// `bspc config ignore_ewmh_struts`: do not reserve space for panels.
    ignore_struts: bool,
    /// `bspc config ignore_ewmh_focus`: activation requests (`xdg_activation_v1`;
    /// Smithay's X11 window manager does not pass `_NET_ACTIVE_WINDOW` on) do
    /// not move focus.
    pub(crate) ignore_focus: bool,
    /// An X11 client owns the clipboard or primary selection; its window may
    /// be gone but the server must stay up to serve it.
    selection_owned: bool,
    /// The pending "no X11 windows left" check (`schedule_idle_stop`).
    idle_timer: Option<RegistrationToken>,
}

/// How long a new X11 window waits for its properties before it is managed without them.
const MAP_ANSWER_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// What the X worker's answer to [`Job::Extra`] completes.
enum Pending {
    /// A managed window that maps once its type and state are known.
    Map(Box<X11Surface>),
    /// An override-redirect window (already shown) whose struts are still unknown.
    Struts,
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
///
/// One directory per compositor process (`bspwm-rs-xwayland-shim-<pid>`): a
/// second instance on another VT, possibly another build, must not repoint the
/// first one's shim. Removed again by [`remove_shim_dir`].
fn shim_dir() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let dir = PathBuf::from(runtime).join(format!("{SHIM_DIR_PREFIX}{}", std::process::id()));
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
    let real = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .filter(|dir| !is_shim_dir(dir))
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

/// The start of every instance's shim directory name.
const SHIM_DIR_PREFIX: &str = "bspwm-rs-xwayland-shim-";

/// Whether `dir` is a shim directory (of any instance).
fn is_shim_dir(dir: &std::path::Path) -> bool {
    dir.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(SHIM_DIR_PREFIX) || n == "bspwm-rs-xwayland-shim")
}

/// Removes this process's shim directory; call on exit.
pub fn remove_shim_dir() {
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(runtime).join(format!("{SHIM_DIR_PREFIX}{}", std::process::id()));
        let _ = std::fs::remove_file(dir.join(SHIM_NAME));
        let _ = std::fs::remove_dir(&dir);
    }
}

/// Starts the lazy `Xwayland` machinery: claims a display, exports `DISPLAY`
/// and leaves the dormant shim waiting. Call once at startup, before
/// `bspwmrc` runs (its children inherit `DISPLAY`). Logs and continues
/// without X11 support on failure.
pub fn init<Bd: Backend + 'static>(state: &mut State<Bd>) {
    use smithay::reexports::calloop::channel::{channel, Event};
    // The X worker's answers come back through this channel.
    let (tx, rx) = channel::<Reply>();
    match state.handle.insert_source(rx, |event, _, state| {
        if let Event::Msg(reply) = event {
            handle_reply(state, reply);
        }
    }) {
        Ok(_) => state.xwayland.replies = Some(tx),
        Err(err) => tracing::warn!("cannot register the X worker channel: {err}"),
    }
    let _ = spawn(state);
}

/// Sends `job` to the X worker (starting it on the first call). `false` if there
/// is no X display or worker to send it to.
fn submit<Bd: Backend + 'static>(state: &mut State<Bd>, job: Job) -> bool {
    let Some(display) = state.xwayland.display else {
        return false;
    };
    if state.xwayland.worker.is_none() {
        let Some(replies) = state.xwayland.replies.clone() else {
            return false;
        };
        state.xwayland.worker = crate::xworker::spawn(display, replies);
    }
    state.xwayland.worker.as_ref().is_some_and(|w| w.submit(job))
}

/// The X worker answered.
fn handle_reply<Bd: Backend + 'static>(state: &mut State<Bd>, reply: Reply) {
    match reply {
        Reply::Extra(window, extra) => match state.xwayland.pending.remove(&window) {
            Some(Pending::Map(surface)) => {
                // Closed while we were asking: nothing to map.
                if surface.alive() {
                    finish_map(state, *surface, extra);
                }
            }
            Some(Pending::Struts) => apply_struts(state, extra.struts),
            None if state.xwayland.timed_out.remove(&window) => apply_struts(state, extra.struts),
            None => {}
        },
        Reply::Clients(clients) => idle_check(state, clients),
        Reply::Emulated(window, size) => {
            state.xwayland.emulated_asked.remove(&window);
            if state.xwayland.emulated.insert(window, size) != Some(size) {
                // A fullscreen window may now have to be resized.
                crate::shell::sync_wayland_from_core(state);
            }
        }
    }
}

/// Kills the X11 client that owns `window`, as `XKillClient` does; the worker
/// does it and logs a failure.
///
/// bspwm: `src/tree.c` `kill_node()` (`xcb_kill_client`).
pub fn kill_client<Bd: Backend + 'static>(state: &mut State<Bd>, window: u32) -> bool {
    submit(state, Job::Kill(window))
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
    // The worker's connection is to the instance that is going away (and would keep a
    // dormant one from being started by anything but a client): drop it, a new worker
    // starts with the next job.
    state.xwayland.worker = None;
    state.xwayland.pending.clear();
    state.xwayland.timed_out.clear();
    state.xwayland.published_desktops = None;
    state.xwayland.emulated.clear();
    state.xwayland.emulated_asked.clear();
    if let Some(token) = state.xwayland.idle_timer.take() {
        state.handle.remove(token);
    }
    if let Some(token) = state.xwayland.token.take() {
        state.handle.remove(token);
    }
    // Removing the source above dropped the `XWayland` handle, which
    // released the display's lock file and sockets.
    // Every X11 window goes, shown or on a hidden desktop.
    let mut windows: Vec<Window> = Vec::new();
    for w in state.space.elements().chain(state.adapter.windows()) {
        if w.x11_surface().is_some() && !windows.contains(w) {
            windows.push(w.clone());
        }
    }
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

/// The window of `surface`: shown (in the space) or managed on a desktop that
/// is not shown (only in the adapter). Looking in the space alone missed the
/// latter, so a window closed on a hidden desktop was never forgotten: it kept
/// its node and stayed in taskbars and the screen-share window list.
fn window_of<Bd: Backend + 'static>(state: &State<Bd>, surface: &X11Surface) -> Option<Window> {
    state
        .space
        .elements()
        .chain(state.adapter.windows())
        .find(|w| w.x11_surface() == Some(surface))
        .cloned()
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

/// Manages (or shows, if it is a menu or the like) an X11 window whose
/// properties `extra` are now known.
fn finish_map<Bd: Backend + 'static>(state: &mut State<Bd>, window: X11Surface, extra: X11Extra) {
    let element = Window::new_x11_window(window.clone());
    apply_struts(state, extra.struts);
    if is_unmanaged(&window) {
        let geometry = window.geometry();
        state.space.map_element(element.clone(), geometry.loc, true);
        state.xwayland.unmanaged.push(element);
    } else {
        crate::shell::map_new_window(state, element, window.class(), window.instance(), window.title(), type_defaults(&window, &extra));
    }
    state.backend_data.queue_redraw();
}

/// What an X11 window's `_NET_WM_WINDOW_TYPE` asks for, before any rule.
///
/// bspwm: `src/rule.c` `apply_rules()`: toolbar and utility windows are not
/// focused, dialogs float centred, docks/desktops/notifications are not managed.
fn type_defaults(window: &X11Surface, extra: &X11Extra) -> bsp_core::rules::RuleConsequence {
    use bsp_core::node::{ClientState, Layer};
    let mut consequence = bsp_core::rules::RuleConsequence::default();
    // Window type first (bspwm: `_apply_window_type()`).
    if extra.dock || extra.desktop {
        consequence.manage = Some(false);
        return consequence;
    }
    match window.window_type() {
        Some(WmWindowType::Toolbar | WmWindowType::Utility) => consequence.focus = Some(false),
        Some(WmWindowType::Dialog) => {
            consequence.state = Some(ClientState::Floating);
            consequence.center = Some(true);
        }
        Some(WmWindowType::Notification) => consequence.manage = Some(false),
        _ => {}
    }
    // Then `_NET_WM_STATE` (`_apply_window_state()`): a game that maps already
    // fullscreen, or asks to be above/below/sticky.
    if window.is_fullscreen() {
        consequence.state = Some(ClientState::Fullscreen);
    }
    if extra.below {
        consequence.layer = Some(Layer::Below);
    } else if extra.above {
        consequence.layer = Some(Layer::Above);
    }
    if extra.sticky {
        consequence.sticky = Some(true);
    }
    // A transient window floats (`_apply_transient()`), and so does one that
    // cannot be resized (`_apply_hints()`: minimum size equals maximum size).
    if window.is_transient_for().is_some() || fixed_size(window.min_size(), window.max_size()) {
        consequence.state = Some(ClientState::Floating);
    }
    consequence
}

/// Whether a window's minimum and maximum size are the same, non-empty size.
pub(crate) fn fixed_size(min: Option<smithay::utils::Size<i32, Logical>>, max: Option<smithay::utils::Size<i32, Logical>>) -> bool {
    matches!((min, max), (Some(min), Some(max)) if min == max && min.w > 0 && min.h > 0)
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

/// What was last written on a managed X11 window: its `_NET_WM_STATE` and
/// `_NET_WM_DESKTOP`.
struct PublishedEwmh(std::cell::Cell<Option<(crate::xworker::NetState, u32)>>);

/// Keeps the EWMH properties X11 clients read in step with the tree: every
/// managed window's `_NET_WM_STATE` (fullscreen, sticky, hidden, above, below,
/// demands attention, focused) and `_NET_WM_DESKTOP`, and the root window's
/// desktop list. Only what changed is written, by the X worker. Nothing while
/// Xwayland is not running (writing would start it).
///
/// bspwm: `src/ewmh.c` `ewmh_wm_state_update()`, `ewmh_set_wm_desktop()`,
/// `ewmh_update_number_of_desktops()`, `ewmh_update_desktop_names()`,
/// `ewmh_update_current_desktop()`.
pub(crate) fn publish_ewmh<Bd: Backend + 'static>(state: &mut State<Bd>) {
    use crate::xworker::NetState;
    if state.xwayland.xwm.is_none() {
        return;
    }
    // Desktops are numbered across every monitor, in order (bspwm's EWMH index).
    let mut names = Vec::new();
    let mut current = 0u32;
    let mut per_window: std::collections::HashMap<bsp_core::id::WindowId, (NetState, u32)> = std::collections::HashMap::new();
    let focused_window = state.wm.focused_monitor.and_then(|mi| {
        let m = &state.wm.monitors[mi];
        let t = &m.desktops[m.focused?].tree;
        t.node(t.focus?).client.as_ref().map(|c| c.window)
    });
    for (mi, m) in state.wm.monitors.iter().enumerate() {
        for (di, d) in m.desktops.iter().enumerate() {
            let index = names.len() as u32;
            names.push(d.name.clone());
            if state.wm.focused_monitor == Some(mi) && m.focused == Some(di) {
                current = index;
            }
            for n in d.tree.node_ids() {
                let node = d.tree.node(n);
                let Some(c) = node.client.as_ref() else { continue };
                let net = NetState {
                    fullscreen: c.state == bsp_core::node::ClientState::Fullscreen,
                    sticky: node.sticky,
                    hidden: node.hidden,
                    above: c.layer == bsp_core::node::Layer::Above,
                    below: c.layer == bsp_core::node::Layer::Below,
                    demands_attention: c.urgent,
                    focused: focused_window == Some(c.window),
                };
                per_window.insert(c.window, (net, index));
            }
        }
    }
    if state.xwayland.published_desktops.as_ref() != Some(&(names.clone(), current)) {
        state.xwayland.published_desktops = Some((names.clone(), current));
        submit(state, Job::SetDesktops { names, current });
    }
    let windows: Vec<(bsp_core::id::WindowId, Window)> = state
        .adapter
        .windows()
        .filter(|w| w.x11_surface().is_some_and(|x| !x.is_override_redirect()))
        .filter_map(|w| state.adapter.id_of(w).map(|id| (id, w.clone())))
        .collect();
    for (id, window) in windows {
        let (Some(x11), Some(&wanted)) = (window.x11_surface(), per_window.get(&id)) else { continue };
        let published = window.user_data().get_or_insert(|| PublishedEwmh(std::cell::Cell::new(None)));
        let last = published.0.get();
        if last == Some(wanted) {
            continue;
        }
        published.0.set(Some(wanted));
        let xid = x11.window_id();
        if last.map(|l| l.0) != Some(wanted.0) {
            submit(state, Job::SetNetState(xid, wanted.0));
        }
        if last.map(|l| l.1) != Some(wanted.1) {
            submit(state, Job::SetWmDesktop(xid, wanted.1));
        }
    }
}

/// Forgets managed X11 windows whose X window is gone although no destroy or
/// unmap reached us, so none lingers in the tree, taskbars or the screen-share
/// window list. A safety net; run with the periodic syncs.
pub(crate) fn reap_dead<Bd: Backend + 'static>(state: &mut State<Bd>) {
    let dead: Vec<Window> = state
        .adapter
        .windows()
        .filter(|w| w.x11_surface().is_some_and(|x| !x.alive()))
        .cloned()
        .collect();
    for window in dead {
        tracing::info!("forgetting an X11 window that is gone");
        forget_window(state, &window);
    }
}

/// Whether any X11 window is still shown or managed (on a hidden desktop too).
fn has_x11_windows<Bd: Backend + 'static>(state: &State<Bd>) -> bool {
    state.space.elements().chain(state.adapter.windows()).any(|w| w.x11_surface().is_some())
}


/// Decides, once the X worker has counted the clients still connected, whether
/// the idle `Xwayland` stops (`clients`: `None` if it could not tell).
fn idle_check<Bd: Backend + 'static>(state: &mut State<Bd>, clients: Option<usize>) {
    // Things may have changed while the X worker was answering.
    if state.xwayland.xwm.is_some() && !has_x11_windows(state) && !state.xwayland.selection_owned {
        // Unknown (the X worker could not tell) counts as connected: stopping
        // Xwayland kills every X11 client, which is not a guess to make.
        if clients.is_none_or(|n| n > 0) {
            tracing::debug!(clients, "X11 clients without windows are connected (or unknown); keeping Xwayland");
            schedule_idle_stop(state);
        } else {
            tracing::info!("no X11 windows or clients left; stopping Xwayland");
            restart(state);
        }
    }
}

/// Arms the lazy stop: if, [`IDLE_STOP_AFTER`] from now, no X11 window is shown,
/// no X11 client is connected and none owns a selection, `Xwayland` is shut down. Re-arming
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
            // Only if nothing shows a window or owns a selection is it worth asking who
            // is still connected; the answer arrives as a message (`idle_check`).
            if state.xwayland.xwm.is_some() && !has_x11_windows(state) && !state.xwayland.selection_owned && !submit(state, Job::Clients) {
                idle_check(state, None);
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
    fn surface_associated(&mut self, _xwm: XwmId, wl_surface: WlSurface, _surface: X11Surface) {
        crate::input::sync_keyboard_focus(self);
        // An X11 window shown again (its desktop switched to) has no buffer until
        // its next commit, so nothing is under the pointer yet: look again then,
        // or the first click would need a motion first.
        crate::shell::recheck_pointer_on_commit(&wl_surface);
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
        // Its type, state and struts come from the X server without waiting for
        // it here; the window is managed when they arrive (`finish_map`).
        let id = window.window_id();
        self.xwayland.pending.insert(id, Pending::Map(Box::new(window.clone())));
        if !submit(self, Job::Extra(id)) {
            self.xwayland.pending.remove(&id);
            finish_map(self, window, X11Extra::default());
            return;
        }
        // If the X server does not answer in time (stalled, connection broken), the
        // window is managed without its properties instead of staying invisible,
        // and the worker, which may be stuck, is replaced.
        use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
        let _ = self.handle.insert_source(Timer::from_duration(MAP_ANSWER_TIMEOUT), move |_, _, state| {
            if let Some(Pending::Map(surface)) = state.xwayland.pending.remove(&id) {
                tracing::warn!(window = id, "the X server did not answer in time; managing the window without its properties");
                state.xwayland.worker = None;
                state.xwayland.timed_out.insert(id);
                finish_map(state, *surface, X11Extra::default());
            }
            TimeoutAction::Drop
        });
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, window: X11Surface) {
        // Shown at once; a strut it may carry is applied when the answer arrives.
        let id = window.window_id();
        let element = Window::new_x11_window(window.clone());
        self.space.map_element(element.clone(), window.geometry().loc, true);
        self.xwayland.unmanaged.push(element);
        self.backend_data.queue_redraw();
        self.xwayland.pending.insert(id, Pending::Struts);
        if !submit(self, Job::Extra(id)) {
            self.xwayland.pending.remove(&id);
        }
    }

    fn unmapped_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.xwayland.emulated.remove(&window.window_id());
        self.xwayland.timed_out.remove(&window.window_id());
        self.xwayland.pending.remove(&window.window_id());
        if let Some(element) = window_of(self, &window) {
            forget_window(self, &element);
        }
        schedule_idle_stop(self);
    }

    fn destroyed_window(&mut self, _xwm: XwmId, window: X11Surface) {
        self.xwayland.emulated.remove(&window.window_id());
        self.xwayland.timed_out.remove(&window.window_id());
        self.xwayland.pending.remove(&window.window_id());
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
                let r = c.shown_rectangle();
                Rectangle::<i32, Logical>::new((r.x, r.y).into(), (r.width.max(1), r.height.max(1)).into())
            }),
            None => None,
        };
        tracing::debug!(class = window.class(), ?x, ?y, ?w, ?h, fullscreen = window.is_fullscreen(), managed_rect = ?rect, "X11 configure request");
        // A fullscreen game that changed the video mode keeps its emulated size.
        let emulated = if window.is_fullscreen() {
            // A reconfigure may follow a mode change: answer with what is known and
            // ask again; a different answer re-syncs the window.
            self.request_emulated_size(window.window_id());
            self.emulated_size_cached(window.window_id())
        } else {
            None
        };
        let rect = rect.map(|r| match emulated {
            Some((w, h)) if w <= r.size.w && h <= r.size.h => Rectangle::new(r.loc, (w, h).into()),
            _ => r,
        });
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
        // bspwm: `property_notify()` on `WM_HINTS`: the urgency hint makes the
        // node urgent (unless it is the focused one, which `set_urgent` checks).
        if matches!(property, WmWindowProperty::Hints) && window.hints().is_some_and(|h| h.urgent) {
            if let Some((mi, di, node)) = window_of(self, &window)
                .and_then(|el| self.adapter.id_of(&el))
                .and_then(|id| crate::input::locate_window(self, id))
            {
                let trg = bsp_ipc::exec::Coordinates { monitor: mi, desktop: di, node: Some(node) };
                crate::ipc::with_ops(self, |ctx, events| bsp_ipc::exec::set_urgent(ctx, trg, true, events));
            }
        }
        // bspwm: `property_notify()` re-reads `WM_NORMAL_HINTS`.
        if matches!(property, WmWindowProperty::NormalHints) {
            if let Some(el) = window_of(self, &window) {
                if crate::shell::refresh_size_hints(self, &el) {
                    self.request_sync();
                }
            }
        }
        // Title changes reach taskbars through `crate::taskbar::sync`'s diffing.
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        // `ignore_ewmh_fullscreen enter`.
        if self.wm.settings.ignore_ewmh_fullscreen.enter {
            return;
        }
        self.x11_state_request(&window, "fullscreen");
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, window: X11Surface) {
        // `ignore_ewmh_fullscreen exit`.
        if self.wm.settings.ignore_ewmh_fullscreen.exit {
            return;
        }
        // bspwm: `_NET_WM_STATE` remove goes back to `last_state`, and only if
        // the window is fullscreen at all (a floating window stays floating).
        let fullscreen = window_of(self, &window)
            .and_then(|el| self.adapter.id_of(&el))
            .and_then(|id| crate::input::locate_window(self, id))
            .and_then(|(mi, di, node)| self.wm.monitors[mi].desktops[di].tree.node(node).client.as_ref().map(|c| c.state))
            == Some(bsp_core::node::ClientState::Fullscreen);
        if fullscreen {
            self.x11_state_request(&window, "~fullscreen");
        }
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
    /// The size Xwayland emulates for X11 window `window`, as last reported by the
    /// X worker (`None` until it has answered, or if the window has none). Asks
    /// for it the first time; the answer re-syncs the windows.
    pub fn emulated_size_cached(&mut self, window: u32) -> Option<(i32, i32)> {
        if !self.xwayland.emulated.contains_key(&window) {
            self.request_emulated_size(window);
        }
        self.xwayland.emulated.get(&window).copied().flatten()
    }

    /// Asks the X worker for window `window`'s emulated size, unless it is already asked.
    fn request_emulated_size(&mut self, window: u32) {
        if self.xwayland.emulated_asked.insert(window) && !submit(self, Job::Emulated(window)) {
            self.xwayland.emulated_asked.remove(&window);
        }
    }

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
