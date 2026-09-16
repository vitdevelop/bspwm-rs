//! `wlr-foreign-toplevel-management`: the protocol taskbars and window
//! switchers (waybar's `wlr/taskbar`, rofi's window mode) use to list every
//! window, show its title/app id/state, and ask the compositor to focus,
//! close, hide or fullscreen it.
//!
//! bspwm has no such protocol — a panel gets the same information from
//! EWMH atoms (`_NET_CLIENT_LIST`, `_NET_WM_STATE`, `_NET_ACTIVE_WINDOW`),
//! which Wayland lacks (`docs/design.md`, Compatibility). Every request is
//! turned into the equivalent `bspc node` command, so it goes through
//! exactly the code a hotkey or `bspc` would.
//!
//! State is *diffed*, not event-driven: [`sync`] runs after every
//! event-loop turn, compares each window's title, app id, state and output
//! with what clients were last told and sends only the differences. That
//! catches every cause (focus by click, hotkey, `bspc`, a client renaming
//! itself) in one place.

use std::collections::HashMap;

use smithay::desktop::Window;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use bsp_core::id::WindowId;
use bsp_core::node::ClientState;

use crate::state::{Backend, State};

/// `zwlr_foreign_toplevel_handle_v1.state` values.
const STATE_MINIMIZED: u32 = 1;
const STATE_ACTIVATED: u32 = 2;
const STATE_FULLSCREEN: u32 = 3;

/// What a window looks like to taskbar clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WinInfo {
    /// The window.
    pub id: WindowId,
    /// Its title (empty if unset).
    pub title: String,
    /// Its app id (empty if unset).
    pub app_id: String,
    /// `state` array values.
    pub states: Vec<u32>,
    /// The name of the monitor (= output) it is on.
    pub monitor: String,
}

/// Per-protocol state.
#[derive(Default)]
pub struct Taskbar {
    managers: Vec<ZwlrForeignToplevelManagerV1>,
    /// What clients were last told, and the handle objects per window.
    known: HashMap<WindowId, (WinInfo, Vec<ZwlrForeignToplevelHandleV1>)>,
}

/// Every mapped window and how it should be presented.
pub fn collect<Bd: Backend + 'static>(state: &State<Bd>) -> Vec<WinInfo> {
    let mut out = Vec::new();
    for (mi, monitor) in state.wm.monitors.iter().enumerate() {
        for (di, desktop) in monitor.desktops.iter().enumerate() {
            let visible = state.wm.focused_monitor == Some(mi) && monitor.focused == Some(di);
            let tree = &desktop.tree;
            let mut n = tree.first_extrema(tree.root);
            while let Some(id) = n {
                let node = tree.node(id);
                if let Some(client) = &node.client {
                    let mut states = Vec::new();
                    if node.hidden {
                        states.push(STATE_MINIMIZED);
                    }
                    if visible && tree.focus == Some(id) {
                        states.push(STATE_ACTIVATED);
                    }
                    if client.state == ClientState::Fullscreen {
                        states.push(STATE_FULLSCREEN);
                    }
                    let (title, app_id) = state
                        .adapter
                        .window(client.window)
                        .map(title_and_app_id)
                        .unwrap_or_default();
                    out.push(WinInfo {
                        id: client.window,
                        title,
                        app_id,
                        states,
                        monitor: monitor.name.clone(),
                    });
                }
                n = tree.next_leaf(Some(id), tree.root);
            }
        }
    }
    out
}

fn title_and_app_id(window: &Window) -> (String, String) {
    if let Some(x11) = window.x11_surface() {
        // X11: the title and `WM_CLASS` class stand in for title/app id.
        return (x11.title(), x11.class());
    }
    let Some(toplevel) = window.toplevel() else {
        return Default::default();
    };
    with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok())
            .map(|d| (d.title.clone().unwrap_or_default(), d.app_id.clone().unwrap_or_default()))
            .unwrap_or_default()
    })
}

fn state_bytes(states: &[u32]) -> Vec<u8> {
    states.iter().flat_map(|s| s.to_ne_bytes()).collect()
}

/// The `wl_output` objects `client` holds for `output_name`.
fn client_outputs<Bd: Backend + 'static>(state: &State<Bd>, client: &Client, output_name: &str) -> Vec<WlOutput> {
    let output: Option<Output> = state.space.outputs().find(|o| o.name() == output_name).cloned();
    output.map(|o| o.client_outputs(client).collect()).unwrap_or_default()
}

/// Sends a full description of `info` on a fresh `handle`.
fn describe<Bd: Backend + 'static>(state: &State<Bd>, handle: &ZwlrForeignToplevelHandleV1, info: &WinInfo) {
    handle.title(info.title.clone());
    handle.app_id(info.app_id.clone());
    if let Some(client) = handle.client() {
        for output in client_outputs(state, &client, &info.monitor) {
            handle.output_enter(&output);
        }
    }
    handle.state(state_bytes(&info.states));
    handle.done();
}

/// Creates a handle for `info` on `manager` (which belongs to some client).
fn announce<Bd: Backend + 'static>(
    state: &State<Bd>,
    dh: &DisplayHandle,
    manager: &ZwlrForeignToplevelManagerV1,
    info: &WinInfo,
) -> Option<ZwlrForeignToplevelHandleV1> {
    let client = manager.client()?;
    let handle = client
        .create_resource::<ZwlrForeignToplevelHandleV1, WindowId, State<Bd>>(dh, manager.version(), info.id)
        .ok()?;
    manager.toplevel(&handle);
    describe(state, &handle, info);
    Some(handle)
}

/// Brings every taskbar client up to date with the current windows.
pub fn sync<Bd: Backend + 'static>(state: &mut State<Bd>) {
    if state.protocols.taskbar.managers.is_empty() {
        return;
    }
    let current = collect(state);
    let dh = state.display_handle.clone();

    // Gone windows.
    let gone: Vec<WindowId> = state
        .protocols
        .taskbar
        .known
        .keys()
        .filter(|id| !current.iter().any(|w| w.id == **id))
        .copied()
        .collect();
    for id in gone {
        if let Some((_, handles)) = state.protocols.taskbar.known.remove(&id) {
            for handle in handles {
                handle.closed();
            }
        }
    }

    for info in current {
        match state.protocols.taskbar.known.remove(&info.id) {
            None => {
                let managers = state.protocols.taskbar.managers.clone();
                let handles: Vec<_> = managers
                    .iter()
                    .filter_map(|manager| announce(state, &dh, manager, &info))
                    .collect();
                state.protocols.taskbar.known.insert(info.id, (info, handles));
            }
            Some((old, handles)) => {
                if old != info {
                    for handle in &handles {
                        if old.title != info.title {
                            handle.title(info.title.clone());
                        }
                        if old.app_id != info.app_id {
                            handle.app_id(info.app_id.clone());
                        }
                        if old.monitor != info.monitor {
                            if let Some(client) = handle.client() {
                                for output in client_outputs(state, &client, &old.monitor) {
                                    handle.output_leave(&output);
                                }
                                for output in client_outputs(state, &client, &info.monitor) {
                                    handle.output_enter(&output);
                                }
                            }
                        }
                        if old.states != info.states {
                            handle.state(state_bytes(&info.states));
                        }
                        handle.done();
                    }
                }
                state.protocols.taskbar.known.insert(info.id, (info, handles));
            }
        }
    }
}

impl<Bd: Backend + 'static> GlobalDispatch<ZwlrForeignToplevelManagerV1, (), State<Bd>> for State<Bd> {
    fn can_view(client: Client, _: &()) -> bool {
        crate::state::is_privileged(&client)
    }

    fn bind(
        state: &mut State<Bd>,
        dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        let manager = data_init.init(resource, ());
        tracing::debug!("a taskbar client bound wlr-foreign-toplevel-management");
        // Announce every existing window to this new manager only.
        for info in collect(state) {
            if let Some(handle) = announce(state, dh, &manager, &info) {
                state
                    .protocols
                    .taskbar
                    .known
                    .entry(info.id)
                    .or_insert_with(|| (info.clone(), Vec::new()))
                    .1
                    .push(handle);
            }
        }
        state.protocols.taskbar.managers.push(manager);
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrForeignToplevelManagerV1, (), State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        request: zwlr_foreign_toplevel_manager_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Request::Stop = request {
            manager.finished();
            state.protocols.taskbar.managers.retain(|m| m != manager);
        }
    }

    fn destroyed(state: &mut State<Bd>, _client: smithay::reexports::wayland_server::backend::ClientId, manager: &ZwlrForeignToplevelManagerV1, _: &()) {
        state.protocols.taskbar.managers.retain(|m| m != manager);
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrForeignToplevelHandleV1, WindowId, State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _client: &Client,
        _handle: &ZwlrForeignToplevelHandleV1,
        request: zwlr_foreign_toplevel_handle_v1::Request,
        window: &WindowId,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Request;
        let args: Option<&[&str]> = match request {
            Request::Activate { .. } => Some(&["-f"]),
            Request::Close => Some(&["-c"]),
            Request::SetMinimized => Some(&["-g", "hidden=on"]),
            Request::UnsetMinimized => Some(&["-g", "hidden=off"]),
            Request::SetFullscreen { .. } => Some(&["-t", "fullscreen"]),
            Request::UnsetFullscreen => Some(&["-t", "tiled"]),
            // No bspwm equivalent of maximize; the rectangle hint is for
            // minimize animations. Destroy needs nothing.
            _ => None,
        };
        let Some(args) = args else {
            return;
        };
        let Some((mi, di, node)) = crate::input::locate_window(state, *window) else {
            return;
        };
        let desktop = state.wm.monitors[mi].desktops[di].id;
        let Some(wire_id) = state.registry.id_of(desktop, node) else {
            return;
        };
        let id = format!("0x{wire_id:08X}");
        let mut argv: Vec<String> = vec!["node".into(), id];
        argv.extend(args.iter().map(|s| s.to_string()));
        tracing::debug!(?argv, "taskbar request");
        crate::protocols::run_bspc(state, &argv);
    }

    fn destroyed(
        state: &mut State<Bd>,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        handle: &ZwlrForeignToplevelHandleV1,
        window: &WindowId,
    ) {
        if let Some((_, handles)) = state.protocols.taskbar.known.get_mut(window) {
            handles.retain(|h| h != handle);
        }
    }
}
