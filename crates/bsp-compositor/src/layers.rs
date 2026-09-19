//! `wlr-layer-shell`: panels, bars, wallpapers and overlays (waybar, swaybg,
//! launchers, notification daemons).
//!
//! bspwm learns about panels through `_NET_WM_STRUT`; the Wayland
//! equivalent is a layer surface's *exclusive zone* (`docs/design.md`,
//! Compatibility). Every output keeps a Smithay `LayerMap`; after each
//! change the map's non-exclusive zone is turned into the matching
//! `bsp-core` monitor's `struts` (`bsp_core::monitor::Monitor::struts`)
//! and its desktops are re-laid-out, so tiled windows stay clear of a bar.
//!
//! Stacking, top to bottom: cursor, `overlay`, `top`, window borders,
//! windows, `bottom`, `background` (`crate::render::output_elements`).

use smithay::desktop::{layer_map_for_output, LayerSurface as DesktopLayerSurface, WindowSurfaceType};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::compositor::with_states;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::wlr_layer::{
    KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData, WlrLayerShellHandler, WlrLayerShellState,
};

use bsp_core::geometry::Padding;

use crate::state::{Backend, State};

impl<Bd: Backend + 'static> WlrLayerShellHandler for State<Bd> {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.protocols.layer_shell
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: smithay::wayland::shell::xdg::PopupSurface) {
        crate::shell::unconstrain_popup(self, &popup);
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<WlOutput>,
        layer: Layer,
        namespace: String,
    ) {
        // The client's choice, else the output of the focused monitor
        // (a panel asking for "any" output belongs where the user is),
        // else the first output.
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.focused_output())
            .or_else(|| self.space.outputs().next().cloned());
        let Some(output) = output else {
            tracing::warn!(namespace, "layer surface with no output to put it on; closing it");
            surface.send_close();
            return;
        };
        tracing::debug!(namespace, ?layer, output = output.name(), "new layer surface");
        let mapped = layer_map_for_output(&output).map_layer(&DesktopLayerSurface::new(surface, namespace));
        if let Err(err) = mapped {
            tracing::warn!("failed to map a layer surface: {err}");
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let found = self.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            let layer = map
                .layers()
                .find(|l| l.layer_surface() == &surface)
                .cloned();
            layer.map(|l| (output.clone(), l))
        });
        if let Some((output, layer)) = found {
            tracing::debug!(namespace = layer.namespace(), output = output.name(), "layer surface destroyed");
            layer_map_for_output(&output).unmap_layer(&layer);
            self.rearrange_layers(&output);
            // A launcher (rofi) closing while it held the keyboard: hand the
            // keyboard back to the focused window, else input goes to a dead
            // surface and never returns.
            if let Some(keyboard) = self.seat.get_keyboard() {
                let held = keyboard
                    .current_focus()
                    .and_then(|t| t.wl_surface().map(|s| s.into_owned()))
                    .is_some_and(|s| &s == layer.wl_surface());
                if held {
                    keyboard.set_focus(self, None, SERIAL_COUNTER.next_serial());
                    crate::input::sync_keyboard_focus(self);
                }
            }
            self.backend_data.queue_redraw();
        }
    }
}
smithay::delegate_layer_shell!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> State<Bd> {
    /// The `Output` showing the focused `bsp-core` monitor, if any.
    pub fn focused_output(&self) -> Option<Output> {
        let name = &self.wm.monitors.get(self.wm.focused_monitor?)?.name;
        self.space.outputs().find(|o| o.name() == *name).cloned()
    }

    /// Re-arranges `output`'s layer surfaces (which sends configures), then
    /// mirrors the panels' reserved space into the monitor's struts and
    /// re-tiles its desktops if that changed.
    pub fn rearrange_layers(&mut self, output: &Output) {
        layer_map_for_output(output).arrange();
        if tracing::enabled!(tracing::Level::DEBUG) {
            let map = layer_map_for_output(output);
            for layer in map.layers() {
                let cached = layer.cached_state();
                tracing::debug!(
                    namespace = layer.namespace(),
                    layer = ?layer.layer(),
                    geometry = ?map.layer_geometry(layer),
                    requested_size = ?cached.size,
                    anchor = ?cached.anchor,
                    exclusive_zone = ?cached.exclusive_zone,
                    output_mode = ?output.current_mode(),
                    output_scale = ?output.current_scale(),
                    "layer surface arranged"
                );
            }
        }
        let Some(out_geo) = self.space.output_geometry(output) else {
            return;
        };
        let zone = layer_map_for_output(output).non_exclusive_zone();
        let struts = Padding {
            top: zone.loc.y.max(0),
            left: zone.loc.x.max(0),
            right: (out_geo.size.w - zone.loc.x - zone.size.w).max(0),
            bottom: (out_geo.size.h - zone.loc.y - zone.size.h).max(0),
        };
        let name = output.name();
        let Some(mi) = self.wm.monitors.iter().position(|m| m.name == name) else {
            return;
        };
        if self.wm.monitors[mi].struts == struts {
            return;
        }
        tracing::debug!(output = name, ?struts, "panel struts changed");
        self.wm.monitors[mi].struts = struts;
        let settings = self.wm.settings.clone();
        for di in 0..self.wm.monitors[mi].desktops.len() {
            self.wm.monitors[mi].arrange(di, &settings);
        }
        crate::shell::sync_wayland_from_core(self);
    }
}

/// Commit hook: if `surface` is a layer surface, re-arrange its output
/// and give it keyboard focus if it asked for exclusive interactivity
/// (a launcher, a lock-style overlay).
pub fn on_commit<Bd: Backend + 'static>(state: &mut State<Bd>, surface: &WlSurface) {
    let found = state.space.outputs().find_map(|output| {
        let map = layer_map_for_output(output);
        let layer = map
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .cloned();
        layer.map(|l| (output.clone(), l))
    });
    let Some((output, layer)) = found else {
        return;
    };
    state.rearrange_layers(&output);
    crate::protocols::update_fractional_scales(state);
    // The protocol requires the initial configure in response to the
    // surface's first commit, and Smithay's `arrange` deliberately never
    // sends one before that (it would carry a size the client had no
    // chance to influence): send it here, after arranging so it carries
    // the computed size. Without it clients wait (waybar logs "Timed out
    // waiting for initial .configure") and the surface stays 0x0.
    let configured = with_states(layer.wl_surface(), |states| {
        states
            .data_map
            .get::<LayerSurfaceData>()
            .and_then(|data| data.lock().ok().map(|d| d.initial_configure_sent))
            .unwrap_or(true)
    });
    if !configured {
        tracing::debug!(namespace = layer.namespace(), "sending a layer surface's initial configure");
        layer.layer_surface().send_configure();
    }
    let state_now = layer.cached_state();
    if state_now.keyboard_interactivity == KeyboardInteractivity::Exclusive
        && matches!(layer.layer(), Layer::Top | Layer::Overlay)
    {
        if let Some(keyboard) = state.seat.get_keyboard() {
            let current = keyboard.current_focus().and_then(|t| t.wl_surface().map(|s| s.into_owned()));
            let already = current.as_ref() == Some(layer.wl_surface());
            if !already {
                tracing::debug!(namespace = layer.namespace(), "exclusive layer surface takes keyboard focus");
                keyboard.set_focus(state, Some(crate::focus::FocusTarget::Surface(layer.wl_surface().clone())), SERIAL_COUNTER.next_serial());
            }
        }
    }
}

/// The namespace (`waybar`, `wallpaper`, `rofi`) of the layer surface `surface`
/// belongs to, for logging.
pub fn namespace_of<Bd: Backend + 'static>(state: &State<Bd>, surface: &WlSurface) -> Option<String> {
    state.space.outputs().find_map(|output| {
        layer_map_for_output(output)
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .map(|l| l.namespace().to_string())
    })
}

/// Whether `surface` is a layer surface that asked for exclusive keyboard
/// focus (a launcher, a lock prompt). An `on_demand` panel that got focus by
/// a click does not count: it must give the keyboard back when the tree's
/// focus changes.
pub fn holds_exclusive_focus<Bd: Backend + 'static>(state: &State<Bd>, surface: &WlSurface) -> bool {
    state.space.outputs().any(|output| {
        layer_map_for_output(output)
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .is_some_and(|l| l.cached_state().keyboard_interactivity == KeyboardInteractivity::Exclusive)
    })
}

/// A click landed on `surface`: if it is a layer surface that accepts
/// keyboard focus (`on_demand` or `exclusive` interactivity), focus it.
/// Returns whether `surface` is a layer surface at all — a click on a
/// panel must not also click-to-focus the window underneath.
pub fn focus_on_click<Bd: Backend + 'static>(
    state: &mut State<Bd>,
    surface: &WlSurface,
    serial: smithay::utils::Serial,
) -> bool {
    let layer = state.space.outputs().find_map(|output| {
        layer_map_for_output(output)
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .cloned()
    });
    let Some(layer) = layer else {
        return false;
    };
    if layer.can_receive_keyboard_focus() {
        if let Some(keyboard) = state.seat.get_keyboard() {
            keyboard.set_focus(state, Some(crate::focus::FocusTarget::Surface(layer.wl_surface().clone())), serial);
        }
    }
    true
}

/// The surface under `pos` (global logical coordinates) and its origin:
/// overlay and top layer surfaces first, then windows, then the bottom
/// and background layers — the reverse of drawing order.
pub fn surface_under<Bd: Backend + 'static>(
    state: &State<Bd>,
    pos: Point<f64, Logical>,
) -> Option<(WlSurface, Point<f64, Logical>)> {
    if state.protocols.session_lock.locked {
        return crate::session_lock::surface_under(state, pos);
    }
    let output = state.space.output_under(pos).next()?;
    let out_geo = state.space.output_geometry(output)?;
    let rel = pos - out_geo.loc.to_f64();
    let from_layers = |layers: &[Layer]| {
        let map = layer_map_for_output(output);
        for &wanted in layers {
            let Some(layer) = map.layer_under(wanted, rel) else {
                continue;
            };
            let Some(geo) = map.layer_geometry(layer) else {
                continue;
            };
            if let Some((surface, loc)) = layer.surface_under(rel - geo.loc.to_f64(), WindowSurfaceType::ALL) {
                return Some((surface, (loc + geo.loc + out_geo.loc).to_f64()));
            }
        }
        None
    };
    if let Some(hit) = from_layers(&[Layer::Overlay, Layer::Top]) {
        return Some(hit);
    }
    if let Some((window, loc)) = state.space.element_under(pos) {
        if let Some((surface, surf_loc)) =
            window.surface_under(pos - loc.to_f64(), WindowSurfaceType::ALL)
        {
            return Some((surface, (surf_loc + loc).to_f64()));
        }
    }
    from_layers(&[Layer::Bottom, Layer::Background])
}

/// Sends frame callbacks to every layer surface on `output`.
pub fn send_frames(output: &Output, time: std::time::Duration) {
    let map = layer_map_for_output(output);
    for layer in map.layers() {
        layer.send_frame(output, time, Some(std::time::Duration::from_secs(1)), |_, _| Some(output.clone()));
    }
}

/// Closes every layer surface on `output` (it is going away).
#[cfg_attr(not(feature = "real"), allow(dead_code))]
pub fn close_all(output: &Output) {
    let map = layer_map_for_output(output);
    for layer in map.layers() {
        layer.layer_surface().send_close();
    }
}
