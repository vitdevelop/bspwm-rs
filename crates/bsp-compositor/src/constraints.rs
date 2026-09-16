//! `relative-pointer` and `pointer-constraints`: what games, 3D tools and
//! remote-desktop windows need to capture the mouse (FPS look, dragging
//! beyond the window edge).
//!
//! - A **locked** pointer does not move; clients still get relative motion.
//! - A **confined** pointer may not leave the constrained surface.
//! - A constraint activates when its surface has pointer focus and
//!   deactivates when focus moves away (activation ignores the constraint's
//!   region: a region is honoured only by clients that keep the pointer
//!   inside it themselves).
//!
//! bspwm has nothing like it (an X11 client grabs the pointer itself,
//! `XGrabPointer`), so this is the Wayland-mandated protocol form.

use smithay::input::pointer::{MotionEvent, PointerHandle, RelativeMotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraint, PointerConstraintsHandler};

use crate::state::{Backend, State};

impl<Bd: Backend + 'static> PointerConstraintsHandler for State<Bd> {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        // Active at once if the surface already has pointer focus.
        if pointer.current_focus().as_ref() == Some(surface) {
            with_pointer_constraint(surface, pointer, |constraint| {
                if let Some(constraint) = constraint {
                    if !constraint.is_active() {
                        constraint.activate();
                    }
                }
            });
            self.protocols.constrained = Some(surface.clone());
        }
    }

    /// A client that locked the pointer says where it wants the cursor to
    /// reappear (surface-local) when the lock ends, e.g. where the crosshair
    /// was: warp there.
    fn cursor_position_hint(&mut self, surface: &WlSurface, _pointer: &PointerHandle<Self>, hint: Point<f64, Logical>) {
        let Some(window) = self.window_for_surface(surface) else {
            return;
        };
        let Some(origin) = self.space.element_location(&window) else {
            return;
        };
        crate::input::pointer_motion_to(self, origin.to_f64() + hint, 0);
    }
}
smithay::delegate_pointer_constraints!(@<Bd: Backend + 'static> State<Bd>);
smithay::delegate_relative_pointer!(@<Bd: Backend + 'static> State<Bd>);

/// Activates the focused surface's constraint and releases the previous
/// one's — call after anything that can change pointer focus.
pub fn update<Bd: Backend + 'static>(state: &mut State<Bd>) {
    let pointer = state.pointer.clone();
    let focus = pointer.current_focus();
    if state.protocols.constrained != focus {
        if let Some(old) = state.protocols.constrained.take() {
            with_pointer_constraint(&old, &pointer, |constraint| {
                if let Some(constraint) = constraint {
                    if constraint.is_active() {
                        constraint.deactivate();
                    }
                }
            });
        }
    }
    if let Some(surface) = &focus {
        let has = with_pointer_constraint(surface, &pointer, |constraint| {
            constraint.is_some_and(|c| {
                if !c.is_active() {
                    c.activate();
                }
                true
            })
        });
        if has {
            state.protocols.constrained = Some(surface.clone());
        }
    }
}

/// Moves the pointer by a relative `delta` (a real mouse, or a virtual
/// pointer's `motion`), honouring an active lock or confinement, and sends
/// the client `relative_motion`. Also used to keep the pointer inside the
/// union of all outputs.
pub fn relative_motion<Bd: Backend + 'static>(
    state: &mut State<Bd>,
    delta: Point<f64, Logical>,
    delta_unaccel: Point<f64, Logical>,
    utime: u64,
    time: u32,
) {
    let pointer = state.pointer.clone();
    let focus = pointer.current_focus();
    let (locked, confined) = focus.as_ref().map_or((false, false), |surface| {
        with_pointer_constraint(surface, &pointer, |constraint| {
            constraint.map_or((false, false), |c| {
                if !c.is_active() {
                    (false, false)
                } else {
                    match &*c {
                        PointerConstraint::Locked(_) => (true, false),
                        PointerConstraint::Confined(_) => (false, true),
                    }
                }
            })
        })
    });

    let bounds = crate::input::outputs_bounds(state);
    let current = pointer.current_location();
    let mut location = current + delta;
    location.x = location.x.clamp(bounds.loc.x, bounds.loc.x + bounds.size.w);
    location.y = location.y.clamp(bounds.loc.y, bounds.loc.y + bounds.size.h);
    if locked {
        location = current;
    }
    let mut under = crate::layers::surface_under(state, location);
    if confined && under.as_ref().map(|(s, _)| s) != focus.as_ref() {
        // Would leave the confining surface: stay put.
        location = current;
        under = crate::layers::surface_under(state, location);
    }
    pointer.motion(
        state,
        under,
        &MotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time,
        },
    );
    pointer.relative_motion(
        state,
        None,
        &RelativeMotionEvent {
            delta,
            delta_unaccel,
            utime,
        },
    );
    pointer.frame(state);
    update(state);
    crate::toplevel_drag::follow(state);
    state.backend_data.queue_redraw();
}
