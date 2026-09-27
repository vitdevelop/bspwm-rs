//! Client data: the part of a tree node that exists only for leaves with a
//! window attached.
//!
//! bspwm: `src/types.h` `client_t`. Fields tied to X11/ICCCM (`icccm_props`,
//! `wm_flags`) are left out of the core; they belong to `bsp-compositor`,
//! which is the only crate that talks to a real window. The size hints are
//! kept here (filled by the compositor from `WM_NORMAL_HINTS` or the xdg
//! min/max size), since the layout applies them.

use crate::geometry::Rect;
use crate::id::WindowId;
use crate::settings::HonorSizeHints;

/// A window's size hints, each `None` when the window gives none.
///
/// bspwm: `xcb_size_hints_t`, as read by `src/window.c` `apply_size_hints()`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SizeHints {
    /// Minimum size (`P_MIN_SIZE`; xdg `set_min_size`).
    pub min: Option<(i32, i32)>,
    /// Maximum size (`P_MAX_SIZE`; xdg `set_max_size`). A zero component is
    /// unbounded.
    pub max: Option<(i32, i32)>,
    /// Base size (`BASE_SIZE`).
    pub base: Option<(i32, i32)>,
    /// Resize increments (`P_RESIZE_INC`).
    pub inc: Option<(i32, i32)>,
    /// Aspect range `((min_num, min_den), (max_num, max_den))` (`P_ASPECT`).
    pub aspect: Option<((i32, i32), (i32, i32))>,
}

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
    /// The border width actually drawn: `border_width`, or 0 for a fullscreen
    /// window, a tiled window in monocle with `borderless_monocle`, or the
    /// only window on the only monitor with `borderless_singleton`. Set by the
    /// layout (`Tree::apply_layout`); bspwm keeps `client->border_width` as the
    /// configured value and only draws the effective one.
    pub shown_border_width: i32,
    /// Rectangle used while floating.
    pub floating_rectangle: Rect,
    /// Rectangle computed by the tiling layout, cached for `bspc query -T`
    /// style introspection even while not the active state.
    pub tiled_rectangle: Rect,
    /// Demands-attention flag, set by the client or by rules.
    pub urgent: bool,
    /// The window's size hints.
    pub size_hints: SizeHints,
    /// Which states the size hints are honoured in (the setting's value when
    /// the window was managed, a rule's `honor_size_hints=`, or `bspc config`).
    ///
    /// bspwm: `client_t.honor_size_hints`.
    pub honor_size_hints: HonorSizeHints,
    /// Where the window was last put and reported (`node_geometry`): an X11
    /// window's own geometry when it is managed, `None` for a Wayland window
    /// until it is first placed.
    ///
    /// bspwm: the X window's geometry, read back by `get_window_rectangle()`
    /// in `apply_layout()`.
    pub window_rectangle: Option<Rect>,
}

impl Client {
    /// Where the window sits in the stacking order: `3 * layer + state`, with
    /// layer below/normal/above = 0/1/2 and state tiled/floating/fullscreen =
    /// 0/1/2. A higher level is drawn above a lower one.
    ///
    /// bspwm: `src/stack.c` `stack_level()`.
    pub fn stack_level(&self) -> i32 {
        let layer = match self.layer {
            Layer::Below => 0,
            Layer::Normal => 1,
            Layer::Above => 2,
        };
        let state = if self.state.is_tiled() {
            0
        } else if self.state == ClientState::Floating {
            1
        } else {
            2
        };
        3 * layer + state
    }

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
            shown_border_width: border_width,
            floating_rectangle: Rect::default(),
            tiled_rectangle: Rect::default(),
            urgent: false,
            size_hints: SizeHints::default(),
            honor_size_hints: HonorSizeHints::No,
            window_rectangle: None,
        }
    }

    /// Whether the size hints apply in the current state.
    ///
    /// bspwm: `src/helpers.h` `SHOULD_HONOR_SIZE_HINTS`.
    pub fn should_honor_size_hints(&self) -> bool {
        match self.honor_size_hints {
            HonorSizeHints::No => false,
            HonorSizeHints::Yes => self.state != ClientState::Fullscreen,
            HonorSizeHints::Tiled => self.state == ClientState::Tiled,
            HonorSizeHints::Floating => matches!(self.state, ClientState::Floating | ClientState::PseudoTiled),
        }
    }

    /// `(width, height)` adjusted to the size hints: aspect range, minimum,
    /// maximum and increments, in that order. Unchanged when the hints are not
    /// honoured in the current state.
    ///
    /// bspwm: `src/window.c` `apply_size_hints()` (from awesome).
    pub fn apply_size_hints(&self, width: i32, height: i32) -> (i32, i32) {
        if !self.should_honor_size_hints() {
            return (width, height);
        }
        let h = &self.size_hints;
        let (mut w, mut hh) = (width, height);
        let (real_basew, real_baseh) = h.base.unwrap_or((0, 0));
        // Base size stands in for the minimum and the other way round.
        let (basew, baseh) = h.base.or(h.min).unwrap_or((0, 0));
        let (minw, minh) = h.min.or(h.base).unwrap_or((0, 0));

        if let Some(((min_num, min_den), (max_num, max_den))) = h.aspect {
            if min_den > 0 && max_den > 0 && hh > real_baseh && w > real_basew {
                let dx = f64::from(w - real_basew);
                let dy = f64::from(hh - real_baseh);
                let ratio = dx / dy;
                let min = f64::from(min_num) / f64::from(min_den);
                let max = f64::from(max_num) / f64::from(max_den);
                if max > 0.0 && min > 0.0 && ratio > 0.0 {
                    if ratio < min {
                        let dy = dx / min + 0.5;
                        w = dx as i32 + real_basew;
                        hh = dy as i32 + real_baseh;
                    } else if ratio > max {
                        let dx = dy * max + 0.5;
                        w = dx as i32 + real_basew;
                        hh = dy as i32 + real_baseh;
                    }
                }
            }
        }

        w = w.max(minw);
        hh = hh.max(minh);

        if let Some((maxw, maxh)) = h.max {
            if maxw > 0 {
                w = w.min(maxw);
            }
            if maxh > 0 {
                hh = hh.min(maxh);
            }
        }

        // bspwm: `flags & (P_RESIZE_INC | BASE_SIZE)` with both increments > 0.
        if let Some((incw, inch)) = h.inc.filter(|&(a, b)| a > 0 && b > 0) {
            let t1 = (w - basew).max(0);
            let t2 = (hh - baseh).max(0);
            w -= t1 % incw;
            hh -= t2 % inch;
        }
        (w, hh)
    }

    /// The rectangle the window is shown at: the floating rectangle while
    /// floating, the layout's otherwise, adjusted to the size hints.
    ///
    /// bspwm: `apply_layout()`'s `r` after `apply_size_hints(n->client, &r.width, &r.height)`.
    pub fn shown_rectangle(&self) -> Rect {
        let mut r = if self.state == ClientState::Floating { self.floating_rectangle } else { self.tiled_rectangle };
        let (w, h) = self.apply_size_hints(r.width, r.height);
        r.width = w;
        r.height = h;
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hinted(hints: SizeHints, honor: HonorSizeHints) -> Client {
        let mut c = Client::new(WindowId(1), 1);
        c.size_hints = hints;
        c.honor_size_hints = honor;
        c
    }

    #[test]
    fn size_hints_are_ignored_unless_honoured_in_the_state() {
        let c = hinted(SizeHints { max: Some((100, 100)), ..Default::default() }, HonorSizeHints::No);
        assert_eq!(c.apply_size_hints(300, 200), (300, 200));
        let c = hinted(SizeHints { max: Some((100, 100)), ..Default::default() }, HonorSizeHints::Floating);
        assert_eq!(c.apply_size_hints(300, 200), (300, 200), "a tiled window");
        let c = hinted(SizeHints { max: Some((100, 100)), ..Default::default() }, HonorSizeHints::Tiled);
        assert_eq!(c.apply_size_hints(300, 200), (100, 100));
    }

    #[test]
    fn size_hints_follow_apply_size_hints() {
        let yes = HonorSizeHints::Yes;
        // Minimum and maximum (0 = unbounded).
        let c = hinted(SizeHints { min: Some((50, 60)), max: Some((0, 100)), ..Default::default() }, yes);
        assert_eq!(c.apply_size_hints(10, 500), (50, 100));
        // Increments from the base size: a terminal with 7x14 cells and a 4x4 base.
        let c = hinted(SizeHints { base: Some((4, 4)), inc: Some((7, 14)), ..Default::default() }, yes);
        assert_eq!(c.apply_size_hints(804, 604), (802, 592));
        // Increments without a base count from the minimum.
        let c = hinted(SizeHints { min: Some((10, 10)), inc: Some((10, 10)), ..Default::default() }, yes);
        assert_eq!(c.apply_size_hints(105, 99), (100, 90));
        // Aspect 1:1 exactly: too wide shrinks the width, too tall the height.
        let c = hinted(SizeHints { aspect: Some(((1, 1), (1, 1))), ..Default::default() }, yes);
        assert_eq!(c.apply_size_hints(300, 200), (200, 200));
        assert_eq!(c.apply_size_hints(200, 300), (200, 200));
    }

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
