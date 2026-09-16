//! `ext-workspace-v1`: lets bars and pagers list workspaces (waybar's
//! `ext/workspaces` module), show which are active, and switch between
//! them.
//!
//! Replaces bspwm's EWMH desktop atoms (`_NET_NUMBER_OF_DESKTOPS`,
//! `_NET_DESKTOP_NAMES`, `_NET_CURRENT_DESKTOP`, `_NET_WM_DESKTOP`;
//! `docs/design.md`, Compatibility). Mapping: each monitor is a
//! *workspace group* (with its output), each `bsp-core` desktop a
//! *workspace* in its monitor's group, and a monitor's focused desktop is
//! the group's one *active* workspace. Activating a workspace is
//! `bspc desktop ID -f`.
//!
//! Like `crate::taskbar`, state is diffed after every event-loop turn
//! ([`sync`]) rather than pushed from every place a desktop can change.

use std::collections::HashMap;

use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};
use wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

use bsp_core::id::{DesktopId, MonitorId};

use crate::state::{Backend, State};

/// One workspace as it should look to clients.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WsInfo {
    id: DesktopId,
    monitor: MonitorId,
    name: String,
    active: bool,
}

/// What one bound manager (one client) has been told and holds.
struct ManagerEntry {
    manager: ExtWorkspaceManagerV1,
    groups: HashMap<MonitorId, ExtWorkspaceGroupHandleV1>,
    workspaces: HashMap<DesktopId, (WsInfo, ExtWorkspaceHandleV1)>,
    /// `activate` requests received since the last `commit`.
    pending_activations: Vec<DesktopId>,
}

/// Per-protocol state.
#[derive(Default)]
pub struct Workspaces {
    managers: Vec<ManagerEntry>,
}

fn desired<Bd: Backend + 'static>(state: &State<Bd>) -> (Vec<(MonitorId, String)>, Vec<WsInfo>) {
    let mut groups = Vec::new();
    let mut workspaces = Vec::new();
    for monitor in &state.wm.monitors {
        groups.push((monitor.id, monitor.name.clone()));
        for (di, desktop) in monitor.desktops.iter().enumerate() {
            workspaces.push(WsInfo {
                id: desktop.id,
                monitor: monitor.id,
                name: desktop.name.clone(),
                active: monitor.focused == Some(di),
            });
        }
    }
    (groups, workspaces)
}

/// Brings every workspace client up to date with the current desktops.
pub fn sync<Bd: Backend + 'static>(state: &mut State<Bd>) {
    if state.protocols.workspaces.managers.is_empty() {
        return;
    }
    let (groups, workspaces) = desired(state);
    let dh = state.display_handle.clone();
    let mut entries = std::mem::take(&mut state.protocols.workspaces.managers);
    for entry in &mut entries {
        if reconcile(state, &dh, entry, &groups, &workspaces) {
            entry.manager.done();
        }
    }
    state.protocols.workspaces.managers = entries;
}

/// Reconciles one manager; returns whether anything changed (so `done` is due).
fn reconcile<Bd: Backend + 'static>(
    state: &State<Bd>,
    dh: &DisplayHandle,
    entry: &mut ManagerEntry,
    groups: &[(MonitorId, String)],
    workspaces: &[WsInfo],
) -> bool {
    let Some(client) = entry.manager.client() else {
        return false;
    };
    let version = entry.manager.version();
    let mut changed = false;

    // Workspaces that are gone (or moved to another monitor: leave, re-enter below).
    let stale: Vec<DesktopId> = entry
        .workspaces
        .iter()
        .filter(|(id, (old, _))| workspaces.iter().find(|w| w.id == **id).is_none_or(|w| w.monitor != old.monitor))
        .map(|(id, _)| *id)
        .collect();
    for id in stale {
        if let Some((old, ws)) = entry.workspaces.remove(&id) {
            if let Some(group) = entry.groups.get(&old.monitor) {
                group.workspace_leave(&ws);
            }
            if !workspaces.iter().any(|w| w.id == id) {
                ws.removed();
            }
            changed = true;
        }
    }
    // Groups that are gone.
    let gone: Vec<MonitorId> = entry
        .groups
        .keys()
        .filter(|m| !groups.iter().any(|(id, _)| id == *m))
        .copied()
        .collect();
    for monitor in gone {
        if let Some(group) = entry.groups.remove(&monitor) {
            group.removed();
            changed = true;
        }
    }
    // New groups.
    for (monitor, name) in groups {
        if entry.groups.contains_key(monitor) {
            continue;
        }
        let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, MonitorId, State<Bd>>(dh, version, *monitor) else {
            continue;
        };
        entry.manager.workspace_group(&group);
        group.capabilities(ext_workspace_group_handle_v1::GroupCapabilities::empty());
        if let Some(output) = state.space.outputs().find(|o| o.name() == *name) {
            for wl_output in output.client_outputs(&client) {
                group.output_enter(&wl_output);
            }
        }
        entry.groups.insert(*monitor, group);
        changed = true;
    }
    // New and changed workspaces.
    for info in workspaces {
        let bits = if info.active {
            ext_workspace_handle_v1::State::Active
        } else {
            ext_workspace_handle_v1::State::empty()
        };
        match entry.workspaces.get_mut(&info.id) {
            None => {
                let Some(group) = entry.groups.get(&info.monitor) else {
                    continue;
                };
                let Ok(ws) = client.create_resource::<ExtWorkspaceHandleV1, DesktopId, State<Bd>>(dh, version, info.id) else {
                    continue;
                };
                entry.manager.workspace(&ws);
                ws.id(format!("0x{:08X}", info.id.0));
                ws.name(info.name.clone());
                ws.coordinates(Vec::new());
                ws.state(bits);
                ws.capabilities(ext_workspace_handle_v1::WorkspaceCapabilities::Activate);
                group.workspace_enter(&ws);
                entry.workspaces.insert(info.id, (info.clone(), ws));
                changed = true;
            }
            Some((old, ws)) => {
                if old.name != info.name {
                    ws.name(info.name.clone());
                    changed = true;
                }
                if old.active != info.active {
                    ws.state(bits);
                    changed = true;
                }
                *old = info.clone();
            }
        }
    }
    changed
}

impl<Bd: Backend + 'static> GlobalDispatch<ExtWorkspaceManagerV1, (), State<Bd>> for State<Bd> {
    fn can_view(client: Client, _: &()) -> bool {
        crate::state::is_privileged(&client)
    }

    fn bind(
        state: &mut State<Bd>,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        let manager = data_init.init(resource, ());
        tracing::debug!("a workspace client bound ext-workspace-v1");
        state.protocols.workspaces.managers.push(ManagerEntry {
            manager,
            groups: HashMap::new(),
            workspaces: HashMap::new(),
            pending_activations: Vec::new(),
        });
        // The initial state is sent by the next `sync` (end of this turn).
    }
}

impl<Bd: Backend + 'static> Dispatch<ExtWorkspaceManagerV1, (), State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _client: &Client,
        manager: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                let activations = state
                    .protocols
                    .workspaces
                    .managers
                    .iter_mut()
                    .find(|e| &e.manager == manager)
                    .map(|e| std::mem::take(&mut e.pending_activations))
                    .unwrap_or_default();
                for id in activations {
                    tracing::debug!(desktop = id.0, "workspace client activated a desktop");
                    let argv = vec!["desktop".to_string(), format!("0x{:08X}", id.0), "-f".to_string()];
                    crate::protocols::run_bspc(state, &argv);
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                manager.finished();
                state.protocols.workspaces.managers.retain(|e| &e.manager != manager);
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut State<Bd>, _client: ClientId, manager: &ExtWorkspaceManagerV1, _: &()) {
        state.protocols.workspaces.managers.retain(|e| &e.manager != manager);
    }
}

impl<Bd: Backend + 'static> Dispatch<ExtWorkspaceGroupHandleV1, MonitorId, State<Bd>> for State<Bd> {
    fn request(
        _state: &mut State<Bd>,
        _client: &Client,
        _group: &ExtWorkspaceGroupHandleV1,
        _request: ext_workspace_group_handle_v1::Request,
        _: &MonitorId,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        // `create_workspace` is not advertised (capabilities 0); `destroy` needs nothing.
    }
}

impl<Bd: Backend + 'static> Dispatch<ExtWorkspaceHandleV1, DesktopId, State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _client: &Client,
        workspace: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        desktop: &DesktopId,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        // Only `activate` is advertised; it takes effect at the manager's `commit`.
        if let ext_workspace_handle_v1::Request::Activate = request {
            if let Some(entry) = state
                .protocols
                .workspaces
                .managers
                .iter_mut()
                .find(|e| e.workspaces.values().any(|(_, ws)| ws == workspace))
            {
                entry.pending_activations.push(*desktop);
            }
        }
    }
}
