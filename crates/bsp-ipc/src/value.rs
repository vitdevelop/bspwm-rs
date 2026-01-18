//! Small `bspc` argument value grammars that have no existing home in
//! `bsp-core` (which owns `SplitType`, `Direction`, `FlipAxis`,
//! `CirculateDir`, `Layout`, `ChildPolarity`, `ClientState`, `Layer`).
//!
//! bspwm: `src/parse.c`.

/// `next`/`prev`: a cyclic direction through a traversal order.
///
/// bspwm: `src/types.h` `cycle_dir_t`, `src/parse.c` `parse_cycle_direction()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CycleDir {
    /// Forward in traversal order.
    Next,
    /// Backward in traversal order.
    Prev,
}

impl CycleDir {
    /// Parses `next`/`prev`.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "next" => Some(CycleDir::Next),
            "prev" => Some(CycleDir::Prev),
            _ => None,
        }
    }
}

/// Which edge or corner `node --resize` drags.
///
/// bspwm: `src/types.h` `resize_handle_t`, `src/parse.c` `parse_resize_handle()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResizeHandle {
    /// The left edge.
    Left,
    /// The top edge.
    Top,
    /// The right edge.
    Right,
    /// The bottom edge.
    Bottom,
    /// The top-left corner.
    TopLeft,
    /// The top-right corner.
    TopRight,
    /// The bottom-right corner.
    BottomRight,
    /// The bottom-left corner.
    BottomLeft,
}

impl ResizeHandle {
    /// Parses `left|top|right|bottom|top_left|top_right|bottom_right|bottom_left`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "left" => ResizeHandle::Left,
            "top" => ResizeHandle::Top,
            "right" => ResizeHandle::Right,
            "bottom" => ResizeHandle::Bottom,
            "top_left" => ResizeHandle::TopLeft,
            "top_right" => ResizeHandle::TopRight,
            "bottom_right" => ResizeHandle::BottomRight,
            "bottom_left" => ResizeHandle::BottomLeft,
            _ => return None,
        })
    }
}

/// Whether a flag-setting command sets an explicit value or toggles.
///
/// bspwm: `src/types.h` `alter_state_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlterState {
    /// Flip the current value.
    Toggle,
    /// Set the value explicitly.
    Set(bool),
}

/// Parses `true`/`on` as `Some(true)` and `false`/`off` as `Some(false)`.
///
/// bspwm: `src/parse.c` `parse_bool()`.
pub fn parse_bool(s: &str) -> Option<bool> {
    match s {
        "true" | "on" => Some(true),
        "false" | "off" => Some(false),
        _ => None,
    }
}

/// Parses a degree argument: any integer, normalized into `0..360` and
/// required to be a multiple of 90.
///
/// bspwm: `src/parse.c` `parse_degree()`.
pub fn parse_degree(s: &str) -> Option<i32> {
    let mut i: i32 = s.parse().ok()?;
    i = i.rem_euclid(360);
    if i % 90 == 0 {
        Some(i)
    } else {
        None
    }
}

/// Parses a `bspc` numeric id: `0x`/`0X`-prefixed hexadecimal, or decimal.
/// The whole string must be consumed (matching `strtol` + `*end != '\0'`
/// rejection in bspwm's `parse_id()`).
///
/// Octal (a bare `0`-prefixed string, which `strtol(s, &end, 0)` also
/// accepts) is not recognized: no id bspwm ever prints has a leading zero,
/// so no real selector needs it, and skipping it avoids surprising a
/// decimal-looking id such as `010` into being read as `8`.
///
/// bspwm: `src/parse.c` `parse_id()`.
pub fn parse_id(s: &str) -> Option<u32> {
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).ok()
    } else {
        s.parse().ok()
    }
}

/// Parses a `^<n>` index selector (`bspc rule --remove`, desktop `^<n>`).
///
/// bspwm: `src/parse.c` `parse_index()`.
pub fn parse_index(s: &str) -> Option<u16> {
    s.strip_prefix('^')?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bool_accepts_bspwm_spellings() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("on"), Some(true));
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("off"), Some(false));
        assert_eq!(parse_bool("yes"), None);
    }

    #[test]
    fn parse_degree_normalizes_and_rejects_non_multiples_of_90() {
        assert_eq!(parse_degree("90"), Some(90));
        assert_eq!(parse_degree("-90"), Some(270));
        assert_eq!(parse_degree("450"), Some(90));
        assert_eq!(parse_degree("45"), None);
    }

    #[test]
    fn parse_id_accepts_hex_and_decimal_and_rejects_trailing_garbage() {
        assert_eq!(parse_id("0x00C00003"), Some(0x00C00003));
        assert_eq!(parse_id("42"), Some(42));
        assert_eq!(parse_id("42x"), None);
    }

    #[test]
    fn parse_index_requires_caret_prefix() {
        assert_eq!(parse_index("^2"), Some(2));
        assert_eq!(parse_index("2"), None);
    }
}
