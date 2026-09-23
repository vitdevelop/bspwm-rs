//! The stacking order of every managed window, bottom to top.
//!
//! bspwm keeps one list for all desktops (`src/stack.c` `stack_head`/
//! `stack_tail`) and asks the X server to restack a window whenever its place
//! in the list changes. Here the list is only the order; the compositor
//! applies it to its scene. A window's *level* (`Client::stack_level`) is
//! `3 * layer + state`: layer below/normal/above, then tiled, floating,
//! fullscreen. A window never sits below one of a lower level.

use crate::id::WindowId;

/// Where [`StackingList::stack`] put a window relative to another one.
///
/// bspwm: the two `node_stack` events, `<node> below <sibling>` and
/// `<node> above <sibling>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackMove {
    /// The window that was placed.
    pub window: WindowId,
    /// The window it was placed next to.
    pub sibling: WindowId,
    /// `true` if `window` now sits above `sibling`, `false` if below it.
    pub above: bool,
}

/// Every managed window, bottom first.
///
/// bspwm: `stacking_list_t`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StackingList {
    order: Vec<WindowId>,
}

impl StackingList {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// The windows, bottom to top.
    pub fn windows(&self) -> &[WindowId] {
        &self.order
    }

    /// Forgets `window` (it was unmanaged).
    ///
    /// bspwm: `src/stack.c` `remove_stack_node()`.
    pub fn remove(&mut self, window: WindowId) {
        self.order.retain(|w| *w != window);
    }

    /// Places `window` within its level: on top of it when `focused` (the
    /// window was focused or raised), at the bottom of it otherwise (a window
    /// that appeared without taking focus). A window never crosses into another
    /// level. `level` gives every listed window's level; `None` (unknown window)
    /// leaves the list untouched.
    ///
    /// bspwm: `src/stack.c` `stack()`, one leaf: `limit_above()`/`limit_below()`
    /// pick the neighbour, then `stack_insert_before()`/`stack_insert_after()`.
    pub fn stack(&mut self, window: WindowId, focused: bool, level: impl Fn(WindowId) -> Option<i32>) -> Option<StackMove> {
        let own = level(window)?;
        if self.order.is_empty() {
            self.order.push(window);
            return None;
        }
        let lv = |w: WindowId| level(w).unwrap_or(0);
        let last = self.order.len() - 1;
        // The neighbour: the first window strictly above `own`'s level (else the
        // top one) when focused; the last strictly below it (else the bottom one)
        // when not. Never the window itself: then its own neighbour instead.
        let mut idx = if focused {
            self.order.iter().position(|w| lv(*w) > own).unwrap_or(last)
        } else {
            self.order.iter().rposition(|w| lv(*w) < own).unwrap_or(0)
        };
        if self.order[idx] == window {
            idx = if focused { idx.checked_sub(1)? } else if idx < last { idx + 1 } else { return None };
        }
        let sibling = self.order[idx];
        let cmp = own - lv(sibling);
        let above = !(cmp < 0 || (cmp == 0 && !focused));
        self.order.retain(|w| *w != window);
        let at = self.order.iter().position(|w| *w == sibling)?;
        self.order.insert(if above { at + 1 } else { at }, window);
        Some(StackMove { window, sibling, above })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn w(n: u32) -> WindowId {
        WindowId(n)
    }

    fn levels(pairs: &[(u32, i32)]) -> impl Fn(WindowId) -> Option<i32> {
        let map: HashMap<WindowId, i32> = pairs.iter().map(|(n, l)| (w(*n), *l)).collect();
        move |id| map.get(&id).copied()
    }

    fn order(s: &StackingList) -> Vec<u32> {
        s.windows().iter().map(|w| w.0).collect()
    }

    // Levels: tiled 3, floating 4, fullscreen 5 (normal layer).

    #[test]
    fn a_new_window_that_took_focus_goes_on_top_of_its_level() {
        let l = levels(&[(1, 3), (2, 4), (3, 3)]);
        let mut s = StackingList::new();
        s.stack(w(1), true, &l);
        s.stack(w(2), true, &l);
        s.stack(w(3), true, &l);
        // The tiled window 3 sits above tiled 1 but below floating 2.
        assert_eq!(order(&s), [1, 3, 2]);
    }

    #[test]
    fn a_window_that_did_not_take_focus_goes_to_the_bottom_of_its_level() {
        let l = levels(&[(1, 3), (2, 4), (3, 3)]);
        let mut s = StackingList::new();
        s.stack(w(1), true, &l);
        s.stack(w(2), true, &l);
        s.stack(w(3), false, &l);
        assert_eq!(order(&s), [3, 1, 2]);
    }

    #[test]
    fn a_floating_window_stays_above_tiled_ones_when_the_tiled_one_is_raised() {
        // The bug this replaces: re-laying out a tiled window put it on top.
        let l = levels(&[(1, 3), (2, 4)]);
        let mut s = StackingList::new();
        s.stack(w(1), true, &l);
        s.stack(w(2), true, &l);
        s.stack(w(1), true, &l);
        assert_eq!(order(&s), [1, 2]);
    }

    #[test]
    fn raising_a_window_reports_where_it_went() {
        let l = levels(&[(1, 3), (2, 3), (3, 3)]);
        let mut s = StackingList::new();
        for n in [1, 2, 3] {
            s.stack(w(n), true, &l);
        }
        assert_eq!(order(&s), [1, 2, 3]);
        let m = s.stack(w(1), true, &l).unwrap();
        assert_eq!(order(&s), [2, 3, 1]);
        assert_eq!((m.window, m.sibling, m.above), (w(1), w(3), true));
    }

    #[test]
    fn a_window_changing_level_moves_across_the_others() {
        // 1 tiled, 2 floating, then 1 goes fullscreen (level 5): above both.
        let mut s = StackingList::new();
        let l = levels(&[(1, 3), (2, 4)]);
        s.stack(w(1), true, &l);
        s.stack(w(2), true, &l);
        let l = levels(&[(1, 5), (2, 4)]);
        s.stack(w(1), true, &l);
        assert_eq!(order(&s), [2, 1]);
        // ...and back to tiled, focused: below the floating one again.
        let l = levels(&[(1, 3), (2, 4)]);
        s.stack(w(1), true, &l);
        assert_eq!(order(&s), [1, 2]);
    }

    #[test]
    fn layers_dominate_states() {
        // normal-layer fullscreen (5) is below an above-layer tiled window (6).
        let l = levels(&[(1, 5), (2, 6), (3, 0)]);
        let mut s = StackingList::new();
        for n in [2, 1, 3] {
            s.stack(w(n), true, &l);
        }
        assert_eq!(order(&s), [3, 1, 2]);
    }

    #[test]
    fn removing_and_unknown_windows_are_harmless() {
        let l = levels(&[(1, 3)]);
        let mut s = StackingList::new();
        s.stack(w(1), true, &l);
        assert_eq!(s.stack(w(9), true, &l), None);
        assert_eq!(order(&s), [1]);
        s.remove(w(1));
        s.remove(w(1));
        assert!(s.windows().is_empty());
    }
}
