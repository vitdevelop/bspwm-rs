//! `bsp-core`: bspwm's tree, desktop, monitor, rule and settings logic.
//!
//! This crate owns every piece of bspwm state and behavior and has no
//! Wayland or Smithay dependency (`docs/design.md`, hard rule 1: "`bsp-core`
//! never depends on `smithay`, `wayland-server` or `calloop`"), so it can be
//! tested without a display and reused if the rendering backend ever
//! changes. It is a functional core: every operation here takes and
//! mutates plain data ([`tree::Tree`], [`desktop::Desktop`],
//! [`monitor::Monitor`]) and returns plain values; nothing in this crate
//! calls out to a window system.
//!
//! The core (`docs/design.md` roadmap) delivers the tree and every
//! structural operation on it (split, rotate, flip, balance, equalize,
//! circulate, swap, transplant), desktops and monitors, and rule matching.
//! Side effects bspwm performs alongside these — drawing borders, EWMH,
//! the input focus, the stacking list, `subscribe` reports, history — need
//! an adapter with a real display and are left to later work; each
//! function that leaves one out says so and names the bspwm function it
//! mirrors.

#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod desktop;
pub mod geometry;
pub mod history;
pub mod id;
pub mod monitor;
pub mod node;
pub mod rules;
pub mod settings;
pub mod tree;
pub mod wm;
