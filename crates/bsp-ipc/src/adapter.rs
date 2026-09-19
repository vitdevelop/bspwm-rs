//! The interface [`crate::exec`] needs from whatever manages real windows:
//! `bsp-compositor` eventually, [`FakeAdapter`] for tests now (the "fake
//! adapter" the the IPC roadmap names, `docs/design.md`).
//!
//! `bsp-core::node::Client` deliberately does not store a window's class
//! and instance name (`docs/design.md`: "the adapter keeps the map from
//! `WindowId` to Smithay windows"), and closing/killing a window is an
//! action on the real window system that `bsp-ipc` cannot perform itself
//! — both need this trait.

use bsp_core::id::WindowId;

use crate::command::{InputAction, OutputAction};

/// What the executor needs from the window-system adapter.
pub trait Adapter {
    /// The class and instance name of `window`, for `query -T`'s JSON and
    /// the `same_class` selector modifier. Returns empty strings for an
    /// unknown window rather than failing: every caller treats "unknown"
    /// the same as "empty" (bspwm always has this data by the time a
    /// window is manageable, so the empty case does not arise there).
    fn window_class(&self, window: WindowId) -> (String, String);

    /// Asks `window` to close itself (bspwm: `close_node()`, `WM_DELETE_WINDOW`
    /// on X11; the Wayland equivalent is `xdg_toplevel::close`).
    fn close_window(&mut self, window: WindowId);

    /// Forcibly terminates `window`'s client (bspwm: `kill_node()`,
    /// `xcb_kill_client`; on Wayland this is killing the client process).
    fn kill_window(&mut self, window: WindowId);

    /// Real hardware output configuration (`bspc output`,
    /// `docs/design.md`'s "Configuration beyond bspwm" — not a bspwm
    /// command, `xrandr`'s replacement). Every method below defaults to
    /// "no known outputs"/"not supported", so a backend that hasn't
    /// implemented real hardware output config yet (the nested winit
    /// backend today) needs no changes to keep compiling; the eventual
    /// DRM backend (hardware backend) overrides them for real.
    ///
    /// Every known output's name, in display order.
    fn output_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// `name`'s current settings, formatted for `Reply::Ok`, or `None`
    /// if `name` names no known output.
    fn output_settings(&self, name: &str) -> Option<String> {
        let _ = name;
        None
    }

    /// Applies one `bspc output` action to the named output. `Err`'s
    /// string becomes the `Reply::Fail` message verbatim (already
    /// `\n`-terminated, matching this crate's other failure messages).
    fn set_output(&mut self, name: &str, action: &OutputAction) -> Result<(), String> {
        let _ = (name, action);
        Err("output: not supported (no hardware output backend yet).\n".to_string())
    }

    /// Real input device configuration (`bspc input`,
    /// `docs/design.md`'s "Configuration beyond bspwm" — not a bspwm
    /// command, `setxkbmap`/`xset r rate`/`xinput`'s replacement). Same
    /// "defaults to not supported" reasoning as the output methods above.
    ///
    /// Every known input device's name, in no particular order.
    fn input_names(&self) -> Vec<String> {
        Vec::new()
    }

    /// `device`'s current settings, formatted for `Reply::Ok`, or `None`
    /// if `device` names no known input device.
    fn input_settings(&self, device: &str) -> Option<String> {
        let _ = device;
        None
    }

    /// Applies one `bspc input` action to the named device. `Err`'s
    /// string becomes the `Reply::Fail` message verbatim.
    fn set_input(&mut self, device: &str, action: &InputAction) -> Result<(), String> {
        let _ = (device, action);
        Err("input: not supported (no hardware input backend yet).\n".to_string())
    }
}

/// A recording, in-memory [`Adapter`] for tests: no real window system,
/// just a class/instance lookup table and a record of what was closed or
/// killed.
#[derive(Debug, Clone, Default)]
pub struct FakeAdapter {
    /// Class/instance names to answer [`Adapter::window_class`] with.
    pub classes: std::collections::HashMap<WindowId, (String, String)>,
    /// Windows [`Adapter::close_window`] was called with, in call order.
    pub closed: Vec<WindowId>,
    /// Windows [`Adapter::kill_window`] was called with, in call order.
    pub killed: Vec<WindowId>,
}

impl FakeAdapter {
    /// An adapter with no windows registered yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `window`'s class/instance name for
    /// [`Adapter::window_class`] to return.
    pub fn set_class(&mut self, window: WindowId, class_name: &str, instance_name: &str) {
        self.classes
            .insert(window, (class_name.to_string(), instance_name.to_string()));
    }
}

impl Adapter for FakeAdapter {
    fn window_class(&self, window: WindowId) -> (String, String) {
        self.classes.get(&window).cloned().unwrap_or_default()
    }

    fn close_window(&mut self, window: WindowId) {
        self.closed.push(window);
    }

    fn kill_window(&mut self, window: WindowId) {
        self.killed.push(window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_adapter_returns_registered_class() {
        let mut a = FakeAdapter::new();
        a.set_class(WindowId(1), "Firefox", "Navigator");
        assert_eq!(
            a.window_class(WindowId(1)),
            ("Firefox".to_string(), "Navigator".to_string())
        );
    }

    #[test]
    fn fake_adapter_returns_empty_for_unknown_window() {
        let a = FakeAdapter::new();
        assert_eq!(a.window_class(WindowId(9)), (String::new(), String::new()));
    }

    #[test]
    fn fake_adapter_records_close_and_kill() {
        let mut a = FakeAdapter::new();
        a.close_window(WindowId(1));
        a.kill_window(WindowId(2));
        assert_eq!(a.closed, vec![WindowId(1)]);
        assert_eq!(a.killed, vec![WindowId(2)]);
    }
}
