//! Virtual (headless) outputs: `bspc output --create-headless`.
//!
//! An output with no display behind it. It is a real `wl_output` and a real
//! `bsp-core` monitor, so windows can be put on it and clients can capture it
//! (screencopy renders an output on demand, whether or not anything scans it
//! out): `wayvnc HEADLESS-1` streams it, a recorder films it. Both backends
//! offer it, since nothing here touches DRM.
//!
//! bspwm has nothing like it (an X11 screen is what the server says it is);
//! Hyprland's `hyprctl output create headless` is the model.

use smithay::output::{Mode as WlMode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::utils::Transform;

use bsp_core::geometry::Rect;
use bsp_core::id::{DesktopId, MonitorId};
use bsp_core::monitor::Monitor;
use bsp_ipc::command::OutputMode;
use bsp_ipc::report::Event;

use crate::hardware::{HwOutput, HEADLESS_PREFIX};
use crate::state::{Backend, State};

/// The mode a virtual output starts with when none is asked for.
const DEFAULT_MODE: OutputMode = OutputMode { width: 1920, height: 1080, refresh_mhz: 60_000 };

thread_local! {
    /// Whether the frame timer below is running.
    static TICKING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A virtual output has no vblank, so nothing tells its clients when to draw
/// the next frame or a capture client when the screen changed. This timer does,
/// every 16 ms while any virtual output exists: it sends frame callbacks to the
/// windows on those outputs and counts a rendered frame for screencopy's
/// wait-for-damage captures. It stops when the last virtual output is gone.
fn start_frame_timer<Bd: Backend + 'static>(state: &mut State<Bd>) {
    use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
    if TICKING.with(|t| t.replace(true)) {
        return;
    }
    let period = std::time::Duration::from_millis(16);
    let started = state.handle.insert_source(Timer::from_duration(period), move |_, _, state| {
        let outputs: Vec<Output> = state.space.outputs().filter(|o| is_headless(o)).cloned().collect();
        if outputs.is_empty() {
            TICKING.with(|t| t.set(false));
            return TimeoutAction::Drop;
        }
        for output in &outputs {
            crate::extras::finish_frame(state, output);
        }
        if state.protocols.screencopy.has_pending() {
            state.protocols.screencopy.frames_rendered += 1;
        }
        TimeoutAction::ToDuration(period)
    });
    if let Err(err) = started {
        TICKING.with(|t| t.set(false));
        tracing::warn!("cannot start the virtual output frame timer: {err}");
    }
}

/// Marks an `Output` as virtual, in its user data, and keeps its `wl_output`
/// global so it can be taken away again.
pub struct HeadlessOutput(smithay::reexports::wayland_server::backend::GlobalId);

/// Whether `output` is a virtual one.
pub fn is_headless(output: &Output) -> bool {
    output.user_data().get::<HeadlessOutput>().is_some()
}

/// The modes a virtual output offers besides its own: common sizes, so
/// `bspc output NAME -m` has something to pick.
fn offered_modes(own: OutputMode) -> Vec<OutputMode> {
    let mut modes = vec![own];
    for (width, height) in [(3840, 2160), (2560, 1440), (1920, 1080), (1600, 900), (1280, 720)] {
        let mode = OutputMode { width, height, refresh_mhz: 60_000 };
        if !modes.contains(&mode) {
            modes.push(mode);
        }
    }
    modes
}

/// The first `HEADLESS-N` no output or monitor uses.
fn free_name<Bd: Backend + 'static>(state: &State<Bd>) -> String {
    (1..)
        .map(|n| format!("{HEADLESS_PREFIX}{n}"))
        .find(|name| !state.space.outputs().any(|o| o.name() == *name) && !state.wm.monitors.iter().any(|m| &m.name == name))
        .unwrap_or_else(|| format!("{HEADLESS_PREFIX}0"))
}

/// Adds a virtual output of `mode` (default 1920x1080@60) to the right of every
/// other output, with a monitor and one desktop; returns its name.
pub fn create<Bd: Backend + 'static>(state: &mut State<Bd>, mode: Option<OutputMode>, events: &mut Vec<Event>) -> String {
    let requested = mode.unwrap_or(DEFAULT_MODE);
    let name = free_name(state);
    let wl_mode = WlMode { size: (requested.width, requested.height).into(), refresh: requested.refresh_mhz };
    let output = Output::new(
        name.clone(),
        PhysicalProperties { size: (0, 0).into(), subpixel: Subpixel::Unknown, make: "bspwm-rs".into(), model: "Virtual".into() },
    );
    let global = output.create_global::<State<Bd>>(&state.display_handle);
    let x = state.space.outputs().fold(0, |acc, o| acc + state.space.output_geometry(o).map_or(0, |g| g.size.w));
    let position = (x, 0).into();
    output.add_mode(wl_mode);
    output.set_preferred(wl_mode);
    output.change_current_state(Some(wl_mode), Some(Transform::Normal), Some(Scale::Integer(1)), Some(position));
    output.user_data().insert_if_missing(|| HeadlessOutput(global));
    state.space.map_output(&output, position);

    let settings = state.wm.settings.clone();
    let monitor_id = MonitorId(state.wm.monitors.iter().map(|m| m.id.0).max().unwrap_or(0) + 1);
    let rectangle = Rect::new(position.x, position.y, requested.width, requested.height);
    let mut monitor = Monitor::new(monitor_id, Some(&name), rectangle, &settings);
    monitor.virtual_output = true;
    let desktop_id = state.wm.monitors.iter().flat_map(|m| m.desktops.iter().map(|d| d.id.0)).max().unwrap_or(0) + 1;
    monitor.add_desktop(bsp_core::desktop::Desktop::new(DesktopId(desktop_id), Some("I"), &settings));
    state.wm.add_monitor(monitor);
    events.push(Event::MonitorAdd { id: monitor_id.0, name: name.clone(), geometry: rectangle });
    state.adapter.hw.outputs.push(HwOutput {
        name: name.clone(),
        modes: offered_modes(requested),
        mode: requested,
        scale: 1.0,
        position: (position.x, position.y),
        transform: Default::default(),
    });
    tracing::info!(name, ?requested, "virtual output created");
    start_frame_timer(state);
    state.backend_data.queue_redraw();
    name
}

/// Removes the virtual output `name`: its windows move to another monitor, then
/// its monitor and `wl_output` go.
pub fn remove<Bd: Backend + 'static>(state: &mut State<Bd>, name: &str, events: &mut Vec<Event>) -> Result<(), String> {
    let Some(output) = state.space.outputs().find(|o| o.name() == name && is_headless(o)).cloned() else {
        return Err(format!("output: '{name}' is not a virtual output.\n"));
    };
    let Some(index) = state.wm.monitors.iter().position(|m| m.name == name) else {
        return Err(format!("output: no monitor named '{name}'.\n"));
    };
    let _ = events;
    if bsp_ipc::exec::merge_target(&state.wm, index).is_none() {
        return Err("output: cannot remove the last monitor.\n".to_string());
    }
    // Its desktops go to another monitor, as bspwm's `merge_monitors()`.
    if let Some(HeadlessOutput(global)) = output.user_data().get::<HeadlessOutput>() {
        state.display_handle.remove_global::<State<Bd>>(global.clone());
    }
    crate::lifecycle::output_removed(state, &output, true);
    tracing::info!(name, "virtual output removed");
    state.backend_data.queue_redraw();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_virtual_output_offers_its_own_mode_first_and_no_duplicates() {
        let own = OutputMode { width: 1280, height: 720, refresh_mhz: 60_000 };
        let modes = offered_modes(own);
        assert_eq!(modes[0], own);
        assert_eq!(modes.iter().filter(|m| **m == own).count(), 1);
        let custom = OutputMode { width: 800, height: 600, refresh_mhz: 30_000 };
        assert_eq!(offered_modes(custom)[0], custom);
        assert!(offered_modes(custom).len() > 1);
    }
}
