//! The nested (winit) backend: a development-only "monitor" that is
//! really just a window on the host compositor.
//!
//! bspwm has no equivalent — X11 window managers do not nest inside
//! another X server as a normal part of development the way a Wayland
//! compositor can nest inside another Wayland compositor. Kept behind the
//! `nested` cargo feature and left out of release builds
//! (`docs/design.md`, Performance budget).

use std::time::Duration;

use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::ImportMemWl;
use smithay::backend::winit::{self, WinitEvent, WinitGraphicsBackend};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::signals::{Signal, Signals};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::wayland_server::Display;
use smithay::reexports::winit::platform::pump_events::PumpStatus;
use smithay::utils::Transform;

use bsp_core::geometry::Rect;
use bsp_core::id::{DesktopId, MonitorId};
use bsp_core::monitor::Monitor as CoreMonitor;
use bsp_core::settings::Settings;
use bsp_core::wm::Wm;

use crate::state::{insert_client, State};

/// This build's output name (bspwm: a monitor's name is normally its DRM
/// connector name; the nested backend has no connector, so it uses a
/// fixed placeholder — matching Smithay's own anvil example).
pub const OUTPUT_NAME: &str = "winit";

/// Winit-backend-specific state: the graphics backend and its damage
/// tracker.
pub struct WinitData {
    backend: WinitGraphicsBackend<GlesRenderer>,
    damage_tracker: OutputDamageTracker,
}

/// Builds the initial `bsp-core::wm::Wm`: one monitor sized to the winit
/// window, with one desktop, matching what `bspwm` itself does on
/// startup before `bspwmrc` adds more (`src/bspwm.c` `init()`).
fn initial_wm(size: (i32, i32)) -> Wm {
    let settings = Settings::default();
    let mut wm = Wm::new(settings.clone());
    let mut monitor = CoreMonitor::new(
        MonitorId(1),
        Some(OUTPUT_NAME),
        Rect::new(0, 0, size.0, size.1),
        &settings,
    );
    monitor.add_desktop(bsp_core::desktop::Desktop::new(
        DesktopId(1),
        Some("I"),
        &settings,
    ));
    wm.add_monitor(monitor);
    wm.focus_monitor(0);
    wm.monitors[0].focused = Some(0);
    wm
}

/// Runs the nested compositor until the winit window is closed.
pub fn run() {
    let mut event_loop: EventLoop<State> =
        EventLoop::try_new().expect("failed to create the event loop");
    let display: Display<State> = Display::new().expect("failed to create the Wayland display");
    let mut display_handle = display.handle();

    let (backend, mut winit) = match winit::init::<GlesRenderer>() {
        Ok(ret) => ret,
        Err(err) => {
            tracing::error!("failed to initialize the winit backend: {err}");
            return;
        }
    };
    // `ImportEgl::bind_wl_display` (`wl_drm` GPU buffer sharing) was
    // tried here and reverted: on this project's NVIDIA development
    // machine it does not fix `docs/bsp-compositor.md`'s known
    // vertical-flip issue and instead makes a GPU-accelerated client
    // (`alacritty`) fail to start at all (`Context` error, raw code
    // 12828) — a worse outcome. See that doc's Known issues section
    // before re-attempting this.
    let size = backend.window_size();

    let mode = Mode {
        size,
        refresh: 60_000,
    };
    let output = Output::new(
        OUTPUT_NAME.to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "bspwm-rs".into(),
            model: "nested".into(),
        },
    );
    output.create_global::<State>(&display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    let wm = initial_wm((size.w, size.h));

    let backend_data = WinitData {
        damage_tracker: OutputDamageTracker::from_output(&output),
        backend,
    };

    let socket_source = smithay::wayland::socket::ListeningSocketSource::new_auto()
        .expect("failed to create the Wayland listening socket");
    let socket_name = socket_source.socket_name().to_string_lossy().into_owned();
    event_loop
        .handle()
        .insert_source(socket_source, |stream, _, state: &mut State| {
            insert_client(&state.display_handle, stream);
        })
        .expect("failed to register the Wayland socket with the event loop");
    // SAFETY: `display` is moved into the `Generic` source below and is
    // not touched again outside `dispatch_clients`, which Smithay
    // requires for this call; this mirrors Smithay's own anvil example
    // (`anvil/src/state.rs` `AnvilState::init`) exactly.
    event_loop
        .handle()
        .insert_source(
            smithay::reexports::calloop::generic::Generic::new(
                display,
                smithay::reexports::calloop::Interest::READ,
                smithay::reexports::calloop::Mode::Level,
            ),
            |_, display, state: &mut State| {
                // SAFETY: see the comment on the `insert_source` call above.
                unsafe {
                    display.get_mut().dispatch_clients(state)?;
                }
                Ok(smithay::reexports::calloop::PostAction::Continue)
            },
        )
        .expect("failed to register the Wayland display with the event loop");

    tracing::info!(socket = socket_name, "listening on Wayland socket");
    // SAFETY: `WAYLAND_DISPLAY` is process environment, set once here
    // before any client (including our own future `bspwmrc` launcher,
    // Hotkeys) could read it; nothing else in this process touches the
    // environment concurrently at this point in startup.
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &socket_name);
    }

    let hotkeys = crate::hotkeys::init();

    let mut state = State::new(
        display_handle.clone(),
        event_loop.handle(),
        backend_data,
        wm,
        hotkeys,
    );
    // `State::new` builds `hotkey_matcher` from the raw, just-loaded
    // chords — `Modifier::Hyper`/`Meta` unresolved, since resolving them
    // needs `KeyboardHandle::with_xkb_state`, which needs a `&mut State`
    // that doesn't exist yet while `State::new` is still constructing
    // one. This second pass, now that `state` exists, is what actually
    // resolves them
    // (`crate::hotkeys::canonicalize_virtual_modifiers`'s own doc
    // comment).
    crate::hotkeys::canonicalize_virtual_modifiers(&mut state);
    state
        .shm_state
        .update_formats(state.backend_data.backend.renderer().shm_formats());
    state.space.map_output(&output, (0, 0));
    crate::ipc::init(&mut state);
    // bspwm: `run_config()` is called right after the control socket
    // starts listening (`src/bspwm.c` `main()`) and before the main
    // loop begins, since `bspwmrc` typically issues `bspc` commands
    // against it as it runs.
    crate::bspwmrc::run();

    // bspwm's sxhkd: `SIGUSR1` reloads sxhkdrc (`src/sxhkd.c` `hold()`/
    // `reload_cmd()`, `docs/bsp-hotkeys.md`'s "Binding execution").
    match Signals::new(&[Signal::SIGUSR1]) {
        Ok(signals) => {
            if let Err(err) = event_loop.handle().insert_source(signals, |_, _, state| {
                tracing::info!("SIGUSR1: reloading sxhkdrc");
                crate::hotkeys::reload(state);
            }) {
                tracing::warn!("failed to register the SIGUSR1 handler: {err}");
            }
        }
        Err(err) => tracing::warn!("failed to set up SIGUSR1 handling: {err}"),
    }

    tracing::info!("nested compositor ready");

    while state.running {
        let status = winit.dispatch_new_events(|event| match event {
            WinitEvent::Resized { size, .. } => {
                let mode = Mode {
                    size,
                    refresh: 60_000,
                };
                output.change_current_state(Some(mode), None, None, None);
                output.set_preferred(mode);
                state.wm.monitors[0].rectangle = Rect::new(0, 0, size.w, size.h);
                let settings = state.wm.settings.clone();
                for di in 0..state.wm.monitors[0].desktops.len() {
                    state.wm.monitors[0].arrange(di, &settings);
                }
                crate::shell::sync_wayland_from_core(&mut state);
            }
            WinitEvent::Input(event) => {
                crate::input::process_input_event(&mut state, event, &output)
            }
            _ => (),
        });

        if let PumpStatus::Exit(_) = status {
            state.running = false;
            break;
        }

        let backend = &mut state.backend_data.backend;
        let age = backend.buffer_age().unwrap_or(0);
        let render_res = backend.bind().and_then(|(renderer, mut fb)| {
            crate::render::render_frame(
                &output,
                &state.space,
                &state.wm,
                renderer,
                &mut fb,
                &mut state.backend_data.damage_tracker,
                age,
            )
            .map_err(
                |err: smithay::backend::renderer::damage::Error<_>| match err {
                    smithay::backend::renderer::damage::Error::Rendering(err) => err.into(),
                    _ => unreachable!(),
                },
            )
        });

        match render_res {
            Ok(render_output_result) => {
                if let Some(damage) = render_output_result.damage {
                    if let Err(err) = backend.submit(Some(damage)) {
                        tracing::warn!("failed to submit the frame: {err}");
                    }
                }
                let now = state.start_time.elapsed();
                for window in state.space.elements() {
                    window.send_frame(&output, now, Some(Duration::from_secs(1)), |_, _| {
                        Some(output.clone())
                    });
                }
            }
            Err(smithay::backend::SwapBuffersError::ContextLost(err)) => {
                tracing::error!("critical rendering error: {err}");
                state.running = false;
            }
            Err(err) => tracing::warn!("rendering error: {err}"),
        }

        if event_loop
            .dispatch(Some(Duration::from_millis(1)), &mut state)
            .is_err()
        {
            state.running = false;
        } else {
            state.space.refresh();
            state.popups.cleanup();
            let _ = display_handle.flush_clients();
        }
    }
}
