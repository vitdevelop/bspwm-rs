//! What can hold keyboard focus.
//!
//! Almost everything is a plain `wl_surface` (Wayland toplevels, layer
//! surfaces, lock surfaces). An X11 window is different: keyboard focus on
//! its `wl_surface` only tells *Xwayland* which surface is focused, while
//! the X server itself still needs an `XSetInputFocus` on the window — which
//! Smithay performs in `X11Surface`'s own `KeyboardTarget::enter`. So the
//! seat's focus type is this enum, and X11 windows are focused as
//! `FocusTarget::X11` (`crate::input::focus_target_of`).

use std::borrow::Cow;

use smithay::backend::input::KeyState;
use smithay::desktop::Window;
use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Serial};
use smithay::wayland::seat::WaylandFocus;
use smithay::xwayland::X11Surface;

use crate::state::{Backend, State};

/// A keyboard focus target.
#[derive(Debug, Clone, PartialEq)]
pub enum FocusTarget {
    /// A Wayland surface (toplevel, layer surface, lock surface).
    Surface(WlSurface),
    /// An X11 window (through XWayland).
    X11(X11Surface),
}

impl IsAlive for FocusTarget {
    fn alive(&self) -> bool {
        match self {
            FocusTarget::Surface(s) => s.alive(),
            FocusTarget::X11(x) => x.alive(),
        }
    }
}

impl WaylandFocus for FocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            FocusTarget::Surface(s) => Some(Cow::Borrowed(s)),
            FocusTarget::X11(x) => x.wl_surface().map(Cow::Owned),
        }
    }
}

impl From<WlSurface> for FocusTarget {
    fn from(surface: WlSurface) -> Self {
        FocusTarget::Surface(surface)
    }
}

/// The focus target for a mapped window: its X11 surface if it is an X11
/// window (even before XWayland has associated its `wl_surface`), else its
/// `wl_surface`.
pub fn focus_target_of(window: &Window) -> Option<FocusTarget> {
    if let Some(x11) = window.x11_surface() {
        return Some(FocusTarget::X11(x11.clone()));
    }
    window.wl_surface().map(|s| FocusTarget::Surface(s.into_owned()))
}

impl<Bd: Backend + 'static> KeyboardTarget<State<Bd>> for FocusTarget {
    fn enter(&self, seat: &Seat<State<Bd>>, data: &mut State<Bd>, keys: Vec<KeysymHandle<'_>>, serial: Serial) {
        match self {
            FocusTarget::Surface(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            FocusTarget::X11(x) => KeyboardTarget::enter(x, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<State<Bd>>, data: &mut State<Bd>, serial: Serial) {
        match self {
            FocusTarget::Surface(s) => KeyboardTarget::leave(s, seat, data, serial),
            FocusTarget::X11(x) => KeyboardTarget::leave(x, seat, data, serial),
        }
    }

    fn key(&self, seat: &Seat<State<Bd>>, data: &mut State<Bd>, key: KeysymHandle<'_>, state: KeyState, serial: Serial, time: u32) {
        match self {
            FocusTarget::Surface(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
            FocusTarget::X11(x) => KeyboardTarget::key(x, seat, data, key, state, serial, time),
        }
    }

    fn modifiers(&self, seat: &Seat<State<Bd>>, data: &mut State<Bd>, modifiers: ModifiersState, serial: Serial) {
        match self {
            FocusTarget::Surface(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
            FocusTarget::X11(x) => KeyboardTarget::modifiers(x, seat, data, modifiers, serial),
        }
    }
}
