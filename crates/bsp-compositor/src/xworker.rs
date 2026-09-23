//! Reading X11 window properties without ever waiting for Xwayland on the
//! compositor thread.
//!
//! Xwayland is a Wayland client of this compositor. If the compositor blocked on
//! an X reply while Xwayland was itself blocked on a Wayland reply from the
//! compositor, both would hang for good; and every blocking round trip (a fresh
//! connection each time, before) is a stall of every client. So the X requests
//! run on one worker thread that keeps one connection to the display, and its
//! answers come back to the event loop as messages (`calloop::channel`).

use std::sync::mpsc;

use smithay::reexports::calloop::channel::Sender;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt};
use x11rb::rust_connection::RustConnection;

/// What Smithay's `X11Surface` does not tell us about a window.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct X11Extra {
    /// `_NET_WM_STRUT_PARTIAL`.
    pub struts: Option<bsp_core::wm::EwmhStruts>,
    /// `_NET_WM_WINDOW_TYPE` includes `_NET_WM_WINDOW_TYPE_DOCK`.
    pub dock: bool,
    /// `_NET_WM_WINDOW_TYPE` includes `_NET_WM_WINDOW_TYPE_DESKTOP`.
    pub desktop: bool,
    /// `_NET_WM_STATE` includes `_NET_WM_STATE_ABOVE`.
    pub above: bool,
    /// `_NET_WM_STATE` includes `_NET_WM_STATE_BELOW`.
    pub below: bool,
    /// `_NET_WM_STATE` includes `_NET_WM_STATE_STICKY`.
    pub sticky: bool,
}

/// The `_NET_WM_STATE` of a managed window, as bspwm writes it.
///
/// bspwm: `src/ewmh.c` `ewmh_wm_state_update()`; `focused` is the newer
/// `_NET_WM_STATE_FOCUSED` that Xwayland clients (Chromium, Electron) read.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NetState {
    /// `_NET_WM_STATE_FULLSCREEN`.
    pub fullscreen: bool,
    /// `_NET_WM_STATE_STICKY`.
    pub sticky: bool,
    /// `_NET_WM_STATE_HIDDEN` (the node is hidden).
    pub hidden: bool,
    /// `_NET_WM_STATE_ABOVE`.
    pub above: bool,
    /// `_NET_WM_STATE_BELOW`.
    pub below: bool,
    /// `_NET_WM_STATE_DEMANDS_ATTENTION` (the node is urgent).
    pub demands_attention: bool,
    /// `_NET_WM_STATE_FOCUSED`.
    pub focused: bool,
}

/// A request to the worker.
#[derive(Debug)]
pub enum Job {
    /// Read [`X11Extra`] of the window.
    Extra(u32),
    /// Read the size Xwayland emulates for the window.
    Emulated(u32),
    /// `XKillClient` on the window's owner (bspwm: `xcb_kill_client`).
    Kill(u32),
    /// Count the X11 clients connected other than this compositor.
    Clients,
    /// Replace the window's `_NET_WM_STATE`.
    SetNetState(u32, NetState),
    /// The desktop list on the root window: `_NET_NUMBER_OF_DESKTOPS`,
    /// `_NET_DESKTOP_NAMES`, `_NET_CURRENT_DESKTOP` (bspwm `ewmh_update_*`).
    SetDesktops {
        /// Every desktop's name, all monitors in order.
        names: Vec<String>,
        /// The index of the focused desktop in `names`.
        current: u32,
    },
    /// The window's `_NET_WM_DESKTOP` (bspwm `ewmh_set_wm_desktop()`).
    SetWmDesktop(u32, u32),
}

/// A worker's answer.
#[derive(Debug)]
pub enum Reply {
    /// [`Job::Extra`]'s result (all-default if the server did not answer).
    Extra(u32, X11Extra),
    /// [`Job::Emulated`]'s result.
    Emulated(u32, Option<(i32, i32)>),
    /// [`Job::Clients`]'s result (`None` if the server did not say).
    Clients(Option<usize>),
}

/// The handle the compositor keeps: jobs go in here.
pub struct XWorker {
    jobs: mpsc::Sender<Job>,
}

impl XWorker {
    /// Queues `job`. `false` if the worker is gone.
    pub fn submit(&self, job: Job) -> bool {
        self.jobs.send(job).is_ok()
    }
}

/// Starts the worker for X display `display`; its answers are sent on `replies`.
/// It connects on the first job, and again after a failed one (Xwayland may have
/// been restarted). It ends when the returned handle is dropped.
pub fn spawn(display: u32, replies: Sender<Reply>) -> Option<XWorker> {
    let (jobs, incoming) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("x11-worker".into())
        .spawn(move || {
            let mut conn: Option<RustConnection> = None;
            for job in incoming {
                if conn.is_none() {
                    conn = RustConnection::connect(Some(&format!(":{display}"))).ok().map(|(c, _)| c);
                }
                let Some(c) = conn.as_ref() else {
                    // No server to ask: the default answer keeps the caller going.
                    let _ = match job {
                        Job::Extra(w) => replies.send(Reply::Extra(w, X11Extra::default())),
                        Job::Emulated(w) => replies.send(Reply::Emulated(w, None)),
                        Job::Clients => replies.send(Reply::Clients(None)),
                        Job::Kill(_) | Job::SetNetState(..) | Job::SetDesktops { .. } | Job::SetWmDesktop(..) => Ok(()),
                    };
                    continue;
                };
                let (reply, broken) = match job {
                    Job::Extra(w) => match read_extra(c, w) {
                        Some(extra) => (Some(Reply::Extra(w, extra)), false),
                        None => (Some(Reply::Extra(w, X11Extra::default())), true),
                    },
                    Job::Emulated(w) => match emulated_size(c, w) {
                        Ok(size) => (Some(Reply::Emulated(w, size)), false),
                        Err(()) => (Some(Reply::Emulated(w, None)), true),
                    },
                    Job::Clients => {
                        let count = foreign_x_clients(c);
                        (Some(Reply::Clients(count)), count.is_none())
                    }
                    Job::SetNetState(w, net) => (None, write_net_state(c, w, net).is_none()),
                    Job::SetDesktops { names, current } => (None, write_desktops(c, &names, current).is_none()),
                    Job::SetWmDesktop(w, desktop) => (None, write_cardinal(c, w, "_NET_WM_DESKTOP", desktop).is_none()),
                    Job::Kill(w) => {
                        let killed = c.kill_client(w).ok().and_then(|cookie| cookie.check().ok());
                        if killed.is_none() {
                            tracing::warn!(window = w, "cannot kill an X11 client");
                        }
                        (None, killed.is_none())
                    }
                };
                if broken {
                    conn = None;
                }
                if let Some(reply) = reply {
                    if replies.send(reply).is_err() {
                        return;
                    }
                }
            }
        })
        .ok()?;
    Some(XWorker { jobs })
}

fn intern(conn: &RustConnection, name: &str) -> Option<u32> {
    Some(conn.intern_atom(false, name.as_bytes()).ok()?.reply().ok()?.atom)
}

/// Writes `_NET_WM_STATE` (`None` if the server did not take it).
fn write_net_state(conn: &RustConnection, window: u32, net: NetState) -> Option<()> {
    use x11rb::connection::Connection;
    use x11rb::wrapper::ConnectionExt as _;
    let wanted = [
        (net.fullscreen, "_NET_WM_STATE_FULLSCREEN"),
        (net.sticky, "_NET_WM_STATE_STICKY"),
        (net.hidden, "_NET_WM_STATE_HIDDEN"),
        (net.above, "_NET_WM_STATE_ABOVE"),
        (net.below, "_NET_WM_STATE_BELOW"),
        (net.demands_attention, "_NET_WM_STATE_DEMANDS_ATTENTION"),
        (net.focused, "_NET_WM_STATE_FOCUSED"),
    ];
    let mut atoms = Vec::new();
    for (on, name) in wanted {
        if on {
            atoms.push(intern(conn, name)?);
        }
    }
    let property = intern(conn, "_NET_WM_STATE")?;
    conn.change_property32(x11rb::protocol::xproto::PropMode::REPLACE, window, property, AtomEnum::ATOM, &atoms).ok()?;
    conn.flush().ok()
}

/// Writes one `CARDINAL` property.
fn write_cardinal(conn: &RustConnection, window: u32, name: &str, value: u32) -> Option<()> {
    use x11rb::connection::Connection;
    use x11rb::wrapper::ConnectionExt as _;
    let property = intern(conn, name)?;
    conn.change_property32(x11rb::protocol::xproto::PropMode::REPLACE, window, property, AtomEnum::CARDINAL, &[value]).ok()?;
    conn.flush().ok()
}

/// Writes the desktop list on the root window.
fn write_desktops(conn: &RustConnection, names: &[String], current: u32) -> Option<()> {
    use x11rb::connection::Connection;
    use x11rb::wrapper::ConnectionExt as _;
    let root = conn.setup().roots.first()?.root;
    write_cardinal(conn, root, "_NET_NUMBER_OF_DESKTOPS", names.len() as u32)?;
    write_cardinal(conn, root, "_NET_CURRENT_DESKTOP", current)?;
    let utf8 = intern(conn, "UTF8_STRING")?;
    let property = intern(conn, "_NET_DESKTOP_NAMES")?;
    let mut bytes = Vec::new();
    for name in names {
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
    }
    conn.change_property8(x11rb::protocol::xproto::PropMode::REPLACE, root, property, utf8, &bytes).ok()?;
    conn.flush().ok()
}

/// How many X11 clients other than this compositor are connected, counted with
/// the X Resource extension by client process id (`None` if the server does not
/// answer, or does not have the extension).
///
/// A client can hold an X connection without ever mapping a window: Firefox
/// opens one for WebRTC screen capture. Stopping Xwayland under such a client
/// kills it with an XIO error.
fn foreign_x_clients(conn: &RustConnection) -> Option<usize> {
    use x11rb::protocol::res::{ClientIdMask, ClientIdSpec, ConnectionExt};
    let spec = ClientIdSpec { client: 0, mask: ClientIdMask::LOCAL_CLIENT_PID };
    let reply = conn.res_query_client_ids(&[spec]).ok()?.reply().ok()?;
    let own = std::process::id();
    // A client whose pid is unknown (not reported) counts as foreign.
    // Entry 0 is the X server itself, not a client of it.
    Some(reply.ids.iter().filter(|id| id.spec.client != 0 && id.value.first().copied() != Some(own)).count())
}

/// Reads [`X11Extra`] of window `window`; `None` if the connection failed.
fn read_extra(conn: &RustConnection, window: u32) -> Option<X11Extra> {
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
    if let Some(state) = atom(b"_NET_WM_STATE") {
        if let Ok(reply) = conn.get_property(false, window, state, AtomEnum::ATOM, 0, 32).ok()?.reply() {
            let states: Vec<u32> = reply.value32().map(|v| v.collect()).unwrap_or_default();
            let has = |name: &[u8]| atom(name).is_some_and(|a| states.contains(&a));
            extra.above = has(b"_NET_WM_STATE_ABOVE");
            extra.below = has(b"_NET_WM_STATE_BELOW");
            extra.sticky = has(b"_NET_WM_STATE_STICKY");
        }
    }
    Some(extra)
}

/// The size Xwayland emulates for `window`: a game that changed the video mode
/// (XRandR, or the old VidMode) sees a screen of that size, and Xwayland scales
/// a fullscreen window of exactly that size up to the real output. It says so
/// in `_XWAYLAND_RANDR_EMU_MONITOR_RECTS` (x, y, width, height per monitor), and
/// the window must then keep that size instead of being stretched to the output.
/// `Err` if the connection failed; `Ok(None)` if the window has no such property.
fn emulated_size(conn: &RustConnection, window: u32) -> Result<Option<(i32, i32)>, ()> {
    let atom = conn.intern_atom(true, b"_XWAYLAND_RANDR_EMU_MONITOR_RECTS").map_err(|_| ())?.reply().map_err(|_| ())?.atom;
    if atom == 0 {
        return Ok(None);
    }
    let Ok(Ok(reply)) = conn.get_property(false, window, atom, AtomEnum::CARDINAL, 0, 4).map(|c| c.reply()) else {
        // A window that is already gone answers with an error: no size, not a broken connection.
        return Ok(None);
    };
    let values: Vec<u32> = reply.value32().map(|v| v.collect()).unwrap_or_default();
    Ok(match values[..] {
        [_, _, w, h] if w > 0 && h > 0 => Some((w as i32, h as i32)),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_worker_with_no_server_still_answers_every_job() {
        // Display 9999 does not exist: connecting fails, and the compositor must
        // still get the default reply it is waiting for (a window is pending on it).
        let (tx, rx) = smithay::reexports::calloop::channel::channel::<Reply>();
        let worker = spawn(9999, tx).unwrap();
        assert!(worker.submit(Job::Extra(7)));
        assert!(worker.submit(Job::Emulated(8)));
        assert!(worker.submit(Job::Kill(9)));
        // `Channel` is an event source; drain it through a tiny loop.
        let mut event_loop: smithay::reexports::calloop::EventLoop<Vec<String>> = smithay::reexports::calloop::EventLoop::try_new().unwrap();
        event_loop
            .handle()
            .insert_source(rx, |event, _, seen| {
                if let smithay::reexports::calloop::channel::Event::Msg(reply) = event {
                    seen.push(format!("{reply:?}"));
                }
            })
            .unwrap();
        let mut seen = Vec::new();
        for _ in 0..50 {
            event_loop.dispatch(Some(std::time::Duration::from_millis(20)), &mut seen).unwrap();
            if seen.len() >= 2 {
                break;
            }
        }
        assert!(seen.iter().any(|s| s.starts_with("Extra(7,")), "{seen:?}");
        assert!(seen.iter().any(|s| s.starts_with("Emulated(8, None)")), "{seen:?}");
    }
}
