//! `wlr-output-power-management-unstable-v1`: `swayidle`-style tools turn an
//! output's display off (DPMS standby) after inactivity and on again at the
//! next activity.
//!
//! X11 has `xset dpms force off`; here it is the protocol. The DRM backend
//! writes the connector's legacy `DPMS` property (`Backend::set_output_power`)
//! and stops rendering that output while it is off; waking it forces a
//! repaint. Backends without DPMS (nested) answer `failed`.

use smithay::output::Output;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum};
use wayland_protocols_wlr::output_power_management::v1::server::{
    zwlr_output_power_manager_v1::{self, ZwlrOutputPowerManagerV1},
    zwlr_output_power_v1::{self, Mode, ZwlrOutputPowerV1},
};

use crate::state::{Backend, State};

/// User data of an output-power object.
pub struct PowerData(Option<Output>);

/// Per-protocol state: every live power object, to broadcast mode changes.
#[derive(Default)]
pub struct OutputPower {
    objects: Vec<(String, ZwlrOutputPowerV1)>,
}

impl<Bd: Backend + 'static> GlobalDispatch<ZwlrOutputPowerManagerV1, (), State<Bd>> for State<Bd> {
    fn can_view(client: Client, _: &()) -> bool {
        crate::state::is_privileged(&client)
    }

    fn bind(_: &mut State<Bd>, _: &DisplayHandle, _: &Client, resource: New<ZwlrOutputPowerManagerV1>, _: &(), data_init: &mut DataInit<'_, State<Bd>>) {
        data_init.init(resource, ());
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrOutputPowerManagerV1, (), State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _: &Client,
        _: &ZwlrOutputPowerManagerV1,
        request: zwlr_output_power_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        if let zwlr_output_power_manager_v1::Request::GetOutputPower { id, output } = request {
            let output = Output::from_resource(&output);
            let power = data_init.init(id, PowerData(output.clone()));
            match output.as_ref().and_then(|o| state.backend_data.output_power(o)) {
                Some(on) => {
                    power.mode(if on { Mode::On } else { Mode::Off });
                    if let Some(output) = &output {
                        state.protocols.output_power.objects.push((output.name(), power));
                    }
                }
                None => power.failed(),
            }
        }
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrOutputPowerV1, PowerData, State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _: &Client,
        power: &ZwlrOutputPowerV1,
        request: zwlr_output_power_v1::Request,
        data: &PowerData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, State<Bd>>,
    ) {
        if let zwlr_output_power_v1::Request::SetMode { mode } = request {
            let on = match mode {
                WEnum::Value(Mode::On) => true,
                WEnum::Value(Mode::Off) => false,
                _ => {
                    power.post_error(zwlr_output_power_v1::Error::InvalidMode, "invalid power mode");
                    return;
                }
            };
            let Some(output) = &data.0 else {
                power.failed();
                return;
            };
            match state.backend_data.set_output_power(output, on) {
                Ok(()) => {
                    tracing::debug!(output = output.name(), on, "output power changed");
                    let name = output.name();
                    for (n, object) in &state.protocols.output_power.objects {
                        if *n == name {
                            object.mode(if on { Mode::On } else { Mode::Off });
                        }
                    }
                    state.backend_data.queue_redraw();
                }
                Err(err) => {
                    tracing::debug!(output = output.name(), "output power change failed: {err}");
                    power.failed();
                }
            }
        }
    }

    fn destroyed(state: &mut State<Bd>, _: ClientId, power: &ZwlrOutputPowerV1, _: &PowerData) {
        state.protocols.output_power.objects.retain(|(_, o)| o != power);
    }
}
