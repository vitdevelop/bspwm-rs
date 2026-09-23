//! A monitor: a rectangle, padding/gap overrides, and the desktops on it.
//!
//! bspwm: `src/types.h` `monitor_t`, `src/monitor.c`. As in `desktop.c`,
//! functions tied to RandR, EWMH, the stacking list or a global monitor
//! order list are left out of the core; hotplug and physical output
//! ordering are the hardware backend (`docs/design.md` roadmap).

use crate::desktop::Desktop;
use crate::geometry::{Padding, Rect};
use crate::id::MonitorId;
use crate::settings::Settings;
use crate::tree::{Layout, LayoutOptions};

const DEFAULT_MON_NAME: &str = "MONITOR";

/// A monitor: a name, rectangle, padding/gap overrides, and an ordered
/// list of desktops, one of which is focused.
///
/// bspwm: `src/types.h` `monitor_t`. `desk`/`desk_head`/`desk_tail`'s
/// linked list becomes a `Vec` plus a focused index; `wired`/`randr_id`
/// (RandR bookkeeping) are left for the hardware backend.
#[derive(Debug, Clone)]
pub struct Monitor {
    /// Stable identifier for `bsp-ipc` (IPC) to reference.
    pub id: MonitorId,
    /// Display name; on real hardware, the DRM connector name
    /// (`docs/design.md`, Compatibility).
    pub name: String,
    /// The monitor's full rectangle.
    pub rectangle: Rect,
    /// Padding around this monitor's usable area.
    pub padding: Padding,
    /// Space reserved by panels (Wayland layer-shell exclusive zones,
    /// `docs/design.md` Compatibility: bspwm's `_NET_WM_STRUT` analogue),
    /// added to `padding` when laying out. Owned by the compositor, not
    /// `bspc config` — so a configured padding and a panel's strut
    /// never overwrite each other.
    pub struts: Padding,
    /// Default gap between tiled windows for desktops on this monitor.
    pub window_gap: i32,
    /// Default border width for desktops on this monitor.
    pub border_width: i32,
    /// This monitor's desktops, in display order.
    pub desktops: Vec<Desktop>,
    /// Index into `desktops` of the focused desktop, or `None` if the
    /// monitor holds no desktops.
    pub focused: Option<usize>,
    /// Whether this is the only monitor (kept up to date by `Wm::add_monitor`
    /// and `Wm::remove_monitor`); `borderless_singleton` applies only then.
    ///
    /// bspwm: `apply_layout()`'s `!m->prev && !m->next`.
    pub sole: bool,
    /// Whether the monitor is a virtual (headless) output: it does not count
    /// against `sole` on the real screens.
    pub virtual_output: bool,
    /// Whether an output shows this monitor. An unplugged output leaves its
    /// monitor (desktops and windows included) in place, unwired, until the
    /// output comes back, unless `remove_unplugged_monitors` is set.
    ///
    /// bspwm: `src/types.h` `monitor_t.wired`.
    pub wired: bool,
}

impl Monitor {
    /// The sticky nodes of the monitor (they are always on its shown desktop).
    ///
    /// bspwm: `monitor_t.sticky_count`, kept as a counter there.
    pub fn sticky_count(&self) -> u32 {
        self.desktops.iter().map(|d| d.tree.sticky_count(d.tree.root)).sum()
    }

    /// Creates a monitor with no desktops.
    ///
    /// bspwm: `src/monitor.c` `make_monitor()`, minus the RandR/root
    /// window fields.
    pub fn new(id: MonitorId, name: Option<&str>, rectangle: Rect, settings: &Settings) -> Self {
        Self {
            id,
            name: name.unwrap_or(DEFAULT_MON_NAME).to_string(),
            rectangle,
            padding: settings.padding,
            struts: Padding::default(),
            window_gap: settings.window_gap,
            border_width: settings.border_width,
            desktops: Vec::new(),
            focused: None,
            sole: true,
            virtual_output: false,
            wired: true,
        }
    }

    /// Renames the monitor.
    ///
    /// bspwm: `src/monitor.c` `rename_monitor()`, minus the X11 window
    /// name property.
    pub fn rename(&mut self, name: &str) {
        self.name = name.to_string();
    }

    /// Appends a desktop, inheriting this monitor's gap and border width
    /// (bspwm does the same in `add_desktop()` so that a desktop created
    /// after `bspc config -m <mon> window_gap <n>` picks up the override).
    /// Focuses it if it is the monitor's first desktop.
    ///
    /// bspwm: `src/desktop.c` `add_desktop()`, minus EWMH and the
    /// `subscribe` report.
    pub fn add_desktop(&mut self, mut d: Desktop) {
        d.border_width = self.border_width;
        d.window_gap = self.window_gap;
        self.insert_desktop(d);
    }

    /// Appends a desktop as it is, keeping its own gap and border width, and
    /// focuses it if it is the monitor's first desktop. This is what moving an
    /// existing desktop from another monitor uses.
    ///
    /// bspwm: `src/desktop.c` `insert_desktop()` (called by `transfer_desktop()`;
    /// only `add_desktop()`, for a new desktop, overwrites the gap and border).
    pub fn insert_desktop(&mut self, d: Desktop) {
        self.desktops.push(d);
        if self.focused.is_none() {
            self.focused = Some(0);
        }
    }

    /// Removes the desktop at `index`, returning it. Refocuses onto a
    /// neighbor if the removed desktop was focused.
    ///
    /// bspwm: `src/desktop.c` `remove_desktop()`/`unlink_desktop()`, minus
    /// `remove_node` on the desktop's root (the caller must empty the
    /// desktop's tree first — bspwm refuses to destroy windows silently)
    /// and history/EWMH/refocusing-the-window side effects.
    pub fn remove_desktop(&mut self, index: usize) -> Desktop {
        let d = self.desktops.remove(index);
        self.focused = match self.focused {
            Some(f) if self.desktops.is_empty() => {
                let _ = f;
                None
            }
            Some(f) if f > index => Some(f - 1),
            Some(f) if f == index => Some(f.min(self.desktops.len() - 1)),
            other => other,
        };
        d
    }

    /// Focuses the desktop at `index`. Returns `false` (as bspwm's
    /// `activate_desktop()` does for `d == m->desk`) if it is already
    /// focused.
    ///
    /// bspwm: `src/desktop.c` `activate_desktop()`, minus sticky-node
    /// transfer, history and the `subscribe` report.
    pub fn activate_desktop(&mut self, index: usize) -> bool {
        if self.focused == Some(index) {
            return false;
        }
        self.focused = Some(index);
        true
    }

    /// Swaps the desktops at `i` and `j` (within this monitor), keeping
    /// the focused index pointed at whichever desktop was focused before
    /// the swap.
    ///
    /// bspwm: `src/desktop.c` `swap_desktops()`, restricted to a single
    /// monitor. Moving a desktop to a *different* monitor is
    /// `Vec::remove` from one `Monitor::desktops` followed by
    /// `Monitor::add_desktop` (or `insert`) on the other, which needs no
    /// dedicated method.
    pub fn swap_desktops(&mut self, i: usize, j: usize) {
        if i == j {
            return;
        }
        self.desktops.swap(i, j);
        self.focused = match self.focused {
            Some(f) if f == i => Some(j),
            Some(f) if f == j => Some(i),
            other => other,
        };
    }

    /// Computes the starting rectangle for the desktop at `index` (monitor
    /// rectangle, shrunk by monitor and desktop padding, then by the
    /// window gap, matching monocle padding/gaplessness) and lays out its
    /// tree into it.
    ///
    /// bspwm: `src/tree.c` `arrange()`.
    pub fn arrange(&mut self, index: usize, settings: &Settings) {
        let sole = self.sole;
        self.desktops[index].apply_single_monocle(settings.single_monocle);
        let m_rect = self.rectangle;
        let m_padding = Padding {
            top: self.padding.top + self.struts.top,
            right: self.padding.right + self.struts.right,
            bottom: self.padding.bottom + self.struts.bottom,
            left: self.padding.left + self.struts.left,
        };
        let d = &mut self.desktops[index];

        if d.tree.root.is_none() {
            return;
        }

        let mut rect = m_rect;
        rect.x += m_padding.left + d.padding.left;
        rect.y += m_padding.top + d.padding.top;
        rect.width -= m_padding.left + d.padding.left + d.padding.right + m_padding.right;
        rect.height -= m_padding.top + d.padding.top + d.padding.bottom + m_padding.bottom;

        if d.layout == Layout::Monocle {
            rect.x += settings.monocle_padding.left;
            rect.y += settings.monocle_padding.top;
            rect.width -= settings.monocle_padding.left + settings.monocle_padding.right;
            rect.height -= settings.monocle_padding.top + settings.monocle_padding.bottom;
        }

        if !settings.gapless_monocle || d.layout != Layout::Monocle {
            rect.x += d.window_gap;
            rect.y += d.window_gap;
            rect.width -= d.window_gap;
            rect.height -= d.window_gap;
        }

        let options = LayoutOptions {
            gapless_monocle: settings.gapless_monocle,
            borderless_monocle: settings.borderless_monocle,
            borderless_singleton: settings.borderless_singleton && sole,
            center_pseudo_tiled: settings.center_pseudo_tiled,
        };
        d.tree
            .apply_layout(d.tree.root, rect, d.window_gap, d.layout, m_rect, options);
    }
}

/// Proportionally repositions every floating client under `n` (via its
/// tree) when its bounding rectangle changes from `rs` to `rd` — e.g. a
/// node moving to a monitor of a different size, or a monitor being
/// resized. A client's position within `rs` (0.0 at `rs`'s near edge, 1.0
/// at its far edge) is preserved in `rd`; a client's own rectangle is
/// clipped to fit inside `rs` first (and the clip undone after) so the
/// fit is well defined even if the client hung outside `rs`'s bounds.
///
/// bspwm: `src/monitor.c` `adapt_geometry()`.
pub fn adapt_geometry(
    tree: &mut crate::tree::Tree,
    root: Option<crate::id::NodeId>,
    rs: Rect,
    rd: Rect,
) {
    let mut f = tree.first_extrema(root);
    while let Some(n) = f {
        if let Some(client) = tree.node(n).client.clone() {
            let mut fr = client.floating_rectangle;

            let left_adjust = (rs.x - fr.x).max(0);
            let top_adjust = (rs.y - fr.y).max(0);
            let right_adjust = (fr.right() - rs.right()).max(0);
            let bottom_adjust = (fr.bottom() - rs.bottom()).max(0);
            fr.x += left_adjust;
            fr.y += top_adjust;
            fr.width -= left_adjust + right_adjust;
            fr.height -= top_adjust + bottom_adjust;

            let dx_s = fr.x - rs.x;
            let dy_s = fr.y - rs.y;

            let deno_x = rs.width - fr.width;
            let deno_y = rs.height - fr.height;
            let dx_d = if deno_x == 0 {
                0
            } else {
                (dx_s * (rd.width - fr.width)) / deno_x
            };
            let dy_d = if deno_y == 0 {
                0
            } else {
                (dy_s * (rd.height - fr.height)) / deno_y
            };

            fr.width += left_adjust + right_adjust;
            fr.height += top_adjust + bottom_adjust;
            fr.x = rd.x + dx_d - left_adjust;
            fr.y = rd.y + dy_d - top_adjust;

            if let Some(client) = tree.node_mut(n).client.as_mut() {
                client.floating_rectangle = fr;
            }
        }
        f = tree.next_leaf(Some(n), root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::DesktopId;

    fn settings() -> Settings {
        Settings::default()
    }

    #[test]
    fn new_monitor_has_no_desktops_and_no_focus() {
        let m = Monitor::new(MonitorId(1), None, Rect::new(0, 0, 1920, 1080), &settings());
        assert_eq!(m.name, "MONITOR");
        assert!(m.desktops.is_empty());
        assert_eq!(m.focused, None);
    }

    #[test]
    fn add_desktop_focuses_the_first_one_only() {
        let settings = settings();
        let mut m = Monitor::new(MonitorId(1), None, Rect::new(0, 0, 1920, 1080), &settings);
        m.add_desktop(Desktop::new(DesktopId(1), Some("I"), &settings));
        assert_eq!(m.focused, Some(0));
        m.add_desktop(Desktop::new(DesktopId(2), Some("II"), &settings));
        assert_eq!(m.focused, Some(0));
        assert_eq!(m.desktops.len(), 2);
    }

    #[test]
    fn add_desktop_inherits_monitor_gap_and_border_width() {
        let mut settings = settings();
        settings.window_gap = 10;
        settings.border_width = 3;
        let mut m = Monitor::new(MonitorId(1), None, Rect::new(0, 0, 1920, 1080), &settings);
        m.window_gap = 99;
        m.border_width = 4;
        m.add_desktop(Desktop::new(DesktopId(1), None, &settings));
        assert_eq!(m.desktops[0].window_gap, 99);
        assert_eq!(m.desktops[0].border_width, 4);
    }

    #[test]
    fn insert_desktop_keeps_the_desktops_own_gap_and_border_width() {
        // bspwm: `transfer_desktop()` calls `insert_desktop()`, which unlike
        // `add_desktop()` leaves the desktop's own values alone.
        let settings = settings();
        let mut m = Monitor::new(MonitorId(1), None, Rect::new(0, 0, 1920, 1080), &settings);
        m.window_gap = 99;
        m.border_width = 4;
        let mut d = Desktop::new(DesktopId(1), None, &settings);
        d.window_gap = 7;
        d.border_width = 2;
        m.insert_desktop(d);
        assert_eq!((m.desktops[0].window_gap, m.desktops[0].border_width), (7, 2));
        assert_eq!(m.focused, Some(0));
    }

    #[test]
    fn single_monocle_follows_the_tiled_window_count_when_arranging() {
        // bspwm: `single_monocle` blocks in `manage_window()`/`remove_node()`/`set_state()`.
        let mut settings = settings();
        settings.single_monocle = true;
        let mut m = Monitor::new(MonitorId(1), None, Rect::new(0, 0, 800, 600), &settings);
        m.add_desktop(Desktop::new(DesktopId(1), None, &settings));
        let insert = |m: &mut Monitor, w: u32, anchor| {
            let t = &mut m.desktops[0].tree;
            let n = t.new_client_node(&settings, crate::node::Client::new(crate::id::WindowId(w), 1));
            t.insert_node(&settings, n, anchor);
            n
        };
        let a = insert(&mut m, 1, None);
        m.arrange(0, &settings);
        assert_eq!(m.desktops[0].layout, Layout::Monocle);
        let b = insert(&mut m, 2, Some(a));
        m.arrange(0, &settings);
        assert_eq!(m.desktops[0].layout, Layout::Tiled, "a second window restores the user's layout");
        m.desktops[0].tree.remove_node(&settings, b);
        m.arrange(0, &settings);
        assert_eq!(m.desktops[0].layout, Layout::Monocle);
        // Off: the layout is left alone.
        settings.single_monocle = false;
        m.desktops[0].layout = Layout::Tiled;
        m.arrange(0, &settings);
        assert_eq!(m.desktops[0].layout, Layout::Tiled);
    }

    #[test]
    fn remove_desktop_refocuses_a_neighbor() {
        let settings = settings();
        let mut m = Monitor::new(MonitorId(1), None, Rect::new(0, 0, 1920, 1080), &settings);
        m.add_desktop(Desktop::new(DesktopId(1), Some("I"), &settings));
        m.add_desktop(Desktop::new(DesktopId(2), Some("II"), &settings));
        m.add_desktop(Desktop::new(DesktopId(3), Some("III"), &settings));
        m.activate_desktop(2);
        m.remove_desktop(2);
        assert_eq!(m.focused, Some(1));
        assert_eq!(m.desktops.len(), 2);
    }

    #[test]
    fn remove_last_desktop_leaves_no_focus() {
        let settings = settings();
        let mut m = Monitor::new(MonitorId(1), None, Rect::new(0, 0, 1920, 1080), &settings);
        m.add_desktop(Desktop::new(DesktopId(1), None, &settings));
        m.remove_desktop(0);
        assert_eq!(m.focused, None);
    }

    #[test]
    fn activate_desktop_returns_false_when_already_focused() {
        let settings = settings();
        let mut m = Monitor::new(MonitorId(1), None, Rect::new(0, 0, 1920, 1080), &settings);
        m.add_desktop(Desktop::new(DesktopId(1), None, &settings));
        assert!(!m.activate_desktop(0));
    }

    #[test]
    fn swap_desktops_follows_focus_to_the_new_index() {
        let settings = settings();
        let mut m = Monitor::new(MonitorId(1), None, Rect::new(0, 0, 1920, 1080), &settings);
        m.add_desktop(Desktop::new(DesktopId(1), Some("I"), &settings));
        m.add_desktop(Desktop::new(DesktopId(2), Some("II"), &settings));
        m.activate_desktop(0);
        m.swap_desktops(0, 1);
        assert_eq!(m.desktops[0].name, "II");
        assert_eq!(m.desktops[1].name, "I");
        assert_eq!(m.focused, Some(1));
    }

    #[test]
    fn arrange_fills_the_monitor_minus_gap_for_a_single_window() {
        let settings = settings();
        let mut m = Monitor::new(MonitorId(1), None, Rect::new(0, 0, 800, 600), &settings);
        let mut d = Desktop::new(DesktopId(1), None, &settings);
        let client = crate::node::Client::new(crate::id::WindowId(1), settings.border_width);
        let n = d.tree.new_client_node(&settings, client);
        d.tree.insert_node(&settings, n, None);
        m.add_desktop(d);

        m.arrange(0, &settings);

        let root = m.desktops[0].tree.root.unwrap();
        let r = m.desktops[0].tree.node(root).rect;
        // bspwm: arrange() adds the window_gap once on x/y and once on
        // width/height (see src/tree.c arrange()); a lone window's slot is
        // the full monitor minus that single gap on every edge.
        let gap = settings.window_gap;
        assert_eq!(r, Rect::new(gap, gap, 800 - gap, 600 - gap));
    }

    #[test]
    fn arrange_reserves_panel_struts_on_top_of_padding() {
        let settings = settings();
        let mut m = Monitor::new(MonitorId(1), None, Rect::new(0, 0, 800, 600), &settings);
        m.padding = Padding { top: 5, right: 0, bottom: 0, left: 0 };
        // A 30 px top panel and a 40 px left dock.
        m.struts = Padding { top: 30, right: 0, bottom: 0, left: 40 };
        let mut d = Desktop::new(DesktopId(1), None, &settings);
        let client = crate::node::Client::new(crate::id::WindowId(1), settings.border_width);
        let n = d.tree.new_client_node(&settings, client);
        d.tree.insert_node(&settings, n, None);
        m.add_desktop(d);

        m.arrange(0, &settings);

        let root = m.desktops[0].tree.root.unwrap();
        let r = m.desktops[0].tree.node(root).rect;
        let gap = settings.window_gap;
        assert_eq!(r, Rect::new(40 + gap, 35 + gap, 800 - 40 - gap, 600 - 35 - gap));
    }

    #[test]
    fn adapt_geometry_preserves_relative_position() {
        let mut tree = crate::tree::Tree::new();
        let mut client = crate::node::Client::new(crate::id::WindowId(1), 1);
        client.state = crate::node::ClientState::Floating;
        client.floating_rectangle = Rect::new(100, 100, 200, 100);
        let n = tree.new_client_node(&settings(), client);
        tree.insert_node(&settings(), n, None);

        let rs = Rect::new(0, 0, 1000, 1000);
        let rd = Rect::new(0, 0, 2000, 500);
        let root = tree.root;
        adapt_geometry(&mut tree, root, rs, rd);

        let fr = tree.node(n).client.as_ref().unwrap().floating_rectangle;
        // bspwm: src/monitor.c adapt_geometry(). The client's near-edge
        // offset (100) as a fraction of its travel range in the source
        // (1000 - 200 = 800) is 0.125; applied to the destination's travel
        // range (2000 - 200 = 1800) that is floor(0.125 * 1800) = 225.
        // Likewise for y: 100 / (1000 - 100) applied to (500 - 100) is
        // floor(100 * 400 / 900) = 44.
        assert_eq!(fr.x, 225);
        assert_eq!(fr.y, 44);
        assert_eq!(fr.width, 200);
        assert_eq!(fr.height, 100);
    }
}
