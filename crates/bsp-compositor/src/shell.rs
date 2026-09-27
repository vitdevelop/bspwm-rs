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

    /// Not supported (see the capabilities in `State::new`); answered with the
    /// current configure so the client does not wait for one (Smithay already
    /// does this for `maximize_request`).
    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        if surface.is_initial_configure_sent() {
            surface.send_configure();
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
    /// The managed `Window` of `surface`, if any: one shown in the space, or
    /// one on a desktop that is not shown (which is out of the space but still
    /// in the tree, and must still be found when its client closes it or dies).
    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|w| w.wl_surface().as_deref() == Some(surface))
            .cloned()
            .or_else(|| self.adapter.window_of_surface(surface))
    }

    /// Gives the `Activated` state to the toplevel that holds keyboard focus
    /// (`focused`) and takes it from every other shown one, sending a configure
    /// where it changed: clients draw a focused window differently (GTK's
    /// headerbars, caret blinking) and would keep whatever they mapped with.
    pub fn update_activated(&mut self, focused: Option<&WlSurface>) {
        // Every managed window, not just the shown ones: one that was focused
        // when its desktop was switched away must lose `Activated` too.
        for window in self.adapter.windows() {
            let Some(toplevel) = window.toplevel() else {
                continue;
            };
            let active = focused == Some(toplevel.wl_surface());
            if window.set_activated(active) && toplevel.is_initial_configure_sent() {
                toplevel.send_pending_configure();
            }
        }
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
    // The same order bspwm applies a window's own hints in: type (a modal
    // dialog), then state (fullscreen), then transient and fixed size (both
    // float), then the rules on top.
    let mut defaults = bsp_core::rules::RuleConsequence::default();
    if modal {
        defaults.state = Some(bsp_core::node::ClientState::Floating);
        defaults.center = Some(true);
    }
    // A client that asked for fullscreen before its first commit (Qt does,
    // e.g. flameshot's capture overlay) starts fullscreen.
    if toplevel.with_pending_state(|s| s.states.contains(xdg_toplevel::State::Fullscreen)) {
        defaults.state = Some(bsp_core::node::ClientState::Fullscreen);
    }
    // A dialog with a parent floats (`WM_TRANSIENT_FOR`), and so does a window
    // whose minimum and maximum size are equal.
    let (min_size, max_size) = with_states(toplevel.wl_surface(), |states| {
        let mut cached = states.cached_state.get::<smithay::wayland::shell::xdg::SurfaceCachedState>();
        let current = cached.current();
        (Some(current.min_size), Some(current.max_size))
    });
    if toplevel.parent().is_some() || crate::xwayland::fixed_size(min_size, max_size) {
        defaults.state = Some(bsp_core::node::ClientState::Floating);
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
    let mut consequence = defaults;
    consequence.merge(&bsp_core::rules::match_rules(&mut state.wm.rules, &class, &instance, &title));
    // `external_rules_command` may add effects; the window waits for it (the
    // compositor does not, `spawn::start_external_rules`), as bspwm keeps a
    // window pending while its rules command runs.
    let command = state.wm.settings.external_rules_command.clone();
    if command.is_empty() {
        map_new_window_with(state, window, class, instance, title, consequence);
        return;
    }
    let wid = window.x11_surface().map_or(0, |x| x.window_id());
    let described = consequence.clone();
    let (c, i) = (class.clone(), instance.clone());
    crate::spawn::start_external_rules(
        state,
        &command,
        wid,
        &class,
        &instance,
        &described,
        Box::new(move |state, extra| {
            use smithay::utils::IsAlive;
            let mut consequence = consequence;
            if let Some(extra) = extra {
                consequence.merge(&extra);
            }
            // Closed while the command ran: nothing to manage.
            if window.alive() {
                map_new_window_with(state, window, c, i, title, consequence);
            }
        }),
    );
}

/// [`map_new_window`] once the rules, including the external command's, are
/// merged into `consequence`.
fn map_new_window_with<Bd: Backend + 'static>(
    state: &mut State<Bd>,
    window: Window,
    class: String,
    instance: String,
    title: String,
    consequence: bsp_core::rules::RuleConsequence,
) {
    let app_id = class.clone();
    let focus_before = crate::input::focus_key(state);
    if state.wm.focused_monitor.and_then(|mi| state.wm.monitors[mi].focused).is_none() {
        tracing::warn!("no focused desktop to map a new window onto");
        return;
    }

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

    // The tree side, shared with the golden tests: placement, rules, the
    // `node_add` report, and focus, activation or stacking.
    let size_hints = window_size_hints(&window);
    // An X11 window has a geometry of its own. A Wayland window has none
    // before its first buffer, except the size it cannot leave (minimum equal
    // to maximum, a Java dialog): at no position, so it is centred, as bspwm
    // centres an X11 window that asked for none.
    let geometry = match window.x11_surface() {
        Some(x11) => Some(x11.geometry()).filter(|g| g.size.w > 0 && g.size.h > 0).map(|g| {
            bsp_core::geometry::Rect { x: g.loc.x, y: g.loc.y, width: g.size.w, height: g.size.h }
        }),
        None => size_hints.min.filter(|min| Some(*min) == size_hints.max && min.0 > 0 && min.1 > 0).map(|(w, h)| bsp_core::geometry::Rect::new(0, 0, w, h)),
    };
    let new = bsp_ipc::exec::NewWindow { window: window_id, geometry, size_hints };
    let placed = crate::ipc::with_ops(state, |ctx, events| bsp_ipc::exec::manage_window(ctx, &new, &consequence, events));
    let Some(bsp_ipc::exec::Coordinates { monitor: mi, desktop: di, node: Some(node) }) = placed else {
        return;
    };
    // Remembered, so a later hints change is seen as one.
    refresh_size_hints(state, &window);

    // A floating Wayland window of no known size chooses its own (a configure
    // without a size) and is centred at it on its first buffer (`on_commit`).
    let natural = window.toplevel().is_some()
        && new.geometry.is_none()
        && consequence.rect.is_none()
        && state.wm.monitors[mi].desktops[di].tree.node(node).client.as_ref().is_some_and(|c| c.state == bsp_core::node::ClientState::Floating);
    if natural {
        window.user_data().insert_if_missing(|| NaturalSize(std::cell::Cell::new(false)));
        if let Some(n) = window.user_data().get::<NaturalSize>() {
            n.0.set(true);
        }
    }
    // Tiled only because nothing said otherwise yet: a Wayland dialog may declare
    // its fixed size after the first configure (`float_late_fixed_size`).
    let tiled_by_default = window.toplevel().is_some()
        && consequence.state.is_none()
        && state.wm.monitors[mi].desktops[di].tree.node(node).client.as_ref().is_some_and(|c| c.state == bsp_core::node::ClientState::Tiled);
    if tiled_by_default {
        window.user_data().insert_if_missing(|| LateFixedSize(std::cell::Cell::new(false)));
        if let Some(m) = window.user_data().get::<LateFixedSize>() {
            m.0.set(true);
        }
    }

    let Some((rect, tiled, fullscreen)) = state.wm.monitors[mi].desktops[di].tree.node(node).client.as_ref().map(|client| {
        let fullscreen = client.state == bsp_core::node::ClientState::Fullscreen;
        let tiled = client.state.is_tiled();
        let rect = if tiled || fullscreen { client.tiled_rectangle } else { client.floating_rectangle };
        (rect, tiled, fullscreen)
    }) else {
        return;
    };

    if let Some(toplevel) = window.toplevel() {
        toplevel.with_pending_state(|s| {
            s.size = (!natural).then(|| Size::from((rect.width.max(1), rect.height.max(1))));
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

    // A window placed on a desktop that is not shown stays out of the space;
    // it appears when its desktop does (`sync_wayland_from_core`).
    let hidden = state.wm.monitors[mi].desktops[di].tree.node(node).hidden;
    if state.wm.monitors[mi].focused == Some(di) && !hidden {
        state.space.map_element(window, (rect.x, rect.y), true);
    }
    // `manage_window` focused it: the keyboard follows.
    let focused = state.wm.focused_monitor == Some(mi)
        && state.wm.monitors[mi].focused == Some(di)
        && state.wm.monitors[mi].desktops[di].tree.focus == Some(node);
    if focused {
        crate::input::focus_node(state, mi, di, node);
    }

    // The new window split an existing one (or, for a floating or hidden one,
    // changed nothing): the siblings whose rectangles just changed are resized
    // by the one sync at the end of this event-loop turn.
    // bspwm: `focus_node()` centres the pointer on what it focuses.
    state.request_warp(focus_before);

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
    // A window whose node is already gone from the tree (`bspc node -k` removes
    // it at once) still leaves the space, the adapter and the foreign-toplevel
    // list when its surface dies.
    let located = state.locate_client(window_id);
    state.wm.stacking.remove(window_id);

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

    if located.is_none() {
        state.backend_data.queue_redraw();
        return;
    }
    let focus_before = crate::input::focus_key(state);
    // bspwm: `unmanage_window()`: `node_remove`, the node out of the tree (the
    // focus moving on if it held it), and the desktop re-arranged.
    crate::ipc::with_ops(state, |ctx, events| bsp_ipc::exec::unmanage_window(ctx, window_id, events));
    // The siblings grow into the freed space, and keyboard focus moves to
    // the node the tree now focuses, at the end of this event-loop turn.
    state.request_warp(focus_before);

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
    let recheck = with_states(surface, |states| states.data_map.get::<RecheckPointer>().is_some_and(|r| r.0.replace(false)));
    if recheck {
        crate::input::refresh_pointer_focus(state);
    }
    let Some(toplevel) = window.toplevel() else {
        return;
    };
    // A client that changed its min/max size is shown at the new limits.
    if refresh_size_hints(state, &window) {
        state.request_sync();
    }
    adopt_natural_size(state, &window);
    float_late_fixed_size(state, &window);
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

/// A window's size hints: xdg `set_min_size`/`set_max_size` (only those two
/// exist on Wayland), or X11 `WM_NORMAL_HINTS` (min, max, base, increments,
/// aspect), as Smithay keeps it current.
///
/// bspwm: `client_t.size_hints`, read in `src/window.c` `manage_window()` and
/// on `PropertyNotify` of `WM_NORMAL_HINTS`.
pub(crate) fn window_size_hints(window: &Window) -> bsp_core::node::SizeHints {
    let positive = |(w, h): (i32, i32)| (w > 0 || h > 0).then_some((w.max(0), h.max(0)));
    if let Some(toplevel) = window.toplevel() {
        return with_states(toplevel.wl_surface(), |states| {
            let mut cached = states.cached_state.get::<smithay::wayland::shell::xdg::SurfaceCachedState>();
            let current = cached.current();
            bsp_core::node::SizeHints {
                min: positive((current.min_size.w, current.min_size.h)),
                max: positive((current.max_size.w, current.max_size.h)),
                ..Default::default()
            }
        });
    }
    if let Some(hints) = window.x11_surface().and_then(|x11| x11.size_hints()) {
        return bsp_core::node::SizeHints {
            min: hints.min_size.and_then(positive),
            max: hints.max_size.and_then(positive),
            base: hints.base_size.and_then(positive),
            inc: hints.size_increment.filter(|&(w, h)| w > 0 && h > 0),
            aspect: hints.aspect.map(|(min, max)| ((min.numerator, min.denominator), (max.numerator, max.denominator))),
        };
    }
    bsp_core::node::SizeHints::default()
}

/// The hints last copied into the tree for a window, so a commit that did not
/// change them costs no tree walk.
struct KnownSizeHints(std::cell::Cell<Option<bsp_core::node::SizeHints>>);

/// Copies the window's current size hints into its client when they changed.
/// Returns whether they did (the window needs a new configure).
pub(crate) fn refresh_size_hints<Bd: Backend + 'static>(state: &mut State<Bd>, window: &Window) -> bool {
    let hints = window_size_hints(window);
    let known = window.user_data().get_or_insert(|| KnownSizeHints(std::cell::Cell::new(None)));
    if known.0.get() == Some(hints) {
        return false;
    }
    let Some((mi, di, node)) = state.adapter.id_of(window).and_then(|id| state.locate_client(id)) else {
        return false;
    };
    known.0.set(Some(hints));
    let Some(client) = state.wm.monitors[mi].desktops[di].tree.node_mut(node).client.as_mut() else {
        return false;
    };
    client.size_hints = hints;
    true
}

/// A Wayland window tiled at map time only because nothing asked otherwise;
/// cleared on its first buffer (`float_late_fixed_size`).
struct LateFixedSize(std::cell::Cell<bool>);

/// On a window's first buffer: if it was tiled only by default and has since
/// given itself a fixed size (minimum equal to maximum), it floats centred at
/// that size, as a fixed-size X11 window does (bspwm `_apply_hints()`).
/// LibreOffice's and GTK's message dialogs have no parent and set their size
/// only after the first configure, so the map-time check cannot see it.
fn float_late_fixed_size<Bd: Backend + 'static>(state: &mut State<Bd>, window: &Window) {
    let Some(marker) = window.user_data().get::<LateFixedSize>() else {
        return;
    };
    if !marker.0.get() || window.geometry().size.w <= 0 || window.geometry().size.h <= 0 {
        return;
    }
    marker.0.set(false);
    let hints = window_size_hints(window);
    let Some((w, h)) = hints.min.filter(|min| Some(*min) == hints.max && min.0 > 0 && min.1 > 0) else {
        return;
    };
    let Some((mi, di, node)) = state.adapter.id_of(window).and_then(|id| state.locate_client(id)) else {
        return;
    };
    let monitor = state.wm.monitors[mi].rectangle;
    let desktop = state.wm.monitors[mi].desktops[di].id;
    let Some(client) = state.wm.monitors[mi].desktops[di].tree.node_mut(node).client.as_mut() else {
        return;
    };
    if client.state != bsp_core::node::ClientState::Tiled {
        return;
    }
    let mut rect = bsp_core::geometry::Rect::new(0, 0, w, h);
    bsp_ipc::exec::center_rect(&mut rect, monitor, client.border_width);
    client.floating_rectangle = rect;
    let Some(wire_id) = state.registry.id_of(desktop, node) else {
        return;
    };
    tracing::debug!(window = wire_id, w, h, "a tiled window gave itself a fixed size; floating it");
    crate::protocols::run_bspc(state, &["node".to_string(), format!("0x{wire_id:08X}"), "-t".to_string(), "floating".to_string()]);
}

/// A floating Wayland window that was mapped without a size, choosing its own
/// (`map_new_window_with`); cleared once it has a buffer.
struct NaturalSize(std::cell::Cell<bool>);

fn waits_for_natural_size(window: &Window) -> bool {
    window.user_data().get::<NaturalSize>().is_some_and(|n| n.0.get())
}

/// A floating Wayland window mapped without a size drew its first buffer at the
/// size it chose: that becomes its floating rectangle, centred on its monitor.
fn adopt_natural_size<Bd: Backend + 'static>(state: &mut State<Bd>, window: &Window) {
    if !waits_for_natural_size(window) {
        return;
    }
    let size = window.geometry().size;
    if size.w <= 0 || size.h <= 0 {
        return;
    }
    if let Some(n) = window.user_data().get::<NaturalSize>() {
        n.0.set(false);
    }
    let Some((mi, di, node)) = state.adapter.id_of(window).and_then(|id| state.locate_client(id)) else {
        return;
    };
    let monitor = state.wm.monitors[mi].rectangle;
    if let Some(client) = state.wm.monitors[mi].desktops[di].tree.node_mut(node).client.as_mut() {
        if client.state == bsp_core::node::ClientState::Floating {
            let mut rect = bsp_core::geometry::Rect::new(0, 0, size.w, size.h);
            bsp_ipc::exec::center_rect(&mut rect, monitor, client.border_width);
            client.floating_rectangle = rect;
        }
    }
    state.request_sync();
}

/// Tells an `xdg_toplevel` whether it is suspended: on a desktop that is not
/// shown, or hidden (xdg-shell v6 `suspended`, as cosmic-comp does), so
/// browsers and games stop drawing frames nobody sees. Configured only when it
/// changes.
fn set_suspended(window: &Window, suspended: bool) {
    let Some(toplevel) = window.toplevel() else { return };
    let changed = toplevel.with_pending_state(|s| {
        if s.states.contains(xdg_toplevel::State::Suspended) == suspended {
            return false;
        }
        if suspended {
            s.states.set(xdg_toplevel::State::Suspended);
        } else {
            s.states.unset(xdg_toplevel::State::Suspended);
        }
        true
    });
    if changed && toplevel.is_initial_configure_sent() {
        toplevel.send_configure();
    }
}

/// Set on a surface whose next commit should re-check what is under the pointer.
struct RecheckPointer(std::cell::Cell<bool>);

/// Makes the next commit of `surface` re-check the pointer focus
/// ([`crate::input::refresh_pointer_focus`]).
pub(crate) fn recheck_pointer_on_commit(surface: &WlSurface) {
    with_states(surface, |states| {
        states.data_map.insert_if_missing(|| RecheckPointer(std::cell::Cell::new(false)));
        if let Some(r) = states.data_map.get::<RecheckPointer>() {
            r.0.set(true);
        }
    });
}

/// Whether a managed X11 window was unmapped by [`set_x11_shown`].
struct X11Hidden(std::cell::Cell<bool>);

/// Maps or unmaps a managed X11 window in the X server as its node is shown
/// or hidden (another desktop, `node -g hidden`).
///
/// bspwm: `show_node()`/`hide_node()` -> `window_show()`/`window_hide()`, which
/// unmap the X window. Taking it out of the space alone is not enough: a game
/// that holds an X pointer or keyboard grab (`XGrabPointer`) keeps it while
/// its window stays viewable, and Xwayland then sends every click on the other
/// X11 windows (Steam) to the game. Unmapping makes the window unviewable,
/// which ends its grabs. Smithay unmaps the frame, not the client window, so
/// this is not seen as the client withdrawing it, and the window is marked
/// `IconicState`.
fn set_x11_shown(window: &Window, shown: bool) {
    let Some(x11) = window.x11_surface() else { return };
    if x11.is_override_redirect() {
        return;
    }
    let hidden = window.user_data().get_or_insert(|| X11Hidden(std::cell::Cell::new(false)));
    if hidden.0.get() != shown {
        return;
    }
    hidden.0.set(!shown);
    if let Err(err) = x11.set_mapped(shown) {
        tracing::debug!(shown, "cannot map or unmap an X11 window: {err}");
    }
}

/// Runs the reconcile asked for this turn (`State::request_sync`), then the
/// pointer warp (`State::request_warp`). Called after each event-loop turn and
/// before each input event (a key pressed right after a desktop switch goes to
/// the window focused now), when no Smithay lock is held.
pub fn run_deferred_sync<Bd: Backend + 'static>(state: &mut State<Bd>) {
    if std::mem::take(&mut state.sync_pending) {
        sync_wayland_from_core(state);
    }
    if let Some(before) = state.warp_from.take() {
        crate::input::warp_pointer_for_focus(state, before);
    }
}

/// Puts the shown windows in the space in `bsp-core`'s stacking order
/// (`Wm::stacking`, bottom first). `Space::map_element` raises the window it
/// touches, so without this a tiled window that is merely re-laid-out would end
/// up above a floating one. Unmanaged X11 windows are raised over them again.
pub(crate) fn apply_stacking<Bd: Backend + 'static>(state: &mut State<Bd>) {
    debug_assert!(!crate::pointer_action::in_grab_callback(), "stacking from a pointer grab callback; use `request_sync`");
    let wanted: Vec<Window> = state
        .wm
        .stacking
        .windows()
        .iter()
        .filter_map(|w| state.adapter.window(*w).cloned())
        .filter(|w| state.space.element_location(w).is_some())
        .collect();
    let current: Vec<&Window> = state.space.elements().filter(|w| state.adapter.id_of(w).is_some()).collect();
    if !current.iter().copied().eq(wanted.iter()) {
        for window in &wanted {
            state.space.raise_element(window, false);
        }
        state.backend_data.queue_redraw();
    }
    // Menus, tooltips and override-redirect (game) windows of X11 clients stay
    // above the managed ones, whichever of them was just raised.
    state.raise_unmanaged_x11();
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
    debug_assert!(!crate::pointer_action::in_grab_callback(), "sync inside a pointer grab callback deadlocks; set `sync_pending`");
    // Collected first, rather than acted on while borrowing `state.wm`,
    // since applying each one needs `&mut state.space`/`&mut state.adapter`.
    let mut updates = Vec::new();
    for m in &state.wm.monitors {
        for (di, d) in m.desktops.iter().enumerate() {
            // bspwm shows only a monitor's focused desktop
            // (`src/desktop.c` `show_desktop()`/`hide_desktop()`): windows
            // of every other desktop are unmapped, exactly like a hidden
            // node (a sticky node is moved to the shown desktop by
            // `exec::transfer_sticky_nodes` when the desktop changes).
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
            set_x11_shown(&window, false);
            set_suspended(&window, true);
            continue;
        }
        set_x11_shown(&window, true);
        set_suspended(&window, false);
        sync_one_window(state, &window, &client);
    }
    apply_stacking(state);
    crate::input::sync_keyboard_focus(state);
    let focused = state.seat.get_keyboard().and_then(|k| k.current_focus()).and_then(|t| t.wl_surface().map(|s| s.into_owned()));
    state.update_activated(focused.as_ref());
    crate::input::refresh_pointer_focus(state);
    crate::protocols::update_fractional_scales(state);
    crate::xwayland::publish_ewmh(state);
}

fn sync_one_window<Bd: Backend + 'static>(
    state: &mut State<Bd>,
    window: &Window,
    client: &bsp_core::node::Client,
) {
    // bspwm: `apply_layout()` moves the window to the layout's rectangle
    // after `apply_size_hints()`.
    let rect = client.shown_rectangle();
    let want_fullscreen = client.state == bsp_core::node::ClientState::Fullscreen;

    let current_loc = state.space.element_location(window);
    let target_loc = (rect.x, rect.y).into();
    if current_loc != Some(target_loc) {
        state.space.map_element(window.clone(), target_loc, false);
    }

    // Still choosing its own size (`adopt_natural_size`).
    if waits_for_natural_size(window) {
        return;
    }
    if let Some(toplevel) = window.toplevel() {
        let target = Size::from((rect.width.max(1), rect.height.max(1)));
        let want_tiled = client.state.is_tiled();
        let (current, is_fullscreen, is_tiled) = toplevel.with_pending_state(|s| {
            (s.size, s.states.contains(xdg_toplevel::State::Fullscreen), s.states.contains(xdg_toplevel::State::TiledLeft))
        });
        if current != Some(target) || is_fullscreen != want_fullscreen || is_tiled != want_tiled {
            toplevel.with_pending_state(|s| {
                s.size = Some(target);
                if want_fullscreen {
                    s.states.set(xdg_toplevel::State::Fullscreen);
                } else {
                    s.states.unset(xdg_toplevel::State::Fullscreen);
                }
                // The tiled states follow the node's state (a window turned
                // floating stops looking tiled, and back), so a client draws
                // its corners and shadows accordingly.
                for edge in [
                    xdg_toplevel::State::TiledLeft,
                    xdg_toplevel::State::TiledRight,
                    xdg_toplevel::State::TiledTop,
                    xdg_toplevel::State::TiledBottom,
                ] {
                    if want_tiled {
                        s.states.set(edge);
                    } else {
                        s.states.unset(edge);
                    }
                }
            });
            if toplevel.is_initial_configure_sent() {
                toplevel.send_configure();
            }
        }
    } else if let Some(x11) = window.x11_surface() {
        let mut target = smithay::utils::Rectangle::new(
            (rect.x, rect.y).into(),
            (rect.width.max(1), rect.height.max(1)).into(),
        );
        // A fullscreen game that changed the video mode keeps the size Xwayland
        // emulates (and scales up itself) instead of being stretched.
        if want_fullscreen && x11.geometry() != target {
            let emulated = state.emulated_size_cached(x11.window_id());
            tracing::debug!(class = x11.class(), geometry = ?x11.geometry(), ?target, ?emulated, "fullscreen X11 window differs from its slot");
            if let Some((w, h)) = emulated.filter(|&(w, h)| w <= target.size.w && h <= target.size.h) {
                target.size = (w, h).into();
            }
        }
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
    // `ignore_ewmh_fullscreen`: the request is answered, with no change.
    let ignored = state.wm.settings.ignore_ewmh_fullscreen;
    if (on && ignored.enter) || (!on && ignored.exit) {
        surface.send_configure();
        return;
    }
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
