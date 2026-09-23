//! `bspwm-rs`: the Wayland compositor binary.
//!
//! The nested compositor (`docs/design.md` roadmap): a nested (winit) compositor with
//! xdg-shell, tiling through `bsp-core`, focus and borders — plus a
//! first slice of the hotkeys and config: sxhkdrc is read at startup and every keyboard
//! event is matched against it (`crate::hotkeys`), and `bspwmrc` is run
//! at startup (`crate::bspwmrc`). Real hardware (DRM/KMS), input devices
//! beyond the nested backend's, XWayland and every protocol past
//! xdg-shell are later work — see `docs/bsp-compositor.md`.
//!
//! This is the only crate allowed `unsafe` (`docs/design.md` hard rule 2:
//! every block needs a `// SAFETY:` comment); `undocumented_unsafe_blocks`
//! is warned on here to help catch a missing one as soon as it lands.
#![deny(missing_docs)]
#![warn(clippy::undocumented_unsafe_blocks)]

// Every module below `state` builds a generic `State<B: state::Backend>`
// (`docs/design.md` roadmap, the hardware backend Stage C) and is shared by both
// backends; only `winit_backend`/`udev_backend` themselves are specific
// to one.
mod adapter;
mod bspwmrc;
mod spawn;
mod constraints;
mod cursor;
#[cfg(feature = "real")]
mod devices;
#[cfg(any(feature = "real", test))]
mod edid;
mod ext_capture;
mod export_dmabuf;
mod extras;
mod focus;
mod gamma;
mod hardware;
mod headless;
mod hotkeys;
mod input;
mod layers;
#[cfg_attr(not(feature = "real"), allow(dead_code))]
mod lifecycle;
mod ipc;
mod output_management;
mod output_power;
mod pointer_action;
mod protocols;
mod render;
mod screencopy;
mod session_lock;
mod shell;
mod state;
mod taskbar;
mod tearing;
mod toplevel_drag;
mod virtual_pointer;
mod workspaces;
mod xwayland;
mod xworker;
#[cfg(feature = "real")]
mod udev_backend;
#[cfg(feature = "nested")]
mod winit_backend;

// `nested` (winit) and `real` (DRM/KMS, `docs/design.md` roadmap, hardware
// backend) are kept mutually exclusive for now: both build a full `State<B>`
// compositor, and nothing yet needs both compiled into the same binary
// (no runtime backend selection exists) — see `Cargo.toml`'s `real`
// feature comment for the full reasoning.
#[cfg(not(any(feature = "nested", feature = "real")))]
compile_error!(
    "bsp-compositor requires the `nested` or `real` feature (docs/design.md roadmap, the hardware backend)"
);
#[cfg(all(feature = "nested", feature = "real"))]
compile_error!(
    "bsp-compositor: build with exactly one of `nested`/`real` at a time for now, not both (docs/design.md roadmap, the hardware backend)"
);

fn main() {
    // Run as the lazy-start shim when invoked through the `Xwayland` symlink.
    xwayland::run_shim_if_invoked();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("BSPWM_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    #[cfg(feature = "nested")]
    winit_backend::run();
    #[cfg(feature = "real")]
    udev_backend::run();
    xwayland::remove_shim_dir();

    // `bspc quit STATUS`: the compositor's own exit status (bspwm: `bspwm.c`'s
    // `exit_status`).
    let status = state::EXIT_STATUS.load(std::sync::atomic::Ordering::Relaxed);
    if status != 0 {
        std::process::exit(status);
    }
}
