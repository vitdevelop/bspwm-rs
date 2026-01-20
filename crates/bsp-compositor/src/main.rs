//! `bspwm-rs`: the Wayland compositor binary.
//!
//! The nested compositor (`docs/design.md` roadmap): a nested (winit) compositor with
//! xdg-shell, tiling through `bsp-core`, focus and borders. Real hardware
//! (DRM/KMS), input devices beyond the nested backend's, `bspwmrc`/
//! `bsp-hotkeys`, XWayland and every protocol past xdg-shell are later
//! steps — see `docs/bsp-compositor.md`.
//!
//! This is the only crate allowed `unsafe` (`docs/design.md` hard rule 2:
//! every block needs a `// SAFETY:` comment); `undocumented_unsafe_blocks`
//! is warned on here to help catch a missing one as soon as it lands.
#![deny(missing_docs)]
#![warn(clippy::undocumented_unsafe_blocks)]

mod adapter;
mod input;
mod render;
mod shell;
mod state;
mod winit_backend;

// `winit_backend` is the only backend that exists yet; the
// `nested` feature (on by default, see `Cargo.toml`) exists so a later
// step's real-hardware backend can be built without it, per
// `docs/design.md`'s Performance budget ("nested winit backend ... sits
// behind a cargo feature and is left out of release builds").
#[cfg(not(feature = "nested"))]
compile_error!("bsp-compositor currently requires the `nested` feature: no other backend exists yet (docs/design.md roadmap)");

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("BSPWM_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    winit_backend::run();
}
