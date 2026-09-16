//! `ext-session-lock-v1`: screen lockers (`swaylock`, `waylock`, `gtklock`).
//!
//! bspwm has no lock of its own — under X11 a locker (`i3lock`, `xsecurelock`)
//! grabs the keyboard and covers the root window, which is unreliable (the
//! very reason this protocol exists). Here the compositor enforces it:
//!
//! - once a client's `lock` request is accepted, *every* output shows only
//!   that client's lock surface for the output (black until it has one);
//!   no window, panel or border is drawn (`crate::render::output_elements`);
//! - all keyboard and pointer input goes to the lock surface only —
//!   sxhkdrc bindings and pointer bindings are off; the hardcoded emergency
//!   keys (quit, VT switch) still work;
//! - `locked` is confirmed only once every output has a lock surface;
//! - the lock ends only on the locker's `unlock_and_destroy`. If the locker
//!   dies, the session **stays locked** (as the protocol requires) — a new
//!   locker can take over, or the emergency quit key ends the compositor.

use std::sync::Mutex;

use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, Size};
use smithay::wayland::session_lock::{LockSurface, SessionLockHandler, SessionLockManagerState, SessionLocker};

use crate::state::{Backend, State};

/// Per-output lock data, kept in the `Output`'s user data so the renderer
/// (which only sees the `Output`) can tell.
#[derive(Default)]
pub struct OutputLock {
    /// The output's lock surface, if the locker made one.
    surface: Mutex<Option<WlSurface>>,
    locked: std::sync::atomic::AtomicBool,
}

/// Per-protocol state.
#[derive(Default)]
pub struct SessionLock {
    /// A `lock` request waiting to be confirmed once every output is covered.
    pending: Option<SessionLocker>,
    /// Whether the session is locked.
    pub locked: bool,
}

fn data(output: &Output) -> &OutputLock {
    output.user_data().get_or_insert_threadsafe(OutputLock::default)
}

/// Whether `output` currently shows only a lock screen.
pub fn is_locked(output: &Output) -> bool {
    data(output).locked.load(std::sync::atomic::Ordering::SeqCst)
}

/// `output`'s lock surface, if it has one.
pub fn lock_surface(output: &Output) -> Option<WlSurface> {
    data(output).surface.lock().ok().and_then(|s| s.clone())
}

impl<Bd: Backend + 'static> SessionLockHandler for State<Bd> {
    fn lock_state(&mut self) -> &mut SessionLockManagerState {
        &mut self.protocols._session_lock_manager
    }

    fn lock(&mut self, confirmation: SessionLocker) {
        tracing::info!("session lock requested");
        self.protocols.session_lock.locked = true;
        self.protocols.session_lock.pending = Some(confirmation);
        for output in self.space.outputs() {
            data(output).locked.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.backend_data.queue_redraw();
        // Keyboard focus leaves the windows at once.
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, None, smithay::utils::SERIAL_COUNTER.next_serial());
        }
    }

    fn unlock(&mut self) {
        tracing::info!("session unlocked");
        self.protocols.session_lock.locked = false;
        self.protocols.session_lock.pending = None;
        for output in self.space.outputs() {
            let d = data(output);
            d.locked.store(false, std::sync::atomic::Ordering::SeqCst);
            if let Ok(mut surface) = d.surface.lock() {
                *surface = None;
            }
        }
        crate::input::sync_keyboard_focus(self);
        self.backend_data.queue_redraw();
    }

    fn new_surface(&mut self, surface: LockSurface, wl_output: WlOutput) {
        let Some(output) = Output::from_resource(&wl_output) else {
            return;
        };
        let Some(geo) = self.space.output_geometry(&output) else {
            return;
        };
        tracing::debug!(output = output.name(), "lock surface created");
        surface.with_pending_state(|state| state.size = Some(Size::from((geo.size.w as u32, geo.size.h as u32))));
        surface.send_configure();
        if let Ok(mut slot) = data(&output).surface.lock() {
            *slot = Some(surface.wl_surface().clone());
        }
        // The lock surface owns the keyboard.
        if let Some(keyboard) = self.seat.get_keyboard() {
            keyboard.set_focus(self, Some(crate::focus::FocusTarget::Surface(surface.wl_surface().clone())), smithay::utils::SERIAL_COUNTER.next_serial());
        }
        self.backend_data.queue_redraw();
    }
}
smithay::delegate_session_lock!(@<Bd: Backend + 'static> State<Bd>);

/// Confirms a pending lock once every output has a lock surface. Called
/// after each event-loop turn.
pub fn poll<Bd: Backend + 'static>(state: &mut State<Bd>) {
    if state.protocols.session_lock.pending.is_none() {
        return;
    }
    if state.space.outputs().all(|o| lock_surface(o).is_some()) {
        if let Some(locker) = state.protocols.session_lock.pending.take() {
            tracing::info!("session locked (every output covered)");
            locker.lock();
        }
    }
}

/// The lock surface under `pos` (global coordinates) and its origin, while locked.
pub fn surface_under<Bd: Backend + 'static>(state: &State<Bd>, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
    let output = state.space.output_under(pos).next()?;
    let surface = lock_surface(output)?;
    let origin = state.space.output_geometry(output)?.loc.to_f64();
    Some((surface, origin))
}
