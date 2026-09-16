//! Touchpad gestures, touchscreens and graphics tablets: the events the
//! DRM backend's libinput delivers besides keys, buttons and motion,
//! forwarded to clients through `zwp_pointer_gestures_v1`, `wl_touch` and
//! `zwp_tablet_v2`. The nested backend has no such devices.
//!
//! bspwm has no counterpart (X11 clients read these devices through XInput
//! themselves); a touch or a tablet-pen tap focuses the window under it,
//! like a click does (`pointer_action::click_to_focus`).

use smithay::backend::input::{
    AbsolutePositionEvent, Event, GestureBeginEvent, GestureEndEvent, GesturePinchUpdateEvent, GestureSwipeUpdateEvent,
    InputBackend, InputEvent, ProximityState, TabletToolButtonEvent, TabletToolEvent,
    TabletToolProximityEvent, TabletToolTipEvent, TabletToolTipState, TouchEvent,
};
use smithay::input::pointer::{
    GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent as PinchUpdate,
    GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent as SwipeUpdate, MotionEvent,
};
use smithay::input::touch::{DownEvent, MotionEvent as TouchMotion, UpEvent};
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::tablet_manager::{TabletDescriptor, TabletSeatTrait};

use crate::state::{Backend, State};

/// Forwards `event` if it is a gesture, touch or tablet event; returns
/// whether it was one.
pub fn process<B: InputBackend, Bd: Backend + 'static>(state: &mut State<Bd>, event: InputEvent<B>) -> bool {
    match event {
        InputEvent::GestureSwipeBegin { event } => {
            let event = GestureSwipeBeginEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), fingers: event.fingers() };
            let pointer = state.pointer.clone();
            pointer.gesture_swipe_begin(state, &event);
        }
        InputEvent::GestureSwipeUpdate { event } => {
            let event = SwipeUpdate { time: event.time_msec(), delta: event.delta() };
            let pointer = state.pointer.clone();
            pointer.gesture_swipe_update(state, &event);
        }
        InputEvent::GestureSwipeEnd { event } => {
            let event = GestureSwipeEndEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), cancelled: event.cancelled() };
            let pointer = state.pointer.clone();
            pointer.gesture_swipe_end(state, &event);
        }
        InputEvent::GesturePinchBegin { event } => {
            let event = GesturePinchBeginEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), fingers: event.fingers() };
            let pointer = state.pointer.clone();
            pointer.gesture_pinch_begin(state, &event);
        }
        InputEvent::GesturePinchUpdate { event } => {
            let event = PinchUpdate { time: event.time_msec(), delta: event.delta(), scale: event.scale(), rotation: event.rotation() };
            let pointer = state.pointer.clone();
            pointer.gesture_pinch_update(state, &event);
        }
        InputEvent::GesturePinchEnd { event } => {
            let event = GesturePinchEndEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), cancelled: event.cancelled() };
            let pointer = state.pointer.clone();
            pointer.gesture_pinch_end(state, &event);
        }
        InputEvent::GestureHoldBegin { event } => {
            let event = GestureHoldBeginEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), fingers: event.fingers() };
            let pointer = state.pointer.clone();
            pointer.gesture_hold_begin(state, &event);
        }
        InputEvent::GestureHoldEnd { event } => {
            let event = GestureHoldEndEvent { serial: SERIAL_COUNTER.next_serial(), time: event.time_msec(), cancelled: event.cancelled() };
            let pointer = state.pointer.clone();
            pointer.gesture_hold_end(state, &event);
        }
        InputEvent::TouchDown { event } => touch_down::<B, Bd>(state, event),
        InputEvent::TouchMotion { event } => touch_motion::<B, Bd>(state, event),
        InputEvent::TouchUp { event } => {
            if let Some(touch) = state.seat.get_touch() {
                touch.up(state, &UpEvent { slot: event.slot(), serial: SERIAL_COUNTER.next_serial(), time: event.time_msec() });
            }
        }
        InputEvent::TouchFrame { .. } => {
            if let Some(touch) = state.seat.get_touch() {
                touch.frame(state);
            }
        }
        InputEvent::TouchCancel { .. } => {
            if let Some(touch) = state.seat.get_touch() {
                touch.cancel(state);
            }
        }
        InputEvent::TabletToolAxis { event } => tablet_axis::<B, Bd>(state, event),
        InputEvent::TabletToolProximity { event } => tablet_proximity::<B, Bd>(state, event),
        InputEvent::TabletToolTip { event } => tablet_tip::<B, Bd>(state, event),
        InputEvent::TabletToolButton { event } => {
            if let Some(tool) = state.seat.tablet_seat().get_tool(&event.tool()) {
                tool.button(event.button(), event.button_state(), SERIAL_COUNTER.next_serial(), event.time_msec());
            }
        }
        _ => return false,
    }
    true
}

/// Registers a tablet device with the seat (`InputEvent::DeviceAdded`).
pub fn tablet_added<Bd: Backend + 'static>(state: &mut State<Bd>, device: &smithay::reexports::input::Device) {
    let descriptor = TabletDescriptor::from(device);
    state.seat.tablet_seat().add_tablet::<State<Bd>>(&state.display_handle, &descriptor);
}

/// Unregisters a tablet device (`InputEvent::DeviceRemoved`).
pub fn tablet_removed<Bd: Backend + 'static>(state: &mut State<Bd>, device: &smithay::reexports::input::Device) {
    state.seat.tablet_seat().remove_tablet(&TabletDescriptor::from(device));
}

/// Where an absolute-position device (touchscreen, tablet) points on
/// screen: it spans the internal panel (`eDP-*`) if there is one, else the
/// first output.
fn absolute_location<B: InputBackend, Bd: Backend + 'static, E: AbsolutePositionEvent<B>>(
    state: &State<Bd>,
    event: &E,
) -> Option<Point<f64, Logical>> {
    let output = state
        .space
        .outputs()
        .find(|output| output.name().starts_with("eDP"))
        .or_else(|| state.space.outputs().next())?;
    let geometry = state.space.output_geometry(output)?;
    let transform = output.current_transform();
    let size = transform.invert().transform_size(geometry.size);
    Some(transform.transform_point_in(event.position_transformed(size), &size.to_f64()) + geometry.loc.to_f64())
}

/// A tap or pen-down focuses the window under `location`.
fn focus_at<Bd: Backend + 'static>(state: &mut State<Bd>, location: Point<f64, Logical>) {
    let Some((mi, di, node)) = crate::input::window_under(state, location).and_then(|id| crate::input::locate_window(state, id)) else {
        return;
    };
    let already = state.wm.monitors[mi].desktops[di].tree.focus == Some(node) && state.wm.focused_monitor == Some(mi);
    if !already {
        crate::input::set_focus(state, mi, di, node, SERIAL_COUNTER.next_serial());
    }
}

fn touch_down<B: InputBackend, Bd: Backend + 'static>(state: &mut State<Bd>, event: B::TouchDownEvent) {
    let Some(touch) = state.seat.get_touch() else {
        return;
    };
    let Some(location) = absolute_location::<B, Bd, _>(state, &event) else {
        return;
    };
    focus_at(state, location);
    let under = crate::layers::surface_under(state, location);
    touch.down(
        state,
        under,
        &DownEvent { slot: event.slot(), location, serial: SERIAL_COUNTER.next_serial(), time: event.time_msec() },
    );
}

fn touch_motion<B: InputBackend, Bd: Backend + 'static>(state: &mut State<Bd>, event: B::TouchMotionEvent) {
    let Some(touch) = state.seat.get_touch() else {
        return;
    };
    let Some(location) = absolute_location::<B, Bd, _>(state, &event) else {
        return;
    };
    let under = crate::layers::surface_under(state, location);
    touch.motion(state, under, &TouchMotion { slot: event.slot(), location, time: event.time_msec() });
}

fn tablet_axis<B: InputBackend, Bd: Backend + 'static>(state: &mut State<Bd>, event: B::TabletToolAxisEvent) {
    let Some(location) = absolute_location::<B, Bd, _>(state, &event) else {
        return;
    };
    let tablet_seat = state.seat.tablet_seat();
    let under = crate::layers::surface_under(state, location);
    let pointer = state.pointer.clone();
    pointer.motion(state, under.clone(), &MotionEvent { location, serial: SERIAL_COUNTER.next_serial(), time: event.time_msec() });
    if let (Some(tablet), Some(tool)) = (tablet_seat.get_tablet(&TabletDescriptor::from(&event.device())), tablet_seat.get_tool(&event.tool())) {
        if event.pressure_has_changed() {
            tool.pressure(event.pressure());
        }
        if event.distance_has_changed() {
            tool.distance(event.distance());
        }
        if event.tilt_has_changed() {
            tool.tilt(event.tilt());
        }
        if event.slider_has_changed() {
            tool.slider_position(event.slider_position());
        }
        if event.rotation_has_changed() {
            tool.rotation(event.rotation());
        }
        if event.wheel_has_changed() {
            tool.wheel(event.wheel_delta(), event.wheel_delta_discrete());
        }
        tool.motion(location, under, &tablet, SERIAL_COUNTER.next_serial(), event.time_msec());
    }
    pointer.frame(state);
    state.backend_data.queue_redraw();
}

fn tablet_proximity<B: InputBackend, Bd: Backend + 'static>(state: &mut State<Bd>, event: B::TabletToolProximityEvent) {
    let Some(location) = absolute_location::<B, Bd, _>(state, &event) else {
        return;
    };
    let tablet_seat = state.seat.tablet_seat();
    let dh = state.display_handle.clone();
    let tool = event.tool();
    tablet_seat.add_tool::<State<Bd>>(state, &dh, &tool);
    let under = crate::layers::surface_under(state, location);
    let pointer = state.pointer.clone();
    pointer.motion(state, under.clone(), &MotionEvent { location, serial: SERIAL_COUNTER.next_serial(), time: event.time_msec() });
    pointer.frame(state);
    if let (Some(under), Some(tablet), Some(tool)) =
        (under, tablet_seat.get_tablet(&TabletDescriptor::from(&event.device())), tablet_seat.get_tool(&tool))
    {
        match event.state() {
            ProximityState::In => tool.proximity_in(location, under, &tablet, SERIAL_COUNTER.next_serial(), event.time_msec()),
            ProximityState::Out => tool.proximity_out(event.time_msec()),
        }
    }
    state.backend_data.queue_redraw();
}

fn tablet_tip<B: InputBackend, Bd: Backend + 'static>(state: &mut State<Bd>, event: B::TabletToolTipEvent) {
    let Some(tool) = state.seat.tablet_seat().get_tool(&event.tool()) else {
        return;
    };
    match event.tip_state() {
        TabletToolTipState::Down => {
            tool.tip_down(SERIAL_COUNTER.next_serial(), event.time_msec());
            let location = state.pointer.current_location();
            focus_at(state, location);
        }
        TabletToolTipState::Up => tool.tip_up(event.time_msec()),
    }
}
