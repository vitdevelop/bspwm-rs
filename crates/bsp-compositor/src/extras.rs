//! Smaller protocols Smithay ships handlers for, each wired to what
//! bspwm-rs can honestly do with it:
//!
//! - `wp_alpha_modifier_v1`: surface opacity; Smithay's surface render
//!   element applies it, nothing else to do.
//! - `wp_fifo_v1` and `wp_commit_timing_v1`: presentation pacing for games
//!   and video players. Smithay parks a commit behind a barrier; the
//!   compositor releases the barriers once per presented frame
//!   ([`finish_frame`]).
//! - `xdg_dialog_v1`: a modal dialog is floated (bspwm's own answer to
//!   dialogs is the `state=floating` rule).
//! - `xdg_foreign_v2`: parent/child links between toplevels of different clients.
//! - `xdg_toplevel_icon_v1`, `xdg_toplevel_tag_v1`: accepted and kept on
//!   the surface; bspwm draws no titles or icons, so nothing shows them.
//! - `xdg_system_bell_v1`: rung bells are logged.
//! - `zwp_xwayland_keyboard_grab_v1`: X11 programs (VNC/remote desktop
//!   viewers) that must see every key.
//! - `zwp_pointer_gestures_v1`, `zwp_tablet_manager_v2`, `wl_touch`:
//!   input protocols whose events the DRM backend forwards
//!   (`crate::udev_backend`).

use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::alpha_modifier::AlphaModifierState;
use smithay::wayland::commit_timing::{CommitTimerBarrierStateUserData, CommitTimingManagerState, Timestamp};
use smithay::reexports::wayland_server::Resource;
use smithay::wayland::compositor::{with_surface_tree_downward, CompositorHandler, TraversalAction};
use smithay::wayland::fifo::{FifoBarrierCachedState, FifoManagerState};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::pointer_gestures::PointerGesturesState;
use smithay::wayland::shell::xdg::dialog::{XdgDialogHandler, XdgDialogState};
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::wayland::tablet_manager::TabletManagerState;
use smithay::wayland::xdg_foreign::{XdgForeignHandler, XdgForeignState};
use smithay::wayland::xdg_system_bell::{XdgSystemBellHandler, XdgSystemBellState};
use smithay::wayland::xdg_toplevel_icon::{XdgToplevelIconHandler, XdgToplevelIconManager};
use smithay::wayland::xdg_toplevel_tag::{XdgToplevelTagHandler, XdgToplevelTagManager};
use smithay::wayland::xwayland_keyboard_grab::{XWaylandKeyboardGrabHandler, XWaylandKeyboardGrabState};

use crate::state::{Backend, State};

/// The globals of this module, kept alive for as long as the compositor runs.
pub struct Extras {
    _alpha: AlphaModifierState,
    _fifo: FifoManagerState,
    _commit_timing: CommitTimingManagerState,
    _dialog: XdgDialogState,
    foreign: XdgForeignState,
    _icon: XdgToplevelIconManager,
    _tag: XdgToplevelTagManager,
    _bell: XdgSystemBellState,
    _keyboard_grab: XWaylandKeyboardGrabState,
    _gestures: PointerGesturesState,
    _tablet: TabletManagerState,
    /// The deadline (µs, monotonic) the armed commit-timing timer waits for.
    commit_timer_at: Option<u64>,
}

impl Extras {
    /// Creates every global on `display`.
    pub fn new<Bd: Backend + 'static>(display: &DisplayHandle) -> Self {
        let mut icon = XdgToplevelIconManager::new::<State<Bd>>(display);
        // Sizes a client should offer icons in (a taskbar's typical sizes).
        for size in [16, 32, 48, 64] {
            icon.add_icon_size(size);
        }
        Self {
            _alpha: AlphaModifierState::new::<State<Bd>>(display),
            _fifo: FifoManagerState::new::<State<Bd>>(display),
            _commit_timing: CommitTimingManagerState::new::<State<Bd>>(display),
            _dialog: XdgDialogState::new::<State<Bd>>(display),
            foreign: XdgForeignState::new::<State<Bd>>(display),
            _icon: icon,
            _tag: XdgToplevelTagManager::new::<State<Bd>>(display),
            _bell: XdgSystemBellState::new::<State<Bd>>(display),
            _keyboard_grab: XWaylandKeyboardGrabState::new::<State<Bd>>(display),
            _gestures: PointerGesturesState::new::<State<Bd>>(display),
            _tablet: TabletManagerState::new::<State<Bd>>(display),
            commit_timer_at: None,
        }
    }
}

/// Every window's and layer surface's root surface, on every output.
fn shown_surfaces<Bd: Backend + 'static>(state: &State<Bd>) -> Vec<WlSurface> {
    let mut surfaces: Vec<WlSurface> = state.space.elements().filter_map(|w| w.wl_surface().map(|s| s.into_owned())).collect();
    for output in state.space.outputs() {
        for layer in smithay::desktop::layer_map_for_output(output).layers() {
            surfaces.push(layer.wl_surface().clone());
        }
    }
    surfaces
}

/// Releases the pacing barriers of every shown surface (and its
/// subsurfaces): the FIFO barrier if `fifo` (a frame was presented), and
/// every commit-timer barrier due by now. A commit that waited behind a
/// released barrier is applied by telling its client's compositor state the
/// blocker cleared (Smithay does not poll blockers).
fn release_barriers<Bd: Backend + 'static>(state: &mut State<Bd>, fifo: bool) {
    let now = state.clock.now();
    // The surfaces are collected first: applying a commit below may touch the
    // layer maps `shown_surfaces` locked.
    let surfaces = shown_surfaces(state);
    let mut clients: Vec<smithay::reexports::wayland_server::Client> = Vec::new();
    for root in &surfaces {
        with_surface_tree_downward(
            root,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |surface, states, _| {
                let mut signalled = false;
                if fifo {
                    let barrier = states.cached_state.get::<FifoBarrierCachedState>().current().barrier.take();
                    if let Some(barrier) = barrier {
                        barrier.signal();
                        signalled = true;
                    }
                }
                if let Some(mut timer) = states.data_map.get::<CommitTimerBarrierStateUserData>().and_then(|timer| timer.lock().ok()) {
                    signalled |= timer.signal_until(now);
                }
                if signalled {
                    if let Some(client) = surface.client() {
                        if !clients.iter().any(|c| c.id() == client.id()) {
                            clients.push(client);
                        }
                    }
                }
            },
            |_, _, _| true,
        );
    }
    let dh = state.display_handle.clone();
    for client in clients {
        state.client_compositor_state(&client).blocker_cleared(state, &dh);
    }
}

/// The output showing most of `window` (the first on a tie), `None` while it is
/// not in the space.
fn primary_output<Bd: Backend + 'static>(state: &State<Bd>, window: &smithay::desktop::Window) -> Option<smithay::output::Output> {
    let geometry = state.space.element_geometry(window)?;
    let mut best: Option<(i64, smithay::output::Output)> = None;
    for output in state.space.outputs_for_element(window) {
        let area = state
            .space
            .output_geometry(&output)
            .and_then(|g| g.intersection(geometry))
            .map_or(0, |i| i64::from(i.size.w) * i64::from(i.size.h));
        if best.as_ref().is_none_or(|(a, _)| area > *a) {
            best = Some((area, output));
        }
    }
    best.map(|(_, o)| o)
}

/// Ends a frame on `output`: sends frame callbacks to every window and
/// layer surface, then releases their pacing barriers — the next FIFO
/// commit of each may proceed, and commit-timed content due by now is
/// applied. Runs for empty frames too: a client that committed without
/// visible damage still waits on the callback to draw its next frame.
pub fn finish_frame<Bd: Backend + 'static>(state: &mut State<Bd>, output: &smithay::output::Output) {
    let now = state.start_time.elapsed();
    // A window on this output is paced by it: a callback every frame. Every other
    // window, on another output or on a desktop that is not shown (out of the
    // space), still gets one about once a second, so a client blocked waiting
    // for a callback (an Xwayland one stalls the others) is not stuck for good.
    // A window that lies on several outputs is paced by the one showing most
    // of it, not by each of them. An Xwayland window is never throttled (as in
    // cosmic-comp): Xwayland serializes presentation, so one slow callback
    // would delay every other X11 window.
    let windows: Vec<smithay::desktop::Window> = state
        .space
        .elements()
        .chain(state.adapter.windows())
        .fold(Vec::new(), |mut all, w| {
            if !all.contains(w) {
                all.push(w.clone());
            }
            all
        });
    for window in windows {
        let here = primary_output(state, &window).as_ref() == Some(output);
        let throttle = if window.x11_surface().is_some() {
            std::time::Duration::ZERO
        } else {
            std::time::Duration::from_millis(995)
        };
        window.send_frame(output, now, Some(throttle), |_, _| here.then(|| output.clone()));
    }
    crate::layers::send_frames(output, now);
    release_barriers(state, true);
}

/// How often the taskbar, workspace and output-management diffs run at most.
/// They rebuild their view of every window and desktop and compare it with what
/// clients were last told, which nothing needs a thousand times a second: an
/// event loop turn per pointer-motion report would run them at the mouse's
/// polling rate. A change waits at most this long (imperceptible for a bar).
const PERIODIC_SYNC_EVERY: std::time::Duration = std::time::Duration::from_millis(8);

/// How much longer to wait before the periodic syncs may run again, or `None`
/// if they are due now.
fn periodic_wait(last: Option<std::time::Instant>, now: std::time::Instant) -> Option<std::time::Duration> {
    let due = last? + PERIODIC_SYNC_EVERY;
    due.checked_duration_since(now).filter(|d| !d.is_zero())
}

/// Runs the taskbar, workspace and output-management diffs, at most once per
/// [`PERIODIC_SYNC_EVERY`]. A turn that comes too early arms one timer for the
/// moment they are due, so the last change before a quiet spell is still
/// reported on time (the timer's wake-up is a turn of its own).
pub fn periodic_syncs<Bd: Backend + 'static>(state: &mut State<Bd>) {
    use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
    crate::ipc::flush_ipc(state);
    crate::xwayland::reap_dead(state);
    let now = std::time::Instant::now();
    if let Some(wait) = periodic_wait(state.last_periodic_sync, now) {
        if !state.periodic_timer_armed {
            state.periodic_timer_armed = true;
            let armed = state.handle.insert_source(Timer::from_duration(wait), |_, _, state| {
                state.periodic_timer_armed = false;
                TimeoutAction::Drop
            });
            if armed.is_err() {
                state.periodic_timer_armed = false;
            }
        }
        return;
    }
    state.last_periodic_sync = Some(now);
    crate::taskbar::sync(state);
    crate::workspaces::sync(state);
    crate::output_management::sync(state);
}

#[cfg(test)]
mod periodic_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn the_periodic_syncs_run_at_most_once_per_interval() {
        let t0 = Instant::now();
        assert_eq!(periodic_wait(None, t0), None, "the first turn runs");
        assert_eq!(periodic_wait(Some(t0), t0 + Duration::from_millis(1)), Some(PERIODIC_SYNC_EVERY - Duration::from_millis(1)));
        assert_eq!(periodic_wait(Some(t0), t0 + PERIODIC_SYNC_EVERY), None);
        assert_eq!(periodic_wait(Some(t0), t0 + Duration::from_secs(1)), None);
    }
}

/// Wakes the compositor when a commit-timed commit becomes due. Such a
/// commit waits behind a barrier that is otherwise only checked when a frame
/// ends, and an idle compositor draws none: so, after each event-loop turn,
/// one timer is armed for the earliest pending deadline; when it fires the
/// due commits are released and a frame is queued. (Event-driven: with no
/// commit-timed client there is no timer.)
pub fn arm_commit_timers<Bd: Backend + 'static>(state: &mut State<Bd>) {
    use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
    use smithay::utils::{Monotonic, Time};
    let mut earliest: Option<Timestamp> = None;
    for root in shown_surfaces(state) {
        with_surface_tree_downward(
            &root,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |_, states, _| {
                let next = states
                    .data_map
                    .get::<CommitTimerBarrierStateUserData>()
                    .and_then(|timer| timer.lock().ok().and_then(|timer| timer.next_deadline()));
                if let Some(next) = next {
                    earliest = Some(earliest.map_or(next, |e| e.min(next)));
                }
            },
            |_, _, _| true,
        );
    }
    let Some(deadline) = earliest else {
        return;
    };
    let target: Time<Monotonic> = deadline.into();
    let at = target.as_micros();
    if state.protocols.extras.commit_timer_at.is_some_and(|armed| armed <= at) {
        return;
    }
    state.protocols.extras.commit_timer_at = Some(at);
    let delay = Time::elapsed(&state.clock.now(), target);
    let armed = state.handle.insert_source(Timer::from_duration(delay), |_, _, state| {
        state.protocols.extras.commit_timer_at = None;
        release_barriers(state, false);
        state.backend_data.queue_redraw();
        TimeoutAction::Drop
    });
    if armed.is_err() {
        state.protocols.extras.commit_timer_at = None;
    }
}

impl<Bd: Backend + 'static> State<Bd> {
    /// Runs `bspc node ID -t STATE` for `window` (a protocol asked for it).
    pub fn window_state_request(&mut self, window: &Window, node_state: &str) {
        let Some(id) = self.adapter.id_of(window) else {
            return;
        };
        let Some((mi, di, node)) = crate::input::locate_window(self, id) else {
            return;
        };
        let desktop = self.wm.monitors[mi].desktops[di].id;
        let Some(wire_id) = self.registry.id_of(desktop, node) else {
            return;
        };
        let argv = vec!["node".to_string(), format!("0x{wire_id:08X}"), "-t".to_string(), node_state.to_string()];
        crate::protocols::run_bspc(self, &argv);
    }
}

impl<Bd: Backend + 'static> XdgDialogHandler for State<Bd> {
    fn modal_changed(&mut self, toplevel: ToplevelSurface, is_modal: bool) {
        if !is_modal {
            return;
        }
        if let Some(window) = self.window_for_surface(toplevel.wl_surface()) {
            self.window_state_request(&window, "floating");
        }
    }
}
smithay::delegate_xdg_dialog!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> XdgForeignHandler for State<Bd> {
    fn xdg_foreign_state(&mut self) -> &mut XdgForeignState {
        &mut self.protocols.extras.foreign
    }
}
smithay::delegate_xdg_foreign!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> XdgToplevelIconHandler for State<Bd> {}
smithay::delegate_xdg_toplevel_icon!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> XdgToplevelTagHandler for State<Bd> {
    fn set_tag(&mut self, _toplevel: smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::XdgToplevel, tag: String) {
        tracing::debug!(tag, "toplevel tag");
    }
}
smithay::delegate_xdg_toplevel_tag!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> XdgSystemBellHandler for State<Bd> {
    fn ring(&mut self, surface: Option<WlSurface>) {
        tracing::info!(has_surface = surface.is_some(), "system bell");
    }
}
smithay::delegate_xdg_system_bell!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> XWaylandKeyboardGrabHandler for State<Bd> {
    fn keyboard_focus_for_xsurface(&self, surface: &WlSurface) -> Option<crate::focus::FocusTarget> {
        let window = self.window_for_surface(surface)?;
        crate::focus::focus_target_of(&window)
    }
}
smithay::delegate_xwayland_keyboard_grab!(@<Bd: Backend + 'static> State<Bd>);

smithay::delegate_alpha_modifier!(@<Bd: Backend + 'static> State<Bd>);
smithay::delegate_fifo!(@<Bd: Backend + 'static> State<Bd>);
smithay::delegate_commit_timing!(@<Bd: Backend + 'static> State<Bd>);
smithay::delegate_pointer_gestures!(@<Bd: Backend + 'static> State<Bd>);

smithay::delegate_tablet_manager!(@<Bd: Backend + 'static> State<Bd>);
