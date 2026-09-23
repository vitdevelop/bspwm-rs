//! `relative-pointer` and `pointer-constraints`: what games, 3D tools and
//! remote-desktop windows need to capture the mouse (FPS look, dragging
//! beyond the window edge).
//!
//! - A **locked** pointer does not move; clients still get relative motion.
//! - A **confined** pointer may not leave the constrained surface.
//! - A constraint activates when its surface has pointer focus and
//!   deactivates when focus moves away. A constraint with a region activates
//!   only while the pointer is inside it, and a confined pointer stays inside
//!   the region.
//!
//! bspwm has nothing like it (an X11 client grabs the pointer itself,
//! `XGrabPointer`), so this is the Wayland-mandated protocol form.

use smithay::input::pointer::{MotionEvent, PointerHandle, RelativeMotionEvent};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};
use smithay::wayland::pointer_constraints::{with_pointer_constraint, PointerConstraint, PointerConstraintsHandler};

use crate::state::{Backend, State};

/// Whether `location` (global) is inside the constraint's region, when the
/// constraint has one. `origin` is the constrained surface's global position;
/// a region is surface-local. Without a region, everywhere is inside.
fn region_contains(constraint: &PointerConstraint, origin: Option<Point<f64, Logical>>, location: Point<f64, Logical>) -> bool {
    let Some(region) = constraint.region() else { return true };
    // A region whose surface cannot be placed: the pointer is not in it.
    let Some(origin) = origin else { return false };
    let local = location - origin;
    region.contains((local.x.floor() as i32, local.y.floor() as i32))
}

/// The global position of `surface`'s origin: from its window's place in the
/// space (as anvil does), else from the surface under `location`.
fn origin_of<Bd: Backend + 'static>(state: &State<Bd>, surface: &WlSurface, location: Point<f64, Logical>) -> Option<Point<f64, Logical>> {
    if let Some(window) = state.window_for_surface(surface) {
        if let Some(loc) = state.space.element_location(&window) {
            return Some((loc - window.geometry().loc).to_f64());
        }
    }
    crate::layers::surface_under(state, location).filter(|(s, _)| s == surface).map(|(_, origin)| origin)
}

impl<Bd: Backend + 'static> PointerConstraintsHandler for State<Bd> {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        tracing::debug!(focused = pointer.current_focus().as_ref() == Some(surface), "pointer constraint created");
        // Active at once if the surface already has pointer focus.
        if pointer.current_focus().as_ref() == Some(surface) {
            let location = pointer.current_location();
            let origin = origin_of(self, surface, location);
            let activated = with_pointer_constraint(surface, pointer, |constraint| {
                let Some(constraint) = constraint else { return false };
                if !region_contains(&constraint, origin, location) {
                    return false;
                }
                if !constraint.is_active() {
                    constraint.activate();
                }
                true
            });
            if activated {
                self.protocols.constrained = Some(surface.clone());
            }
        }
    }

    /// A client that locked the pointer says where it wants the cursor to
    /// reappear (surface-local) when the lock ends, e.g. where the crosshair
    /// was. Only the compositor-side location moves: telling the client
    /// about it (a `motion` event) would feed its own hint back to it as
    /// pointer motion, and Xwayland sends one hint per mouse event while a
    /// game looks around.
    fn cursor_position_hint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>, hint: Point<f64, Logical>) {
        let active = with_pointer_constraint(surface, pointer, |constraint| constraint.is_some_and(|c| c.is_active()));
        if !active {
            return;
        }
        let Some(window) = self.window_for_surface(surface) else {
            return;
        };
        let Some(origin) = self.space.element_location(&window) else {
            return;
        };
        pointer.set_location(origin.to_f64() + hint);
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
                        tracing::debug!("pointer constraint deactivated (focus moved)");
                        constraint.deactivate();
                    }
                }
            });
        }
    }
    if let Some(surface) = &focus {
        let location = pointer.current_location();
        let origin = origin_of(state, surface, location);
        let has = with_pointer_constraint(surface, &pointer, |constraint| {
            constraint.is_some_and(|c| {
                // Outside its region a constraint waits (an active one stays).
                if !c.is_active() && !region_contains(&c, origin, location) {
                    return false;
                }
                if !c.is_active() {
                    tracing::debug!("pointer constraint activated");
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
    let mut focus = pointer.current_focus();
    // The focused surface can be gone from under the pointer without a motion
    // event (its desktop was switched away, the window closed): its lock must
    // not freeze the pointer for whatever is shown now.
    if focus.is_some() && crate::layers::surface_under(state, pointer.current_location()).map(|(s, _)| s) != focus {
        focus = None;
    }
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

    let current = pointer.current_location();
    let mut location = crate::input::clamp_to_outputs(state, current + delta);
    if locked {
        location = current;
    }
    let mut under = crate::layers::surface_under(state, location);
    let outside_region = confined
        && focus.as_ref().is_some_and(|surface| {
            let origin = origin_of(state, surface, location);
            with_pointer_constraint(surface, &pointer, |constraint| {
                constraint.is_some_and(|c| !region_contains(&c, origin, location))
            })
        });
    if confined && (outside_region || under.as_ref().map(|(s, _)| s) != focus.as_ref()) {
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
    crate::input::focus_follows_pointer(state);
}
