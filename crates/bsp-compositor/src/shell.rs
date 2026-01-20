//! xdg-shell: mapping a toplevel surface to a `bsp-core` client node, and
//! back out again when it is destroyed.
//!
//! This is the adapter half of the functional-core/imperative-shell split
//! (`docs/design.md`, Architecture): a Wayland event comes in, the
//! relevant `bsp-core` operation runs, and the result (a computed
//! rectangle) is applied back to the real Wayland surface via a
//! `configure`.

use smithay::desktop::{PopupKind, Window};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::reexports::wayland_server::protocol::wl_seat::WlSeat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Serial, Size};
use smithay::wayland::compositor::with_states;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::{
    Configure, PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};

use bsp_core::id::DesktopId;
use bsp_core::node::Client as CoreClient;

use crate::state::State;

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        map_new_toplevel(self, surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        if let Err(err) = self.popups.track_popup(PopupKind::from(surface)) {
            tracing::warn!("failed to track popup: {err}");
        }
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        surface.send_repositioned(token);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
        // No popup grabs yet (`docs/bsp-compositor.md`'s module table has
        // no popup-input-grab entry for the nested compositor): a popup still shows and
        // positions correctly, it just cannot yet take an implicit
        // pointer/keyboard grab that auto-dismisses it on an outside
        // click.
    }

    fn ack_configure(&mut self, _surface: WlSurface, _configure: Configure) {}

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        unmap_toplevel(self, &surface);
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        let Some(window) = self.window_for_surface(surface.wl_surface()) else {
            return;
        };
        let Some(id) = self.adapter.id_of(&window) else {
            return;
        };
        let app_id = with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap()
                .app_id
                .clone()
        })
        .unwrap_or_default();
        self.adapter.set_app_id(id, &app_id);
    }
}
smithay::delegate_xdg_shell!(State);

impl State {
    /// The mapped `Window` showing `surface`, if any.
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(surface))
            .cloned()
    }

    /// Finds which desktop (by monitor/desktop index) and node currently
    /// hold the client for `window_id`, if any.
    fn locate_client(
        &self,
        window_id: bsp_core::id::WindowId,
    ) -> Option<(usize, usize, bsp_core::id::NodeId)> {
        for (mi, m) in self.wm.monitors.iter().enumerate() {
            for (di, d) in m.desktops.iter().enumerate() {
                let mut n = d.tree.first_extrema(d.tree.root);
                while let Some(id) = n {
                    if d.tree
                        .node(id)
                        .client
                        .as_ref()
                        .is_some_and(|c| c.window == window_id)
                    {
                        return Some((mi, di, id));
                    }
                    n = d.tree.next_leaf(Some(id), d.tree.root);
                }
            }
        }
        None
    }
}

/// A newly created `xdg_toplevel`: insert it as a new client node in the
/// focused monitor's focused desktop's tree, arrange, and configure the
/// surface to the tree-computed size. Mirrors bspwm's `manage_window()`,
/// minus rule evaluation (`docs/bsp-compositor.md` scope: no rule
/// matching wired up yet — every window lands tiled, unconditionally).
fn map_new_toplevel(state: &mut State, surface: ToplevelSurface) {
    let Some(mi) = state.wm.focused_monitor else {
        tracing::warn!("no monitor to map a new window onto");
        return;
    };
    let Some(di) = state.wm.monitors[mi].focused else {
        tracing::warn!("focused monitor has no desktop to map a new window onto");
        return;
    };

    let window = Window::new_wayland_window(surface.clone());
    let window_id = state.adapter.insert(window.clone());

    let settings = state.wm.settings.clone();
    let desktop_id: DesktopId = state.wm.monitors[mi].desktops[di].id;
    let node = {
        let tree = &mut state.wm.monitors[mi].desktops[di].tree;
        let core_client = CoreClient::new(window_id, settings.border_width);
        let node = tree.new_client_node(&settings, core_client);
        tree.insert_node(&settings, node, None);
        node
    };
    state.registry.register(desktop_id, node);

    state.wm.monitors[mi].arrange(di, &settings);

    let rect = state.wm.monitors[mi].desktops[di]
        .tree
        .node(node)
        .client
        .as_ref()
        .unwrap()
        .tiled_rectangle;

    surface.with_pending_state(|s| {
        s.size = Some(Size::from((rect.width.max(1), rect.height.max(1))));
        s.states.set(xdg_toplevel::State::TiledLeft);
        s.states.set(xdg_toplevel::State::TiledRight);
        s.states.set(xdg_toplevel::State::TiledTop);
        s.states.set(xdg_toplevel::State::TiledBottom);
    });

    state.space.map_element(window, (rect.x, rect.y), true);

    // Focus follows a newly mapped window, as bspwm's own `manage_window()`
    // does for a window not marked hidden/no-focus by a rule (no rules
    // exist yet, so this is unconditional — scope, see above).
    state.wm.monitors[mi].desktops[di].tree.focus = Some(node);
    crate::input::focus_node(state, mi, di, node);

    tracing::info!(window = %window_id, ?rect, "mapped a new window");
}

/// A destroyed `xdg_toplevel`: remove its node from the tree, forget its
/// registry/adapter mapping, and re-arrange.
///
/// bspwm: `src/tree.c` `remove_node()`, called from `unmanage_window()`.
fn unmap_toplevel(state: &mut State, surface: &ToplevelSurface) {
    let Some(window) = state.window_for_surface(surface.wl_surface()) else {
        return;
    };
    let Some(window_id) = state.adapter.id_of(&window) else {
        return;
    };
    let Some((mi, di, node)) = state.locate_client(window_id) else {
        return;
    };

    state.space.unmap_elem(&window);
    state.adapter.remove(window_id);

    let settings = state.wm.settings.clone();
    let desktop_id = state.wm.monitors[mi].desktops[di].id;
    state.wm.monitors[mi].desktops[di]
        .tree
        .remove_node(&settings, node);
    state.registry.unregister(desktop_id, node);
    state.wm.monitors[mi].arrange(di, &settings);

    tracing::info!(window = %window_id, "unmapped a window");
}

/// Runs on every `wl_surface.commit`: hands the buffer to Smithay's
/// tracking, and for a still-unconfigured toplevel already mapped by
/// [`map_new_toplevel`], sends the initial `configure` (the size decided
/// there) now that the client is ready to receive it.
///
/// bspwm has no equivalent step — a real X11 `ConfigureWindow` takes
/// effect immediately, it does not need an acknowledged round trip the
/// way `xdg_surface.configure`/`ack_configure` does.
pub fn on_commit(state: &mut State, surface: &WlSurface) {
    smithay::backend::renderer::utils::on_commit_buffer_handler::<State>(surface);
    state.popups.commit(surface);

    let Some(window) = state.window_for_surface(surface) else {
        return;
    };
    window.on_commit();
    let Some(toplevel) = window.toplevel() else {
        return;
    };
    let initial_configure_sent = with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .unwrap()
            .lock()
            .unwrap()
            .initial_configure_sent
    });
    if !initial_configure_sent {
        toplevel.send_configure();
    }
}
