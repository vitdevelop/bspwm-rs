//! The real hardware backend: DRM/KMS rendering, GBM buffer allocation
//! and real `libinput` input, on top of Stage B's session/udev device
//! enumeration (the hardware backend, `docs/design.md` roadmap).
//!
//! Modeled closely on Smithay's own reference compositor, anvil
//! (`anvil/src/udev.rs`, cloned to the scratchpad at git tag `v0.7.0`
//! specifically to read while writing this — its module docs point to
//! it as "the" hardware-backend reference, and it is not vendored in
//! the packaged crates.io source), with scope deliberately trimmed:
//! **not** implemented here (real anvil features left out because they
//! are unrelated to getting displays rendering — see `Cargo.toml`'s `real`
//! feature comment): DRM lease (VR headset passthrough) and explicit sync
//! (`drm_syncobj`). Cursor drawing (`crate::cursor`), presentation-time
//! feedback, gestures, touch and tablets (`crate::devices`) and the
//! multi-GPU copy route are implemented; see `docs/bsp-compositor.md`.
//!
//! bspwm has no equivalent code at all: X11 and its display manager
//! handle session/VT/GPU ownership beneath the window manager entirely.

use std::collections::HashMap;
use std::time::Duration;

use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::drm::compositor::FrameFlags;
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::output::{DrmOutput, DrmOutputManager};
use smithay::backend::drm::{DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType};
use smithay::backend::egl::{context::ContextPriority, EGLDevice, EGLDisplay};
use smithay::backend::input::{Event as _, InputEvent, KeyState, KeyboardKeyEvent};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::multigpu::gbm::GbmGlesBackend;
use smithay::backend::renderer::multigpu::GpuManager;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{self, UdevBackend, UdevEvent};
use smithay::input::keyboard::{FilterResult, LedState};
use smithay::output::{Mode as WlMode, Output, PhysicalProperties};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{EventLoop, LoopHandle, RegistrationToken};
use smithay::reexports::drm::control::{connector, crtc, ModeTypeFlags};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_server::Display;
use smithay::utils::{DeviceFd, Transform};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use bsp_core::geometry::Rect;
use bsp_ipc::command::OutputMode;
use bsp_core::monitor::Monitor as CoreMonitor;
use bsp_core::settings::Settings;
use bsp_core::wm::Wm;

use crate::state::{insert_client, Backend, State};

/// Which GPU node and CRTC an `Output` corresponds to, stored in every
/// DRM `Output`'s user data — bspwm has no equivalent, a monitor's
/// identity there is an X11 RandR output id, not something a compositor
/// process looks up per-frame the way this is (`frame_finish`/`render`,
/// below, both need it to go from "a vblank fired on this crtc" back to
/// the `Output`/`bsp_core::monitor::Monitor` it drives).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct UdevOutputId {
    device_id: DrmNode,
    crtc: crtc::Handle,
}

/// This backend's `State::backend_data`: the session, the multi-GPU
/// renderer manager (one node even with a single GPU — Smithay's DRM
/// rendering stack is built multi-GPU-first, not opt-in, confirmed
/// reading anvil's own `udev.rs`), one [`DrmDeviceData`] per GPU node
/// currently open, and every keyboard currently attached (kept only for
/// `Backend::update_led_state` to forward LED state to, mirroring
/// anvil's own `UdevData::keyboards`).
pub struct DrmData {
    session: LibSeatSession,
    primary_gpu: DrmNode,
    gpus: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    backends: HashMap<DrmNode, DrmDeviceData>,
    keyboards: Vec<smithay::reexports::input::Device>,
    /// Pointer devices, kept for `bspc input DEVICE -a`.
    pointers: Vec<smithay::reexports::input::Device>,
    /// The default cursor image (`crate::cursor`).
    cursor_images: crate::cursor::CursorImages,
    /// When the session was last resumed, until its first frame is on screen
    /// (logged, to time VT switches).
    resumed_at: Option<std::time::Instant>,
    /// A VT switch was asked for and the screen handed over: nothing more is
    /// drawn until the session is resumed (a frame queued in between failed,
    /// DRM master being on its way out).
    handed_over: bool,
}

/// Where a capture is rendered to.
enum CaptureTarget<'a> {
    /// A texture read back into CPU memory (shm clients).
    Memory,
    /// A client's (or our own) dmabuf, rendered on the GPU.
    Dmabuf(&'a mut smithay::backend::allocator::dmabuf::Dmabuf),
}

impl DrmData {
    /// Disables our cursor and overlay planes on `crtc`: the fallback of
    /// `State::switch_vt` when no final frame could be committed. Run while we
    /// still hold DRM master: the next session's compositor only resets the
    /// planes it knows, and an overlay or cursor plane of ours stayed on screen
    /// (a ghost of the last window in Hyprland). The CRTC stays on.
    fn clear_planes(&mut self, node: DrmNode, crtc: crtc::Handle) {
        let Some(device) = self.backends.get_mut(&node) else { return };
        let Some(surface) = device.surfaces.get(&crtc) else { return };
        surface.drm_output.with_compositor(|compositor| {
            let surface = compositor.surface();
            for plane in surface.planes().cursor.iter().chain(surface.planes().overlay.iter()) {
                if let Err(err) = surface.clear_plane(plane.handle) {
                    tracing::info!(plane = ?plane.handle, "failed to clear a plane before a VT switch: {err}");
                }
            }
        });
    }

    /// The render node captures run on: the primary GPU's.
    fn capture_render_node(&self) -> DrmNode {
        self.backends.get(&self.primary_gpu).and_then(|d| d.render_node).unwrap_or(self.primary_gpu)
    }

    /// Renders what `request` asks for (a whole output, or one window)
    /// into `target`. `Some` frame for [`CaptureTarget::Memory`].
    fn capture(
        &mut self,
        request: crate::screencopy::CaptureRequest<'_>,
        target: CaptureTarget<'_>,
    ) -> Result<Option<crate::screencopy::CapturedFrame>, String> {
        use smithay::backend::allocator::Fourcc;
        use smithay::backend::renderer::damage::OutputDamageTracker;
        use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
        use smithay::backend::renderer::element::AsRenderElements;
        use smithay::backend::renderer::gles::GlesTexture;
        use smithay::backend::renderer::{Bind, ExportMem, Offscreen};
        use smithay::utils::{Buffer as BufferCoord, Rectangle, Size};

        let output = request.output;
        let mode = output.current_mode().ok_or_else(|| "output has no mode".to_string())?;
        let render_node = self.capture_render_node();
        let mut multi = self
            .gpus
            .single_renderer(&render_node)
            .map_err(|err| format!("no renderer: {err}"))?;
        // The render node's own GLES renderer: capture needs `Offscreen` and
        // `ExportMem`, which the multi-GPU wrapper does not offer.
        let gles: &mut GlesRenderer = multi.as_mut();

        let scale = smithay::utils::Scale::from(output.current_scale().fractional_scale());
        type Elements = Vec<crate::render::OutputRenderElements<GlesRenderer, <smithay::desktop::Window as AsRenderElements<GlesRenderer>>::RenderElement>>;
        let (elements, size): (Elements, Size<i32, smithay::utils::Physical>) = match request.window {
            Some(window) => {
                // One window's surface tree with its geometry's corner at the origin.
                let geometry = window.geometry();
                let origin = (-geometry.loc.x, -geometry.loc.y);
                let origin = smithay::utils::Point::<i32, smithay::utils::Logical>::from(origin).to_f64().to_physical(scale).to_i32_round();
                let size = geometry.size.to_f64().to_physical(scale).to_i32_ceil();
                let elements = window
                    .render_elements::<WaylandSurfaceRenderElement<GlesRenderer>>(gles, origin, scale, 1.0)
                    .into_iter()
                    .map(crate::render::OutputRenderElements::Layer)
                    .collect();
                (elements, size)
            }
            None => {
                let cursor = match request.cursor {
                    Some((status, location)) => crate::cursor::cursor_elements(gles, &mut self.cursor_images, status, location, scale),
                    None => Vec::new(),
                };
                let elements = crate::render::output_elements(output, request.space, request.wm, gles, cursor)
                    .ok_or_else(|| "output has no elements yet".to_string())?;
                (elements, mode.size)
            }
        };
        if size.w <= 0 || size.h <= 0 {
            return Err("nothing to capture".to_string());
        }

        if let CaptureTarget::Dmabuf(dmabuf) = target {
            let mut framebuffer = gles.bind(dmabuf).map_err(|err| format!("cannot bind the dmabuf: {err}"))?;
            let mut tracker = OutputDamageTracker::new(size, scale, DMABUF_CAPTURE_TRANSFORM);
            let result = tracker
                .render_output(gles, &mut framebuffer, 0, &elements, crate::render::CLEAR_COLOR)
                .map_err(|err| format!("off-screen render failed: {err}"))?;
            // The client reads the buffer as soon as it sees `ready`.
            result.sync.wait().map_err(|err| format!("waiting for the render failed: {err:?}"))?;
            return Ok(None);
        }

        let buffer_size: Size<i32, BufferCoord> = (size.w, size.h).into();
        let mut texture: GlesTexture = gles
            .create_buffer(Fourcc::Abgr8888, buffer_size)
            .map_err(|err| format!("cannot create an off-screen buffer: {err}"))?;
        let mut framebuffer = gles.bind(&mut texture).map_err(|err| format!("cannot bind it: {err}"))?;
        // Rendering into a GLES texture comes out y-inverted (GL's origin is
        // the bottom-left); rendering with `Flipped180` cancels that, so the
        // read-back is top row first, which is what `wl_shm` clients expect
        // (found live: without it `grim` screenshots were upside down).
        let mut tracker = OutputDamageTracker::new(size, scale, Transform::Flipped180);
        tracker
            .render_output(gles, &mut framebuffer, 0, &elements, crate::render::CLEAR_COLOR)
            .map_err(|err| format!("off-screen render failed: {err}"))?;
        let mapping = gles
            .copy_framebuffer(&framebuffer, Rectangle::from_size(buffer_size), Fourcc::Argb8888)
            .map_err(|err| format!("reading back failed: {err}"))?;
        let flipped = smithay::backend::renderer::TextureMapping::flipped(&mapping);
        tracing::debug!(flipped, "screencopy read-back orientation");
        let bytes = gles.map_texture(&mapping).map_err(|err| format!("mapping failed: {err}"))?;

        let (w, h) = (size.w, size.h);
        let stride = (w * 4) as usize;
        let mut data = Vec::with_capacity(stride * h as usize);
        if flipped {
            for row in (0..h as usize).rev() {
                data.extend_from_slice(&bytes[row * stride..(row + 1) * stride]);
            }
        } else {
            data.extend_from_slice(&bytes[..stride * h as usize]);
        }
        Ok(Some(crate::screencopy::CapturedFrame { width: w, height: h, data }))
    }
}

/// How a capture into a dmabuf is oriented (settled by the live test, see `docs/bsp-compositor.md`).
const DMABUF_CAPTURE_TRANSFORM: Transform = Transform::Normal;

impl Backend for DrmData {
    fn seat_name(&self) -> String {
        self.session.seat()
    }

    fn reset_buffers(&mut self, output: &Output) {
        if let Some(id) = output.user_data().get::<UdevOutputId>() {
            if let Some(device) = self.backends.get_mut(&id.device_id) {
                if let Some(surface) = device.surfaces.get_mut(&id.crtc) {
                    surface.drm_output.reset_buffers();
                }
            }
        }
    }

    // Hybrid GPUs: import the committed buffer on the primary GPU right
    // away (anvil's `early_import`), so the cross-GPU copy is not paid for
    // inside the next frame. Pointless with a single GPU.
    fn early_import(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        if self.backends.len() < 2 {
            return;
        }
        if let Err(err) = self.gpus.early_import(self.primary_gpu, surface) {
            tracing::debug!(primary = %self.primary_gpu, "early import of a client buffer failed: {err}");
        }
    }

    fn capture_output(&mut self, request: crate::screencopy::CaptureRequest<'_>) -> Result<crate::screencopy::CapturedFrame, String> {
        self.capture(request, CaptureTarget::Memory)?
            .ok_or_else(|| "no pixels were read back".to_string())
    }

    fn capture_dmabuf_caps(&mut self) -> Option<crate::screencopy::DmabufCaps> {
        use smithay::backend::allocator::{Format, Fourcc};
        use smithay::backend::renderer::Bind;
        let render_node = self.capture_render_node();
        let mut multi = self.gpus.single_renderer(&render_node).ok()?;
        let gles: &mut GlesRenderer = multi.as_mut();
        let supported = Bind::<smithay::backend::allocator::dmabuf::Dmabuf>::supported_formats(gles)?;
        let mut formats: Vec<(Fourcc, Vec<smithay::backend::allocator::Modifier>)> = Vec::new();
        for Format { code, modifier } in supported.iter() {
            if !matches!(code, Fourcc::Argb8888 | Fourcc::Xrgb8888 | Fourcc::Abgr8888 | Fourcc::Xbgr8888) {
                continue;
            }
            match formats.iter_mut().find(|(c, _)| c == code) {
                Some((_, mods)) => mods.push(*modifier),
                None => formats.push((*code, vec![*modifier])),
            }
        }
        if formats.is_empty() {
            return None;
        }
        Some(crate::screencopy::DmabufCaps { device: render_node.dev_id(), formats })
    }

    fn capture_into_dmabuf(
        &mut self,
        request: crate::screencopy::CaptureRequest<'_>,
        dmabuf: &mut smithay::backend::allocator::dmabuf::Dmabuf,
    ) -> Result<(), String> {
        self.capture(request, CaptureTarget::Dmabuf(dmabuf)).map(|_| ())
    }

    fn allocate_capture_buffer(&mut self, width: i32, height: i32) -> Result<smithay::backend::allocator::dmabuf::Dmabuf, String> {
        use smithay::backend::allocator::dmabuf::AsDmabuf;
        use smithay::backend::allocator::{Allocator, Fourcc, Modifier};
        let caps = self.capture_dmabuf_caps().ok_or_else(|| "no dmabuf render formats".to_string())?;
        let (code, modifiers) = caps
            .formats
            .iter()
            .find(|(code, _)| *code == Fourcc::Argb8888)
            .or_else(|| caps.formats.first())
            .ok_or_else(|| "no dmabuf render formats".to_string())?;
        // Linear when possible: every importer understands it.
        let modifiers: Vec<Modifier> = if modifiers.contains(&Modifier::Linear) { vec![Modifier::Linear] } else { modifiers.clone() };
        let render_node = self.capture_render_node();
        let gbm = self
            .backends
            .values()
            .find(|d| d.render_node == Some(render_node))
            .or_else(|| self.backends.values().next())
            .map(|d| d.gbm.clone())
            .ok_or_else(|| "no GPU".to_string())?;
        let mut allocator = GbmAllocator::new(gbm, GbmBufferFlags::RENDERING);
        let buffer = allocator
            .create_buffer(width as u32, height as u32, *code, &modifiers)
            .map_err(|err| format!("cannot allocate a buffer: {err}"))?;
        buffer.export().map_err(|err| format!("cannot export the buffer: {err}"))
    }

    fn output_power(&mut self, output: &Output) -> Option<bool> {
        let id = output.user_data().get::<UdevOutputId>()?;
        Some(self.backends.get(&id.device_id)?.surfaces.get(&id.crtc)?.powered)
    }

    fn set_output_power(&mut self, output: &Output, on: bool) -> Result<(), String> {
        use smithay::reexports::drm::control::Device as ControlDevice;
        let id = *output.user_data().get::<UdevOutputId>().ok_or_else(|| "not a hardware output".to_string())?;
        let device = self.backends.get_mut(&id.device_id).ok_or_else(|| "its GPU is gone".to_string())?;
        let surface = device.surfaces.get_mut(&id.crtc).ok_or_else(|| "output is not active".to_string())?;
        let drm = device.drm_output_manager.device();
        let props = drm.get_properties(surface.connector).map_err(|err| format!("reading connector properties: {err}"))?;
        let (ids, _) = props.as_props_and_values();
        let dpms = ids
            .iter()
            .find(|id| drm.get_property(**id).is_ok_and(|info| info.name().to_bytes() == b"DPMS"))
            .copied()
            .ok_or_else(|| "connector has no DPMS property".to_string())?;
        // DRM_MODE_DPMS_ON = 0, DRM_MODE_DPMS_OFF = 3.
        drm.set_property(surface.connector, dpms, if on { 0 } else { 3 })
            .map_err(|err| format!("setting DPMS failed: {err}"))?;
        tracing::debug!(output = output.name(), on, "DPMS written");
        surface.powered = on;
        if on {
            // The screen lost its image; repaint from scratch.
            surface.drm_output.reset_buffers();
            surface.dirty = true;
        }
        Ok(())
    }

    fn gamma_size(&mut self, output: &Output) -> Option<u32> {
        let (drm, crtc) = self.crtc_of(output)?;
        let (_, value) = crtc_property(drm, crtc, "GAMMA_LUT_SIZE")?;
        u32::try_from(value).ok().filter(|&size| size > 1)
    }

    fn set_gamma(&mut self, output: &Output, ramp: Option<&[u16]>) -> Result<(), String> {
        let (drm, crtc) = self
            .crtc_of(output)
            .ok_or_else(|| "output is not a hardware output".to_string())?;
        let written = write_gamma_lut(drm, crtc, ramp);
        // Remembered even when the write failed (a client updates its ramp while
        // another session owns the display): it is written again after the resume.
        if let Some(id) = output.user_data().get::<UdevOutputId>() {
            if let Some(surface) = self.backends.get_mut(&id.device_id).and_then(|d| d.surfaces.get_mut(&id.crtc)) {
                surface.gamma = ramp.map(<[u16]>::to_vec);
                surface.regamma = written.is_err() && ramp.is_some();
            }
        }
        written
    }

    fn queue_redraw(&mut self) {
        for device in self.backends.values_mut() {
            for surface in device.surfaces.values_mut() {
                surface.dirty = true;
            }
        }
    }

    fn update_led_state(&mut self, led_state: LedState) {
        for device in &mut self.keyboards {
            device.led_update(led_state.into());
        }
    }

    fn dmabuf_imported(&mut self, dmabuf: &smithay::backend::allocator::dmabuf::Dmabuf) -> bool {
        use smithay::backend::renderer::ImportDma;
        let render_node = self
            .backends
            .get(&self.primary_gpu)
            .and_then(|d| d.render_node)
            .unwrap_or(self.primary_gpu);
        match self.gpus.single_renderer(&render_node) {
            Ok(mut renderer) => match renderer.import_dmabuf(dmabuf, None) {
                Ok(_) => {
                    tracing::trace!(%render_node, "imported a client dmabuf");
                    true
                }
                Err(err) => {
                    tracing::debug!("refusing a client dmabuf: {err}");
                    false
                }
            },
            Err(err) => {
                tracing::warn!("no renderer to import a dmabuf: {err}");
                false
            }
        }
    }

    fn set_output_mode(&mut self, output: &Output, mode: OutputMode) -> Result<WlMode, String> {
        let id = output
            .user_data()
            .get::<UdevOutputId>()
            .copied()
            .ok_or_else(|| "output: not a hardware output.\n".to_string())?;
        let device = self
            .backends
            .get_mut(&id.device_id)
            .ok_or_else(|| "output: its GPU is gone.\n".to_string())?;
        let render_node = device.render_node.unwrap_or(self.primary_gpu);
        let surface = device
            .surfaces
            .get_mut(&id.crtc)
            .ok_or_else(|| "output: it is not active.\n".to_string())?;
        let drm_mode = surface
            .modes
            .iter()
            .copied()
            .find(|m| output_mode_of(*m) == mode)
            .ok_or_else(|| "output: no such mode.\n".to_string())?;
        let mut renderer = renderer_for(
            &mut self.gpus,
            self.primary_gpu,
            render_node,
            surface.drm_output.format(),
            surface.copy_route_failed,
        )
            .map_err(|err| format!("output: no renderer: {err}\n"))?;
        surface
            .drm_output
            .use_mode::<_, DrmRenderElements<'_>>(drm_mode, &mut renderer, &Default::default())
            .map_err(|err| format!("output: mode switch failed: {err}\n"))?;
        Ok(WlMode::from(drm_mode))
    }

    fn set_pointer_accel(&mut self, device: &str, accel: f64) -> Result<(), String> {
        let dev = self
            .pointers
            .iter_mut()
            .find(|d| d.name() == device)
            .ok_or_else(|| format!("input: unknown pointer device '{device}'.\n"))?;
        if !dev.config_accel_is_available() {
            return Err(format!("input: '{device}' has no acceleration setting.\n"));
        }
        dev.config_accel_set_speed(accel)
            .map(|_| ())
            .map_err(|err| format!("input: could not set accel on '{device}': {err:?}\n"))
    }
}

/// The `(make, model)` from a connector's EDID property blob, if it has one.
fn connector_make_model(drm: &DrmDevice, connector: &connector::Info) -> Option<(String, String)> {
    use smithay::reexports::drm::control::Device as ControlDevice;
    let props = drm.get_properties(connector.handle()).ok()?;
    let (ids, values) = props.as_props_and_values();
    for (id, value) in ids.iter().zip(values) {
        let info = drm.get_property(*id).ok()?;
        if info.name().to_bytes() == b"EDID" {
            let blob = drm.get_property_blob(*value).ok()?;
            return crate::edid::make_and_model(&blob);
        }
    }
    None
}

/// A CRTC property's handle and current raw value, by name.
fn crtc_property(drm: &DrmDevice, crtc: crtc::Handle, name: &str) -> Option<(smithay::reexports::drm::control::property::Handle, u64)> {
    use smithay::reexports::drm::control::Device as ControlDevice;
    let props = drm.get_properties(crtc).ok()?;
    let (ids, values) = props.as_props_and_values();
    ids.iter().zip(values).find_map(|(id, value)| {
        let info = drm.get_property(*id).ok()?;
        (info.name().to_bytes() == name.as_bytes()).then_some((*id, *value))
    })
}

impl DrmData {
    /// The DRM device and CRTC driving `output`.
    fn crtc_of(&self, output: &Output) -> Option<(&DrmDevice, crtc::Handle)> {
        let id = output.user_data().get::<UdevOutputId>()?;
        let device = self.backends.get(&id.device_id)?;
        Some((device.drm_output_manager.device(), id.crtc))
    }
}

/// Writes `ramp` (red, green and blue tables back to back) into `crtc`'s
/// `GAMMA_LUT`, or restores the identity ramp for `None`.
fn write_gamma_lut(drm: &DrmDevice, crtc: crtc::Handle, ramp: Option<&[u16]>) -> Result<(), String> {
    use smithay::reexports::drm::control::Device as ControlDevice;
    use std::os::fd::AsFd;
    let (lut_prop, _) = crtc_property(drm, crtc, "GAMMA_LUT").ok_or_else(|| "no GAMMA_LUT property".to_string())?;
    let (_, size) = crtc_property(drm, crtc, "GAMMA_LUT_SIZE").ok_or_else(|| "no GAMMA_LUT_SIZE".to_string())?;
    let size = size as usize;
    let value = match ramp {
        None => 0, // no blob: the kernel restores the identity ramp
        Some(ramp) => {
            if ramp.len() != size * 3 {
                return Err(format!("expected {} ramp values, got {}", size * 3, ramp.len()));
            }
            // `struct drm_color_lut { u16 red, green, blue, reserved; }` per entry.
            let mut blob = Vec::with_capacity(size * 8);
            for i in 0..size {
                for channel in 0..3 {
                    blob.extend_from_slice(&ramp[channel * size + i].to_ne_bytes());
                }
                blob.extend_from_slice(&0u16.to_ne_bytes());
            }
            drm_ffi::mode::create_property_blob(drm.as_fd(), &mut blob)
                .map_err(|err| format!("creating the LUT blob failed: {err}"))?
                .blob_id as u64
        }
    };
    tracing::debug!(size, reset = ramp.is_none(), "writing the gamma LUT");
    drm.set_property(crtc, lut_prop, value)
        .map_err(|err| format!("setting GAMMA_LUT failed: {err}"))
}

/// A DRM mode as `bsp-ipc`'s [`OutputMode`] (pixel size, millihertz).
fn output_mode_of(mode: smithay::reexports::drm::control::Mode) -> OutputMode {
    let wl = WlMode::from(mode);
    OutputMode {
        width: wl.size.w,
        height: wl.size.h,
        refresh_mhz: wl.refresh,
    }
}

/// This crate's renderer for the `real` backend: a `GpuManager`-vended
/// renderer over a single GBM/GLES node (no multi-GPU copy path, this
/// module's own doc comment) — the DRM-backend counterpart to the
/// nested backend's plain `GlesRenderer`.
type DrmRenderer<'a> = smithay::backend::renderer::multigpu::MultiRenderer<
    'a,
    'a,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
    GbmGlesBackend<GlesRenderer, DrmDeviceFd>,
>;

/// This backend's concrete render element type, fed to both
/// `DrmOutputManager::initialize_output` (so its internal bandwidth-check
/// compositor knows what it will later be asked to render) and
/// `DrmOutput::render_frame` itself (`render_surface`, below) —
/// `crate::render::output_elements`'s own return type, for whichever
/// concrete renderer this backend uses.
/// A GPU's render node (`/dev/dri/renderD*`) if it has one, else the
/// node itself: `GpuManager` and the primary-GPU comparison are keyed by
/// render node, while udev reports card nodes — the same GPU must not
/// compare unequal just because one side is its card node.
fn render_node_of(node: DrmNode) -> DrmNode {
    node.node_with_type(NodeType::Render)
        .and_then(Result::ok)
        .unwrap_or(node)
}

type Gpus = GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>;

/// The renderer to draw for an output scanned out by GPU `target`
/// (hybrid-GPU support, `docs/bsp-compositor.md` Hardware backend progress).
///
/// - `target` is the `primary` (boot) GPU, or the only one: render on it
///   directly, no copies.
/// - Otherwise the frame is composited on `primary` — where clients'
///   buffers already live — into an offscreen buffer that is copied
///   (dmabuf, `copy_format`) into the scanout buffer on `target`
///   (`GpuManager::renderer`, as Smithay's reference compositor anvil).
/// - If that pairing is unavailable (a GPU missing from the manager, no
///   common dmabuf format …) it falls back to rendering on `target`
///   alone: still correct for shm clients, while a dmabuf client from
///   the other GPU may then fail to import. Every decision is logged at
///   `debug`, so `BSPWM_LOG=debug` shows which route an output takes.
fn renderer_for<'a>(
    gpus: &'a mut Gpus,
    primary: DrmNode,
    target: DrmNode,
    copy_format: smithay::backend::allocator::Fourcc,
    force_single: bool,
) -> Result<DrmRenderer<'a>, String> {
    let (primary, target) = (render_node_of(primary), render_node_of(target));
    if primary == target || force_single {
        tracing::trace!(%target, force_single, "render route: single GPU");
        return gpus
            .single_renderer(&target)
            .map_err(|err| format!("no renderer for {target}: {err}"));
    }
    // Probe first: returning the copy renderer from a `match` arm while
    // also using `gpus` in the other arm is what the borrow checker
    // rejects; creating a `MultiRenderer` is cheap (no copy happens until
    // it draws), so probe, drop, and create again.
    let copy_route = match gpus.renderer(&primary, &target, copy_format) {
        Ok(_) => Ok(()),
        Err(err) => Err(err.to_string()),
    };
    match copy_route {
        Ok(()) => {
            tracing::trace!(%primary, %target, ?copy_format, "render route: primary GPU + copy to target");
            gpus.renderer(&primary, &target, copy_format)
                .map_err(|err| format!("no copy renderer {primary} -> {target}: {err}"))
        }
        Err(reason) => {
            tracing::debug!(
                %primary, %target,
                "cannot render on the primary GPU and copy ({reason}); falling back to rendering on the target GPU alone"
            );
            gpus.single_renderer(&target)
                .map_err(|err| format!("no renderer for {target}: {err}"))
        }
    }
}

type DrmRenderElements<'a> = crate::render::OutputRenderElements<
    DrmRenderer<'a>,
    <smithay::desktop::Window as smithay::backend::renderer::element::AsRenderElements<
        DrmRenderer<'a>,
    >>::RenderElement,
>;

/// What travels with each queued frame: the `wp_presentation` feedback its
/// surfaces asked for, answered when the frame's vblank arrives.
type FrameFeedback = Option<smithay::desktop::utils::OutputPresentationFeedback>;

type GbmDrmOutputManager =
    DrmOutputManager<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, FrameFeedback, DrmDeviceFd>;
type GbmDrmOutput =
    DrmOutput<GbmAllocator<DrmDeviceFd>, GbmFramebufferExporter<DrmDeviceFd>, FrameFeedback, DrmDeviceFd>;

/// Per-GPU-node state: every connector currently scanned into an
/// `Output`/surface, and the machinery `device_added` set up to drive
/// them.
struct DrmDeviceData {
    /// The GBM device, for allocating capture buffers.
    gbm: GbmDevice<DrmDeviceFd>,
    surfaces: HashMap<crtc::Handle, SurfaceData>,
    drm_output_manager: GbmDrmOutputManager,
    drm_scanner: DrmScanner,
    render_node: Option<DrmNode>,
    registration_token: RegistrationToken,
}

/// Per-CRTC state: the `DrmOutput` driving scanout, and which
/// `bsp_core::monitor::Monitor` (`Wm::monitors`, looked up by id since
/// hotplug elsewhere can reorder/remove other monitors and shift plain
/// indices) this connector was matched to.
struct SurfaceData {
    /// The connector this output scans out to (its `DPMS` property).
    connector: connector::Handle,
    /// Display powered on (`wlr-output-power-management`); while off nothing is rendered.
    powered: bool,
    /// The primary-GPU-plus-copy route failed for this output at run time
    /// (`render_surface`): from now on it renders on its own GPU alone.
    /// Never set for outputs on the primary GPU.
    copy_route_failed: bool,
    /// Something changed since the last render: a frame is wanted.
    dirty: bool,
    /// A frame was queued and its vblank has not arrived yet; rendering
    /// waits for it (`frame_finish` clears this).
    frame_pending: bool,
    /// The connector's DRM modes, for `bspc output -m`.
    modes: Vec<smithay::reexports::drm::control::Mode>,
    drm_output: GbmDrmOutput,
    /// The ramp a gamma client (`gammastep`) last set, kept to write it again
    /// after a VT round trip (the other session resets the LUT).
    gamma: Option<Vec<u16>>,
    /// Write `gamma` again once the first frame after a resume has scanned out.
    regamma: bool,
}

impl State<DrmData> {
    /// Opens the DRM device at `path` (a GPU udev just reported), sets up
    /// its renderer and `DrmOutputManager`, and scans its connectors.
    ///
    /// bspwm has no equivalent: X11 and its own DDX driver own GPU
    /// device access entirely; a window manager never opens `/dev/dri/*`
    /// itself.
    fn device_added(&mut self, node: DrmNode, path: &std::path::Path) {
        let fd = match self.backend_data.session.open(
            path,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
        ) {
            Ok(fd) => fd,
            Err(err) => {
                tracing::warn!(?path, "failed to open a DRM device: {err}");
                return;
            }
        };
        let fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, notifier) = match DrmDevice::new(fd.clone(), true) {
            Ok(pair) => pair,
            Err(err) => {
                tracing::warn!(?path, "failed to open a DRM device: {err}");
                return;
            }
        };
        let gbm = match GbmDevice::new(fd) {
            Ok(gbm) => gbm,
            Err(err) => {
                tracing::warn!(?path, "failed to open a GBM device: {err}");
                return;
            }
        };

        let registration_token = match self.handle.insert_source(notifier, move |event, metadata, data| {
            match event {
                DrmEvent::VBlank(crtc) => data.frame_finish(node, crtc, metadata),
                DrmEvent::Error(err) => tracing::warn!("DRM device error: {err}"),
            }
        }) {
            Ok(token) => token,
            Err(err) => {
                tracing::warn!(?path, "failed to register a DRM device with the event loop: {err}");
                return;
            }
        };

        // SAFETY: `EGLDisplay::new` requires `gbm` to outlive the
        // returned `EGLDisplay` — it does, `gbm` is cloned into the
        // `DrmDeviceData`/`GpuManager` this function stores below, kept
        // alive for as long as this GPU node is open.
        let render_node = unsafe { EGLDisplay::new(gbm.clone()) }
            .ok()
            .and_then(|display| EGLDevice::device_for_display(&display).ok())
            .filter(|egl_device| !egl_device.is_software())
            .and_then(|egl_device| egl_device.try_get_render_node().ok().flatten())
            .unwrap_or(node);
        // Canonical key everywhere (`GpuManager`, `render_node_of` in
        // `renderer_for`): the render node. The EGL device is software
        // (llvmpipe, e.g. on virtio-gpu without virgl) or reports none
        // for some drivers, in which case the fallback above is the card
        // node — its render node is still what identifies this GPU.
        let render_node = render_node_of(render_node);
        if let Err(err) = self.backend_data.gpus.as_mut().add_node(render_node, gbm.clone()) {
            tracing::warn!(?render_node, "failed to add a render node to the GPU manager: {err}");
        }

        let allocator = GbmAllocator::new(gbm.clone(), GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT);
        let framebuffer_exporter = GbmFramebufferExporter::new(gbm.clone(), Some(render_node));

        let own_formats = match self.backend_data.gpus.single_renderer(&render_node) {
            Ok(mut renderer) => renderer
                .as_mut()
                .egl_context()
                .dmabuf_render_formats()
                .iter()
                .copied()
                .collect::<smithay::backend::allocator::format::FormatSet>(),
            Err(err) => {
                tracing::warn!(?render_node, "failed to create a renderer: {err}");
                Default::default()
            }
        };
        let primary_gpu = self.backend_data.primary_gpu;
        let is_primary = render_node_of(render_node) == render_node_of(primary_gpu);
        tracing::debug!(
            %node, %render_node, %primary_gpu,
            is_primary,
            own_render_formats = own_formats.iter().count(),
            "GPU added"
        );
        // A secondary GPU's scanout buffers are filled by copying from the
        // primary GPU's render (`renderer_for`), so they must be a format
        // + modifier both GPUs understand: use the intersection. If it is
        // empty (nothing in common) keep the device's own formats — the
        // copy route will then fail and `renderer_for` falls back to
        // rendering on this GPU alone.
        let render_formats = if is_primary {
            own_formats
        } else {
            let primary_formats = match self.backend_data.gpus.single_renderer(&primary_gpu) {
                Ok(mut renderer) => Some(
                    renderer
                        .as_mut()
                        .egl_context()
                        .dmabuf_render_formats()
                        .iter()
                        .copied()
                        .collect::<smithay::backend::allocator::format::FormatSet>(),
                ),
                Err(err) => {
                    tracing::debug!(%primary_gpu, "primary GPU has no renderer yet ({err}); using this GPU's own formats");
                    None
                }
            };
            match primary_formats {
                Some(primary_formats) => {
                    let common: smithay::backend::allocator::format::FormatSet =
                        own_formats.iter().copied().filter(|f| primary_formats.contains(f)).collect();
                    tracing::debug!(
                        primary = primary_formats.iter().count(),
                        own = own_formats.iter().count(),
                        common = common.iter().count(),
                        "secondary GPU render formats intersected with the primary's"
                    );
                    if common.iter().count() == 0 {
                        tracing::warn!(
                            %node, %primary_gpu,
                            "no dmabuf format is common to this secondary GPU and the primary; multi-GPU copy will not work, its outputs render on this GPU alone"
                        );
                        own_formats
                    } else {
                        common
                    }
                }
                None => own_formats,
            }
        };
        const COLOR_FORMATS: &[smithay::reexports::drm::buffer::DrmFourcc] = &[
            smithay::reexports::drm::buffer::DrmFourcc::Argb8888,
            smithay::reexports::drm::buffer::DrmFourcc::Abgr8888,
        ];

        let gbm_for_captures = gbm.clone();
        let drm_output_manager = DrmOutputManager::new(
            drm,
            allocator,
            framebuffer_exporter,
            Some(gbm),
            COLOR_FORMATS.iter().copied(),
            render_formats,
        );

        self.backend_data.backends.insert(
            node,
            DrmDeviceData {
                gbm: gbm_for_captures,
                surfaces: HashMap::new(),
                drm_output_manager,
                drm_scanner: DrmScanner::new(),
                render_node: Some(render_node),
                registration_token,
            },
        );

        self.device_changed(node);
    }

    /// Scans `node`'s connectors for hotplug changes, dispatching each
    /// to [`Self::connector_connected`]/[`Self::connector_disconnected`].
    fn device_changed(&mut self, node: DrmNode) {
        let Some(device) = self.backend_data.backends.get_mut(&node) else {
            return;
        };
        let scan_result = device
            .drm_scanner
            .scan_connectors(device.drm_output_manager.device());
        let events: Vec<_> = match scan_result {
            Ok(events) => events.into_iter().collect(),
            Err(err) => {
                tracing::warn!(?node, "failed to scan connectors: {err}");
                return;
            }
        };
        for event in events {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_connected(node, connector, crtc),
                DrmScanEvent::Disconnected {
                    connector,
                    crtc: Some(crtc),
                } => self.connector_disconnected(node, connector, crtc),
                _ => {}
            }
        }
    }

    /// A connector on GPU node `node` is now connected and has a working
    /// `crtc` — names an `Output` after it (`docs/design.md`'s
    /// Compatibility: DRM connector names), maps a matching
    /// `bsp_core::monitor::Monitor` in on-screen-position order
    /// (`Wm::add_monitor`, `docs/bsp-core.md` Hardware backend progress), and
    /// initializes a `DrmOutput` to scan it out.
    fn connector_connected(&mut self, node: DrmNode, connector: connector::Info, crtc: crtc::Handle) {
        let Some(device) = self.backend_data.backends.get_mut(&node) else {
            return;
        };
        let render_node = device.render_node.unwrap_or(self.backend_data.primary_gpu);
        let mut renderer = match renderer_for(
            &mut self.backend_data.gpus,
            self.backend_data.primary_gpu,
            render_node,
            smithay::backend::allocator::Fourcc::Argb8888,
            false,
        ) {
            Ok(renderer) => renderer,
            Err(err) => {
                tracing::warn!(?render_node, "failed to create a renderer: {err}");
                return;
            }
        };
        tracing::debug!(
            %node, %render_node, primary = %self.backend_data.primary_gpu,
            hybrid = render_node_of(render_node) != render_node_of(self.backend_data.primary_gpu),
            "initializing a connector"
        );

        let output_name = format!("{}-{}", connector.interface().as_str(), connector.interface_id());
        tracing::info!(output_name, ?crtc, "connector connected");

        let mode_id = connector
            .modes()
            .iter()
            .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
            .unwrap_or(0);
        let Some(&drm_mode) = connector.modes().get(mode_id) else {
            tracing::warn!(output_name, "connector has no modes");
            return;
        };
        let wl_mode = WlMode::from(drm_mode);
        let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));

        let (make, model) = connector_make_model(device.drm_output_manager.device(), &connector)
            .unwrap_or_else(|| ("Unknown".into(), "Unknown".into()));
        tracing::debug!(output_name, make, model, "monitor identity (EDID)");
        let output = Output::new(
            output_name.clone(),
            PhysicalProperties {
                size: (phys_w as i32, phys_h as i32).into(),
                subpixel: connector.subpixel().into(),
                make,
                model,
            },
        );
        let global = output.create_global::<State<DrmData>>(&self.display_handle);
        output.user_data().insert_if_missing(|| crate::lifecycle::OutputGlobal(global));
        // bspwm: monitors are laid out however RandR reports them;
        // there is no RandR here yet (`bspc output`'s `-p`/`--position`,
        // `docs/bsp-ipc.md` Hardware backend progress, is parsed but not wired to
        // real hardware), so newly connected outputs are placed
        // left-to-right in connect order, matching anvil's own default.
        let x = self
            .space
            .outputs()
            .fold(0, |acc, o| acc + self.space.output_geometry(o).map_or(0, |g| g.size.w));
        let position = (x, 0).into();
        output.set_preferred(wl_mode);
        output.change_current_state(Some(wl_mode), Some(Transform::Normal), None, Some(position));
        self.space.map_output(&output, position);
        output.user_data().insert_if_missing(|| UdevOutputId { crtc, device_id: node });

        let rect = Rect::new(position.x, position.y, wl_mode.size.w, wl_mode.size.h);
        // bspwm: an output that comes back shows its old (unwired) monitor again.
        let rewired = crate::lifecycle::rewire(&mut self.wm, &output_name);
        let mut added = None;
        if rewired.is_none() {
            let settings = self.wm.settings.clone();
            let monitor_id = self.wm.next_monitor_id();
            let mut monitor = CoreMonitor::new(
                monitor_id,
                Some(&output_name),
                Rect::new(position.x, position.y, wl_mode.size.w, wl_mode.size.h),
                &settings,
            );
            monitor.add_desktop(bsp_core::desktop::Desktop::new(
                self.wm.next_desktop_id(),
                Some(bsp_core::id::DEFAULT_DESKTOP_NAME),
                &settings,
            ));
            self.wm.add_monitor(monitor);
            if self.wm.focused_monitor.is_none() {
                self.wm.focus_monitor(self.wm.monitors.len() - 1);
            }
            added = Some(bsp_ipc::report::Event::MonitorAdd { id: monitor_id.0, name: output_name.clone(), geometry: rect });
        }

        let drm_output = match device.drm_output_manager.initialize_output::<_, DrmRenderElements<'_>>(
            crtc,
            drm_mode,
            &[connector.handle()],
            &output,
            None,
            &mut renderer,
            &Default::default(),
        ) {
            Ok(drm_output) => drm_output,
            Err(err) => {
                tracing::warn!(output_name, "failed to initialize the DRM output: {err}");
                return;
            }
        };
        device.surfaces.insert(
            crtc,
            SurfaceData {
                copy_route_failed: false,
                connector: connector.handle(),
                powered: true,
                dirty: true,
                frame_pending: false,
                modes: connector.modes().to_vec(),
                drm_output,
                gamma: None,
                regamma: false,
            },
        );
        self.adapter.hw.outputs.push(crate::hardware::HwOutput {
            name: output_name.clone(),
            modes: connector.modes().iter().map(|m| output_mode_of(*m)).collect(),
            mode: output_mode_of(drm_mode),
            scale: 1.0,
            position: (position.x, position.y),
            transform: Default::default(),
        });
        if let Some(id) = rewired {
            crate::lifecycle::rewired(self, id, rect);
        }
        // bspwm: `add_monitor()` reports `monitor_add`.
        if let Some(event) = added {
            crate::ipc::broadcast_events(self, &[event]);
        }

        // The surface starts dirty, so the main loop's `render_dirty`
        // draws its first frame.
    }

    fn connector_disconnected(&mut self, node: DrmNode, _connector: connector::Info, crtc: crtc::Handle) {
        tracing::debug!(%node, ?crtc, "connector disconnected");
        let Some(device) = self.backend_data.backends.get_mut(&node) else {
            return;
        };
        let Some(_surface) = device.surfaces.remove(&crtc) else {
            return;
        };
        let output = self
            .space
            .outputs()
            .find(|o| {
                o.user_data()
                    .get::<UdevOutputId>()
                    .is_some_and(|id| *id == UdevOutputId { device_id: node, crtc })
            })
            .cloned();
        if let Some(output) = output {
            tracing::info!(name = output.name(), "connector disconnected");
            crate::lifecycle::output_removed(self, &output, false);
        }
    }

    fn device_removed(&mut self, node: DrmNode) {
        let Some(device) = self.backend_data.backends.remove(&node) else {
            tracing::debug!(%node, "removal of an unknown DRM device ignored");
            return;
        };
        tracing::debug!(
            %node, render_node = ?device.render_node, outputs = device.surfaces.len(),
            "DRM device removed; dropping its outputs and its renderer"
        );
        for (crtc, _surface) in device.surfaces {
            let output = self
                .space
                .outputs()
                .find(|o| {
                    o.user_data()
                        .get::<UdevOutputId>()
                        .is_some_and(|id| *id == UdevOutputId { device_id: node, crtc })
                })
                .cloned();
            if let Some(output) = output {
                crate::lifecycle::output_removed(self, &output, false);
            }
        }
        self.handle.remove(device.registration_token);
        if let Some(render_node) = device.render_node {
            self.backend_data.gpus.as_mut().remove_node(&render_node);
        }
    }

}

impl State<DrmData> {
    /// `Ctrl+Alt+F<vt>`: leaves the screen in a state the next session can take
    /// over at once, then asks logind for the switch.
    ///
    /// Each output gets one last frame composited entirely on its primary plane
    /// (no overlay or cursor plane), committed synchronously while we still hold
    /// DRM master. That commit turns our other planes off, so nothing of ours is
    /// left over the next session's picture, and the CRTC stays on at its mode:
    /// the monitor keeps its signal and the next session needs no full modeset.
    /// Clearing the planes alone made a window shown on an overlay plane (direct
    /// scan-out) vanish for the moment before the switch, leaving its border.
    fn switch_vt(&mut self, vt: i32) {
        let started = std::time::Instant::now();
        tracing::info!(vt, "VT switch requested");
        self.hand_over_screen();
        tracing::info!(vt, elapsed = ?started.elapsed(), "screen handed over; switching VT");
        match self.backend_data.session.change_vt(vt) {
            Ok(()) => {
                self.backend_data.handed_over = true;
                // A switch that never happens (logind refused it quietly) must not
                // leave the screen frozen: draw again after a second.
                use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
                let _ = self.handle.insert_source(Timer::from_duration(std::time::Duration::from_secs(1)), |_, _, state| {
                    if state.backend_data.handed_over && state.backend_data.session.is_active() {
                        tracing::warn!("the VT switch did not happen; drawing again");
                        state.backend_data.handed_over = false;
                        state.backend_data.queue_redraw();
                    }
                    TimeoutAction::Drop
                });
            }
            Err(err) => {
                tracing::warn!(vt, "failed to switch VT: {err}");
                self.backend_data.queue_redraw();
            }
        }
    }

    /// Leaves every output so the next DRM master (another session, the
    /// console after an exit) can take it over at once: one final frame
    /// composited on the primary plane, our cursor and overlay planes off, and
    /// the gamma table back to identity (a gamma client's ramp, gammastep's,
    /// would otherwise stay on the next session; it is written again on
    /// resume). Must run while we still hold DRM master.
    pub(crate) fn hand_over_screen(&mut self) {
        let targets: Vec<(DrmNode, crtc::Handle)> = self
            .backend_data
            .backends
            .iter()
            .flat_map(|(node, device)| device.surfaces.keys().map(|crtc| (*node, *crtc)))
            .collect();
        for (node, crtc) in targets {
            let composited = self.composite_final_frame(node, crtc);
            // Also after a composited frame: any plane still bound to the CRTC is
            // pulled into the next session's first commit by nvidia-drm when that
            // commit changes the HDR infoframe (Hyprland's restore does), and one
            // left from us made that commit fail with EINVAL.
            self.backend_data.clear_planes(node, crtc);
            let mut gamma_reset = false;
            if let Some(device) = self.backend_data.backends.get(&node) {
                if device.surfaces.get(&crtc).is_some_and(|s| s.gamma.is_some()) {
                    match write_gamma_lut(device.drm_output_manager.device(), crtc, None) {
                        Ok(()) => gamma_reset = true,
                        Err(err) => tracing::info!(?crtc, "gamma table not reset before the hand-over: {err}"),
                    }
                }
            }
            tracing::info!(?crtc, composited, gamma_reset, "screen handed over");
        }
    }

    /// Renders the output on `crtc` with everything on the primary plane and
    /// commits it at once. `false` if that could not be done.
    fn composite_final_frame(&mut self, node: DrmNode, crtc: crtc::Handle) -> bool {
        let Some(output) = self
            .space
            .outputs()
            .find(|o| o.user_data().get::<UdevOutputId>().is_some_and(|id| *id == UdevOutputId { device_id: node, crtc }))
            .cloned()
        else {
            return false;
        };
        let Some(device) = self.backend_data.backends.get_mut(&node) else { return false };
        let render_node = device.render_node.unwrap_or(self.backend_data.primary_gpu);
        let Some(surface) = device.surfaces.get_mut(&crtc) else { return false };
        let Ok(mut renderer) = renderer_for(
            &mut self.backend_data.gpus,
            self.backend_data.primary_gpu,
            render_node,
            surface.drm_output.format(),
            surface.copy_route_failed,
        ) else {
            return false;
        };
        // No cursor: it would stay drawn over the next session's picture.
        let Some(elements) = crate::render::output_elements(&output, &self.space, &self.wm, &mut renderer, Vec::new()) else {
            return false;
        };
        let result = match surface.drm_output.render_frame(&mut renderer, &elements, crate::render::CLEAR_COLOR, FrameFlags::empty()) {
            Ok(result) => result,
            Err(err) => {
                tracing::info!(?crtc, "final frame not rendered: {err}");
                return false;
            }
        };
        if result.needs_sync() {
            if let smithay::backend::drm::compositor::PrimaryPlaneElement::Swapchain(element) = &result.primary_element {
                if let Err(err) = element.sync.wait() {
                    tracing::debug!(?crtc, "waiting for the final frame was interrupted: {err:?}");
                }
            }
        }
        if result.is_empty {
            return false;
        }
        match surface.drm_output.commit_frame() {
            Ok(()) => true,
            Err(err) => {
                tracing::info!(?crtc, "final frame not committed: {err}");
                false
            }
        }
    }

    /// `DrmEvent::VBlank`: the previously queued frame finished scanning
    /// out — marks it submitted (releasing its swapchain buffer) and lets
    /// a waiting redraw proceed (`render_dirty`, run by the main loop
    /// right after this event is dispatched). Rendering is demand-driven
    /// — nothing polls when the screen is static — so unlike anvil's
    /// fixed-cadence timer this schedules nothing itself.
    fn frame_finish(
        &mut self,
        node: DrmNode,
        crtc: crtc::Handle,
        metadata: &mut Option<smithay::backend::drm::DrmEventMetadata>,
    ) {
        if let Some(resumed) = self.backend_data.resumed_at.take() {
            tracing::info!(?crtc, elapsed = ?resumed.elapsed(), "first frame on screen after the session resumed");
        }
        let refresh = self
            .space
            .outputs()
            .find(|o| o.user_data().get::<UdevOutputId>().is_some_and(|id| *id == UdevOutputId { device_id: node, crtc }))
            .and_then(|o| o.current_mode())
            .map(|mode| mode.refresh)
            .filter(|&mhz| mhz > 0);
        let now = self.clock.now();
        let Some(device) = self.backend_data.backends.get_mut(&node) else {
            return;
        };
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };
        tracing::debug!(?crtc, "vblank");
        surface.frame_pending = false;
        let regamma = if std::mem::take(&mut surface.regamma) { surface.gamma.clone() } else { None };
        match surface.drm_output.frame_submitted() {
            Ok(Some(Some(mut feedback))) => {
                // `wp_presentation`: the frame reached the screen. The
                // clock is CLOCK_MONOTONIC; use the kernel's timestamp when
                // it gave one in that clock, else "now".
                let time: smithay::utils::Time<smithay::utils::Monotonic> = match metadata.map(|m| m.time) {
                    Some(smithay::backend::drm::DrmEventTime::Monotonic(d)) => d.into(),
                    _ => now,
                };
                let seq = metadata.map_or(0, |m| u64::from(m.sequence));
                let refresh = smithay::wayland::presentation::Refresh::fixed(Duration::from_secs_f64(
                    1000.0 / refresh.unwrap_or(60_000) as f64,
                ));
                use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind;
                feedback.presented::<_, smithay::utils::Monotonic>(time, refresh, seq, Kind::Vsync | Kind::HwClock | Kind::HwCompletion);
            }
            Ok(_) => {}
            Err(err) => tracing::warn!(?crtc, "failed to mark a frame submitted: {err}"),
        }
        if let (Some(ramp), Some(device)) = (regamma, self.backend_data.backends.get(&node)) {
            if let Err(err) = write_gamma_lut(device.drm_output_manager.device(), crtc, Some(&ramp)) {
                tracing::warn!(?crtc, "failed to restore the gamma ramp after a resume: {err}");
            }
        }
    }

    /// Renders every surface that is dirty and has no frame in flight.
    /// Called by the main loop after each event-loop turn; a dirty
    /// surface whose frame is still pending is picked up on the turn
    /// after its vblank.
    fn render_dirty(&mut self) {
        if !self.backend_data.session.is_active() || self.backend_data.handed_over {
            return;
        }
        let mut due = Vec::new();
        for (node, device) in &mut self.backend_data.backends {
            for (crtc, surface) in &mut device.surfaces {
                if surface.dirty && !surface.frame_pending && surface.powered {
                    surface.dirty = false;
                    due.push((*node, *crtc));
                }
            }
        }
        for (node, crtc) in due {
            let now = self.clock.now();
            self.render_surface(node, crtc, now);
        }
    }

    /// Renders and queues the next frame for `crtc` on GPU node `node` —
    /// builds the element list via `crate::render::output_elements`
    /// (shared with the winit backend), calls `DrmOutput::render_frame`,
    /// and `queue_frame`s it for scanout only if something actually
    /// changed (an empty frame is not queued — `frame_finish` above is
    /// only reachable once a frame *was* queued, so a no-damage frame
    /// simply reschedules itself here instead, matching anvil's own
    /// `render_surface` reschedule-on-no-damage path).
    fn render_surface(&mut self, node: DrmNode, crtc: crtc::Handle, frame_target: smithay::utils::Time<smithay::utils::Monotonic>) {
        // Paused (switched to another VT): the DRM device is not ours to
        // touch, and rescheduling would only spin on "device paused"
        // errors — the session's resume handler re-kicks rendering.
        if !self.backend_data.session.is_active() || self.backend_data.handed_over {
            return;
        }
        let Some(output) = self
            .space
            .outputs()
            .find(|o| {
                o.user_data()
                    .get::<UdevOutputId>()
                    .is_some_and(|id| *id == UdevOutputId { device_id: node, crtc })
            })
            .cloned()
        else {
            return;
        };
        let Some(device) = self.backend_data.backends.get_mut(&node) else {
            return;
        };
        let render_node = device.render_node.unwrap_or(self.backend_data.primary_gpu);
        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            return;
        };

        let mut renderer = match renderer_for(
            &mut self.backend_data.gpus,
            self.backend_data.primary_gpu,
            render_node,
            surface.drm_output.format(),
            surface.copy_route_failed,
        ) {
            Ok(renderer) => renderer,
            Err(err) => {
                tracing::warn!(?render_node, "failed to create a renderer: {err}");
                self.reschedule(node, crtc, frame_target);
                return;
            }
        };

        let output_loc = self.space.output_geometry(&output).map_or_else(Default::default, |g| g.loc);
        let cursor = crate::cursor::cursor_elements(
            &mut renderer,
            &mut self.backend_data.cursor_images,
            &self.cursor_status,
            self.pointer.current_location() - output_loc.to_f64(),
            smithay::utils::Scale::from(output.current_scale().fractional_scale()),
        );
        let Some(elements) = crate::render::output_elements(&output, &self.space, &self.wm, &mut renderer, cursor)
        else {
            self.reschedule(node, crtc, frame_target);
            return;
        };

        let render_result = surface.drm_output.render_frame(
            &mut renderer,
            &elements,
            crate::render::CLEAR_COLOR,
            FrameFlags::DEFAULT,
        );
        match render_result {
            Ok(result) => {
                if !result.is_empty {
                    self.protocols.screencopy.frames_rendered += 1;
                    // No fence can be handed to KMS (e.g. no explicit-sync
                    // support): the render must be finished before the
                    // buffer is scanned out, or a static frame stays
                    // partly drawn forever (`RenderFrameResult::needs_sync`;
                    // anvil's `render_surface` does the same wait).
                    if result.needs_sync() {
                        if let smithay::backend::drm::compositor::PrimaryPlaneElement::Swapchain(element) =
                            &result.primary_element
                        {
                            if let Err(err) = element.sync.wait() {
                                tracing::warn!(?crtc, "waiting for the render to finish was interrupted: {err:?}");
                            }
                        }
                    }
                    tracing::debug!(?crtc, "queueing a frame");
                    // Feedback for every surface shown by this frame.
                    let mut feedback = smithay::desktop::utils::OutputPresentationFeedback::new(&output);
                    for window in self.space.elements() {
                        window.take_presentation_feedback(
                            &mut feedback,
                            |_, _| Some(output.clone()),
                            |_, _| smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback::Kind::Vsync,
                        );
                    }
                    match surface.drm_output.queue_frame(Some(feedback)) {
                        Ok(()) => surface.frame_pending = true,
                        Err(err) => tracing::warn!(?crtc, "failed to queue a frame: {err}"),
                    }
                }
                // Frame callbacks go out even for an empty frame: a client
                // that committed without visible damage still waits on one
                // to draw its next frame.
                crate::extras::finish_frame(self, &output);
            }
            Err(err) => {
                let hybrid = render_node_of(render_node) != render_node_of(self.backend_data.primary_gpu);
                if hybrid && !surface.copy_route_failed {
                    // Rendering on the primary GPU and copying to this
                    // output's GPU did not work (typically: no dmabuf
                    // both GPUs can import). Render on this GPU alone
                    // from now on — shm clients still work; a dmabuf
                    // client living on the other GPU may not show.
                    tracing::warn!(
                        ?crtc, %render_node,
                        "multi-GPU copy route failed ({err}); this output now renders on its own GPU only"
                    );
                    surface.copy_route_failed = true;
                    surface.drm_output.reset_buffers();
                } else {
                    tracing::warn!(?crtc, "failed to render a frame: {err}");
                }
                // Retry on a timer (a `dirty` flag alone would wait for the
                // next unrelated event to wake the loop).
                self.reschedule(node, crtc, frame_target);
            }
        }
    }

    /// No damage, or a recoverable render error: try again next frame
    /// rather than waiting for a vblank that will never come (nothing
    /// was queued, so `frame_finish` has nothing to fire from).
    fn reschedule(&mut self, node: DrmNode, crtc: crtc::Handle, frame_target: smithay::utils::Time<smithay::utils::Monotonic>) {
        // One refresh interval of the output (60 Hz when it does not say).
        let refresh_mhz = self
            .space
            .outputs()
            .find(|o| o.user_data().get::<UdevOutputId>().is_some_and(|id| *id == UdevOutputId { device_id: node, crtc }))
            .and_then(|o| o.current_mode())
            .map(|mode| mode.refresh)
            .filter(|&mhz| mhz > 0)
            .unwrap_or(60_000);
        let frame_duration = Duration::from_secs_f64(1000.0 / f64::from(refresh_mhz));
        let next_frame_target = frame_target + frame_duration;
        if let Err(err) = self.handle.insert_source(Timer::from_duration(frame_duration), move |_, _, data| {
            data.render_surface(node, crtc, next_frame_target);
            TimeoutAction::Drop
        }) {
            tracing::warn!(?crtc, "failed to schedule the next frame: {err}");
        }
    }
}

/// This backend's `InputEvent` dispatch, the real-`libinput` sibling of
/// `crate::input::process_input_event` (the windowed one) — confirmed
/// from anvil's own `udev.rs`/`input_handler.rs` that these stay two
/// separate functions rather than one shared across both `InputBackend`
/// shapes, since `WinitInputBackend` only ever sends *absolute* pointer
/// motion (there is no window to be relative *within*) while real
/// `libinput` devices send *relative* motion, needing genuinely
/// different handling — everything else (keyboard, buttons, axis) is
/// identical and reused as-is from `crate::input`.
fn process_input_event(state: &mut State<DrmData>, event: InputEvent<LibinputInputBackend>) {
    // A key or click right after a command goes to what the command left
    // focused and shown.
    crate::shell::run_deferred_sync(state);
    // Any input may move the pointer, change focus or re-tile — and is
    // user activity for idle timers.
    state.backend_data.queue_redraw();
    crate::protocols::notify_activity(state);
    match event {
        InputEvent::DeviceAdded { mut device } => {
            tracing::info!(name = device.name(), "input device added");
            if device.has_capability(smithay::reexports::input::DeviceCapability::TabletTool) {
                crate::devices::tablet_added(state, &device);
            }
            if device.has_capability(smithay::reexports::input::DeviceCapability::Keyboard) {
                if let Some(led_state) = state.seat.get_keyboard().map(|k| k.led_state()) {
                    device.led_update(led_state.into());
                }
                state.backend_data.keyboards.push(device);
            } else if device.has_capability(smithay::reexports::input::DeviceCapability::Pointer) {
                let name = device.name().to_string();
                let accel = if device.config_accel_is_available() { device.config_accel_speed() } else { 0.0 };
                state.adapter.hw.pointers.push((name, accel));
                state.backend_data.pointers.push(device);
            }
        }
        InputEvent::DeviceRemoved { device } => {
            if device.has_capability(smithay::reexports::input::DeviceCapability::TabletTool) {
                crate::devices::tablet_removed(state, &device);
            }
            state.backend_data.keyboards.retain(|d| d != &device);
            if state.backend_data.pointers.iter().any(|d| d == &device) {
                state.backend_data.pointers.retain(|d| d != &device);
                let name = device.name();
                state.adapter.hw.pointers.retain(|(n, _)| n != name);
            }
        }
        InputEvent::Keyboard { event } => {
            let keycode = event.key_code();
            let key_state = event.state();
            let pressed = key_state == KeyState::Pressed;
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            let time = smithay::backend::input::Event::time_msec(&event);
            let Some(keyboard) = state.seat.get_keyboard() else {
                return;
            };
            let mut switch_to = None;
            keyboard.input::<(), _>(state, keycode, key_state, serial, time, |data, mods, sym| {
                if crate::input::is_emergency_quit(mods, &sym, pressed) {
                    data.running = false;
                    return FilterResult::Intercept(());
                }
                if let Some(vt) = crate::input::vt_switch_target(mods, &sym, pressed) {
                    switch_to = Some(vt);
                    return FilterResult::Intercept(());
                }
                crate::hotkeys::filter(data, mods, sym, pressed)
            });
            // Outside the keyboard's own handling: this renders and commits.
            if let Some(vt) = switch_to {
                state.switch_vt(vt);
            }
        }
        InputEvent::PointerMotion { event } => {
            use smithay::backend::input::PointerMotionEvent;
            crate::constraints::relative_motion(
                state,
                event.delta(),
                event.delta_unaccel(),
                event.time(),
                smithay::backend::input::Event::time_msec(&event),
            );
        }
        InputEvent::PointerButton { event } => crate::input::on_pointer_button(state, event),
        InputEvent::PointerAxis { event } => crate::input::on_pointer_axis(state, event),
        other => {
            crate::devices::process::<LibinputInputBackend, DrmData>(state, other);
        }
    }
}

/// Runs the real hardware backend: opens the session, sets up the GPU
/// renderer manager, opens every already-connected DRM device (and
/// reacts to later hotplug), wires real `libinput` input, then serves
/// Wayland clients and the `bspc` control socket exactly like the
/// nested backend does — every piece from `hotkeys`/`bspwmrc`/`ipc`
/// onward is fully shared, generic `State<B>` code (`docs/design.md`
/// roadmap, the hardware backend).
pub fn run() {
    let mut event_loop: EventLoop<State<DrmData>> = match EventLoop::try_new() {
        Ok(l) => l,
        Err(err) => {
            tracing::error!("failed to create the event loop: {err}");
            return;
        }
    };
    let display: Display<State<DrmData>> = match Display::new() {
        Ok(d) => d,
        Err(err) => {
            tracing::error!("failed to create the Wayland display: {err}");
            return;
        }
    };
    let display_handle = display.handle();

    let (session, notifier) = match LibSeatSession::new() {
        Ok(pair) => pair,
        Err(err) => {
            tracing::error!("failed to open a libseat session: {err}");
            return;
        }
    };
    let seat = session.seat();
    tracing::info!(seat, active = session.is_active(), "opened libseat session");

    let boot_gpu = udev::primary_gpu(&seat).ok().flatten();
    let all_gpus = udev::all_gpus(&seat).unwrap_or_default();
    tracing::debug!(?boot_gpu, ?all_gpus, "GPUs on this seat");
    let (primary_gpu, why) = match boot_gpu
        .and_then(|path| DrmNode::from_path(path).ok())
        .map(render_node_of)
    {
        Some(node) => (Some(node), "the boot GPU (boot_vga)"),
        None => (
            all_gpus
                .into_iter()
                .find_map(|path| DrmNode::from_path(path).ok())
                .map(render_node_of),
            "the first GPU found (no boot GPU reported)",
        ),
    };
    let Some(primary_gpu) = primary_gpu else {
        tracing::error!("no GPU found on this seat");
        return;
    };
    tracing::info!(%primary_gpu, "using primary GPU");
    tracing::debug!(%primary_gpu, why, "primary GPU chosen");

    let gbm_backend = GbmGlesBackend::with_context_priority(ContextPriority::High);
    let gpus = match GpuManager::new(gbm_backend) {
        Ok(gpus) => gpus,
        Err(err) => {
            tracing::error!("failed to create the GPU manager: {err}");
            return;
        }
    };

    let backend_data = DrmData {
        session,
        primary_gpu,
        gpus,
        backends: HashMap::new(),
        keyboards: Vec::new(),
        pointers: Vec::new(),
        cursor_images: crate::cursor::CursorImages::load(),
        resumed_at: None,
        handed_over: false,
    };

    let hotkeys = crate::hotkeys::init();
    let wm = Wm::new(Settings::default());
    let mut state = State::new(display_handle.clone(), event_loop.handle(), backend_data, wm, hotkeys);

    let udev_backend = match UdevBackend::new(state.backend_data.seat_name()) {
        Ok(backend) => backend,
        Err(err) => {
            tracing::error!("failed to open the udev backend: {err}");
            return;
        }
    };
    // The primary GPU first: a secondary GPU's setup (format intersection,
    // copy route) needs the primary's renderer to already exist, and udev
    // lists devices in no particular order (a two-GPU VM listed the
    // secondary first).
    let mut initial_devices: Vec<(DrmNode, &std::path::Path)> = udev_backend
        .device_list()
        .filter_map(|(device_id, path)| Some((DrmNode::from_dev_id(device_id).ok()?, path)))
        .collect();
    initial_devices.retain(|(node, _)| node.ty() == NodeType::Primary);
    initial_devices.sort_by_key(|(node, _)| render_node_of(*node) != render_node_of(primary_gpu));
    tracing::debug!(
        devices = ?initial_devices.iter().map(|(node, _)| node.to_string()).collect::<Vec<_>>(),
        "initial DRM devices, primary first"
    );
    for (node, path) in initial_devices {
        state.device_added(node, path);
    }

    // `zwp_linux_dmabuf_v1`: advertise what the primary GPU's renderer can
    // import, with that GPU as the feedback's main device — clients then
    // render straight into buffers this compositor can scan out or
    // texture from, with no shm copy.
    {
        use smithay::backend::renderer::ImportDma;
        use smithay::wayland::dmabuf::DmabufFeedbackBuilder;
        let primary = state.backend_data.primary_gpu;
        let render_node = state
            .backend_data
            .backends
            .get(&primary)
            .and_then(|d| d.render_node)
            .unwrap_or(primary);
        let formats = match state.backend_data.gpus.single_renderer(&render_node) {
            Ok(renderer) => Some(renderer.dmabuf_formats().iter().copied().collect::<Vec<_>>()),
            Err(err) => {
                tracing::warn!("no renderer for dmabuf formats: {err}");
                None
            }
        };
        if let Some(formats) = formats {
            match DmabufFeedbackBuilder::new(render_node.dev_id(), formats).build() {
                Ok(feedback) => {
                    state
                        .dmabuf_state
                        .create_global_with_default_feedback::<State<DrmData>>(&display_handle, &feedback);
                    tracing::info!(%render_node, "linux-dmabuf enabled");
                }
                Err(err) => tracing::warn!("failed to build dmabuf feedback: {err}"),
            }
        }
    }

    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(
            state.backend_data.session.clone().into(),
        );
    if libinput_context.udev_assign_seat(&state.backend_data.seat_name()).is_err() {
        tracing::error!("failed to assign the seat to libinput");
        return;
    }
    // libinput only queues its initial "device added" events on a first
    // `dispatch()`, and its fd is not readable until then — so without
    // this the event loop would not deliver them until the first real
    // input event, leaving keyboards' LEDs and the `bspc input` device
    // list unset until someone touches something. Drain them now.
    if let Err(err) = libinput_context.dispatch() {
        tracing::warn!("initial libinput dispatch failed: {err}");
    }
    let initial_devices: Vec<_> = (&mut libinput_context)
        .filter_map(|event| match event {
            smithay::reexports::input::Event::Device(smithay::reexports::input::event::DeviceEvent::Added(added)) => {
                Some(smithay::reexports::input::event::EventTrait::device(&added))
            }
            _ => None,
        })
        .collect();
    for device in initial_devices {
        process_input_event(&mut state, InputEvent::DeviceAdded { device });
    }
    // Kept for the session notifier below: libinput must be suspended when the
    // session is paused (VT switch away) and resumed when it is active again,
    // or every input device stays closed after the way back.
    let mut libinput_session = libinput_context.clone();
    let libinput_backend = LibinputInputBackend::new(libinput_context);

    let handle: LoopHandle<'static, State<DrmData>> = event_loop.handle();

    if let Err(err) = handle.insert_source(libinput_backend, |event, _, state| {
        process_input_event(state, event);
    }) {
        tracing::error!("failed to register the libinput backend: {err}");
        return;
    }

    if let Err(err) = handle.insert_source(notifier, move |event, (), state| match event {
        SessionEvent::PauseSession => {
            tracing::info!("session paused");
            libinput_session.suspend();
            for device in state.backend_data.backends.values_mut() {
                // Too late to clear anything here (the kernel already took DRM master away on
                // the switch); `DrmData::clear_outputs` runs before we ask for a switch.
                device.drm_output_manager.pause();
            }
        }
        SessionEvent::ActivateSession => {
            tracing::info!("session resumed");
            state.backend_data.resumed_at = Some(std::time::Instant::now());
            state.backend_data.handed_over = false;
            if libinput_session.resume().is_err() {
                tracing::warn!("failed to resume libinput: input devices stay closed");
            }
            for (node, device) in &mut state.backend_data.backends {
                // The manager also resets each output's view of its planes, which
                // `clear_outputs` changed behind its back before the switch.
                if let Err(err) = device.drm_output_manager.activate(false) {
                    tracing::warn!(%node, "failed to reactivate a DRM device: {err}");
                }
            }
            // Put the gamma ramps back before the first frame: the other session left
            // its own LUT, and waiting for that frame showed the wrong colours for a moment.
            for (node, device) in &state.backend_data.backends {
                for (crtc, surface) in &device.surfaces {
                    if let Some(ramp) = &surface.gamma {
                        if let Err(err) = write_gamma_lut(device.drm_output_manager.device(), *crtc, Some(ramp)) {
                            tracing::debug!(%node, "gamma ramp not restored at the resume: {err}");
                        }
                    }
                }
            }
            let crtcs: Vec<(DrmNode, crtc::Handle)> = state
                .backend_data
                .backends
                .iter()
                .flat_map(|(node, device)| device.surfaces.keys().map(|crtc| (*node, *crtc)))
                .collect();
            for (node, crtc) in crtcs {
                // A VT-switch resume invalidates every previously
                // queued buffer's contents (`Backend::reset_buffers`),
                // so the next frame must be rendered from scratch
                // rather than relying on stale damage/buffer-age state.
                if let Some(output) = state.space.outputs().find(|o| {
                    o.user_data()
                        .get::<UdevOutputId>()
                        .is_some_and(|id| *id == UdevOutputId { device_id: node, crtc })
                }) {
                    let output = output.clone();
                    state.backend_data.reset_buffers(&output);
                }
                // Queued frames died with the pause.
                if let Some(surface) = state
                    .backend_data
                    .backends
                    .get_mut(&node)
                    .and_then(|d| d.surfaces.get_mut(&crtc))
                {
                    surface.frame_pending = false;
                    surface.dirty = true;
                    surface.regamma = surface.gamma.is_some();
                }
            }
        }
    }) {
        tracing::error!("failed to register the session notifier: {err}");
        return;
    }

    if let Err(err) = handle.insert_source(udev_backend, |event, _, state| match event {
        UdevEvent::Added { device_id, path } => {
            tracing::debug!(device_id, ?path, "udev: DRM device added");
            match DrmNode::from_dev_id(device_id) {
                // udev also reports each GPU's render node (`renderD*`);
                // it is the same GPU as its card node, which is opened
                // (opening the render node as a DRM device fails).
                Ok(node) if node.ty() != NodeType::Primary => {
                    tracing::debug!(%node, "udev: ignoring a non-primary DRM node");
                }
                Ok(node) => state.device_added(node, &path),
                Err(err) => tracing::debug!(device_id, "udev: not a usable DRM node ({err}); ignored"),
            }
        }
        UdevEvent::Changed { device_id } => {
            tracing::debug!(device_id, "udev: DRM device changed (connector hotplug?)");
            if let Ok(node) = DrmNode::from_dev_id(device_id) {
                state.device_changed(node);
            }
        }
        UdevEvent::Removed { device_id } => {
            tracing::debug!(device_id, "udev: DRM device removed");
            // `DrmNode::from_dev_id` looks the device up in sysfs, which is
            // already gone for a removed device — match the known devices
            // by their dev_t instead.
            let known = state
                .backend_data
                .backends
                .keys()
                .copied()
                .find(|node| node.dev_id() == device_id);
            match known {
                Some(node) => state.device_removed(node),
                None => tracing::debug!(device_id, "udev: removed device is not one we opened; ignored"),
            }
        }
    }) {
        tracing::error!("failed to register the udev backend: {err}");
        return;
    }

    // SAFETY: `display` is moved into the `Generic` source below and is
    // not touched again outside `dispatch_clients`, which Smithay
    // requires for this call — the same pattern as the nested backend
    // (`winit_backend.rs`'s own identical `SAFETY` comment).
    if let Err(err) = handle.insert_source(
        smithay::reexports::calloop::generic::Generic::new(
            display,
            smithay::reexports::calloop::Interest::READ,
            smithay::reexports::calloop::Mode::Level,
        ),
        |_, display, state| {
            // SAFETY: see the comment on the `insert_source` call above.
            unsafe {
                display.get_mut().dispatch_clients(state)?;
            }
            Ok(smithay::reexports::calloop::PostAction::Continue)
        },
    ) {
        tracing::error!("failed to register the Wayland display with the event loop: {err}");
        return;
    }

    let socket_source = match smithay::wayland::socket::ListeningSocketSource::new_auto() {
        Ok(s) => s,
        Err(err) => {
            tracing::error!("failed to create the Wayland listening socket: {err}");
            return;
        }
    };
    let socket_name = socket_source.socket_name().to_string_lossy().into_owned();
    if let Err(err) = handle.insert_source(socket_source, |stream, _, state| {
        insert_client(&state.display_handle, stream);
    }) {
        tracing::error!("failed to register the Wayland socket with the event loop: {err}");
        return;
    }
    tracing::info!(socket = socket_name, "listening on Wayland socket");
    // SAFETY: `WAYLAND_DISPLAY` is process environment, set once here
    // before any client (including `bspwmrc`) could read it; nothing
    // else in this process touches the environment concurrently at this
    // point in startup — the same reasoning as the nested backend's own
    // identical `SAFETY` comment.
    unsafe {
        std::env::set_var("WAYLAND_DISPLAY", &socket_name);
    }

    crate::state::export_session_env();
    crate::xwayland::init(&mut state);
    crate::hotkeys::canonicalize_virtual_modifiers(&mut state);
    crate::ipc::init(&mut state);
    crate::bspwmrc::run();

    match smithay::reexports::calloop::signals::Signals::new(&[
        smithay::reexports::calloop::signals::Signal::SIGUSR1,
    ]) {
        Ok(signals) => {
            if let Err(err) = handle.insert_source(signals, |_, _, state| {
                tracing::info!("SIGUSR1: reloading sxhkdrc");
                crate::hotkeys::reload(state);
            }) {
                tracing::warn!("failed to register the SIGUSR1 handler: {err}");
            }
        }
        Err(err) => tracing::warn!("failed to set up SIGUSR1 handling: {err}"),
    }

    crate::state::init_quit_signals(&mut state);

    tracing::info!("real hardware compositor ready");

    while state.running {
        if event_loop.dispatch(None, &mut state).is_err() {
            state.running = false;
        } else {
            // The one reconcile of this turn, before anything reads the space.
            crate::shell::run_deferred_sync(&mut state);
            state.space.refresh();
            state.popups.cleanup();
            crate::protocols::refresh_idle_inhibit(&mut state);
            crate::extras::periodic_syncs(&mut state);
            crate::screencopy::fulfill(&mut state);
            crate::ext_capture::fulfill(&mut state);
            crate::export_dmabuf::fulfill(&mut state);
            state.wm.sync_history();
            crate::extras::arm_commit_timers(&mut state);
            crate::session_lock::poll(&mut state);
            state.render_dirty();
            let _ = display_handle.clone().flush_clients();
        }
    }
    // Leave the screen to the console or the next session as a VT switch does
    // (Smithay's own restore of the start-up state fails on NVIDIA: the
    // framebuffer it captured is gone by now).
    if state.backend_data.session.is_active() {
        state.hand_over_screen();
    }
}
