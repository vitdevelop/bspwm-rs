//! Rectangles and padding.
//!
//! Mirrors bspwm's `xcb_rectangle_t` and `padding_t` (`src/types.h`), but
//! [`Rect`] uses `i32` for every field instead of xcb's `i16` x/y and `u16`
//! width/height: bsp-core never encodes the wire format (that is
//! `bsp-ipc`'s job in the IPC), so there is no reason to inherit xcb's
//! narrower integer types here.

use serde::{Deserialize, Serialize};

/// An axis-aligned rectangle in logical pixels.
///
/// `width` and `height` are kept non-negative by every function in this
/// crate that produces a `Rect`; nothing here enforces it at the type
/// level, matching bspwm's own use of unsigned width/height that can still
/// be computed down to zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width in logical pixels.
    pub width: i32,
    /// Height in logical pixels.
    pub height: i32,
}

impl Rect {
    /// Creates a rectangle from its edges and size.
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Area in square logical pixels.
    ///
    /// bspwm: `src/geometry.c` `area()`.
    pub fn area(&self) -> i64 {
        self.width.max(0) as i64 * self.height.max(0) as i64
    }

    /// The x coordinate just past the right edge.
    pub fn right(&self) -> i32 {
        self.x + self.width
    }

    /// The y coordinate just past the bottom edge.
    pub fn bottom(&self) -> i32 {
        self.y + self.height
    }

    /// Returns `true` if `self` fully contains `other`.
    ///
    /// bspwm: `src/geometry.c` `contains()`.
    pub fn contains_rect(&self, other: &Rect) -> bool {
        self.x <= other.x
            && self.right() >= other.right()
            && self.y <= other.y
            && self.bottom() >= other.bottom()
    }

    /// Returns `true` if the point `(px, py)` lies inside `self`.
    ///
    /// bspwm: `src/geometry.c` `is_inside()`.
    pub fn contains_point(&self, px: i32, py: i32) -> bool {
        px >= self.x && px < self.right() && py >= self.y && py < self.bottom()
    }
}

/// Padding around a desktop's or monitor's usable area.
///
/// bspwm: `src/types.h` `padding_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Padding {
    /// Top padding.
    pub top: i32,
    /// Right padding.
    pub right: i32,
    /// Bottom padding.
    pub bottom: i32,
    /// Left padding.
    pub left: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn area_matches_bspwm_area() {
        // bspwm: src/geometry.c area() -- width * height.
        assert_eq!(Rect::new(0, 0, 10, 20).area(), 200);
    }

    #[test]
    fn contains_rect_matches_bspwm_contains() {
        let outer = Rect::new(0, 0, 100, 100);
        let inner = Rect::new(10, 10, 50, 50);
        assert!(outer.contains_rect(&inner));
        assert!(!inner.contains_rect(&outer));
    }

    #[test]
    fn contains_point_is_half_open() {
        let r = Rect::new(0, 0, 10, 10);
        assert!(r.contains_point(0, 0));
        assert!(r.contains_point(9, 9));
        assert!(!r.contains_point(10, 0));
        assert!(!r.contains_point(0, 10));
    }
}
