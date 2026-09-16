//! `wlr-gamma-control-unstable-v1`: `gammastep`, `wlsunset` and similar
//! tools tint the screen (night light) by loading a per-channel gamma ramp
//! into the display hardware.
//!
//! bspwm leaves colour to the X server (`xrandr --gamma`, `redshift`); on
//! Wayland the compositor must own the display's gamma LUT. The DRM backend
//! writes the ramp into the CRTC's `GAMMA_LUT` property
//! (`crate::udev_backend`'s `Backend::set_gamma`); backends without a LUT
//! (the nested one, virtual GPUs that expose none) answer `failed`, which
//! well-behaved clients handle.
//!
//! One client controls an output at a time; a second request for the same
//! output is answered `failed`. When the controlling client goes away (or
//! destroys its object) the ramp is reset to identity.

use std::io::Read;
use std::os::fd::OwnedFd;
use std::sync::Mutex;

use smithay::output::Output;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};
use wayland_protocols_wlr::gamma_control::v1::server::{
    zwlr_gamma_control_manager_v1::{self, ZwlrGammaControlManagerV1},
    zwlr_gamma_control_v1::{self, ZwlrGammaControlV1},
};

use crate::state::{Backend, State};

/// User data of a gamma control object.
pub struct GammaData {
    output: Option<Output>,
    /// Entries per ramp (0 if this control failed).
    size: u32,
    /// Whether this object owns its output's ramp.
    owns: Mutex<bool>,
}

/// Per-protocol state: which output names are currently under control.
#[derive(Default)]
pub struct Gamma {
    controlled: Vec<String>,
}

impl<Bd: Backend + 'static> GlobalDispatch<ZwlrGammaControlManagerV1, (), State<Bd>> for State<Bd> {
    fn can_view(client: Client, _: &()) -> bool {
        crate::state::is_privileged(&client)
    }

    fn bind(
        _state: &mut State<Bd>,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrGammaControlManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        data_init.init(resource, ());
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrGammaControlManagerV1, (), State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _client: &Client,
        _manager: &ZwlrGammaControlManagerV1,
        request: zwlr_gamma_control_manager_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        let zwlr_gamma_control_manager_v1::Request::GetGammaControl { id, output } = request else {
            return;
        };
        let output = Output::from_resource(&output);
        let size = output
            .as_ref()
            .filter(|o| !state.protocols.gamma.controlled.contains(&o.name()))
            .and_then(|o| state.backend_data.gamma_size(o))
            .unwrap_or(0);
        let control = data_init.init(
            id,
            GammaData { output: output.clone(), size, owns: Mutex::new(false) },
        );
        match (&output, size) {
            (Some(output), size) if size > 0 => {
                tracing::debug!(output = output.name(), size, "gamma control granted");
                state.protocols.gamma.controlled.push(output.name());
                if let Some(data) = control.data::<GammaData>() {
                    if let Ok(mut owns) = data.owns.lock() {
                        *owns = true;
                    }
                }
                control.gamma_size(size);
            }
            _ => {
                tracing::debug!(
                    output = ?output.map(|o| o.name()),
                    "gamma control refused (no LUT, or the output is already controlled)"
                );
                control.failed();
            }
        }
    }
}

/// Reads `3 * size` native-endian `u16`s from `fd`.
fn read_ramp(fd: OwnedFd, size: u32) -> std::io::Result<Vec<u16>> {
    let mut bytes = vec![0u8; size as usize * 3 * 2];
    let mut file = std::fs::File::from(fd);
    file.read_exact(&mut bytes)?;
    Ok(bytes.chunks_exact(2).map(|c| u16::from_ne_bytes([c[0], c[1]])).collect())
}

impl<Bd: Backend + 'static> Dispatch<ZwlrGammaControlV1, GammaData, State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _client: &Client,
        control: &ZwlrGammaControlV1,
        request: zwlr_gamma_control_v1::Request,
        data: &GammaData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        if let zwlr_gamma_control_v1::Request::SetGamma { fd } = request {
            let (Some(output), true) = (&data.output, data.size > 0) else {
                return;
            };
            match read_ramp(fd, data.size) {
                Ok(ramp) => {
                    if let Err(msg) = state.backend_data.set_gamma(output, Some(&ramp)) {
                        tracing::debug!(output = output.name(), "setting the gamma ramp failed: {msg}");
                        control.failed();
                    }
                }
                Err(err) => {
                    tracing::debug!("could not read the gamma ramp: {err}");
                    control.post_error(zwlr_gamma_control_v1::Error::InvalidGamma, "gamma ramp has the wrong size");
                }
            }
        }
    }

    fn destroyed(state: &mut State<Bd>, _client: ClientId, _control: &ZwlrGammaControlV1, data: &GammaData) {
        let owned = data.owns.lock().map(|o| *o).unwrap_or(false);
        if let (true, Some(output)) = (owned, &data.output) {
            tracing::debug!(output = output.name(), "gamma control released; restoring the identity ramp");
            state.protocols.gamma.controlled.retain(|n| n != &output.name());
            let _ = state.backend_data.set_gamma(output, None);
        }
    }
}
