//! Forwarding winit input events to the Wayland seat, and click-to-focus.
//!
//! Every keyboard event is checked against the hardcoded emergency quit
//! key first, then matched against `bsp-hotkeys`' chord matcher
//! (`crate::hotkeys::filter`) before it would otherwise reach the
//! focused client. Ctrl+Alt+F1–F12 (TTY switch, `docs/design.md`'s
//! Reliability section) is a real DRM/libseat session concept the nested
//! winit backend has nothing to switch *to*: [`vt_switch_target`] only
//! classifies the key, and `crate::udev_backend` acts on it.

use smithay::backend::input::{
    Axis, AxisSource, Event, InputBackend, InputEvent, KeyState, KeyboardKeyEvent,
    PointerAxisEvent, PointerButtonEvent, PointerMotionAbsoluteEvent,
};
use smithay::input::keyboard::{KeysymHandle, ModifiersState};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_pointer;
use smithay::utils::SERIAL_COUNTER;
use smithay::wayland::seat::WaylandFocus;

use bsp_core::id::NodeId;

use crate::state::{Backend, State};

/// Handles one winit-sourced input event.
///
/// `nested`-only in practice (`crate::udev_backend::process_input_event`
/// is the `real`-backend sibling, `docs/bsp-compositor.md` The hardware backend
/// progress) — this module itself stays unconditionally compiled, since
/// its `on_pointer_button`/`on_pointer_axis` are shared by both
/// backends, so a `real`-only build would otherwise warn on this
/// function specifically as unreachable dead code.
#[cfg_attr(not(feature = "nested"), allow(dead_code))]
pub fn process_input_event<B: InputBackend, Bd: Backend + 'static>(
    state: &mut State<Bd>,
    event: InputEvent<B>,
    output: &Output,
) {
    crate::protocols::notify_activity(state);
    match event {
        InputEvent::Keyboard { event } => {
            let keycode = event.key_code();
            let key_state = event.state();
            let pressed = key_state == KeyState::Pressed;
            let serial = SERIAL_COUNTER.next_serial();
            let time = Event::time_msec(&event);
            let Some(keyboard) = state.seat.get_keyboard() else {
                return;
            };
            keyboard.input::<(), _>(
                state,
                keycode,
                key_state,
                serial,
                time,
                |data, mods, sym| {
                    if is_emergency_quit(mods, &sym, pressed) {
                        data.running = false;
                        return smithay::input::keyboard::FilterResult::Intercept(());
                    }
                    crate::hotkeys::filter(data, mods, sym, pressed)
                },
            );
        }
        InputEvent::PointerMotionAbsolute { event } => {
            on_pointer_motion_absolute(state, event, output)
        }
        InputEvent::PointerButton { event } => on_pointer_button(state, event),
        InputEvent::PointerAxis { event } => on_pointer_axis(state, event),
        _ => {}
    }
}

/// `Ctrl+Alt+Shift+Escape`: quits `bspwm-rs` unconditionally.
///
/// `docs/design.md`'s Reliability section: a broken or missing hotkey
/// config must never lock the user in, so this is checked directly
/// against the seat's live modifier state and the key's base keysym —
/// entirely independent of `bsp-hotkeys`/sxhkdrc, before its matcher
/// ever sees the event (a compositor with no separate display server
/// to fall back to needs its own escape hatch; bspwm, running under
/// X11, has no equivalent need).
///
/// Matches on `raw_syms()` (the level-0, unshifted symbol) rather than
/// `modified_sym()` for the same reason `crate::hotkeys::filter` does:
/// held modifiers are compared separately, not folded into the symbol
/// itself.
pub(crate) fn is_emergency_quit(mods: &ModifiersState, keysym: &KeysymHandle<'_>, pressed: bool) -> bool {
    pressed
        && mods.ctrl
        && mods.alt
        && mods.shift
        && keysym.raw_syms().first().copied()
            == Some(xkbcommon::xkb::Keysym::new(
                xkbcommon::xkb::keysyms::KEY_Escape,
            ))
}

/// The VT number (1–12) a `Ctrl+Alt+F<n>` press asks to switch to, if
/// `keysym` is a match and `pressed`.
///
/// bspwm-rs's own emergency key (real bspwm runs under X11, where the
/// kernel/X server handle VT switching). Accepts both plain `F1`–`F12`
/// (the keysym before xkb's `Ctrl+Alt+Fn` → `XF86Switch_VT_n` remap)
/// and the `XF86Switch_VT_1`–`12` keysyms themselves, since which one
/// the layout reports depends on the xkb options in use.
#[cfg_attr(not(feature = "real"), allow(dead_code))]
pub(crate) fn vt_switch_target(mods: &ModifiersState, keysym: &KeysymHandle<'_>, pressed: bool) -> Option<i32> {
    if !pressed || !mods.ctrl || !mods.alt {
        return None;
    }
    keysym.raw_syms().iter().chain(std::iter::once(&keysym.modified_sym())).find_map(|s| vt_for_keysym(s.raw()))
}

/// Maps a raw keysym value to a VT number: `XK_F1..XK_F12` or
/// `XF86XK_Switch_VT_1..12`.
#[cfg_attr(not(feature = "real"), allow(dead_code))]
pub(crate) fn vt_for_keysym(raw: u32) -> Option<i32> {
    const F1: u32 = 0xffbe;
    const SWITCH_VT_1: u32 = 0x1008fe01;
    match raw {
        F1..=0xffc9 => Some((raw - F1) as i32 + 1),
        SWITCH_VT_1..=0x1008fe0c => Some((raw - SWITCH_VT_1) as i32 + 1),
        _ => None,
    }
}

#[cfg_attr(not(feature = "nested"), allow(dead_code))]
fn on_pointer_motion_absolute<B: InputBackend, Bd: Backend + 'static>(
    state: &mut State<Bd>,
    event: impl PointerMotionAbsoluteEvent<B>,
    output: &Output,
) {
    let Some(output_geo) = state.space.output_geometry(output) else {
        return;
    };
    let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
    let serial = SERIAL_COUNTER.next_serial();

    let under = crate::layers::surface_under(state, pos);
    let pointer = state.pointer.clone();
    pointer.motion(
        state,
        under,
        &MotionEvent {
            location: pos,
            serial,
            time: event.time_msec(),
        },
    );
    pointer.frame(state);
    crate::toplevel_drag::follow(state);
}

pub(crate) fn on_pointer_button<B: InputBackend, Bd: Backend + 'static>(
    state: &mut State<Bd>,
    event: impl PointerButtonEvent<B>,
) {
    deliver_button(
        state,
        event.button_code(),
        wl_pointer::ButtonState::from(event.state()),
        event.time_msec(),
    );
}

/// A pointer button changed state, from whatever source (a real device,
/// or `zwlr_virtual_pointer_v1`): click-to-focus and pointer bindings
/// first, then the focused client.
pub(crate) fn deliver_button<Bd: Backend + 'static>(
    state: &mut State<Bd>,
    button: u32,
    button_state: wl_pointer::ButtonState,
    time: u32,
) {
    let serial = SERIAL_COUNTER.next_serial();
    let locked = state.protocols.session_lock.locked;
    tracing::debug!(button, ?button_state, layer = ?state.pointer.current_focus().and_then(|s| crate::layers::namespace_of(state, &s)), "pointer button");

    // `crate::pointer_action::on_button_press` covers both click-to-focus
    // and `pointer_modifier`-held drag bindings, and decides whether
    // this press should still reach the client afterward — see its own
    // doc comment for why a press matching neither is always forwarded
    // untouched, same as bspwm's un-grabbed default.
    let pressed = button_state == wl_pointer::ButtonState::Pressed && !locked;
    if pressed {
        let hit = state.pointer.current_focus();
        crate::shell::dismiss_grabbed_popups(state, hit.as_ref());
    }
    let on_layer = pressed
        && state
            .pointer
            .current_focus()
            .is_some_and(|surface| crate::layers::focus_on_click(state, &surface, serial));
    if pressed && !on_layer && !crate::pointer_action::on_button_press(state, button, serial, time) {
        return;
    }

    let Ok(wire_state) = button_state.try_into() else {
        return;
    };
    let pointer = state.pointer.clone();
    pointer.button(
        state,
        &ButtonEvent {
            button,
            state: wire_state,
            serial,
            time,
        },
    );
    pointer.frame(state);
}

pub(crate) fn on_pointer_axis<B: InputBackend, Bd: Backend + 'static>(
    state: &mut State<Bd>,
    event: impl PointerAxisEvent<B>,
) {
    let horizontal = event
        .amount(Axis::Horizontal)
        .unwrap_or_else(|| event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0);
    let vertical = event
        .amount(Axis::Vertical)
        .unwrap_or_else(|| event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0);

    let mut frame = AxisFrame::new(event.time_msec()).source(event.source());
    if horizontal != 0.0 {
        frame = frame
            .relative_direction(Axis::Horizontal, event.relative_direction(Axis::Horizontal))
            .value(Axis::Horizontal, horizontal);
        if let Some(discrete) = event.amount_v120(Axis::Horizontal) {
            frame = frame.v120(Axis::Horizontal, discrete as i32);
        }
    }
    if vertical != 0.0 {
        frame = frame
            .relative_direction(Axis::Vertical, event.relative_direction(Axis::Vertical))
            .value(Axis::Vertical, vertical);
        if let Some(discrete) = event.amount_v120(Axis::Vertical) {
            frame = frame.v120(Axis::Vertical, discrete as i32);
        }
    }
    if event.source() == AxisSource::Finger {
        if event.amount(Axis::Horizontal) == Some(0.0) {
            frame = frame.stop(Axis::Horizontal);
        }
        if event.amount(Axis::Vertical) == Some(0.0) {
            frame = frame.stop(Axis::Vertical);
        }
    }
    let pointer = state.pointer.clone();
    pointer.axis(state, frame);
    pointer.frame(state);
}

/// Makes the seat's keyboard focus follow `bsp-core`'s focus after a
/// command changed it (`bspc desktop -f`, `bspc node -f`, closing the
/// focused window …): the focused desktop's focused node's window, or no
/// focus at all if there is none. A layer surface holding keyboard focus
/// (a launcher, a lock screen) is left alone.
pub(crate) fn sync_keyboard_focus<Bd: Backend + 'static>(state: &mut State<Bd>) {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    let current = keyboard.current_focus();
    if let Some(surface) = current.as_ref().and_then(|t| t.wl_surface()) {
        // A destroyed surface owns nothing: without the `alive()` check a
        // closed launcher kept the keyboard for good.
        if smithay::reexports::wayland_server::Resource::is_alive(&*surface)
            && crate::layers::holds_exclusive_focus(state, &surface)
        {
            // An exclusive layer surface owns focus. A clicked `on_demand` panel such as
            // waybar does not keep it: focus returns to the tree's.
            return;
        }
    }
    if state.protocols.session_lock.locked {
        // Locked: the lock surface owns the keyboard (set when it was made).
        return;
    }
    // A desktop with windows but no focused node (its focus was cleared)
    // gets its most recent window back.
    if let Some((mi, di)) = state
        .wm
        .focused_monitor
        .and_then(|mi| state.wm.monitors.get(mi).and_then(|m| m.focused).map(|di| (mi, di)))
    {
        state.wm.refocus_after_removal(mi, di);
    }
    let wanted = state
        .wm
        .focused_monitor
        .and_then(|mi| state.wm.monitors.get(mi))
        .and_then(|m| m.focused.and_then(|di| m.desktops.get(di)))
        .and_then(|d| d.tree.focus.map(|f| d.tree.node(f)))
        .filter(|node| !node.hidden)
        .and_then(|node| node.client.as_ref())
        .and_then(|client| state.adapter.window(client.window).cloned())
        .and_then(|window| crate::focus::focus_target_of(&window));
    if wanted != current {
        tracing::debug!("keyboard focus follows the focused node");
        keyboard.set_focus(state, wanted, SERIAL_COUNTER.next_serial());
    }
}

/// Warps the pointer to `location` (global logical coordinates) and tells
/// the surface under it — the motion half shared by absolute devices and
/// `zwlr_virtual_pointer_v1` (real relative motion, which also sends
/// `relative_motion`, has its own path in `crate::udev_backend`).
pub(crate) fn pointer_motion_to<Bd: Backend + 'static>(
    state: &mut State<Bd>,
    location: smithay::utils::Point<f64, smithay::utils::Logical>,
    time: u32,
) {
    let serial = SERIAL_COUNTER.next_serial();
    let under = crate::layers::surface_under(state, location);
    tracing::trace!(?location, has_surface = under.is_some(), "pointer_motion_to");
    let pointer = state.pointer.clone();
    pointer.motion(state, under, &MotionEvent { location, serial, time });
    pointer.frame(state);
    crate::constraints::update(state);
    crate::toplevel_drag::follow(state);
    state.backend_data.queue_redraw();
}

/// The bounding box of every output, in global logical coordinates (where
/// the pointer may go).
pub(crate) fn outputs_bounds<Bd: Backend + 'static>(state: &State<Bd>) -> smithay::utils::Rectangle<f64, smithay::utils::Logical> {
    let mut bounds: Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>> = None;
    for output in state.space.outputs() {
        if let Some(geo) = state.space.output_geometry(output) {
            bounds = Some(bounds.map_or(geo, |b| b.merge(geo)));
        }
    }
    bounds.map_or_else(|| smithay::utils::Rectangle::from_size((0.0, 0.0).into()), |b| b.to_f64())
}

/// The window currently showing under `location`, resolved down to a
/// `bsp-core` client, if any — `state.space.element_under` plus the
/// adapter's `Window` ↔ `WindowId` map plus a tree scan, factored out
/// so `crate::pointer_action` can resolve "what's under the pointer"
/// the same way click-to-focus does.
pub(crate) fn window_under<Bd: Backend + 'static>(
    state: &State<Bd>,
    location: smithay::utils::Point<f64, smithay::utils::Logical>,
) -> Option<bsp_core::id::WindowId> {
    let (window, _) = state.space.element_under(location)?;
    state.adapter.id_of(window)
}

/// Scans every monitor/desktop's tree for the leaf showing `window_id`,
/// returning its `(monitor index, desktop index, NodeId)`. `bsp-core`
/// has no reverse `WindowId -> NodeId` index of its own (`docs/design.md`
/// Architecture: that mapping is the adapter's job, `bsp-ipc::registry`
/// only tracks the *wire* id), so this is a linear scan — fine at the
/// scale a single compositor's monitors/desktops/windows reach.
pub(crate) fn locate_window<Bd: Backend + 'static>(
    state: &State<Bd>,
    window_id: bsp_core::id::WindowId,
) -> Option<(usize, usize, NodeId)> {
    for mi in 0..state.wm.monitors.len() {
        for di in 0..state.wm.monitors[mi].desktops.len() {
            let tree = &state.wm.monitors[mi].desktops[di].tree;
            let mut n = tree.first_extrema(tree.root);
            while let Some(id) = n {
                if tree
                    .node(id)
                    .client
                    .as_ref()
                    .is_some_and(|c| c.window == window_id)
                {
                    return Some((mi, di, id));
                }
                n = tree.next_leaf(Some(id), tree.root);
            }
        }
    }
    None
}

/// Sets both halves of "node is focused": `bsp-core`'s tree/monitor
/// focus, and the Wayland seat's keyboard focus on its client surface.
/// Shared by click-to-focus and `crate::pointer_action`'s `Focus`
/// pointer action, which both end up doing exactly this (bspwm: both
/// paths call `focus_node()`, `src/tree.c`).
pub(crate) fn set_focus<Bd: Backend + 'static>(
    state: &mut State<Bd>,
    mi: usize,
    di: usize,
    node: NodeId,
    serial: smithay::utils::Serial,
) {
    state.wm.monitors[mi].desktops[di].tree.focus = Some(node);
    state.wm.focused_monitor = Some(mi);
    state.wm.monitors[mi].focused = Some(di);
    let Some(client) = state.wm.monitors[mi].desktops[di]
        .tree
        .node(node)
        .client
        .clone()
    else {
        return;
    };
    let Some(window) = state.adapter.window(client.window).cloned() else {
        return;
    };
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    keyboard.set_focus(state, crate::focus::focus_target_of(&window), serial);
}

/// Sets the seat's keyboard focus to `node`'s client surface (`bsp-core`'s
/// side is the caller's job — this only drives the Wayland-visible half).
pub fn focus_node<Bd: Backend + 'static>(state: &mut State<Bd>, mi: usize, di: usize, node: NodeId) {
    let Some(client) = state.wm.monitors[mi].desktops[di]
        .tree
        .node(node)
        .client
        .clone()
    else {
        return;
    };
    let Some(window) = state.adapter.window(client.window).cloned() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    keyboard.set_focus(state, crate::focus::focus_target_of(&window), serial);
}

#[cfg(test)]
mod tests {
    use super::vt_for_keysym;

    #[test]
    fn function_keys_map_to_vts() {
        assert_eq!(vt_for_keysym(0xffbe), Some(1));
        assert_eq!(vt_for_keysym(0xffc9), Some(12));
        assert_eq!(vt_for_keysym(0xffca), None);
    }

    #[test]
    fn switch_vt_keysyms_map_to_vts() {
        assert_eq!(vt_for_keysym(0x1008fe01), Some(1));
        assert_eq!(vt_for_keysym(0x1008fe0c), Some(12));
        assert_eq!(vt_for_keysym(0x1008fe0d), None);
        assert_eq!(vt_for_keysym(0x61), None);
    }
}
