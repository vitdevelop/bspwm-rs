//! `bspwm-rs`: the Wayland compositor binary.
//!
//! The nested compositor (`docs/design.md` roadmap): a nested (winit) compositor with
//! xdg-shell, tiling through `bsp-core`, focus and borders — plus a
//! first slice of hotkeys: sxhkdrc is read at startup and every keyboard
//! event is matched against it (`crate::hotkeys`), and `bspwmrc` is run
//! at startup (`crate::bspwmrc`). Real hardware (DRM/KMS), input devices
//! beyond the nested backend's, XWayland and every protocol past
//! xdg-shell are later steps — see `docs/bsp-compositor.md`.
//!
//! This is the only crate allowed `unsafe` (`docs/design.md` hard rule 2:
//! every block needs a `// SAFETY:` comment); `undocumented_unsafe_blocks`
//! is warned on here to help catch a missing one as soon as it lands.
#![deny(missing_docs)]
#![warn(clippy::undocumented_unsafe_blocks)]

// Every module below `state` builds a generic `State<B: state::Backend>`
// (`docs/design.md` roadmap, Stage C) and is shared by both
// backends; only `winit_backend`/`udev_backend` themselves are specific
// to one.
mod adapter;
mod bspwmrc;
mod cursor;
mod hardware;
mod hotkeys;
mod input;
mod ipc;
mod pointer_action;
mod render;
mod shell;
mod state;
#[cfg(feature = "real")]
mod udev_backend;
#[cfg(feature = "nested")]
mod winit_backend;

// `nested` (winit) and `real` (DRM/KMS, `docs/design.md` roadmap, step
// 5) are kept mutually exclusive for now: both build a full `State<B>`
// compositor, and nothing yet needs both compiled into the same binary
// (no runtime backend selection exists) — see `Cargo.toml`'s `real`
// feature comment for the full reasoning.
#[cfg(not(any(feature = "nested", feature = "real")))]
compile_error!(
    "bsp-compositor requires the `nested` or `real` feature (docs/design.md roadmap)"
);
#[cfg(all(feature = "nested", feature = "real"))]
compile_error!(
    "bsp-compositor: build with exactly one of `nested`/`real` at a time for now, not both (docs/design.md roadmap)"
);

fn main() {
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
}
