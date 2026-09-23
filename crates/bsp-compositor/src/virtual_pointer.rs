//! `wlr-virtual-pointer-v1`: lets a program inject pointer motion, buttons
//! and scrolling (`wayvnc` remote control, `wlrctl`, test tooling).
//!
//! Events go to the same seat as a physical mouse — through
//! `crate::input::pointer_motion_to`/`deliver_button` — so click-to-focus
//! and `pointer_action` bindings see them exactly like real input. It is
//! offered to every client for now; restricting it (like virtual
//! keyboard) waits for security-context.

use std::sync::Mutex;

use smithay::backend::input::{Axis, AxisSource};
use smithay::input::pointer::AxisFrame;
use smithay::reexports::wayland_server::protocol::wl_pointer;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum};
use wayland_protocols_wlr::virtual_pointer::v1::server::{
    zwlr_virtual_pointer_manager_v1::{self, ZwlrVirtualPointerManagerV1},
    zwlr_virtual_pointer_v1::{self, ZwlrVirtualPointerV1},
};

use crate::state::{Backend, State};

/// Scroll events accumulated until the client's `frame` request.
#[derive(Default)]
pub struct PointerData {
    inner: Mutex<Pending>,
    /// The output absolute motion maps onto (`create_virtual_pointer_with_output`).
    output: Option<smithay::output::Output>,
}

#[derive(Default)]
struct Pending {
    time: u32,
    source: Option<AxisSource>,
    /// `(axis, value, discrete steps, stopped)` per axis.
    axes: Vec<(Axis, f64, Option<i32>, bool)>,
}

fn axis_of(axis: WEnum<wl_pointer::Axis>) -> Option<Axis> {
    match axis {
        WEnum::Value(wl_pointer::Axis::VerticalScroll) => Some(Axis::Vertical),
        WEnum::Value(wl_pointer::Axis::HorizontalScroll) => Some(Axis::Horizontal),
        _ => None,
    }
}

impl<Bd: Backend + 'static> GlobalDispatch<ZwlrVirtualPointerManagerV1, (), State<Bd>> for State<Bd> {
    fn can_view(client: Client, _: &()) -> bool {
        crate::state::is_privileged(&client)
    }

    fn bind(
        _state: &mut State<Bd>,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrVirtualPointerManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        data_init.init(resource, ());
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrVirtualPointerManagerV1, (), State<Bd>> for State<Bd> {
    fn request(
        _state: &mut State<Bd>,
        _client: &Client,
        _manager: &ZwlrVirtualPointerManagerV1,
        request: zwlr_virtual_pointer_manager_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        use zwlr_virtual_pointer_manager_v1::Request;
        match request {
            Request::CreateVirtualPointer { id, .. } => {
                data_init.init(id, PointerData::default());
            }
            Request::CreateVirtualPointerWithOutput { output, id, .. } => {
                let output = output.as_ref().and_then(smithay::output::Output::from_resource);
                data_init.init(id, PointerData { inner: Mutex::default(), output });
            }
            _ => {}
        }
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrVirtualPointerV1, PointerData, State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _client: &Client,
        pointer: &ZwlrVirtualPointerV1,
        request: zwlr_virtual_pointer_v1::Request,
        data: &PointerData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        use zwlr_virtual_pointer_v1::Request;
        tracing::trace!(?request, "virtual pointer request");
        match request {
            Request::Motion { time, dx, dy } => {
                let delta = smithay::utils::Point::from((dx, dy));
                crate::constraints::relative_motion(state, delta, delta, u64::from(time) * 1000, time);
            }
            Request::MotionAbsolute { time, x, y, x_extent, y_extent } => {
                if x_extent == 0 || y_extent == 0 {
                    return;
                }
                // Onto the chosen output, else the whole layout.
                let area = data
                    .output
                    .as_ref()
                    .and_then(|o| state.space.output_geometry(o))
                    .map(|g| g.to_f64())
                    .unwrap_or_else(|| crate::input::outputs_bounds(state));
                let location = (
                    area.loc.x + area.size.w * (x.min(x_extent) as f64 / x_extent as f64),
                    area.loc.y + area.size.h * (y.min(y_extent) as f64 / y_extent as f64),
                )
                    .into();
                tracing::debug!(?location, area = ?area, "virtual pointer: absolute motion");
                crate::input::pointer_motion_to(state, location, time);
                crate::input::focus_follows_pointer(state);
            }
            Request::Button { time, button, state: WEnum::Value(button_state) } => {
                crate::input::deliver_button(state, button, button_state, time);
            }
            Request::Axis { time, axis, value } => {
                let Some(axis) = axis_of(axis) else {
                    pointer.post_error(zwlr_virtual_pointer_v1::Error::InvalidAxis, "invalid axis");
                    return;
                };
                if let Ok(mut pending) = data.inner.lock() {
                    pending.time = time;
                    pending.axes.push((axis, value, None, false));
                }
            }
            Request::AxisDiscrete { time, axis, value, discrete } => {
                let Some(axis) = axis_of(axis) else {
                    pointer.post_error(zwlr_virtual_pointer_v1::Error::InvalidAxis, "invalid axis");
                    return;
                };
                if let Ok(mut pending) = data.inner.lock() {
                    pending.time = time;
                    pending.axes.push((axis, value, Some(discrete), false));
                }
            }
            Request::AxisStop { time, axis } => {
                let Some(axis) = axis_of(axis) else {
                    pointer.post_error(zwlr_virtual_pointer_v1::Error::InvalidAxis, "invalid axis");
                    return;
                };
                if let Ok(mut pending) = data.inner.lock() {
                    pending.time = time;
                    pending.axes.push((axis, 0.0, None, true));
                }
            }
            Request::AxisSource { axis_source } => {
                let source = match axis_source {
                    WEnum::Value(wl_pointer::AxisSource::Wheel) => Some(AxisSource::Wheel),
                    WEnum::Value(wl_pointer::AxisSource::Finger) => Some(AxisSource::Finger),
                    WEnum::Value(wl_pointer::AxisSource::Continuous) => Some(AxisSource::Continuous),
                    WEnum::Value(wl_pointer::AxisSource::WheelTilt) => Some(AxisSource::WheelTilt),
                    _ => None,
                };
                match (source, data.inner.lock()) {
                    (Some(source), Ok(mut pending)) => pending.source = Some(source),
                    (None, _) => pointer.post_error(zwlr_virtual_pointer_v1::Error::InvalidAxisSource, "invalid axis source"),
                    _ => {}
                }
            }
            Request::Frame => {
                let pending = match data.inner.lock() {
                    Ok(mut pending) => std::mem::take(&mut *pending),
                    Err(_) => return,
                };
                if pending.axes.is_empty() {
                    return;
                }
                let mut frame = AxisFrame::new(pending.time);
                if let Some(source) = pending.source {
                    frame = frame.source(source);
                }
                for (axis, value, discrete, stopped) in pending.axes {
                    if stopped {
                        frame = frame.stop(axis);
                    } else {
                        frame = frame.value(axis, value);
                        if let Some(steps) = discrete {
                            frame = frame.v120(axis, steps * 120);
                        }
                    }
                }
                let seat_pointer = state.pointer.clone();
                seat_pointer.axis(state, frame);
                seat_pointer.frame(state);
            }
            _ => {}
        }
    }
}
