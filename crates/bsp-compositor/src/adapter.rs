//! The `WindowId` ↔ Smithay `Window` map, and this crate's
//! `bsp_ipc::adapter::Adapter` implementation.
//!
//! bspwm reuses one X11 window id both as the node's identity and the
//! client's identity; `bsp-core` keeps `WindowId` separate on purpose
//! (`crates/bsp-core/src/id.rs`), so this adapter is where the two
//! meet: it hands out fresh `WindowId`s for newly mapped toplevels and
//! keeps the map Smithay's callbacks and `bsp-ipc`'s executor both need.

use std::collections::HashMap;

use bsp_core::id::WindowId;
use smithay::desktop::Window;

/// Native Wayland windows get ids from the top half of the `u32` range,
/// which XWayland (real 32-bit X11 resource ids, allocated from a much
/// lower base by the X server) never reaches in practice — see
/// `docs/design.md`, Compatibility: "native Wayland windows get IDs from
/// a range X11 never uses".
const FIRST_WAYLAND_WINDOW_ID: u32 = 0x8000_0000;

/// Maps `bsp-core`'s `WindowId` to the Smithay `Window` it names, and
/// supplies the class/instance metadata and close/kill actions
/// `bsp-ipc::exec` needs through the `Adapter` trait.
pub struct WindowAdapter {
    next_id: u32,
    windows: HashMap<WindowId, Window>,
    classes: HashMap<WindowId, (String, String)>,
    /// Outputs/input devices as `bspc output`/`bspc input` see them.
    pub hw: crate::hardware::HwModel,
}

impl WindowAdapter {
    /// An adapter with no windows yet.
    pub fn new() -> Self {
        Self {
            next_id: FIRST_WAYLAND_WINDOW_ID,
            windows: HashMap::new(),
            classes: HashMap::new(),
            hw: crate::hardware::HwModel {
                // Matches `State::new`'s `seat.add_keyboard(_, 200, 25)`.
                repeat: (25, 200),
                ..Default::default()
            },
        }
    }

    /// Registers a newly mapped window, allocating a fresh `WindowId` for
    /// it.
    pub fn insert(&mut self, window: Window) -> WindowId {
        let id = WindowId(self.next_id);
        self.next_id += 1;
        self.windows.insert(id, window);
        id
    }

    /// Forgets a destroyed window's mapping.
    pub fn remove(&mut self, id: WindowId) -> Option<Window> {
        self.classes.remove(&id);
        self.windows.remove(&id)
    }

    /// The Smithay `Window` for `id`, if it is still mapped.
    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.windows.get(&id)
    }

    /// The `WindowId` for a Smithay `Window`, if it is mapped.
    pub fn id_of(&self, window: &Window) -> Option<WindowId> {
        self.windows
            .iter()
            .find(|(_, w)| *w == window)
            .map(|(id, _)| *id)
    }

    /// Records a window's class/instance name (from the `xdg_toplevel`
    /// `app_id`; native Wayland windows have no separate instance name,
    /// so both are set to `app_id`, matching how XWayland windows keep
    /// distinct class/instance — `docs/design.md`, Compatibility).
    pub fn set_app_id(&mut self, id: WindowId, app_id: &str) {
        self.classes
            .insert(id, (app_id.to_string(), app_id.to_string()));
    }
}

impl Default for WindowAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl bsp_ipc::adapter::Adapter for WindowAdapter {
    fn window_class(&self, window: WindowId) -> (String, String) {
        self.classes.get(&window).cloned().unwrap_or_default()
    }

    fn close_window(&mut self, window: WindowId) {
        if let Some(w) = self.windows.get(&window) {
            if let Some(toplevel) = w.toplevel() {
                toplevel.send_close();
            }
        }
    }

    fn kill_window(&mut self, window: WindowId) {
        // No signal-based kill exists for a Wayland client (unlike
        // bspwm's `XKillClient` — there is no display-server-mediated
        // forced termination in the Wayland protocol): ask it to close,
        // same as `close_window`. A real force-kill needs the client's
        // pid (from `wl_client_get_credentials`), not implemented yet.
        self.close_window(window);
    }

    fn output_names(&self) -> Vec<String> {
        self.hw.output_names()
    }

    fn output_settings(&self, name: &str) -> Option<String> {
        self.hw.output_settings(name)
    }

    fn set_output(&mut self, name: &str, action: &bsp_ipc::command::OutputAction) -> Result<(), String> {
        self.hw.set_output(name, action)
    }

    fn input_names(&self) -> Vec<String> {
        self.hw.input_names()
    }

    fn input_settings(&self, device: &str) -> Option<String> {
        self.hw.input_settings(device)
    }

    fn set_input(&mut self, device: &str, action: &bsp_ipc::command::InputAction) -> Result<(), String> {
        self.hw.set_input(device, action)
    }
}

#[cfg(test)]
mod tests {
    // `WindowAdapter` cannot be unit-tested without a live Wayland
    // display to construct a `smithay::desktop::Window` from (it wraps a
    // real `ToplevelSurface`/`WlSurface` resource) — exercised instead by
    // actually running the compositor (`docs/bsp-compositor.md`).
}
