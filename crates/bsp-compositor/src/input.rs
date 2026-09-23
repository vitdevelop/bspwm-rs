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
    focus_follows_pointer(state);
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
        state.swallowed_buttons.insert(button);
        return;
    }
    // The release of a swallowed press goes nowhere either, unless a drag grab
    // owns it (the grab ends on it).
    if button_state == wl_pointer::ButtonState::Released && state.swallowed_buttons.remove(&button) && !state.pointer.is_grabbed() {
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
/// `relative_motion`, has its own path in `crate::udev_backend`), and by the
/// compositor's own re-targeting and warps.
///
/// It does not apply `focus_follows_pointer`: bspwm changes focus only on real
/// pointer motion (`src/events.c` `motion_notify()`; `enter_notify()` caused
/// by a window appearing under a resting pointer only arms the motion
/// recorder). Callers moving the pointer for a device call
/// [`focus_follows_pointer`] themselves.
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

/// `focus_follows_pointer`: focus the managed window the pointer has moved
/// onto, or, over an empty stretch of another monitor, that monitor. Not while a
/// button is held (a drag), a session is locked, or a panel or launcher has the
/// pointer.
///
/// bspwm: `src/events.c` `enter_notify()` / `motion_notify()`.
pub(crate) fn focus_follows_pointer<Bd: Backend + 'static>(state: &mut State<Bd>) {
    if !state.wm.settings.focus_follows_pointer || state.pointer.is_grabbed() || state.protocols.session_lock.locked {
        return;
    }
    if let Some(surface) = state.pointer.current_focus() {
        if crate::layers::namespace_of(state, &surface).is_some() {
            return;
        }
    }
    let location = state.pointer.current_location();
    let serial = SERIAL_COUNTER.next_serial();
    match state.space.element_under(location).map(|(w, _)| w.clone()) {
        Some(window) => {
            let Some((mi, di, node)) = state.adapter.id_of(&window).and_then(|id| locate_window(state, id)) else {
                return;
            };
            let focused = state.wm.focused_monitor == Some(mi)
                && state.wm.monitors[mi].focused == Some(di)
                && state.wm.monitors[mi].desktops[di].tree.focus == Some(node);
            if !focused {
                set_focus(state, mi, di, node, serial);
            }
        }
        None => {
            let Some(name) = state.space.output_under(location).next().map(|o| o.name()) else {
                return;
            };
            let Some(mi) = state.wm.monitors.iter().position(|m| m.name == name) else {
                return;
            };
            if state.wm.focused_monitor != Some(mi) {
                let Some(di) = state.wm.monitors[mi].focused else {
                    return;
                };
                let dst = bsp_ipc::exec::Coordinates { monitor: mi, desktop: di, node: None };
                crate::ipc::with_ops(state, |ctx, events| bsp_ipc::exec::focus_node(ctx, dst, events));
            }
        }
    }
}

/// What has focus: the focused monitor and its shown desktop and node.
pub(crate) type FocusKey = Option<(usize, usize, Option<NodeId>)>;

/// The current [`FocusKey`].
pub(crate) fn focus_key<Bd: Backend + 'static>(state: &State<Bd>) -> FocusKey {
    let mi = state.wm.focused_monitor?;
    let di = state.wm.monitors[mi].focused?;
    Some((mi, di, state.wm.monitors[mi].desktops[di].tree.focus))
}

/// `pointer_follows_focus` / `pointer_follows_monitor`: after a command moved
/// the focus from `before`, put the pointer at the centre of the newly focused
/// window, or, when only the monitor changed, of that monitor.
///
/// bspwm: `src/tree.c` `focus_node()`'s `center_pointer()` calls.
pub(crate) fn warp_pointer_for_focus<Bd: Backend + 'static>(state: &mut State<Bd>, before: FocusKey) {
    debug_assert!(!crate::pointer_action::in_grab_callback(), "moving the pointer inside a grab callback deadlocks");
    let settings = &state.wm.settings;
    if !settings.pointer_follows_focus && !settings.pointer_follows_monitor {
        return;
    }
    let after = focus_key(state);
    let (Some(before_key), Some((mi, di, node))) = (before, after) else {
        return;
    };
    let mut target = None;
    if before_key.0 != mi && settings.pointer_follows_monitor {
        target = Some(state.wm.monitors[mi].rectangle);
    }
    if settings.pointer_follows_focus && (before_key.0, before_key.1, before_key.2) != (mi, di, node) {
        if let Some(client) = node.and_then(|n| state.wm.monitors[mi].desktops[di].tree.node(n).client.as_ref()) {
            target = Some(client.shown_rectangle());
        }
    }
    if let Some(r) = target {
        pointer_motion_to(state, ((r.x + r.width / 2) as f64, (r.y + r.height / 2) as f64).into(), 0);
    }
}

/// Re-targets the pointer when the surface it is focused on is no longer under
/// it (its desktop was switched away, its window closed): without a motion
/// event the client keeps pointer focus, its lock and its (possibly hidden)
/// cursor image, so the pointer looks gone and cannot click what is shown now.
pub(crate) fn refresh_pointer_focus<Bd: Backend + 'static>(state: &mut State<Bd>) {
    debug_assert!(!crate::pointer_action::in_grab_callback(), "reading the pointer's focus inside a grab callback deadlocks");
    let pointer = state.pointer.clone();
    let focus = pointer.current_focus();
    let location = pointer.current_location();
    let under = crate::layers::surface_under(state, location).map(|(surface, _)| surface);
    // Also when nothing had the pointer and a surface appeared under it (an X11
    // window mapped again when its desktop is shown gets its surface later).
    if under == focus || (focus.is_none() && pointer.is_grabbed()) {
        return;
    }
    tracing::debug!("the pointer's surface is no longer under it; retargeting");
    state.cursor_status = smithay::input::pointer::CursorImageStatus::default_named();
    pointer_motion_to(state, location, 0);
}

/// The point of some output nearest to `p` (`p` itself if it is on one), so the
/// pointer never rests where no output is: with outputs of different sizes or
/// offsets, the bounding box of all of them contains such dead zones.
///
/// anvil: `input_handler.rs` `clamp_coords()`.
pub(crate) fn clamp_to_outputs<Bd: Backend + 'static>(state: &State<Bd>, p: smithay::utils::Point<f64, smithay::utils::Logical>) -> smithay::utils::Point<f64, smithay::utils::Logical> {
    let rects: Vec<_> = state.space.outputs().filter_map(|o| state.space.output_geometry(o)).map(|g| g.to_f64()).collect();
    nearest_in_rects(&rects, p)
}

/// [`clamp_to_outputs`] over plain rectangles. The far edges are exclusive, so
/// the point stops just inside them.
fn nearest_in_rects(
    rects: &[smithay::utils::Rectangle<f64, smithay::utils::Logical>],
    p: smithay::utils::Point<f64, smithay::utils::Logical>,
) -> smithay::utils::Point<f64, smithay::utils::Logical> {
    let mut best: Option<(f64, smithay::utils::Point<f64, smithay::utils::Logical>)> = None;
    for r in rects {
        let q: smithay::utils::Point<f64, smithay::utils::Logical> = (
            p.x.clamp(r.loc.x, r.loc.x + r.size.w - 1.0),
            p.y.clamp(r.loc.y, r.loc.y + r.size.h - 1.0),
        )
            .into();
        let d = (q.x - p.x).powi(2) + (q.y - p.y).powi(2);
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, q));
        }
    }
    best.map_or(p, |(_, q)| q)
}

#[cfg(test)]
mod pointer_tests {
    use super::nearest_in_rects;
    use smithay::utils::Rectangle;

    #[test]
    fn the_pointer_stays_on_an_output_even_where_the_bounding_box_has_a_gap() {
        // A 1920x1080 output with a 1280x720 one to its right, tops aligned:
        // the box around both has a dead corner below the small one.
        let rects = [Rectangle::new((0.0, 0.0).into(), (1920.0, 1080.0).into()), Rectangle::new((1920.0, 0.0).into(), (1280.0, 720.0).into())];
        let on = nearest_in_rects(&rects, (100.0, 100.0).into());
        assert_eq!((on.x, on.y), (100.0, 100.0));
        // In the dead corner: pulled onto the nearest output.
        let corner = nearest_in_rects(&rects, (3000.0, 900.0).into());
        assert_eq!((corner.x, corner.y), (3000.0, 719.0));
        // Past every edge.
        let far = nearest_in_rects(&rects, (-50.0, 5000.0).into());
        assert_eq!((far.x, far.y), (0.0, 1079.0));
        // No outputs: left alone.
        assert_eq!(nearest_in_rects(&[], (5.0, 5.0).into()).x, 5.0);
    }
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
    // bspwm's `focus_node()`: focus, urgent flag, occluding fullscreen windows,
    // and the `node_focus`/`desktop_focus`/`monitor_focus` events and report.
    let was_shown = state.wm.focused_monitor == Some(mi) && state.wm.monitors[mi].focused == Some(di);
    let dst = bsp_ipc::exec::Coordinates { monitor: mi, desktop: di, node: Some(node) };
    if !crate::ipc::with_ops(state, |ctx, events| bsp_ipc::exec::focus_node(ctx, dst, events)) {
        return;
    }
    if !was_shown {
        // Another desktop came on screen: show its windows and hide the old ones.
        crate::shell::sync_wayland_from_core(state);
    }
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
