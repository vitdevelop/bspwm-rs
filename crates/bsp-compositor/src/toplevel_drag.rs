//! `xdg-toplevel-drag-v1`: a window that follows the pointer during a
//! drag-and-drop (a browser tearing a tab out into its own window).
//!
//! The client starts an ordinary drag with a data source, wraps the source
//! in an `xdg_toplevel_drag_v1` and attaches a toplevel to it with the
//! pointer's offset inside that toplevel. While the drag runs the window
//! is floated (a tiled window cannot follow a pointer) and kept under the
//! pointer at the offset; when the drop happens it stays where it is.
//!
//! bspwm has no counterpart (X11 windows move themselves during a drag).

use std::sync::Mutex;

use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel;
use smithay::reexports::wayland_server::protocol::wl_data_source::WlDataSource;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};
use wayland_protocols::xdg::toplevel_drag::v1::server::{
    xdg_toplevel_drag_manager_v1::{self, XdgToplevelDragManagerV1},
    xdg_toplevel_drag_v1::{self, XdgToplevelDragV1},
};

use crate::state::{Backend, State};

/// The toplevel attached to a drag and the pointer's offset inside it.
struct Attached {
    toplevel: XdgToplevel,
    x_offset: i32,
    y_offset: i32,
}

/// User data of an `xdg_toplevel_drag_v1`.
pub struct DragData {
    source: WlDataSource,
    attached: Mutex<Option<Attached>>,
}

/// Per-protocol state.
#[derive(Default)]
pub struct ToplevelDrags {
    drags: Vec<XdgToplevelDragV1>,
    /// The drag whose data source is being dragged right now.
    active: Option<XdgToplevelDragV1>,
}

impl<Bd: Backend + 'static> GlobalDispatch<XdgToplevelDragManagerV1, (), State<Bd>> for State<Bd> {
    fn bind(_: &mut State<Bd>, _: &DisplayHandle, _: &Client, resource: New<XdgToplevelDragManagerV1>, _: &(), data_init: &mut DataInit<'_, State<Bd>>) {
        data_init.init(resource, ());
    }
}

impl<Bd: Backend + 'static> Dispatch<XdgToplevelDragManagerV1, (), State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _: &Client,
        _: &XdgToplevelDragManagerV1,
        request: xdg_toplevel_drag_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        if let xdg_toplevel_drag_manager_v1::Request::GetXdgToplevelDrag { id, data_source } = request {
            let drag = data_init.init(id, DragData { source: data_source, attached: Mutex::new(None) });
            state.protocols.toplevel_drags.drags.push(drag);
        }
    }
}

impl<Bd: Backend + 'static> Dispatch<XdgToplevelDragV1, DragData, State<Bd>> for State<Bd> {
    fn request(
        _: &mut State<Bd>,
        _: &Client,
        drag: &XdgToplevelDragV1,
        request: xdg_toplevel_drag_v1::Request,
        data: &DragData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, State<Bd>>,
    ) {
        if let xdg_toplevel_drag_v1::Request::Attach { toplevel, x_offset, y_offset } = request {
            let Ok(mut attached) = data.attached.lock() else {
                return;
            };
            if attached.is_some() {
                drag.post_error(xdg_toplevel_drag_v1::Error::ToplevelAttached, "a toplevel is already attached");
                return;
            }
            *attached = Some(Attached { toplevel, x_offset, y_offset });
        }
    }

    fn destroyed(state: &mut State<Bd>, _: smithay::reexports::wayland_server::backend::ClientId, drag: &XdgToplevelDragV1, _: &DragData) {
        state.protocols.toplevel_drags.drags.retain(|d| d != drag);
        if state.protocols.toplevel_drags.active.as_ref() == Some(drag) {
            state.protocols.toplevel_drags.active = None;
        }
    }
}

/// A drag with `source` started: if a toplevel drag wraps it, it is the active one.
pub fn drag_started<Bd: Backend + 'static>(state: &mut State<Bd>, source: Option<&WlDataSource>) {
    let Some(source) = source else {
        return;
    };
    state.protocols.toplevel_drags.active = state
        .protocols
        .toplevel_drags
        .drags
        .iter()
        .find(|drag| drag.data::<DragData>().is_some_and(|d| &d.source == source))
        .cloned();
}

/// The drag ended (dropped or cancelled).
pub fn drag_ended<Bd: Backend + 'static>(state: &mut State<Bd>) {
    state.protocols.toplevel_drags.active = None;
}

/// Keeps the attached toplevel under the pointer; call after the pointer moved.
pub fn follow<Bd: Backend + 'static>(state: &mut State<Bd>) {
    let Some(drag) = state.protocols.toplevel_drags.active.clone() else {
        return;
    };
    let Some(data) = drag.data::<DragData>() else {
        return;
    };
    let Some((toplevel, x_offset, y_offset)) = data
        .attached
        .lock()
        .ok()
        .and_then(|a| a.as_ref().map(|a| (a.toplevel.clone(), a.x_offset, a.y_offset)))
    else {
        return;
    };
    let Some(surface) = state.xdg_shell_state.get_toplevel(&toplevel) else {
        return;
    };
    // Not mapped yet (its first commit has not come): nothing to move.
    let Some(window) = state.window_for_surface(surface.wl_surface()) else {
        return;
    };
    let Some(id) = state.adapter.id_of(&window) else {
        return;
    };
    let Some((mi, di, node)) = crate::input::locate_window(state, id) else {
        return;
    };
    let tiled = state.wm.monitors[mi].desktops[di].tree.node(node).client.as_ref().is_some_and(|c| c.state.is_tiled());
    if tiled {
        state.window_state_request(&window, "floating");
    }
    let Some((mi, di, node)) = crate::input::locate_window(state, id) else {
        return;
    };
    let Some(rect) = state.wm.monitors[mi].desktops[di].tree.node(node).client.as_ref().map(|c| c.floating_rectangle) else {
        return;
    };
    let pointer = state.pointer.current_location();
    let (dx, dy) = (pointer.x.round() as i32 - x_offset - rect.x, pointer.y.round() as i32 - y_offset - rect.y);
    if (dx, dy) != (0, 0) && state.wm.monitors[mi].desktops[di].tree.move_floating(node, dx, dy) {
        crate::shell::sync_wayland_from_core(state);
        // Raise it through the stacking order (a plain `Space` raise would be undone).
        let dst = bsp_ipc::exec::Coordinates { monitor: mi, desktop: di, node: Some(node) };
        crate::ipc::with_ops(state, |ctx, events| bsp_ipc::exec::stack_node(ctx, dst, true, events));
        state.backend_data.queue_redraw();
    }
}
