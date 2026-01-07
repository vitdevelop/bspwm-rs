//! Client data: the part of a tree node that exists only for leaves with a
//! window attached.
//!
//! bspwm: `src/types.h` `client_t`. Fields tied to X11/ICCCM (`size_hints`,
//! `icccm_props`, `wm_flags`) are left out of the core; they belong to
//! `bsp-compositor`, which is the only crate that talks to a real window.

use crate::geometry::Rect;
use crate::id::WindowId;

/// A client's tiling state.
///
/// bspwm: `src/types.h` `client_state_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientState {
    /// Occupies its slot in the tree at the tree's computed size.
    Tiled,
    /// Occupies its slot in the tree, but keeps a fixed size within it
    /// (bspwm: `IS_TILED` also covers this state; see `apply_layout`'s
    /// `STATE_PSEUDO_TILED` handling in `src/tree.c`).
    PseudoTiled,
    /// Free-floating, positioned by `floating_rectangle` and absent from
    /// the tiling layout.
    Floating,
    /// Covers the whole monitor, above every other client.
    Fullscreen,
}

impl ClientState {
    /// `true` for `Tiled` and `PseudoTiled`.
    ///
    /// bspwm: `src/helpers.h` `IS_TILED`.
    pub fn is_tiled(self) -> bool {
        matches!(self, ClientState::Tiled | ClientState::PseudoTiled)
    }
}

/// A client's stacking layer.
///
/// bspwm: `src/types.h` `stack_layer_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layer {
    /// Below normal windows.
    Below,
    /// The default layer.
    #[default]
    Normal,
    /// Above normal windows.
    Above,
}

/// The window-specific data attached to a leaf node.
///
/// bspwm: `src/types.h` `client_t`.
#[derive(Debug, Clone, PartialEq)]
pub struct Client {
    /// The window this leaf shows.
    pub window: WindowId,
    /// Current tiling state.
    pub state: ClientState,
    /// State held before the current one, restored when a transient state
    /// (floating, fullscreen) is turned off.
    ///
    /// bspwm: `src/types.h` `client_t.last_state`, set in `set_state()`
    /// (`src/tree.c`).
    pub last_state: ClientState,
    /// Current stacking layer.
    pub layer: Layer,
    /// Layer held before the current one.
    pub last_layer: Layer,
    /// Border width in logical pixels; usually `Settings::border_width`,
    /// but bspwm allows per-client overrides via rules and `bspc config`.
    pub border_width: i32,
    /// Rectangle used while floating.
    pub floating_rectangle: Rect,
    /// Rectangle computed by the tiling layout, cached for `bspc query -T`
    /// style introspection even while not the active state.
    pub tiled_rectangle: Rect,
    /// Demands-attention flag, set by the client or by rules.
    pub urgent: bool,
}

impl Client {
    /// Creates a client in the default state: tiled, normal layer, not
    /// urgent.
    ///
    /// bspwm: `src/tree.c` `make_client()`.
    pub fn new(window: WindowId, border_width: i32) -> Self {
        Self {
            window,
            state: ClientState::Tiled,
            last_state: ClientState::Tiled,
            layer: Layer::Normal,
            last_layer: Layer::Normal,
            border_width,
            floating_rectangle: Rect::default(),
            tiled_rectangle: Rect::default(),
            urgent: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_tiled_matches_bspwm_is_tiled_macro() {
        assert!(ClientState::Tiled.is_tiled());
        assert!(ClientState::PseudoTiled.is_tiled());
        assert!(!ClientState::Floating.is_tiled());
        assert!(!ClientState::Fullscreen.is_tiled());
    }

    #[test]
    fn new_client_defaults_match_bspwm_make_client() {
        let c = Client::new(WindowId(1), 1);
        assert_eq!(c.state, ClientState::Tiled);
        assert_eq!(c.layer, Layer::Normal);
        assert!(!c.urgent);
    }
}
