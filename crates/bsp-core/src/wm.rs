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

    /// Appends a monitor and returns its index. Focuses it if it is the
    /// first monitor.
    ///
    /// bspwm: `src/monitor.c` `add_monitor()`, minus RandR/EWMH bookkeeping.
    pub fn add_monitor(&mut self, m: Monitor) -> usize {
        self.monitors.push(m);
        let index = self.monitors.len() - 1;
        if self.focused_monitor.is_none() {
            self.focused_monitor = Some(index);
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
}
