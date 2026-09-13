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

    /// Orders two rectangles by on-screen position: whichever is entirely
    /// above the other first, then whichever is entirely to the left,
    /// then — for two rectangles that overlap in both axes — the larger
    /// one first. Used to keep monitors (and, on real hardware, DRM
    /// outputs) in reading order rather than hotplug/connect order.
    ///
    /// bspwm: `src/geometry.c` `rect_cmp()`, translated from its `int`
    /// return (negative/zero/positive) to [`std::cmp::Ordering`].
    pub fn compare(&self, other: &Rect) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        if self.y >= other.bottom() {
            Ordering::Greater
        } else if other.y >= self.bottom() {
            Ordering::Less
        } else if self.x >= other.right() {
            Ordering::Greater
        } else if other.x >= self.right() {
            Ordering::Less
        } else {
            other.area().cmp(&self.area())
        }
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

    #[test]
    fn compare_orders_entirely_above_before_entirely_below() {
        let top = Rect::new(0, 0, 100, 100);
        let bottom = Rect::new(0, 100, 100, 100);
        assert_eq!(top.compare(&bottom), std::cmp::Ordering::Less);
        assert_eq!(bottom.compare(&top), std::cmp::Ordering::Greater);
    }

    #[test]
    fn compare_orders_left_before_right_when_y_ranges_overlap() {
        let left = Rect::new(0, 0, 100, 100);
        let right = Rect::new(100, 0, 100, 100);
        assert_eq!(left.compare(&right), std::cmp::Ordering::Less);
        assert_eq!(right.compare(&left), std::cmp::Ordering::Greater);
    }

    #[test]
    fn compare_orders_partially_overlapping_rows_by_x_not_y() {
        // Neither is entirely above/below the other (their y-ranges
        // overlap), so bspwm's rect_cmp falls through to the x
        // comparison rather than treating them as tied.
        let a = Rect::new(0, 0, 100, 200);
        let b = Rect::new(100, 100, 100, 200);
        assert_eq!(a.compare(&b), std::cmp::Ordering::Less);
    }

    #[test]
    fn compare_prefers_the_larger_rectangle_when_fully_overlapping() {
        let small = Rect::new(0, 0, 50, 50);
        let large = Rect::new(0, 0, 100, 100);
        assert_eq!(large.compare(&small), std::cmp::Ordering::Less);
        assert_eq!(small.compare(&large), std::cmp::Ordering::Greater);
    }

    #[test]
    fn compare_is_equal_for_identical_rectangles() {
        let r = Rect::new(0, 0, 100, 100);
        assert_eq!(r.compare(&r), std::cmp::Ordering::Equal);
    }
}
