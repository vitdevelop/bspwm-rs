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
    /// Border color of an unfocused node, `#rrggbb`. Default `#30302f`
    /// (`NORMAL_BORDER_COLOR`, `src/settings.h`).
    pub normal_border_color: String,
    /// Border color of a desktop's focused node when its monitor is not
    /// the focused one. Default `#474645` (`ACTIVE_BORDER_COLOR`).
    pub active_border_color: String,
    /// Border color of the focused node on the focused monitor. Default
    /// `#817f7f` (`FOCUSED_BORDER_COLOR`).
    pub focused_border_color: String,
    /// Color of the preselection feedback. Default `#f4d775`
    /// (`PRESEL_FEEDBACK_COLOR`). Drawn by the compositor.
    pub presel_feedback_color: String,
    /// Text printed before the first monitor of every status report
    /// (`bspc wm -g`, `subscribe report`). Default `W` (`STATUS_PREFIX`).
    pub status_prefix: String,
    /// Focus the window under the pointer as it moves onto it. Default `false`.
    pub focus_follows_pointer: bool,
    /// Move the pointer to the centre of a window when it gets focus.
    /// Default `false`.
    pub pointer_follows_focus: bool,
    /// Move the pointer to the centre of a monitor when it gets focus.
    /// Default `false`.
    pub pointer_follows_monitor: bool,
    /// Draw the preselection feedback. Default `true`. Stored and reported; the
    /// compositor draws it.
    pub presel_feedback: bool,
    /// How strictly directional focus keeps to a direction
    /// (`directional_focus_tightness`). Default `High`.
    pub directional_focus_tightness: Tightness,
    /// Which fullscreen requests from windows are ignored
    /// (`ignore_ewmh_fullscreen`). Default: none.
    pub ignore_ewmh_fullscreen: StateTransition,
    /// Command run for every new window to add rule effects
    /// (`external_rules_command`). Default: empty (none).
    pub external_rules_command: String,
    /// Which windows honor their size hints (`honor_size_hints`). Stored and
    /// reported; the compositor does not apply size hints yet. Default `false`.
    pub honor_size_hints: HonorSizeHints,
    /// `mapping_events_count`, an X11 keyboard mapping setting. Stored only.
    pub mapping_events_count: i8,
    /// `remove_disabled_monitors`. Stored only (monitors follow the outputs).
    pub remove_disabled_monitors: bool,
    /// `remove_unplugged_monitors`. Stored only.
    pub remove_unplugged_monitors: bool,
    /// `merge_overlapping_monitors`. Stored only.
    pub merge_overlapping_monitors: bool,
}

/// How strictly directional focus keeps to a direction.
///
/// bspwm: `src/types.h` `tightness_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tightness {
    /// A candidate only has to be partly in the direction.
    Low,
    /// A candidate has to be entirely on that side.
    #[default]
    High,
}

/// Which way of a window state change an `ignore_ewmh_*` setting covers.
///
/// bspwm: `src/types.h` `state_transition_t` (`STATE_TRANSITION_ENTER`/`EXIT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StateTransition {
    /// Entering the state (going fullscreen) is ignored.
    pub enter: bool,
    /// Leaving the state is ignored.
    pub exit: bool,
}

/// `honor_size_hints`' value.
///
/// bspwm: `src/types.h` `honor_size_hints_mode_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HonorSizeHints {
    /// `false`.
    #[default]
    No,
    /// `true`.
    Yes,
    /// `floating`.
    Floating,
    /// `tiled`.
    Tiled,
}

/// Whether `s` is a valid color value for a `*_color` setting: `#` plus six
/// hex digits, nothing else.
///
/// bspwm: `src/helpers.c` `is_hex_color()`.
#[must_use]
pub fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Parses a `#rrggbb` color into 8-bit channels; `None` if it is not one.
///
/// bspwm: `src/bspwm.c` `get_color_pixel()` (which falls back to black
/// for anything it cannot read; callers here choose their own fallback).
#[must_use]
pub fn parse_hex_color(s: &str) -> Option<[u8; 3]> {
    if !is_hex_color(s) {
        return None;
    }
    let ch = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
    Some([ch(1)?, ch(3)?, ch(5)?])
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
            normal_border_color: "#30302f".to_string(),
            active_border_color: "#474645".to_string(),
            focused_border_color: "#817f7f".to_string(),
            presel_feedback_color: "#f4d775".to_string(),
            status_prefix: "W".to_string(),
            focus_follows_pointer: false,
            pointer_follows_focus: false,
            pointer_follows_monitor: false,
            presel_feedback: true,
            directional_focus_tightness: Tightness::High,
            ignore_ewmh_fullscreen: StateTransition::default(),
            external_rules_command: String::new(),
            honor_size_hints: HonorSizeHints::No,
            mapping_events_count: 1,
            remove_disabled_monitors: false,
            remove_unplugged_monitors: false,
            merge_overlapping_monitors: false,
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
        assert_eq!(s.normal_border_color, "#30302f");
        assert_eq!(s.active_border_color, "#474645");
        assert_eq!(s.focused_border_color, "#817f7f");
        assert_eq!(s.presel_feedback_color, "#f4d775");
        assert_eq!(s.status_prefix, "W");
    }

    #[test]
    fn hex_colors_need_a_hash_and_six_hex_digits() {
        assert!(is_hex_color("#93a1A1"));
        assert!(!is_hex_color("93a1a1"));
        assert!(!is_hex_color("#93a1a"));
        assert!(!is_hex_color("#93a1a1f"));
        assert!(!is_hex_color("#93a1g1"));
        assert_eq!(parse_hex_color("#0080ff"), Some([0, 128, 255]));
        assert_eq!(parse_hex_color("red"), None);
    }
}
