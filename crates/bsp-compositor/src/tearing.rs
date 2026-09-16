//! `wp-tearing-control-v1`: a game or a low-latency app tells the compositor
//! it prefers *async* presentation (tearing allowed) over vsync.
//!
//! The hint is accepted and remembered per surface ([`wants_tearing`]) but
//! not acted on: presentation stays vsynced. That is allowed — the
//! protocol only says the compositor "may" tear — and the DRM path here
//! (Smithay's `DrmCompositor`) exposes no async page flip. bspwm has no
//! equivalent (the X server decides). Clients still bind the global and
//! run normally instead of failing over a missing protocol.

use std::sync::atomic::{AtomicBool, Ordering};

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum};
use smithay::wayland::compositor::with_states;
use wayland_protocols::wp::tearing_control::v1::server::{
    wp_tearing_control_manager_v1::{self, WpTearingControlManagerV1},
    wp_tearing_control_v1::{self, PresentationHint, WpTearingControlV1},
};

use crate::state::{Backend, State};

/// Per-surface record: does the client prefer async presentation?
#[derive(Default)]
struct SurfaceHint {
    asynchronous: AtomicBool,
    /// A `wp_tearing_control_v1` object exists for the surface.
    has_object: AtomicBool,
}

/// Whether `surface` asked for async (tearing) presentation.
#[allow(dead_code)]
pub fn wants_tearing(surface: &WlSurface) -> bool {
    with_states(surface, |states| {
        states
            .data_map
            .get::<SurfaceHint>()
            .is_some_and(|h| h.asynchronous.load(Ordering::SeqCst))
    })
}

impl<Bd: Backend + 'static> GlobalDispatch<WpTearingControlManagerV1, (), State<Bd>> for State<Bd> {
    fn bind(
        _: &mut State<Bd>,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WpTearingControlManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        data_init.init(resource, ());
    }
}

impl<Bd: Backend + 'static> Dispatch<WpTearingControlManagerV1, (), State<Bd>> for State<Bd> {
    fn request(
        _: &mut State<Bd>,
        _: &Client,
        manager: &WpTearingControlManagerV1,
        request: wp_tearing_control_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        if let wp_tearing_control_manager_v1::Request::GetTearingControl { id, surface } = request {
            let exists = with_states(&surface, |states| {
                let hint = states.data_map.get_or_insert_threadsafe(SurfaceHint::default);
                hint.has_object.swap(true, Ordering::SeqCst)
            });
            if exists {
                manager.post_error(wp_tearing_control_manager_v1::Error::TearingControlExists, "surface already has a tearing control");
                return;
            }
            data_init.init(id, surface);
        }
    }
}

impl<Bd: Backend + 'static> Dispatch<WpTearingControlV1, WlSurface, State<Bd>> for State<Bd> {
    fn request(
        _: &mut State<Bd>,
        _: &Client,
        _: &WpTearingControlV1,
        request: wp_tearing_control_v1::Request,
        surface: &WlSurface,
        _: &DisplayHandle,
        _: &mut DataInit<'_, State<Bd>>,
    ) {
        match request {
            wp_tearing_control_v1::Request::SetPresentationHint { hint } => {
                let asynchronous = matches!(hint, WEnum::Value(PresentationHint::Async));
                with_states(surface, |states| {
                    states
                        .data_map
                        .get_or_insert_threadsafe(SurfaceHint::default)
                        .asynchronous
                        .store(asynchronous, Ordering::SeqCst);
                });
                tracing::debug!(asynchronous, "tearing hint set (accepted; presentation stays vsynced)");
            }
            wp_tearing_control_v1::Request::Destroy => reset(surface),
            _ => {}
        }
    }

    fn destroyed(_: &mut State<Bd>, _: smithay::reexports::wayland_server::backend::ClientId, _: &WpTearingControlV1, surface: &WlSurface) {
        reset(surface);
    }
}

/// Back to vsync, no object.
fn reset(surface: &WlSurface) {
    with_states(surface, |states| {
        if let Some(hint) = states.data_map.get::<SurfaceHint>() {
            hint.asynchronous.store(false, Ordering::SeqCst);
            hint.has_object.store(false, Ordering::SeqCst);
        }
    });
}
