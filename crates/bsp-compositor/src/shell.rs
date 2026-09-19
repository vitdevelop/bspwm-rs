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
use bsp_core::tree::Direction;

use crate::state::{Backend, State};

impl<Bd: Backend + 'static> XdgShellHandler for State<Bd> {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Deferred to the surface's first `commit` (`on_commit`, below)
        // rather than mapped here: rule matching needs `app_id`/`title`
        // (`docs/design.md` Compatibility: they stand in for bspwm's
        // ICCCM class/instance/name), and while `set_app_id`/`set_title`
        // take effect as soon as the client sends them — independent of
        // any commit — they are not guaranteed to have arrived yet at
        // the moment the `xdg_toplevel` role itself is created. bspwm
        // has no equivalent gap: `apply_rules()` reads `WM_CLASS` with a
        // synchronous `xcb_icccm_get_wm_class_reply()` call before the
        // window is ever mapped (`src/window.c` `manage_window()`).
        self.pending_toplevels.push(surface);
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        unconstrain_popup(self, &surface);
        tracing::debug!(
            geometry = ?surface.with_pending_state(|s| s.geometry),
            parent = surface.get_parent_surface().is_some(),
            "new popup"
        );
        if let Err(err) = self.popups.track_popup(PopupKind::from(surface)) {
            tracing::warn!("failed to track popup: {err}");
        }
    }

    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>) {
        xdg_fullscreen_request(self, &surface, true);
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        xdg_fullscreen_request(self, &surface, false);
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
        unconstrain_popup(self, &surface);
        surface.send_repositioned(token);
    }

    fn grab(&mut self, surface: PopupSurface, _seat: WlSeat, _serial: Serial) {
        // A click outside a grabbing popup dismisses it (`dismiss_grabbed_popups`);
        // the popup does not take the keyboard or pointer for itself.
        self.grabbed_popups.push(surface);
        // No popup grabs yet (`docs/bsp-compositor.md`'s module table has
        // no popup-input-grab entry for the nested compositor): a popup still shows and
        // positions correctly, it just cannot yet take an implicit
        // pointer/keyboard grab that auto-dismisses it on an outside
        // click.
    }

    fn ack_configure(&mut self, _surface: WlSurface, _configure: Configure) {}

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        // A toplevel can be destroyed before it ever reaches its first
        // commit (and so was never mapped at all) — drop it from the
        // pending queue rather than leaking it there forever.
        self.pending_toplevels
            .retain(|t| t.wl_surface() != surface.wl_surface());
        unmap_toplevel(self, &surface);
        self.backend_data.queue_redraw();
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
                .and_then(|data| data.lock().ok().map(|d| d.app_id.clone()))
                .flatten()
        })
        .unwrap_or_default();
        self.adapter.set_app_id(id, &app_id);
        let title = toplevel_title(&surface);
        self.toplevel_changed(id, &title, &app_id);
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let Some(window) = self.window_for_surface(surface.wl_surface()) else {
            return;
        };
        let Some(id) = self.adapter.id_of(&window) else {
            return;
        };
        let app_id = self.adapter.class_of(id).0;
        let title = toplevel_title(&surface);
        self.toplevel_changed(id, &title, &app_id);
    }
}

/// A toplevel's current title (empty if unset or unreadable).
fn toplevel_title(surface: &ToplevelSurface) -> String {
    with_states(surface.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok().map(|d| d.title.clone()))
            .flatten()
    })
    .unwrap_or_default()
}
smithay::delegate_xdg_shell!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> State<Bd> {
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

/// A toplevel's first `commit`: reads `app_id`/title, matches `bspc rule`
/// entries against them, and inserts a new client node into the focused
/// monitor's focused desktop's tree — anchored at the desktop's current
/// focus, split per any matched `split_dir`/`split_ratio` — arranges, and
/// applies the rest of the matched consequence (layer, state, hidden,
/// sticky, private, locked, marked, border, focus). Mirrors bspwm's
/// `manage_window()`.
///
/// `monitor=`/`desktop=`/`node=`/`rectangle=`/`honor_size_hints=`
/// targeting is not applied (`docs/bsp-ipc.md`'s IPC progress: not
/// retained in structured form yet), so every window still lands on the
/// monitor/desktop it would have without any rule. `manage=false` is not
/// applied either: bspwm's `manage=false` maps the X11 window exactly
/// where the client itself put it, with no WM involvement at all
/// (`src/window.c` `manage_window()`'s `!csq->manage` branch calling only
/// `window_show()`) — a raw client-controlled position xdg-shell has no
/// equivalent for, so honoring it needs its own placement policy decision
/// rather than an improvised default position (hard rule 7). Every window
/// is managed unconditionally until that is decided.
fn map_new_toplevel<Bd: Backend + 'static>(state: &mut State<Bd>, toplevel: ToplevelSurface) {
    let (app_id, title, modal) = with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok())
            .map(|d| (d.app_id.clone().unwrap_or_default(), d.title.clone().unwrap_or_default(), d.modal))
            .unwrap_or_default()
    });
    // A modal dialog (`xdg_dialog_v1`) floats centred, like an X11 dialog
    // (bspwm: `src/rule.c` `apply_rules()`, `_NET_WM_WINDOW_TYPE_DIALOG`).
    let mut defaults = bsp_core::rules::RuleConsequence::default();
    // A client that asked for fullscreen before its first commit (Qt does,
    // e.g. flameshot's capture overlay) starts fullscreen.
    if toplevel.with_pending_state(|s| s.states.contains(xdg_toplevel::State::Fullscreen)) {
        defaults.state = Some(bsp_core::node::ClientState::Fullscreen);
    }
    if modal {
        defaults.state = Some(bsp_core::node::ClientState::Floating);
        defaults.center = true;
    }
    // Native Wayland windows match `app_id` as both class and instance,
    // and the surface title as name (`docs/design.md` Compatibility).
    map_new_window(state, Window::new_wayland_window(toplevel), app_id.clone(), app_id, title, defaults);
}

/// Manages a new window of either kind (an `xdg_toplevel`, or an X11 window
/// from XWayland — `crate::xwayland`): everything [`map_new_toplevel`]'s doc
/// comment describes, given the window and the `class`/`instance`/`title`
/// `bspc rule`s match on. Only the final size/state announcement to the
/// client differs by kind.
///
/// `defaults` is what the window's own hints ask for (an X11 dialog wants to
/// float centred, a dock wants no management); matched rules are merged on
/// top of it, so a rule overrides a hint (bspwm: `src/rule.c` `apply_rules()`
/// applies `_NET_WM_WINDOW_TYPE` first, then the rules).
pub(crate) fn map_new_window<Bd: Backend + 'static>(
    state: &mut State<Bd>,
    window: Window,
    class: String,
    instance: String,
    title: String,
    defaults: bsp_core::rules::RuleConsequence,
) {
    let app_id = class.clone();
    let Some(mi) = state.wm.focused_monitor else {
        tracing::warn!("no monitor to map a new window onto");
        return;
    };
    let Some(di) = state.wm.monitors[mi].focused else {
        tracing::warn!("focused monitor has no desktop to map a new window onto");
        return;
    };

    let mut consequence = defaults;
    consequence.merge(&bsp_core::rules::match_rules(&mut state.wm.rules, &class, &instance, &title));

    // bspwm: `manage_window()`'s `!csq->manage` branch shows the window
    // where it is, outside the tree. Only an X11 window has a position of
    // its own to be shown at; an `xdg_toplevel` is managed regardless.
    if !consequence.should_manage() && crate::xwayland::map_unmanaged(state, &window) {
        return;
    }

    window.on_commit();
    let window_id = state.adapter.insert(window.clone());
    state.adapter.set_class(window_id, &class, &instance);
    state.toplevel_mapped(window_id, &title, &app_id);

    let settings = state.wm.settings.clone();
    let desktop_id: DesktopId = state.wm.monitors[mi].desktops[di].id;
    // bspwm anchors a new node at the desktop's currently focused node
    // (`manage_window()`: `f = mon->desk->focus`), splitting its slot in
    // two — not always the tree root, which only coincides with focus
    // when the tree has at most one leaf.
    let anchor = state.wm.monitors[mi].desktops[di].tree.focus;

    let node = {
        let tree = &mut state.wm.monitors[mi].desktops[di].tree;
        if let Some(anchor) = anchor {
            // bspwm: `manage_window()` calls `presel_dir`/`presel_ratio`
            // on the anchor before `insert_node`, so the split this
            // window's insertion performs honors them.
            if let Some(dir) = consequence.split_dir {
                tree.presel_dir(anchor, dir, settings.split_ratio);
            }
            if let Some(ratio) = consequence.split_ratio {
                // bspwm: `presel_ratio()`'s own default direction when no
                // presel exists yet is unconditionally east (matched by
                // `exec::exec_node`'s `NodeAction::PreselRatio` handler).
                tree.presel_ratio(anchor, ratio, Direction::East);
            }
        }
        let border_width = if consequence.should_border() {
            settings.border_width
        } else {
            0
        };
        let core_client = CoreClient::new(window_id, border_width);
        let node = tree.new_client_node(&settings, core_client);
        tree.insert_node(&settings, node, anchor);
        node
    };
    state.registry.register(desktop_id, node);

    if let Some(layer) = consequence.layer {
        state.wm.monitors[mi].desktops[di]
            .tree
            .set_layer(node, layer);
    }

    // A first `arrange()` here, before any state/vacancy change below,
    // computes a real tiled slot for this node — needed as the stand-in
    // `floating_rectangle` source just below. `state`/`hidden` are
    // applied only afterward, since apply_layout skips a vacant node
    // entirely and would otherwise leave its `tiled_rectangle` at
    // `Rect::default()` (0×0), reintroducing the bug fixed by this same
    // seeding step (`CHANGELOG.md`, `shell::map_new_toplevel` "Changed").
    state.wm.monitors[mi].arrange(di, &settings);
    if let Some(client) = state.wm.monitors[mi].desktops[di]
        .tree
        .node_mut(node)
        .client
        .as_mut()
    {
        // bspwm seeds `floating_rectangle` from the window's own requested
        // geometry at map time (`src/window.c`
        // `initialize_floating_rectangle()`, an `xcb_get_geometry` call on
        // the not-yet-tiled X11 window). No Wayland equivalent exists — an
        // xdg-shell client has no on-screen geometry before its first
        // `configure` — so this seeds it from the tiled slot just computed
        // instead, the closest available stand-in, rather than leaving it
        // at `Rect::default()` (0×0), which would collapse the window to
        // nothing the moment it is set floating (`bspc node -t floating`).
        client.floating_rectangle = client.tiled_rectangle;
        // An X11 window does have a geometry of its own to start from, and
        // bspwm uses it (`initialize_floating_rectangle()`): its size, at
        // the position it asked for.
        if let Some(geometry) = window.x11_surface().map(|x11| x11.geometry()).filter(|g| g.size.w > 0 && g.size.h > 0) {
            client.floating_rectangle = bsp_core::geometry::Rect {
                x: geometry.loc.x,
                y: geometry.loc.y,
                width: geometry.size.w,
                height: geometry.size.h,
            };
        }
    }

    if let Some(cstate) = consequence.state {
        state.wm.monitors[mi].desktops[di]
            .tree
            .set_state(node, cstate);
    }
    {
        let tree = &mut state.wm.monitors[mi].desktops[di].tree;
        tree.set_hidden(node, consequence.hidden.unwrap_or(false));
        tree.set_sticky(node, consequence.sticky.unwrap_or(false));
        tree.set_private(node, consequence.private.unwrap_or(false));
        tree.set_locked(node, consequence.locked.unwrap_or(false));
        tree.set_marked(node, consequence.marked.unwrap_or(false));
    }
    // bspwm: `manage_window()`'s `if (csq->center && is_floating(...))
    // window_center()`: the floating rectangle centred in the monitor's
    // rectangle (`src/window.c` `window_center()`).
    if consequence.center {
        let monitor_rect = state.wm.monitors[mi].rectangle;
        if let Some(client) = state.wm.monitors[mi].desktops[di].tree.node_mut(node).client.as_mut() {
            if !client.state.is_tiled() {
                client.floating_rectangle.x = monitor_rect.x + (monitor_rect.width - client.floating_rectangle.width) / 2;
                client.floating_rectangle.y = monitor_rect.y + (monitor_rect.height - client.floating_rectangle.height) / 2;
            }
        }
    }
    // A second `arrange()`: the first pass above may now be stale for
    // tiled siblings if `state`/`hidden` just made this node vacant
    // (bspwm: the single `arrange(m, d)` at the end of `manage_window()`,
    // split into two passes here only because of the seeding step above).
    state.wm.monitors[mi].arrange(di, &settings);

    let fullscreen = state.wm.monitors[mi].desktops[di]
        .tree
        .node(node)
        .client
        .as_ref()
        .is_some_and(|c| c.state == bsp_core::node::ClientState::Fullscreen);
    let Some((rect, tiled, hidden)) = ({
        let node_ref = state.wm.monitors[mi].desktops[di].tree.node(node);
        node_ref.client.as_ref().map(|client| {
            let tiled = client.state.is_tiled();
            let rect = if tiled || client.state == bsp_core::node::ClientState::Fullscreen {
                client.tiled_rectangle
            } else {
                client.floating_rectangle
            };
            (rect, tiled, node_ref.hidden)
        })
    }) else {
        return;
    };

    if let Some(toplevel) = window.toplevel() {
        toplevel.with_pending_state(|s| {
            s.size = Some(Size::from((rect.width.max(1), rect.height.max(1))));
            if tiled {
                s.states.set(xdg_toplevel::State::TiledLeft);
                s.states.set(xdg_toplevel::State::TiledRight);
                s.states.set(xdg_toplevel::State::TiledTop);
                s.states.set(xdg_toplevel::State::TiledBottom);
            }
            if fullscreen {
                s.states.set(xdg_toplevel::State::Fullscreen);
            }
        });
        toplevel.send_configure();
    } else if let Some(x11) = window.x11_surface() {
        let geometry = smithay::utils::Rectangle::new(
            (rect.x, rect.y).into(),
            (rect.width.max(1), rect.height.max(1)).into(),
        );
        if let Err(err) = x11.configure(geometry) {
            tracing::warn!("failed to configure a new X11 window: {err}");
        }
    }

    state.space.map_element(window, (rect.x, rect.y), true);

    // bspwm: `manage_window()`'s `if (!csq->hidden && csq->focus)` branch
    // (simplified: always operating on the currently focused monitor and
    // desktop, since no monitor=/desktop= targeting exists yet, so the
    // `d == mon->desk || csq->follow` distinction never applies).
    if !hidden && consequence.should_focus() {
        state.wm.monitors[mi].desktops[di].tree.focus = Some(node);
        crate::input::focus_node(state, mi, di, node);
    }

    // The new window split an existing one (or, for a floating or hidden one,
    // changed nothing): resize the siblings whose rectangles just changed.
    // Without this they keep their old size until something else (an IPC
    // command, a pointer drag) happens to sync the tree to the surfaces.
    sync_wayland_from_core(state);

    tracing::info!(window = %window_id, ?rect, class = %app_id, fullscreen, "mapped a new window");
}

/// A destroyed `xdg_toplevel`: remove its node from the tree, forget its
/// registry/adapter mapping, and re-arrange.
///
/// bspwm: `src/tree.c` `remove_node()`, called from `unmanage_window()`.
fn unmap_toplevel<Bd: Backend + 'static>(state: &mut State<Bd>, surface: &ToplevelSurface) {
    let Some(window) = state.window_for_surface(surface.wl_surface()) else {
        return;
    };
    unmap_window(state, &window);
}

/// Removes a window of either kind (see [`map_new_window`]) from the
/// tree, the space and every mapping.
pub(crate) fn unmap_window<Bd: Backend + 'static>(state: &mut State<Bd>, window: &Window) {
    let window = window.clone();
    let Some(window_id) = state.adapter.id_of(&window) else {
        return;
    };
    let Some((mi, di, node)) = state.locate_client(window_id) else {
        return;
    };

    // The closing window must not keep the keyboard: once it is unmapped it no
    // longer counts as a window, and `sync_keyboard_focus` would take its
    // still-alive surface for a foreign owner of focus and leave it there.
    if let (Some(keyboard), Some(surface)) = (state.seat.get_keyboard(), window.wl_surface()) {
        let holds = keyboard
            .current_focus()
            .and_then(|t| t.wl_surface().map(|s| s.into_owned()))
            .is_some_and(|s| s == *surface);
        if holds {
            keyboard.set_focus(state, None, smithay::utils::SERIAL_COUNTER.next_serial());
        }
    }

    state.space.unmap_elem(&window);
    state.adapter.remove(window_id);
    state.toplevel_unmapped(window_id);

    let settings = state.wm.settings.clone();
    let desktop_id = state.wm.monitors[mi].desktops[di].id;
    state.wm.monitors[mi].desktops[di]
        .tree
        .remove_node(&settings, node);
    state.registry.unregister(desktop_id, node);
    // Closing the focused window leaves the tree without a focused node, and
    // then no directional focus command has a starting point: hand focus to
    // the window focused before it.
    state.wm.refocus_after_removal(mi, di);
    state.wm.monitors[mi].arrange(di, &settings);
    // The siblings grow into the freed space, and keyboard focus moves to
    // the node the tree now focuses.
    sync_wayland_from_core(state);

    tracing::info!(window = %window_id, "unmapped a window");
}

/// Runs on every `wl_surface.commit`: hands the buffer to Smithay's
/// tracking, and either maps a still-pending toplevel now that its
/// `app_id`/title are available ([`map_new_toplevel`]), or, for a window
/// already mapped, sends the initial `configure` if it somehow hasn't
/// gone out yet (a safety net — [`map_new_toplevel`] already sends it as
/// part of the same first commit that triggers it).
///
/// bspwm has no equivalent step — a real X11 `ConfigureWindow` takes
/// effect immediately, it does not need an acknowledged round trip the
/// way `xdg_surface.configure`/`ack_configure` does.
pub fn on_commit<Bd: Backend + 'static>(state: &mut State<Bd>, surface: &WlSurface) {
    smithay::backend::renderer::utils::on_commit_buffer_handler::<State<Bd>>(surface);
    state.popups.commit(surface);
    // An xdg_popup stays unmapped until the compositor answers its first
    // commit with a configure (xdg-shell: "the compositor must respond with
    // an initial configure"); menus and tooltips depend on it.
    if let Some(PopupKind::Xdg(popup)) = state.popups.find_popup(surface) {
        tracing::debug!(
            initial_sent = popup.is_initial_configure_sent(),
            geometry = ?popup.with_pending_state(|s| s.geometry),
            "popup committed"
        );
        if !popup.is_initial_configure_sent() {
            if let Err(err) = popup.send_configure() {
                tracing::warn!("popup initial configure failed: {err}");
            }
        }
    }

    if let Some(pos) = state
        .pending_toplevels
        .iter()
        .position(|t| t.wl_surface() == surface)
    {
        let toplevel = state.pending_toplevels.remove(pos);
        map_new_toplevel(state, toplevel);
        return;
    }

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
            .and_then(|data| data.lock().ok().map(|d| d.initial_configure_sent))
            // Unreadable: assume sent, so no configure is pushed twice.
            .unwrap_or(true)
    });
    if !initial_configure_sent {
        toplevel.send_configure();
    }
}

/// Reconciles every mapped client's Wayland-visible position and size
/// with `bsp-core`'s tree, which an executed `bsp-ipc` command (or a
/// direct tree operation) may have changed without touching `Space` or
/// the client's surface at all. Call after anything that might have
/// re-arranged a desktop.
///
/// bspwm has no equivalent: `xcb_configure_window` there takes effect the
/// moment `tree.c` calls it, in the same step as the tree update; here
/// the two are necessarily separate because Wayland's `configure`/
/// `ack_configure` round trip means only the client can actually resize
/// its own surface.
pub fn sync_wayland_from_core<Bd: Backend + 'static>(state: &mut State<Bd>) {
    // Collected first, rather than acted on while borrowing `state.wm`,
    // since applying each one needs `&mut state.space`/`&mut state.adapter`.
    let mut updates = Vec::new();
    for m in &state.wm.monitors {
        for (di, d) in m.desktops.iter().enumerate() {
            // bspwm shows only a monitor's focused desktop
            // (`src/desktop.c` `show_desktop()`/`hide_desktop()`): windows
            // of every other desktop are unmapped, exactly like a hidden
            // node (sticky nodes, which bspwm keeps visible across
            // desktops, are not implemented in `bsp-core` yet).
            let on_shown_desktop = m.focused == Some(di);
            let mut n = d.tree.first_extrema(d.tree.root);
            while let Some(id) = n {
                let node = d.tree.node(id);
                if let Some(client) = &node.client {
                    updates.push((client.window, client.clone(), node.hidden || !on_shown_desktop));
                }
                n = d.tree.next_leaf(Some(id), d.tree.root);
            }
        }
    }

    for (window_id, client, hidden) in updates {
        let Some(window) = state.adapter.window(window_id).cloned() else {
            continue;
        };
        if hidden {
            if state.space.element_location(&window).is_some() {
                state.space.unmap_elem(&window);
            }
            continue;
        }
        sync_one_window(state, &window, &client);
    }
    crate::input::sync_keyboard_focus(state);
    crate::protocols::update_fractional_scales(state);
    state.raise_unmanaged_x11();
}

fn sync_one_window<Bd: Backend + 'static>(
    state: &mut State<Bd>,
    window: &Window,
    client: &bsp_core::node::Client,
) {
    let rect = match client.state {
        bsp_core::node::ClientState::Floating => client.floating_rectangle,
        _ => client.tiled_rectangle,
    };
    let want_fullscreen = client.state == bsp_core::node::ClientState::Fullscreen;

    let current_loc = state.space.element_location(window);
    let target_loc = (rect.x, rect.y).into();
    if current_loc != Some(target_loc) {
        state.space.map_element(window.clone(), target_loc, false);
    }

    if let Some(toplevel) = window.toplevel() {
        let target = Size::from((rect.width.max(1), rect.height.max(1)));
        let (current, is_fullscreen) =
            toplevel.with_pending_state(|s| (s.size, s.states.contains(xdg_toplevel::State::Fullscreen)));
        if current != Some(target) || is_fullscreen != want_fullscreen {
            toplevel.with_pending_state(|s| {
                s.size = Some(target);
                if want_fullscreen {
                    s.states.set(xdg_toplevel::State::Fullscreen);
                } else {
                    s.states.unset(xdg_toplevel::State::Fullscreen);
                }
            });
            if toplevel.is_initial_configure_sent() {
                toplevel.send_configure();
            }
        }
    } else if let Some(x11) = window.x11_surface() {
        let target = smithay::utils::Rectangle::new(
            (rect.x, rect.y).into(),
            (rect.width.max(1), rect.height.max(1)).into(),
        );
        if x11.geometry() != target {
            if let Err(err) = x11.configure(target) {
                tracing::debug!("cannot configure an X11 window: {err}");
            }
        }
    }
}

/// Moves `popup` so it lies inside the output of the window or layer surface
/// it belongs to, using the client's own `constraint_adjustment` (flip,
/// slide, resize). Without it a menu opened near a screen edge (waybar's, at
/// the top) can land outside the screen. Does nothing while the popup has no
/// parent yet; a layer-shell popup is handled again from
/// `WlrLayerShellHandler::new_popup` once it has one.
pub fn unconstrain_popup<Bd: Backend + 'static>(state: &State<Bd>, popup: &PopupSurface) {
    use smithay::desktop::{layer_map_for_output, find_popup_root_surface, WindowSurfaceType};
    let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
        return;
    };
    // The root's origin in global coordinates, and the output it is on.
    let (origin, output) = if let Some(window) = state.window_for_surface(&root) {
        let Some(geo) = state.space.element_geometry(&window) else {
            return;
        };
        let output = state.space.outputs_for_element(&window).into_iter().next();
        (geo.loc, output)
    } else {
        let found = state.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            let layer = map.layer_for_surface(&root, WindowSurfaceType::TOPLEVEL)?;
            let geo = map.layer_geometry(layer)?;
            let out_loc = state.space.output_geometry(output)?.loc;
            Some((geo.loc + out_loc, output.clone()))
        });
        match found {
            Some((loc, output)) => (loc, Some(output)),
            None => return,
        }
    };
    let Some(mut target) = output.and_then(|o| state.space.output_geometry(&o)) else {
        return;
    };
    target.loc -= origin;
    popup.with_pending_state(|s| {
        s.geometry = s.positioner.get_unconstrained_geometry(target);
    });
}

/// A button press landed on `hit` (the surface under the pointer): every
/// popup that grabbed and is not the press's own popup tree gets
/// `popup_done`, as a client expects from a compositor honouring
/// `xdg_popup.grab` (menus close on a click elsewhere). Returns whether any
/// popup was dismissed.
pub fn dismiss_grabbed_popups<Bd: Backend + 'static>(state: &mut State<Bd>, hit: Option<&WlSurface>) -> bool {
    state.grabbed_popups.retain(|p| p.alive());
    if state.grabbed_popups.is_empty() {
        return false;
    }
    // The root of the subsurface tree that was hit.
    let mut root = hit.cloned();
    while let Some(parent) = root.as_ref().and_then(smithay::wayland::compositor::get_parent) {
        root = Some(parent);
    }
    if let Some(root) = &root {
        if state.grabbed_popups.iter().any(|p| p.wl_surface() == root) {
            return false;
        }
    }
    for popup in std::mem::take(&mut state.grabbed_popups) {
        popup.send_popup_done();
    }
    true
}

/// An `xdg_toplevel.set_fullscreen` / `unset_fullscreen` request. A mapped
/// window goes through `bspc node -t fullscreen` (a toggle, so only when the
/// state actually differs); a toplevel that has not mapped yet just records
/// the state, and `map_new_toplevel` starts it fullscreen.
fn xdg_fullscreen_request<Bd: Backend + 'static>(state: &mut State<Bd>, surface: &ToplevelSurface, on: bool) {
    use bsp_core::node::ClientState;
    let mapped = state
        .window_for_surface(surface.wl_surface())
        .or_else(|| state.adapter.window_of_surface(surface.wl_surface()))
        .and_then(|w| state.adapter.id_of(&w));
    let Some(id) = mapped else {
        surface.with_pending_state(|s| {
            if on {
                s.states.set(xdg_toplevel::State::Fullscreen);
            } else {
                s.states.unset(xdg_toplevel::State::Fullscreen);
            }
        });
        return;
    };
    let Some((mi, di, node)) = crate::input::locate_window(state, id) else {
        return;
    };
    let is_fullscreen = state.wm.monitors[mi].desktops[di]
        .tree
        .node(node)
        .client
        .as_ref()
        .is_some_and(|c| c.state == ClientState::Fullscreen);
    if is_fullscreen == on {
        // Nothing to change, but the client still expects an answer.
        surface.send_configure();
        return;
    }
    let desktop = state.wm.monitors[mi].desktops[di].id;
    let Some(wire_id) = state.registry.id_of(desktop, node) else {
        return;
    };
    let argv = vec![
        "node".to_string(),
        format!("0x{wire_id:08X}"),
        "-t".to_string(),
        if on { "fullscreen" } else { "~fullscreen" }.to_string(),
    ];
    crate::protocols::run_bspc(state, &argv);
}
