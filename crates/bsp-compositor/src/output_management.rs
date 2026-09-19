//! `wlr-output-management-unstable-v1`: `wlr-randr`, `kanshi` and
//! `nwg-displays` list outputs (name, modes, position, scale) and change
//! them.
//!
//! bspwm leaves this to `xrandr`/RandR; here it is the protocol form of
//! `bspc output` (`docs/design.md`, "Configuration beyond bspwm") and shares
//! its machinery: heads mirror `crate::hardware::HwModel`, and an applied
//! configuration becomes the same validated, queued changes `bspc output`
//! makes, applied by `crate::ipc::apply_hardware_now`.
//!
//! Supported: mode (including custom modes that match a real one), position,
//! scale. Not supported, and reported as a failed configuration: turning an
//! output off, transforms other than `normal`, adaptive sync.
//!
//! Heads are diffed after every event-loop turn ([`sync`]).

use std::collections::HashMap;
use std::sync::Mutex;

use bsp_ipc::command::{OutputAction, OutputMode};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_output;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum};
use wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1::{self, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::ZwlrOutputHeadV1,
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::ZwlrOutputModeV1,
};

use crate::hardware::HwOutput;
use crate::state::{Backend, State};

/// User data of a mode object: which head and which mode it stands for.
pub struct ModeData {
    head: String,
    mode: OutputMode,
}

/// User data of a head object: the output's name.
pub struct HeadData(String);

/// A requested change to one head.
#[derive(Default, Clone)]
struct Change {
    mode: Option<OutputMode>,
    position: Option<(i32, i32)>,
    scale: Option<f64>,
    transform: Option<bsp_ipc::command::OutputTransform>,
}

/// User data of a configuration object.
pub struct ConfigData(Mutex<Config>);

struct Config {
    serial: u32,
    /// `(head name, enabled, change)`.
    heads: Vec<(String, bool, Change)>,
    used: bool,
}

/// User data of a configuration-head object.
pub struct ConfigHeadData {
    config: ZwlrOutputConfigurationV1,
    head: String,
}

struct HeadEntry {
    head: ZwlrOutputHeadV1,
    modes: Vec<(OutputMode, ZwlrOutputModeV1)>,
    last: HwOutput,
}

struct ManagerEntry {
    manager: ZwlrOutputManagerV1,
    heads: HashMap<String, HeadEntry>,
}

/// Per-protocol state.
#[derive(Default)]
pub struct OutputManagement {
    managers: Vec<ManagerEntry>,
    /// Bumped whenever the set of heads or any head's state changes; a
    /// configuration made against an older serial is `cancelled`.
    serial: u32,
}

fn make_head<Bd: Backend + 'static>(
    state: &State<Bd>,
    dh: &DisplayHandle,
    entry: &mut ManagerEntry,
    client: &Client,
    hw: &HwOutput,
) {
    let version = entry.manager.version();
    let Ok(head) = client.create_resource::<ZwlrOutputHeadV1, HeadData, State<Bd>>(dh, version, HeadData(hw.name.clone())) else {
        return;
    };
    entry.manager.head(&head);
    head.name(hw.name.clone());
    let output = state.space.outputs().find(|o| o.name() == hw.name).cloned();
    if let Some(output) = &output {
        let props = output.physical_properties();
        head.description(output.description());
        if props.size.w > 0 && props.size.h > 0 {
            head.physical_size(props.size.w, props.size.h);
        }
        if version >= 2 {
            head.make(props.make.clone());
            head.model(props.model.clone());
        }
    } else {
        head.description(hw.name.clone());
    }
    let mut modes = Vec::new();
    let mut current = None;
    for (i, mode) in hw.modes.iter().enumerate() {
        let Ok(mode_obj) = client.create_resource::<ZwlrOutputModeV1, ModeData, State<Bd>>(
            dh,
            head.version(),
            ModeData { head: hw.name.clone(), mode: *mode },
        ) else {
            continue;
        };
        head.mode(&mode_obj);
        mode_obj.size(mode.width, mode.height);
        mode_obj.refresh(mode.refresh_mhz);
        if i == 0 {
            mode_obj.preferred();
        }
        if *mode == hw.mode {
            current = Some(mode_obj.clone());
        }
        modes.push((*mode, mode_obj));
    }
    head.enabled(1);
    if let Some(current) = current {
        head.current_mode(&current);
    }
    head.position(hw.position.0, hw.position.1);
    head.transform(wl_transform_of(hw.transform));
    head.scale(hw.scale);
    entry.heads.insert(hw.name.clone(), HeadEntry { head, modes, last: hw.clone() });
}

fn wl_transform_of(t: bsp_ipc::command::OutputTransform) -> wl_output::Transform {
    use bsp_ipc::command::OutputTransform as T;
    match t {
        T::Normal => wl_output::Transform::Normal,
        T::Rotate90 => wl_output::Transform::_90,
        T::Rotate180 => wl_output::Transform::_180,
        T::Rotate270 => wl_output::Transform::_270,
        T::Flipped => wl_output::Transform::Flipped,
        T::Flipped90 => wl_output::Transform::Flipped90,
        T::Flipped180 => wl_output::Transform::Flipped180,
        T::Flipped270 => wl_output::Transform::Flipped270,
    }
}

fn transform_of_wl(t: wl_output::Transform) -> Option<bsp_ipc::command::OutputTransform> {
    use bsp_ipc::command::OutputTransform as T;
    Some(match t {
        wl_output::Transform::Normal => T::Normal,
        wl_output::Transform::_90 => T::Rotate90,
        wl_output::Transform::_180 => T::Rotate180,
        wl_output::Transform::_270 => T::Rotate270,
        wl_output::Transform::Flipped => T::Flipped,
        wl_output::Transform::Flipped90 => T::Flipped90,
        wl_output::Transform::Flipped180 => T::Flipped180,
        wl_output::Transform::Flipped270 => T::Flipped270,
        _ => return None,
    })
}

/// Brings every output-management client up to date with the current outputs.
pub fn sync<Bd: Backend + 'static>(state: &mut State<Bd>) {
    if state.protocols.output_management.managers.is_empty() {
        return;
    }
    let outputs = state.adapter.hw.outputs.clone();
    let dh = state.display_handle.clone();
    let mut entries = std::mem::take(&mut state.protocols.output_management.managers);
    let mut changed = false;
    for entry in &mut entries {
        let Some(client) = entry.manager.client() else {
            continue;
        };
        let mut entry_changed = false;
        // Removed outputs.
        let gone: Vec<String> = entry
            .heads
            .keys()
            .filter(|n| !outputs.iter().any(|o| &o.name == *n))
            .cloned()
            .collect();
        for name in gone {
            if let Some(head) = entry.heads.remove(&name) {
                for (_, mode) in &head.modes {
                    mode.finished();
                }
                head.head.finished();
                entry_changed = true;
            }
        }
        for hw in &outputs {
            match entry.heads.get_mut(&hw.name) {
                None => {
                    make_head(state, &dh, entry, &client, hw);
                    entry_changed = true;
                }
                Some(head) => {
                    if head.last.mode != hw.mode {
                        if let Some((_, mode)) = head.modes.iter().find(|(m, _)| *m == hw.mode) {
                            head.head.current_mode(mode);
                        }
                        entry_changed = true;
                    }
                    if head.last.position != hw.position {
                        head.head.position(hw.position.0, hw.position.1);
                        entry_changed = true;
                    }
                    if head.last.transform != hw.transform {
                        head.head.transform(wl_transform_of(hw.transform));
                        entry_changed = true;
                    }
                    if head.last.scale != hw.scale {
                        head.head.scale(hw.scale);
                        entry_changed = true;
                    }
                    head.last = hw.clone();
                }
            }
        }
        changed |= entry_changed;
    }
    if changed {
        state.protocols.output_management.serial = state.protocols.output_management.serial.wrapping_add(1);
        let serial = state.protocols.output_management.serial;
        for entry in &entries {
            entry.manager.done(serial);
        }
    }
    state.protocols.output_management.managers = entries;
}

impl<Bd: Backend + 'static> GlobalDispatch<ZwlrOutputManagerV1, (), State<Bd>> for State<Bd> {
    fn can_view(client: Client, _: &()) -> bool {
        crate::state::is_privileged(&client)
    }

    fn bind(
        state: &mut State<Bd>,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrOutputManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        let manager = data_init.init(resource, ());
        tracing::debug!("an output-management client bound wlr-output-management");
        let mut entry = ManagerEntry { manager, heads: HashMap::new() };
        for hw in state.adapter.hw.outputs.clone() {
            make_head(state, dh, &mut entry, client, &hw);
        }
        entry.manager.done(state.protocols.output_management.serial);
        state.protocols.output_management.managers.push(entry);
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrOutputManagerV1, (), State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _client: &Client,
        manager: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                data_init.init(id, ConfigData(Mutex::new(Config { serial, heads: Vec::new(), used: false })));
            }
            zwlr_output_manager_v1::Request::Stop => {
                manager.finished();
                state.protocols.output_management.managers.retain(|e| &e.manager != manager);
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut State<Bd>, _client: ClientId, manager: &ZwlrOutputManagerV1, _: &()) {
        state.protocols.output_management.managers.retain(|e| &e.manager != manager);
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrOutputHeadV1, HeadData, State<Bd>> for State<Bd> {
    fn request(
        _: &mut State<Bd>,
        _: &Client,
        _: &ZwlrOutputHeadV1,
        _: <ZwlrOutputHeadV1 as Resource>::Request,
        _: &HeadData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, State<Bd>>,
    ) {
        // Only `release`.
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrOutputModeV1, ModeData, State<Bd>> for State<Bd> {
    fn request(
        _: &mut State<Bd>,
        _: &Client,
        _: &ZwlrOutputModeV1,
        _: <ZwlrOutputModeV1 as Resource>::Request,
        _: &ModeData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, State<Bd>>,
    ) {
        // Only `release`.
    }
}

/// Checks (and, with `apply`, queues) every change in `config`. `Err` is
/// why the configuration cannot be honoured.
fn evaluate<Bd: Backend + 'static>(state: &mut State<Bd>, config: &Config, apply: bool) -> Result<(), String> {
    let outputs = state.adapter.hw.outputs.clone();
    // Every currently enabled output must stay enabled: switching one off
    // is not supported.
    for hw in &outputs {
        let listed = config.heads.iter().find(|(name, _, _)| name == &hw.name);
        if !matches!(listed, Some((_, true, _))) {
            return Err(format!("turning output '{}' off is not supported", hw.name));
        }
    }
    let mut actions: Vec<(String, OutputAction)> = Vec::new();
    for (name, _, change) in &config.heads {
        let Some(hw) = outputs.iter().find(|o| &o.name == name) else {
            return Err(format!("unknown output '{name}'"));
        };
        if let Some(transform) = change.transform {
            if transform != hw.transform {
                actions.push((name.clone(), OutputAction::SetTransform(transform)));
            }
        }
        if let Some(mode) = change.mode {
            if mode != hw.mode {
                actions.push((name.clone(), OutputAction::SetMode(mode)));
            }
        }
        if let Some(position) = change.position {
            if position != hw.position {
                actions.push((name.clone(), OutputAction::SetPosition(position.0, position.1)));
            }
        }
        if let Some(scale) = change.scale {
            if (scale - hw.scale).abs() > f64::EPSILON {
                actions.push((name.clone(), OutputAction::SetScale(scale)));
            }
        }
    }
    for (name, action) in &actions {
        if apply {
            state.adapter.hw.set_output(name, action)?;
        } else {
            state.adapter.hw.check_output(name, action)?;
        }
    }
    Ok(())
}

impl<Bd: Backend + 'static> Dispatch<ZwlrOutputConfigurationV1, ConfigData, State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _client: &Client,
        resource: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        data: &ConfigData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        use zwlr_output_configuration_v1::Request;
        match request {
            Request::EnableHead { id, head } => {
                let Some(name) = head.data::<HeadData>().map(|h| h.0.clone()) else {
                    return;
                };
                if let Ok(mut config) = data.0.lock() {
                    config.heads.retain(|(n, _, _)| n != &name);
                    config.heads.push((name.clone(), true, Change::default()));
                }
                data_init.init(id, ConfigHeadData { config: resource.clone(), head: name });
            }
            Request::DisableHead { head } => {
                let Some(name) = head.data::<HeadData>().map(|h| h.0.clone()) else {
                    return;
                };
                if let Ok(mut config) = data.0.lock() {
                    config.heads.retain(|(n, _, _)| n != &name);
                    config.heads.push((name, false, Change::default()));
                }
            }
            Request::Apply | Request::Test => {
                let applying = matches!(request, Request::Apply);
                let (serial, snapshot) = match data.0.lock() {
                    Ok(mut config) => {
                        if config.used {
                            resource.post_error(zwlr_output_configuration_v1::Error::AlreadyUsed, "configuration already used");
                            return;
                        }
                        config.used = true;
                        (
                            config.serial,
                            Config { serial: config.serial, heads: config.heads.clone(), used: true },
                        )
                    }
                    Err(_) => return,
                };
                if serial != state.protocols.output_management.serial {
                    tracing::debug!("output configuration is stale; cancelled");
                    resource.cancelled();
                    return;
                }
                match evaluate(state, &snapshot, applying) {
                    Ok(()) if applying => match crate::ipc::apply_hardware_now(state) {
                        Ok(()) => resource.succeeded(),
                        Err(msg) => {
                            tracing::debug!("output configuration failed while applying: {}", msg.trim());
                            resource.failed();
                        }
                    },
                    Ok(()) => resource.succeeded(),
                    Err(msg) => {
                        tracing::debug!("output configuration rejected: {}", msg.trim());
                        state.adapter.hw.pending_outputs.clear();
                        resource.failed();
                    }
                }
            }
            _ => {}
        }
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrOutputConfigurationHeadV1, ConfigHeadData, State<Bd>> for State<Bd> {
    fn request(
        _state: &mut State<Bd>,
        _client: &Client,
        _resource: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        data: &ConfigHeadData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        use zwlr_output_configuration_head_v1::Request;
        let Some(config) = data.config.data::<ConfigData>() else {
            return;
        };
        let Ok(mut config) = config.0.lock() else {
            return;
        };
        let Some((_, _, change)) = config.heads.iter_mut().find(|(n, _, _)| n == &data.head) else {
            return;
        };
        match request {
            Request::SetMode { mode } => {
                if let Some(mode) = mode.data::<ModeData>() {
                    if mode.head == data.head {
                        change.mode = Some(mode.mode);
                    }
                }
            }
            Request::SetCustomMode { width, height, refresh } => {
                // A refresh of 0 means "whatever suits": take the nearest
                // real mode of that size (`hardware::resolve_mode`).
                change.mode = Some(OutputMode { width, height, refresh_mhz: refresh.max(0) });
                if refresh <= 0 {
                    change.mode = Some(OutputMode { width, height, refresh_mhz: 60_000 });
                }
            }
            Request::SetPosition { x, y } => change.position = Some((x, y)),
            Request::SetTransform { transform: WEnum::Value(t) } => change.transform = transform_of_wl(t),
            Request::SetScale { scale } => change.scale = Some(scale),
            _ => {}
        }
    }
}
