//! Forwarding winit input events to the Wayland seat, and click-to-focus.
//!
//! Every keyboard event is checked against the hardcoded emergency quit
//! key first, then matched against `bsp-hotkeys`' chord matcher
//! (`crate::hotkeys::filter`) before it would otherwise reach the
//! focused client. Ctrl+Alt+F1–F12 (TTY switch, `docs/design.md`'s
//! Reliability section) is not implemented: switching a text console is
//! a real DRM/libseat session concept the nested winit backend has
//! nothing to switch *to* — it is deferred to the hardware backend, where a real
//! session exists to drive it.

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

use crate::state::State;

/// Handles one winit-sourced input event.
pub fn process_input_event<B: InputBackend>(
    state: &mut State,
    event: InputEvent<B>,
    output: &Output,
) {
    match event {
        InputEvent::Keyboard { event } => {
            let keycode = event.key_code();
            let key_state = event.state();
            let pressed = key_state == KeyState::Pressed;
            let serial = SERIAL_COUNTER.next_serial();
            let time = Event::time_msec(&event);
            let keyboard = state.seat.get_keyboard().unwrap();
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
fn is_emergency_quit(mods: &ModifiersState, keysym: &KeysymHandle<'_>, pressed: bool) -> bool {
    pressed
        && mods.ctrl
        && mods.alt
        && mods.shift
        && keysym.raw_syms().first().copied()
            == Some(xkbcommon::xkb::Keysym::new(
                xkbcommon::xkb::keysyms::KEY_Escape,
            ))
}

fn on_pointer_motion_absolute<B: InputBackend>(
    state: &mut State,
    event: impl PointerMotionAbsoluteEvent<B>,
    output: &Output,
) {
    let output_geo = state.space.output_geometry(output).unwrap();
    let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
    let serial = SERIAL_COUNTER.next_serial();

    let under = state.space.element_under(pos).and_then(|(window, loc)| {
        window
            .surface_under(pos - loc.to_f64(), smithay::desktop::WindowSurfaceType::ALL)
            .map(|(surface, surf_loc)| (surface, (surf_loc + loc).to_f64()))
    });
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
}

fn on_pointer_button<B: InputBackend>(state: &mut State, event: impl PointerButtonEvent<B>) {
    let serial = SERIAL_COUNTER.next_serial();
    let button = event.button_code();
    let button_state = wl_pointer::ButtonState::from(event.state());

    if button_state == wl_pointer::ButtonState::Pressed {
        let location = state.pointer.current_location();
        focus_under_pointer(state, location, serial);
    }

    let pointer = state.pointer.clone();
    pointer.button(
        state,
        &ButtonEvent {
            button,
            state: button_state.try_into().unwrap(),
            serial,
            time: event.time_msec(),
        },
    );
    pointer.frame(state);
}

fn on_pointer_axis<B: InputBackend>(state: &mut State, event: impl PointerAxisEvent<B>) {
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

/// Click-to-focus: finds the window under `location` and, if it names a
/// `bsp-core` client, focuses it (`bsp-core::tree::Tree::focus`, the
/// monitor's focused desktop, and the seat's keyboard focus, all three —
/// bspwm: `src/tree.c` `focus_node()`).
fn focus_under_pointer(
    state: &mut State,
    location: smithay::utils::Point<f64, smithay::utils::Logical>,
    serial: smithay::utils::Serial,
) {
    let Some((window, _)) = state
        .space
        .element_under(location)
        .map(|(w, p)| (w.clone(), p))
    else {
        return;
    };
    let Some(window_id) = state.adapter.id_of(&window) else {
        return;
    };
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
                    state.wm.monitors[mi].desktops[di].tree.focus = Some(id);
                    state.wm.focused_monitor = Some(mi);
                    state.wm.monitors[mi].focused = Some(di);
                    let keyboard = state.seat.get_keyboard().unwrap();
                    keyboard.set_focus(state, window.wl_surface().map(|s| s.into_owned()), serial);
                    return;
                }
                n = tree.next_leaf(Some(id), tree.root);
            }
        }
    }
}

/// Sets the seat's keyboard focus to `node`'s client surface (`bsp-core`'s
/// side is the caller's job — this only drives the Wayland-visible half).
pub fn focus_node(state: &mut State, mi: usize, di: usize, node: NodeId) {
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
    let keyboard = state.seat.get_keyboard().unwrap();
    keyboard.set_focus(state, window.wl_surface().map(|s| s.into_owned()), serial);
}
