//! Monitors whose output goes away or comes back.
//!
//! bspwm keeps the monitor of an unplugged output, with its desktops and
//! windows, marked unwired, and shows it again when the output returns
//! (`src/monitor.c` `update_monitors()`, `remove_unplugged_monitors` off by
//! default). With `remove_unplugged_monitors` on, its desktops move to the
//! last wired monitor (`merge_monitors()`) and it is removed
//! (`remove_monitor()`). A virtual output that is removed always merges.

use smithay::output::Output;
use smithay::reexports::wayland_server::backend::GlobalId;

use crate::state::{Backend, State};

/// The `wl_output` global of an output, kept so it can be destroyed when the
/// output goes (a stale global would still be offered to clients).
pub struct OutputGlobal(pub GlobalId);

/// `output` is gone: its surfaces, global and hardware entry go, and its
/// monitor is unwired, or merged into another and removed when `merge` (or
/// `remove_unplugged_monitors`) says so.
pub fn output_removed<Bd: Backend + 'static>(state: &mut State<Bd>, output: &Output, merge: bool) {
    let name = output.name();
    crate::layers::close_all(output);
    state.space.unmap_output(output);
    if let Some(OutputGlobal(global)) = output.user_data().get::<OutputGlobal>() {
        state.display_handle.remove_global::<State<Bd>>(global.clone());
    }
    state.adapter.hw.outputs.retain(|o| o.name != name);
    if let Some(index) = state.wm.monitors.iter().position(|m| m.name == name) {
        unplug_monitor(state, index, merge || state.wm.settings.remove_unplugged_monitors);
    }
    state.request_sync();
    state.backend_data.queue_redraw();
}

/// The monitor at `index` has lost its output: merged and removed with
/// `merge` (when another wired monitor exists), unwired otherwise.
pub fn unplug_monitor<Bd: Backend + 'static>(state: &mut State<Bd>, index: usize, merge: bool) {
    match bsp_ipc::exec::merge_target(&state.wm, index) {
        Some(target) if merge => {
            crate::ipc::with_ops(state, |ctx, events| {
                bsp_ipc::exec::merge_monitors(ctx, index, target, events);
                bsp_ipc::exec::remove_monitor(ctx, index, target, events);
            });
            tracing::info!(index, "monitor merged into another and removed");
        }
        _ => {
            state.wm.monitors[index].wired = false;
            tracing::info!(name = state.wm.monitors[index].name, "monitor unwired; kept until its output returns");
        }
    }
}

/// An output named `name` appeared: the unwired monitor with that name, if
/// any, is wired again and its id returned (its desktops and windows come
/// back); `None` means a new monitor is needed. Finish with [`rewired`] once
/// the output has its rectangle.
pub fn rewire(wm: &mut bsp_core::wm::Wm, name: &str) -> Option<bsp_core::id::MonitorId> {
    let m = wm.monitors.iter_mut().find(|m| m.name == name && !m.wired)?;
    m.wired = true;
    Some(m.id)
}

/// Gives the monitor `id`, just wired again, its output's rectangle
/// (adapting and re-arranging its desktops, reporting `monitor_geometry`).
pub fn rewired<Bd: Backend + 'static>(state: &mut State<Bd>, id: bsp_core::id::MonitorId, rect: bsp_core::geometry::Rect) {
    let Some(index) = state.wm.monitor_index(id) else { return };
    crate::ipc::with_ops(state, |ctx, events| bsp_ipc::exec::set_monitor_rectangle(ctx, index, rect, events));
    tracing::info!(name = state.wm.monitors.iter().find(|m| m.id == id).map(|m| m.name.clone()), "unwired monitor shown again");
    state.request_sync();
}
