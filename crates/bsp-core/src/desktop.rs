//! A desktop: one [`Tree`] plus the layout, padding and gap it is arranged
//! with.
//!
//! bspwm: `src/types.h` `desktop_t`, `src/desktop.c`. Functions tied to
//! history, EWMH, the stacking list, or focusing a different monitor are
//! left out of the core (see the note on each function below); they need
//! the monitor/adapter layer added later.

use crate::geometry::Padding;
use crate::id::DesktopId;
use crate::settings::Settings;
pub use crate::tree::Layout;
use crate::tree::Tree;

const DEFAULT_DESK_NAME: &str = "Desktop";

/// A desktop: a name, a layout, padding/gap overrides, and the tree of
/// windows tiled or floating on it.
///
/// bspwm: `src/types.h` `desktop_t`.
#[derive(Debug, Clone)]
pub struct Desktop {
    /// Stable identifier for `bsp-ipc` (IPC) to reference.
    pub id: DesktopId,
    /// Display name (`bspc desktop -n`).
    pub name: String,
    /// Layout actually in effect (can differ from `user_layout` when
    /// `single_monocle` overrides it).
    pub layout: Layout,
    /// Layout the user last asked for with `bspc desktop -l`.
    pub user_layout: Layout,
    /// Padding around this desktop's usable area, added to its monitor's.
    pub padding: Padding,
    /// Gap between tiled windows, overriding the global default.
    pub window_gap: i32,
    /// Border width applied to new clients on this desktop.
    pub border_width: i32,
    /// The window tree.
    pub tree: Tree,
}

impl Desktop {
    /// Creates an empty desktop.
    ///
    /// bspwm: `src/desktop.c` `make_desktop()`. `single_monocle` choosing
    /// the initial layout is left to the caller (`crate::monitor::Monitor`
    /// knows the desktop count, `make_desktop` alone does not).
    pub fn new(id: DesktopId, name: Option<&str>, settings: &Settings) -> Self {
        Self {
            id,
            name: name.unwrap_or(DEFAULT_DESK_NAME).to_string(),
            layout: Layout::Tiled,
            user_layout: Layout::Tiled,
            padding: settings.padding,
            window_gap: settings.window_gap,
            border_width: settings.border_width,
            tree: Tree::new(),
        }
    }

    /// Renames the desktop.
    ///
    /// bspwm: `src/desktop.c` `rename_desktop()`, minus EWMH and the
    /// `subscribe` report.
    pub fn rename(&mut self, name: &str) {
        self.name = name.to_string();
    }

    /// Sets the desktop's layout. If `user` is `true`, this also updates
    /// `user_layout`, and `single_monocle` can veto it back to monocle
    /// when the desktop holds more than one tiled window — the caller
    /// (which knows `settings.single_monocle` and the desktop's tiled
    /// count) passes that decision in as `effective`. Returns `true` if
    /// the effective layout changed.
    ///
    /// bspwm: `src/desktop.c` `set_layout()`, minus presel-feedback
    /// visibility and the `subscribe` report.
    pub fn set_layout(&mut self, layout: Layout, user: bool, effective: Layout) -> bool {
        if user {
            if self.user_layout == layout && self.layout == effective {
                return false;
            }
            self.user_layout = layout;
        } else if self.layout == layout {
            return false;
        }

        let old = self.layout;
        self.layout = if user { effective } else { layout };
        self.layout != old
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_desktop_inherits_settings_defaults() {
        let settings = Settings::default();
        let d = Desktop::new(DesktopId(1), None, &settings);
        assert_eq!(d.name, "Desktop");
        assert_eq!(d.layout, Layout::Tiled);
        assert_eq!(d.user_layout, Layout::Tiled);
        assert_eq!(d.window_gap, settings.window_gap);
        assert_eq!(d.border_width, settings.border_width);
        assert!(d.tree.root.is_none());
    }

    #[test]
    fn rename_changes_name_only() {
        let settings = Settings::default();
        let mut d = Desktop::new(DesktopId(1), Some("web"), &settings);
        d.rename("mail");
        assert_eq!(d.name, "mail");
    }

    #[test]
    fn set_layout_user_true_updates_both_when_not_overridden() {
        let settings = Settings::default();
        let mut d = Desktop::new(DesktopId(1), None, &settings);
        assert!(d.set_layout(Layout::Monocle, true, Layout::Monocle));
        assert_eq!(d.user_layout, Layout::Monocle);
        assert_eq!(d.layout, Layout::Monocle);
    }

    #[test]
    fn set_layout_no_op_when_unchanged() {
        let settings = Settings::default();
        let mut d = Desktop::new(DesktopId(1), None, &settings);
        assert!(!d.set_layout(Layout::Tiled, true, Layout::Tiled));
    }
}
