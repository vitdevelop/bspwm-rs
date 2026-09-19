//! Identifiers, kept deliberately distinct from one another.
//!
//! bspwm reuses a single X window id both as the node's identity and the
//! client's identity. `bsp-core` keeps three separate id spaces instead
//! (see `docs/design.md`, Architecture: "`bsp-core` uses its own
//! `WindowId(u32)` and `Rect`; the adapter keeps the map from `WindowId` to
//! Smithay windows"):
//!
//! - [`NodeId`] names a slot in a [`crate::tree::Tree`]'s arena. It is
//!   reused after the node it names is freed, and it changes when a node
//!   moves to a different tree (see `transplant_node`).
//! - [`WindowId`] names one client window for the lifetime of that client;
//!   chosen by the adapter, never reused.
//! - [`DesktopId`] and [`MonitorId`] name a desktop or monitor for as long
//!   as it exists, for `bsp-ipc` (IPC) to reference in queries.

use core::fmt;

/// Index of a node in a [`crate::tree::Tree`]'s arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub(crate) u32);

impl NodeId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// Identifier of a client window, chosen by the adapter.
///
/// bspwm prints window ids in hex (`0x00C00003`); that formatting is
/// `bsp-ipc`'s job (IPC). Native Wayland windows get ids from a range
/// XWayland never uses (see `docs/design.md`, Compatibility).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WindowId(pub u32);

impl fmt::Display for WindowId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:08X}", self.0)
    }
}

/// Identifier of a desktop, stable for as long as the desktop exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DesktopId(pub u32);

/// Identifier of a monitor, stable for as long as the monitor exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MonitorId(pub u32);

/// Monotonic generator for [`DesktopId`] and [`MonitorId`] values.
///
/// bspwm generates these from the X server (`xcb_generate_id`); bsp-core
/// has no display to ask, so it counts instead. Ids are never reused.
#[derive(Debug, Clone, Default)]
pub struct IdGen(u32);

impl IdGen {
    /// Creates a generator that starts at 1 (0 is reserved as "none", as in
    /// bspwm's use of `XCB_NONE`).
    pub fn new() -> Self {
        Self(0)
    }

    /// Returns the next id, starting at 1.
    pub fn alloc(&mut self) -> u32 {
        self.0 += 1;
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_id_displays_as_bspwm_hex_format() {
        assert_eq!(WindowId(0xC00003).to_string(), "0x00C00003");
    }

    #[test]
    fn id_gen_never_returns_zero_or_repeats() {
        let mut gen = IdGen::new();
        let a = gen.alloc();
        let b = gen.alloc();
        assert_ne!(a, 0);
        assert_ne!(a, b);
    }
}
