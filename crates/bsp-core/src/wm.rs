//! The whole window manager: every monitor, the global rule list, and the
//! settings every new monitor/desktop/node inherits.
//!
//! bspwm keeps this as global variables — `src/bspwm.h` `mon_head`/
//! `mon_tail`/`mon` (focused), `rule_head`/`rule_tail` — rather than a
//! struct. `bsp-core` collects them into one so `bsp-ipc` has a
//! single root to resolve selectors and run commands against.

use crate::id::MonitorId;
use crate::monitor::Monitor;
use crate::rules::Rule;
use crate::settings::Settings;

/// The `_NET_WM_STRUT_PARTIAL` property of a panel window (EWMH): how much
/// of each screen edge it covers, and along which stretch of that edge.
///
/// bspwm: `xcb_ewmh_wm_strut_partial_t`, read by `src/ewmh.c` `ewmh_handle_struts()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EwmhStruts {
    /// Width reserved at the left edge.
    pub left: u32,
    /// Width reserved at the right edge.
    pub right: u32,
    /// Height reserved at the top edge.
    pub top: u32,
    /// Height reserved at the bottom edge.
    pub bottom: u32,
    /// First `y` the left strut covers.
    pub left_start_y: u32,
    /// Last `y` the left strut covers.
    pub left_end_y: u32,
    /// First `y` the right strut covers.
    pub right_start_y: u32,
    /// Last `y` the right strut covers.
    pub right_end_y: u32,
    /// First `x` the top strut covers.
    pub top_start_x: u32,
    /// Last `x` the top strut covers.
    pub top_end_x: u32,
    /// First `x` the bottom strut covers.
    pub bottom_start_x: u32,
    /// Last `x` the bottom strut covers.
    pub bottom_end_x: u32,
}

impl EwmhStruts {
    /// Builds the struts from the property's twelve `CARDINAL`s, in the
    /// order EWMH defines: left, right, top, bottom, then the start/end pairs.
    pub fn from_cardinals(v: &[u32]) -> Option<Self> {
        let v: &[u32; 12] = v.try_into().ok()?;
        Some(Self {
            left: v[0],
            right: v[1],
            top: v[2],
            bottom: v[3],
            left_start_y: v[4],
            left_end_y: v[5],
            right_start_y: v[6],
            right_end_y: v[7],
            top_start_x: v[8],
            top_end_x: v[9],
            bottom_start_x: v[10],
            bottom_end_x: v[11],
        })
    }
}

/// The complete state of the window manager: every monitor in display
/// order, which one is focused, the global rule list, and settings.
///
/// bspwm: `src/bspwm.h` `mon_head`/`mon_tail`/`mon`, `rule_head`/
/// `rule_tail`. `pri_mon` (the primary monitor, an X11/RandR concept with
/// no Wayland equivalent yet) is left out; see `docs/bsp-ipc.md`.
#[derive(Debug, Clone)]
pub struct Wm {
    /// Every monitor, in display order.
    pub monitors: Vec<Monitor>,
    /// Index into `monitors` of the focused monitor, or `None` if there are
    /// no monitors.
    pub focused_monitor: Option<usize>,
    /// The global rule list, in the order rules were added. bspwm tries
    /// them head to tail and applies every match, not just the first
    /// (`src/rule.c` `apply_rules()`); `bsp-ipc` owns that traversal.
    pub rules: Vec<Rule>,
    /// Settings every new monitor/desktop/node inherits, and the defaults
    /// `bspc config` reads and writes.
    pub settings: Settings,
}

impl Wm {
    /// Creates a window manager with no monitors and no rules.
    pub fn new(settings: Settings) -> Self {
        Self {
            monitors: Vec::new(),
            focused_monitor: None,
            rules: Vec::new(),
            settings,
        }
    }

    /// Appends a monitor, then walks it into on-screen-position order
    /// among its neighbors ([`reorder_monitor`](Self::reorder_monitor)),
    /// and returns its final index. Focuses it if it is the first
    /// monitor.
    ///
    /// bspwm: `src/monitor.c` `add_monitor()`, minus RandR/EWMH
    /// bookkeeping — and structured differently to reach the same result:
    /// bspwm walks its linked list to find `m`'s correct position and
    /// splices it straight in there; this pushes to the end and reuses
    /// [`swap_monitors`](Self::swap_monitors) (via `reorder_monitor`) to
    /// bubble it into place, so the already-correct focus-index
    /// bookkeeping that function does is not duplicated here. The only
    /// observable difference is for two monitors with the *exact* same
    /// rectangle (`Rect::compare` returns `Equal`): bspwm's insertion
    /// loop stops at the first tie and splices the new monitor in
    /// *before* it (reversing arrival order for ties), while this leaves
    /// ties in arrival order (a tie never satisfies `reorder_monitor`'s
    /// strict `Less`/`Greater` swap condition) — real monitors
    /// essentially never share an identical rectangle, so this is not
    /// expected to matter in practice.
    pub fn add_monitor(&mut self, m: Monitor) -> usize {
        self.monitors.push(m);
        let index = self.monitors.len() - 1;
        if self.focused_monitor.is_none() {
            self.focused_monitor = Some(index);
        }
        self.reorder_monitor(index)
    }

    /// Moves the monitor at `index` earlier or later among its immediate
    /// neighbors, one swap at a time, until it is positioned correctly
    /// relative to them by on-screen position (`Rect::compare`) — a
    /// local reordering after one monitor's rectangle changes (or it is
    /// newly added), not a full re-sort of `monitors`. Returns the
    /// monitor's index afterward (it may have moved); every swap goes
    /// through [`swap_monitors`](Self::swap_monitors), so `focused_monitor`
    /// stays correct throughout.
    ///
    /// bspwm: `src/monitor.c` `reorder_monitor()`. Bspwm's monitors are a
    /// doubly linked list, so a monitor consults its own `prev`/`next`
    /// directly; `self.monitors` is a `Vec`, so `index - 1`/`index + 1`
    /// plays the same role.
    pub fn reorder_monitor(&mut self, mut index: usize) -> usize {
        use std::cmp::Ordering;
        while index > 0
            && self.monitors[index]
                .rectangle
                .compare(&self.monitors[index - 1].rectangle)
                == Ordering::Less
        {
            self.swap_monitors(index, index - 1);
            index -= 1;
        }
        while index + 1 < self.monitors.len()
            && self.monitors[index]
                .rectangle
                .compare(&self.monitors[index + 1].rectangle)
                == Ordering::Greater
        {
            self.swap_monitors(index, index + 1);
            index += 1;
        }
        index
    }

    /// Removes the monitor at `index`, returning it. Refocuses onto a
    /// neighbor if the removed monitor was focused.
    ///
    /// bspwm: `src/monitor.c` `remove_monitor()`, minus merging its
    /// desktops elsewhere first (the caller must empty `desktops` itself,
    /// as `Monitor::remove_desktop` documents) and EWMH bookkeeping.
    pub fn remove_monitor(&mut self, index: usize) -> Monitor {
        let m = self.monitors.remove(index);
        self.focused_monitor = match self.focused_monitor {
            Some(f) if self.monitors.is_empty() => {
                let _ = f;
                None
            }
            Some(f) if f > index => Some(f - 1),
            Some(f) if f == index => Some(f.min(self.monitors.len() - 1)),
            other => other,
        };
        m
    }

    /// Focuses the monitor at `index`. Returns `false` if it is already
    /// focused.
    ///
    /// bspwm: `src/monitor.c` `focus_node()`'s monitor-focusing half.
    pub fn focus_monitor(&mut self, index: usize) -> bool {
        if self.focused_monitor == Some(index) {
            return false;
        }
        self.focused_monitor = Some(index);
        true
    }

    /// Swaps the monitors at `i` and `j`, keeping the focused index pointed
    /// at whichever monitor was focused before the swap.
    ///
    /// bspwm: `src/monitor.c` `swap_monitors()`, minus the sticky-desktop
    /// carry-over (a monitor's desktops move with it here, since `i`/`j`
    /// name slots in `self.monitors` rather than linked-list nodes).
    pub fn swap_monitors(&mut self, i: usize, j: usize) {
        if i == j {
            return;
        }
        self.monitors.swap(i, j);
        self.focused_monitor = match self.focused_monitor {
            Some(f) if f == i => Some(j),
            Some(f) if f == j => Some(i),
            other => other,
        };
    }

    /// Reserves the space a panel's `_NET_WM_STRUT_PARTIAL` asks for on every
    /// monitor whose edge it touches, growing that monitor's `padding`
    /// (never shrinking it); `screen` is the X screen's `(width, height)`.
    /// Returns whether any padding changed, in which case the caller
    /// re-arranges every desktop.
    ///
    /// bspwm: `src/ewmh.c` `ewmh_handle_struts()`. A negative padding
    /// (`bspc config -m M top_padding -10`) is offset by the strut instead
    /// of maxed with it, exactly as there. Like bspwm, nothing ever gives
    /// the space back when the panel goes away.
    pub fn apply_ewmh_struts(&mut self, struts: &EwmhStruts, screen: (i32, i32)) -> bool {
        let (screen_width, screen_height) = screen;
        let mut changed = false;
        let grow = |padding: &mut i32, d: i32| {
            if *padding < 0 {
                *padding += d;
            } else {
                *padding = d.max(*padding);
            }
        };
        for m in &mut self.monitors {
            let rect = m.rectangle;
            let (left, right, top, bottom) = (struts.left as i32, struts.right as i32, struts.top as i32, struts.bottom as i32);
            if rect.x < left
                && left < rect.x + rect.width - 1
                && struts.left_end_y as i32 >= rect.y
                && (struts.left_start_y as i32) < rect.y + rect.height
            {
                grow(&mut m.padding.left, left - rect.x);
                changed = true;
            }
            if rect.x + rect.width > screen_width - right
                && screen_width - right > rect.x
                && struts.right_end_y as i32 >= rect.y
                && (struts.right_start_y as i32) < rect.y + rect.height
            {
                grow(&mut m.padding.right, rect.x + rect.width - screen_width + right);
                changed = true;
            }
            if rect.y < top
                && top < rect.y + rect.height - 1
                && struts.top_end_x as i32 >= rect.x
                && (struts.top_start_x as i32) < rect.x + rect.width
            {
                grow(&mut m.padding.top, top - rect.y);
                changed = true;
            }
            if rect.y + rect.height > screen_height - bottom
                && screen_height - bottom > rect.y
                && struts.bottom_end_x as i32 >= rect.x
                && (struts.bottom_start_x as i32) < rect.x + rect.width
            {
                grow(&mut m.padding.bottom, rect.y + rect.height - screen_height + bottom);
                changed = true;
            }
        }
        changed
    }

    /// The focused monitor, if any.
    pub fn focused_monitor(&self) -> Option<&Monitor> {
        self.focused_monitor.map(|i| &self.monitors[i])
    }

    /// The focused monitor, if any (mutable).
    pub fn focused_monitor_mut(&mut self) -> Option<&mut Monitor> {
        let i = self.focused_monitor?;
        Some(&mut self.monitors[i])
    }

    /// The index of the monitor with the given id, if it exists.
    pub fn monitor_index(&self, id: MonitorId) -> Option<usize> {
        self.monitors.iter().position(|m| m.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Rect;

    fn settings() -> Settings {
        Settings::default()
    }

    fn monitor(id: u32) -> Monitor {
        Monitor::new(
            MonitorId(id),
            None,
            Rect::new(0, 0, 1920, 1080),
            &settings(),
        )
    }

    fn monitor_at(id: u32, rect: Rect) -> Monitor {
        Monitor::new(MonitorId(id), None, rect, &settings())
    }

    #[test]
    fn add_monitor_inserts_in_on_screen_position_order() {
        // Monitor 2 is to the left of monitor 1; add_monitor should walk
        // it into place ahead of 1 rather than leaving it appended after.
        let mut wm = Wm::new(settings());
        wm.add_monitor(monitor_at(1, Rect::new(1920, 0, 1920, 1080)));
        wm.add_monitor(monitor_at(2, Rect::new(0, 0, 1920, 1080)));
        assert_eq!(
            wm.monitors.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![MonitorId(2), MonitorId(1)]
        );
    }

    #[test]
    fn add_monitor_keeps_arrival_order_for_identical_rectangles() {
        let mut wm = Wm::new(settings());
        wm.add_monitor(monitor(1));
        wm.add_monitor(monitor(2));
        assert_eq!(
            wm.monitors.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![MonitorId(1), MonitorId(2)]
        );
    }

    #[test]
    fn add_monitor_follows_focus_when_a_new_monitor_lands_ahead_of_it() {
        let mut wm = Wm::new(settings());
        wm.add_monitor(monitor_at(1, Rect::new(1920, 0, 1920, 1080)));
        assert_eq!(wm.focused_monitor, Some(0));
        wm.add_monitor(monitor_at(2, Rect::new(0, 0, 1920, 1080)));
        // Monitor 1 (still the only focused one) is now at index 1.
        assert_eq!(wm.focused_monitor, Some(1));
    }

    #[test]
    fn reorder_monitor_moves_a_resized_monitor_back_into_position() {
        let mut wm = Wm::new(settings());
        wm.add_monitor(monitor_at(1, Rect::new(0, 0, 1920, 1080)));
        wm.add_monitor(monitor_at(2, Rect::new(1920, 0, 1920, 1080)));
        // Monitor 1 moves to the right of monitor 2.
        wm.monitors[0].rectangle = Rect::new(3840, 0, 1920, 1080);
        let new_index = wm.reorder_monitor(0);
        assert_eq!(new_index, 1);
        assert_eq!(
            wm.monitors.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![MonitorId(2), MonitorId(1)]
        );
    }

    #[test]
    fn add_monitor_focuses_the_first_one_only() {
        let mut wm = Wm::new(settings());
        wm.add_monitor(monitor(1));
        assert_eq!(wm.focused_monitor, Some(0));
        wm.add_monitor(monitor(2));
        assert_eq!(wm.focused_monitor, Some(0));
    }

    #[test]
    fn remove_monitor_refocuses_a_neighbor() {
        let mut wm = Wm::new(settings());
        wm.add_monitor(monitor(1));
        wm.add_monitor(monitor(2));
        wm.add_monitor(monitor(3));
        wm.focus_monitor(2);
        wm.remove_monitor(2);
        assert_eq!(wm.focused_monitor, Some(1));
        assert_eq!(wm.monitors.len(), 2);
    }

    #[test]
    fn remove_last_monitor_leaves_no_focus() {
        let mut wm = Wm::new(settings());
        wm.add_monitor(monitor(1));
        wm.remove_monitor(0);
        assert_eq!(wm.focused_monitor, None);
    }

    #[test]
    fn swap_monitors_follows_focus_to_the_new_index() {
        let mut wm = Wm::new(settings());
        wm.add_monitor(monitor(1));
        wm.add_monitor(monitor(2));
        wm.focus_monitor(0);
        wm.swap_monitors(0, 1);
        assert_eq!(wm.monitors[0].id, MonitorId(2));
        assert_eq!(wm.monitors[1].id, MonitorId(1));
        assert_eq!(wm.focused_monitor, Some(1));
    }

    #[test]
    fn monitor_index_finds_by_id() {
        let mut wm = Wm::new(settings());
        wm.add_monitor(monitor(1));
        wm.add_monitor(monitor(7));
        assert_eq!(wm.monitor_index(MonitorId(7)), Some(1));
        assert_eq!(wm.monitor_index(MonitorId(9)), None);
    }

    fn strut_monitor(id: u32, x: i32, y: i32, w: i32, h: i32) -> Monitor {
        Monitor::new(MonitorId(id), Some("m"), crate::geometry::Rect { x, y, width: w, height: h }, &settings())
    }

    /// A 30px-high bar across the top of a 1000x800 screen.
    fn top_bar() -> EwmhStruts {
        EwmhStruts { top: 30, top_start_x: 0, top_end_x: 999, ..Default::default() }
    }

    #[test]
    fn a_top_strut_pads_the_monitor_it_touches() {
        let mut wm = Wm::new(settings());
        wm.add_monitor(strut_monitor(1, 0, 0, 1000, 800));
        assert!(wm.apply_ewmh_struts(&top_bar(), (1000, 800)));
        assert_eq!(wm.monitors[0].padding.top, 30);
        assert_eq!(wm.monitors[0].padding.bottom, 0);
    }

    #[test]
    fn a_strut_never_shrinks_padding_it_only_maxes_it() {
        let mut wm = Wm::new(settings());
        wm.add_monitor(strut_monitor(1, 0, 0, 1000, 800));
        wm.monitors[0].padding.top = 50;
        wm.apply_ewmh_struts(&top_bar(), (1000, 800));
        assert_eq!(wm.monitors[0].padding.top, 50);
    }

    #[test]
    fn a_negative_padding_is_offset_by_the_strut() {
        let mut wm = Wm::new(settings());
        wm.add_monitor(strut_monitor(1, 0, 0, 1000, 800));
        wm.monitors[0].padding.top = -10;
        wm.apply_ewmh_struts(&top_bar(), (1000, 800));
        assert_eq!(wm.monitors[0].padding.top, 20);
    }

    #[test]
    fn a_strut_along_a_stretch_only_touches_monitors_in_that_stretch() {
        // Two monitors side by side; the bar covers only the left one's x range.
        let mut wm = Wm::new(settings());
        wm.add_monitor(strut_monitor(1, 0, 0, 1000, 800));
        wm.add_monitor(strut_monitor(2, 1000, 0, 1000, 800));
        let bar = EwmhStruts { top: 30, top_start_x: 0, top_end_x: 999, ..Default::default() };
        assert!(wm.apply_ewmh_struts(&bar, (2000, 800)));
        assert_eq!(wm.monitors[0].padding.top, 30);
        assert_eq!(wm.monitors[1].padding.top, 0);
    }

    #[test]
    fn a_bottom_and_a_right_strut_measure_from_the_screen_edge() {
        let mut wm = Wm::new(settings());
        wm.add_monitor(strut_monitor(1, 0, 0, 1000, 800));
        let s = EwmhStruts { bottom: 40, bottom_end_x: 999, right: 20, right_end_y: 799, ..Default::default() };
        assert!(wm.apply_ewmh_struts(&s, (1000, 800)));
        assert_eq!(wm.monitors[0].padding.bottom, 40);
        assert_eq!(wm.monitors[0].padding.right, 20);
    }

    #[test]
    fn an_all_zero_strut_changes_nothing() {
        let mut wm = Wm::new(settings());
        wm.add_monitor(strut_monitor(1, 0, 0, 1000, 800));
        assert!(!wm.apply_ewmh_struts(&EwmhStruts::default(), (1000, 800)));
    }

    #[test]
    fn struts_need_exactly_twelve_cardinals() {
        assert!(EwmhStruts::from_cardinals(&[0; 11]).is_none());
        assert_eq!(EwmhStruts::from_cardinals(&[1, 2, 3, 4, 0, 0, 0, 0, 0, 0, 0, 0]).map(|s| (s.left, s.right, s.top, s.bottom)), Some((1, 2, 3, 4)));
    }
}
