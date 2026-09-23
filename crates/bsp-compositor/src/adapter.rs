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
    /// Windows `bspc node -k` asked to kill; `ipc::apply_pending_kills` does it,
    /// where the display and the X connection are at hand.
    pub pending_kills: Vec<Window>,
    /// Where the pointer was, and the managed window under it, when the current
    /// command started (`ipc::execute_and_broadcast` refreshes it): what the
    /// `pointed` selector resolves against.
    pub pointer: (Option<(i32, i32)>, Option<WindowId>),
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
            pending_kills: Vec::new(),
            pointer: (None, None),
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
        window.user_data().insert_if_missing(|| crate::render::WindowKey(id));
        self.windows.insert(id, window);
        id
    }

    /// Every managed window, shown or not (a window on a desktop that is not
    /// shown is out of the `Space` but still here).
    pub fn windows(&self) -> impl Iterator<Item = &Window> {
        self.windows.values()
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

    /// The mapped window showing `surface`, hidden desktops included (the
    /// space only holds the windows of shown desktops).
    pub fn window_of_surface(&self, surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface) -> Option<Window> {
        use smithay::wayland::seat::WaylandFocus;
        self.windows.values().find(|w| w.wl_surface().as_deref() == Some(surface)).cloned()
    }

    /// The `WindowId` for a Smithay `Window`, if it is mapped.
    pub fn id_of(&self, window: &Window) -> Option<WindowId> {
        self.windows
            .iter()
            .find(|(_, w)| *w == window)
            .map(|(id, _)| *id)
    }

    /// A window's `(class, instance)` as recorded by [`WindowAdapter::set_app_id`]
    /// (empty strings if unknown).
    pub fn class_of(&self, id: WindowId) -> (String, String) {
        self.classes.get(&id).cloned().unwrap_or_default()
    }

    /// Records a window's class and instance names: an X11 window's
    /// `WM_CLASS` pair, which `bspc rule` matches separately
    /// (`docs/design.md`, Compatibility).
    pub fn set_class(&mut self, id: WindowId, class: &str, instance: &str) {
        self.classes
            .insert(id, (class.to_string(), instance.to_string()));
    }

    /// Records a native Wayland window's class/instance name from its
    /// `xdg_toplevel` `app_id`; there is no separate instance name, so both
    /// are `app_id` (see [`WindowAdapter::set_class`] for X11 windows).
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
            } else if let Some(x11) = w.x11_surface() {
                if let Err(err) = x11.close() {
                    tracing::debug!("cannot close an X11 window: {err}");
                }
            }
        }
    }

    fn pointer_state(&self) -> (Option<(i32, i32)>, Option<WindowId>) {
        self.pointer
    }

    fn kill_window(&mut self, window: WindowId) {
        if let Some(w) = self.windows.get(&window) {
            self.pending_kills.push(w.clone());
        }
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
