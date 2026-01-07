//! Every `bspc config` key relevant to the tree engine, with bspwm's defaults.
//!
//! Values and defaults: bspwm `src/settings.h` and `src/settings.c`
//! `load_settings()`. Settings that only affect X11/EWMH/pointer behavior
//! (`pointer_modifier`, `click_to_focus`, `ignore_ewmh_*`, ...) are left out
//! of the core; they belong to `bsp-compositor` once there is input to read.

use crate::geometry::Padding;
use crate::tree::ChildPolarity;

/// The automatic split scheme used when inserting a node with no
/// preselection.
///
/// bspwm: `src/types.h` `automatic_scheme_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutomaticScheme {
    /// Split along the longer side of the anchor node's rectangle.
    LongestSide,
    /// Alternate split orientation with each ancestor.
    Alternate,
    /// Rotate the anchor's subtree to grow a spiral.
    Spiral,
}

/// Every `bspc config` key the tree engine consults.
///
/// bspwm: `src/settings.c` `load_settings()`.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Gap, in logical pixels, left between tiled windows and around the
    /// desktop's usable area. Default: 6 (`WINDOW_GAP`).
    pub window_gap: i32,
    /// Border width, in logical pixels, applied to every client by default.
    /// Default: 1 (`BORDER_WIDTH`).
    pub border_width: i32,
    /// Split ratio assigned to a new split. Default: 0.5 (`SPLIT_RATIO`).
    pub split_ratio: f64,
    /// Which child a newly inserted node becomes. Default: `SecondChild`
    /// (`src/settings.c`: `initial_polarity = SECOND_CHILD`, not the
    /// `FIRST_CHILD` one might expect from `types.h`'s declaration order).
    pub initial_polarity: ChildPolarity,
    /// Automatic split scheme. Default: `LongestSide`
    /// (`AUTOMATIC_SCHEME` = `SCHEME_LONGEST_SIDE`).
    pub automatic_scheme: AutomaticScheme,
    /// Whether removing a node re-splits its sibling to fill the freed
    /// space. Default: `true` (`REMOVAL_ADJUSTMENT`).
    pub removal_adjustment: bool,
    /// Padding applied around every desktop's usable area by default.
    /// Default: `{0, 0, 0, 0}` (`PADDING`).
    pub padding: Padding,
    /// Extra padding applied only in monocle layout. Default:
    /// `{0, 0, 0, 0}` (`MONOCLE_PADDING`).
    pub monocle_padding: Padding,
    /// Suppress the window gap in monocle layout. Default: `false`
    /// (`GAPLESS_MONOCLE`).
    pub gapless_monocle: bool,
    /// Suppress borders on tiled windows in monocle layout. Default:
    /// `false` (`BORDERLESS_MONOCLE`).
    pub borderless_monocle: bool,
    /// Suppress the border when a desktop holds exactly one window.
    /// Default: `false` (`BORDERLESS_SINGLETON`).
    pub borderless_singleton: bool,
    /// Switch a desktop to monocle layout automatically when it holds at
    /// most one tiled window. Default: `false` (`SINGLE_MONOCLE`).
    pub single_monocle: bool,
    /// Center a pseudo-tiled client's floating-sized rectangle within its
    /// tiled slot. Default: `true` (`CENTER_PSEUDO_TILED`).
    pub center_pseudo_tiled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            window_gap: 6,
            border_width: 1,
            split_ratio: 0.5,
            initial_polarity: ChildPolarity::Second,
            automatic_scheme: AutomaticScheme::LongestSide,
            removal_adjustment: true,
            padding: Padding::default(),
            monocle_padding: Padding::default(),
            gapless_monocle: false,
            borderless_monocle: false,
            borderless_singleton: false,
            single_monocle: false,
            center_pseudo_tiled: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_bspwm_settings_c() {
        let s = Settings::default();
        assert_eq!(s.window_gap, 6);
        assert_eq!(s.border_width, 1);
        assert_eq!(s.split_ratio, 0.5);
        assert_eq!(s.initial_polarity, ChildPolarity::Second);
        assert_eq!(s.automatic_scheme, AutomaticScheme::LongestSide);
        assert!(s.removal_adjustment);
        assert!(s.center_pseudo_tiled);
        assert!(!s.single_monocle);
        assert!(!s.gapless_monocle);
        assert!(!s.borderless_monocle);
        assert!(!s.borderless_singleton);
    }
}
