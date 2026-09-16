//! `wlr-screencopy-unstable-v1`: `grim` (screenshots), `wf-recorder` and
//! `wayvnc` read the screen. bspwm has no counterpart — under X11 any client
//! may read the root window — so this is the deliberate, protocol form.
//!
//! A client asks to capture an output (or a region of it), the compositor
//! announces the shm buffer format and size it expects, the client attaches
//! a matching `wl_shm` buffer and says `copy`, and the compositor renders
//! the output off-screen into it and answers `ready` (or `failed`).
//!
//! Whole-output captures may also target a `linux-dmabuf` buffer (the
//! `linux_dmabuf` event, rendered on the GPU without a CPU read-back); shm
//! is what `grim` uses and is always offered. The actual render is the backend's ([`Backend::capture_output`]);
//! this module owns the protocol and the copy into the client's buffer.
//! `copy_with_damage` waits for the next rendered frame before answering
//! (so a recorder is paced by real screen updates) and reports the whole
//! frame as damaged.

use smithay::input::pointer::CursorImageStatus;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};
use smithay::utils::{Logical, Point};
use wayland_protocols_wlr::screencopy::v1::server::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};

use crate::state::{Backend, State};

/// The pixel format offered: `Argb8888` (`grim` accepts it).
pub(crate) const FORMAT: wl_shm::Format = wl_shm::Format::Argb8888;

/// A whole output rendered off-screen: `Argb8888`, top row first.
pub struct CapturedFrame {
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// `width * height * 4` bytes.
    pub data: Vec<u8>,
}

/// What a backend needs to render an output for a capture.
#[cfg_attr(not(feature = "real"), allow(dead_code))]
pub struct CaptureRequest<'a> {
    /// The output to render.
    pub output: &'a Output,
    /// The window space.
    pub space: &'a smithay::desktop::Space<smithay::desktop::Window>,
    /// The window manager (for borders).
    pub wm: &'a bsp_core::wm::Wm,
    /// The cursor to draw, if requested: its image and output-local position.
    pub cursor: Option<(&'a CursorImageStatus, Point<f64, Logical>)>,
    /// Capture only this window (`ext-foreign-toplevel-image-capture-source`):
    /// its surface tree alone, sized to its geometry, drawn at `output`'s scale.
    pub window: Option<&'a smithay::desktop::Window>,
}

/// What a backend can render into a client's `linux-dmabuf` buffer.
pub struct DmabufCaps {
    /// `dev_t` of the render node the buffers must be usable on.
    pub device: u64,
    /// Supported `(format, modifiers)` pairs.
    pub formats: Vec<(smithay::backend::allocator::Fourcc, Vec<smithay::backend::allocator::Modifier>)>,
}

/// The `(format, modifiers)` pairs `caps` supports for a whole-output
/// capture, most preferred (`Argb8888`) first.
pub(crate) fn preferred_formats(caps: &DmabufCaps) -> Vec<&(smithay::backend::allocator::Fourcc, Vec<smithay::backend::allocator::Modifier>)> {
    use smithay::backend::allocator::Fourcc;
    let mut formats: Vec<_> = caps.formats.iter().collect();
    formats.sort_by_key(|(code, _)| match code {
        Fourcc::Argb8888 => 0,
        Fourcc::Xrgb8888 => 1,
        _ => 2,
    });
    formats
}

/// Whether `buffer` is a `linux-dmabuf` buffer of exactly `w`×`h` in a
/// format and modifier `caps` offers.
pub(crate) fn dmabuf_matches(buffer: &WlBuffer, w: i32, h: i32, caps: &DmabufCaps) -> bool {
    use smithay::backend::allocator::Buffer;
    let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(buffer) else {
        return false;
    };
    let size = dmabuf.size();
    let format = dmabuf.format();
    size.w == w && size.h == h && caps.formats.iter().any(|(code, mods)| *code == format.code && mods.contains(&format.modifier))
}

/// User data of a frame object.
pub struct FrameData {
    output: Option<Output>,
    /// Captured area in the output's pixels: `(x, y, width, height)`.
    region: (i32, i32, i32, i32),
    overlay_cursor: bool,
}

struct Pending {
    frame: ZwlrScreencopyFrameV1,
    buffer: WlBuffer,
    /// `Some(n)`: wait until more than `n` frames have been rendered.
    wait_after: Option<u64>,
}

/// Per-protocol state.
#[derive(Default)]
pub struct Screencopy {
    pending: Vec<Pending>,
    /// Frames the backend has put on screen (paces `copy_with_damage`).
    pub frames_rendered: u64,
}

impl<Bd: Backend + 'static> GlobalDispatch<ZwlrScreencopyManagerV1, (), State<Bd>> for State<Bd> {
    fn can_view(client: Client, _: &()) -> bool {
        crate::state::is_privileged(&client)
    }

    fn bind(
        _state: &mut State<Bd>,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrScreencopyManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        data_init.init(resource, ());
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrScreencopyManagerV1, (), State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _client: &Client,
        _manager: &ZwlrScreencopyManagerV1,
        request: zwlr_screencopy_manager_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        use zwlr_screencopy_manager_v1::Request;
        let (frame, overlay_cursor, output, region) = match request {
            Request::CaptureOutput { frame, overlay_cursor, output } => (frame, overlay_cursor != 0, output, None),
            Request::CaptureOutputRegion { frame, overlay_cursor, output, x, y, width, height } => {
                (frame, overlay_cursor != 0, output, Some((x, y, width, height)))
            }
            _ => return,
        };
        let output = Output::from_resource(&output);
        // The output's pixel size and scale decide the buffer size.
        let geometry = output.as_ref().and_then(|o| {
            let mode = o.current_mode()?;
            Some((mode.size.w, mode.size.h, o.current_scale().fractional_scale()))
        });
        let (Some((out_w, out_h, scale)), true) = (geometry, output.is_some()) else {
            let frame = data_init.init(frame, FrameData { output: None, region: (0, 0, 0, 0), overlay_cursor });
            frame.failed();
            return;
        };
        // A region is in the output's logical coordinates; the buffer is in pixels.
        let (x, y, w, h) = match region {
            None => (0, 0, out_w, out_h),
            Some((x, y, w, h)) => (
                (x as f64 * scale).round() as i32,
                (y as f64 * scale).round() as i32,
                (w as f64 * scale).round() as i32,
                (h as f64 * scale).round() as i32,
            ),
        };
        let (x0, y0) = (x.clamp(0, out_w), y.clamp(0, out_h));
        let (w, h) = (w.min(out_w - x0), h.min(out_h - y0));
        let frame = data_init.init(frame, FrameData { output, region: (x0, y0, w, h), overlay_cursor });
        if w <= 0 || h <= 0 {
            frame.failed();
            return;
        }
        tracing::debug!(w, h, overlay_cursor, "screencopy: capture requested");
        frame.buffer(FORMAT, w as u32, h as u32, w as u32 * 4);
        if frame.version() >= 3 {
            // A whole-output capture can also go into a dmabuf.
            if region.is_none() {
                if let Some(caps) = state.backend_data.capture_dmabuf_caps() {
                    for (code, _) in preferred_formats(&caps) {
                        frame.linux_dmabuf(*code as u32, w as u32, h as u32);
                    }
                }
            }
            frame.buffer_done();
        }
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrScreencopyFrameV1, FrameData, State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _client: &Client,
        frame: &ZwlrScreencopyFrameV1,
        request: zwlr_screencopy_frame_v1::Request,
        data: &FrameData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        use zwlr_screencopy_frame_v1::Request;
        let (buffer, with_damage) = match request {
            Request::Copy { buffer } => (buffer, false),
            Request::CopyWithDamage { buffer } => (buffer, true),
            _ => return,
        };
        if state.protocols.screencopy.pending.iter().any(|p| &p.frame == frame) {
            frame.post_error(zwlr_screencopy_frame_v1::Error::AlreadyUsed, "frame already copied");
            return;
        }
        // The buffer must be shm with the announced format, size and
        // stride, or a dmabuf of the announced size.
        let (_, _, w, h) = data.region;
        let ok = smithay::wayland::shm::with_buffer_contents(&buffer, |_, _, meta| {
            meta.format == FORMAT && meta.width == w && meta.height == h && meta.stride == w * 4
        })
        .unwrap_or_else(|_| {
            state.backend_data.capture_dmabuf_caps().is_some_and(|caps| dmabuf_matches(&buffer, w, h, &caps))
        });
        if !ok {
            frame.post_error(zwlr_screencopy_frame_v1::Error::InvalidBuffer, "buffer does not match the announced format/size");
            return;
        }
        let wait_after = with_damage.then_some(state.protocols.screencopy.frames_rendered);
        state.protocols.screencopy.pending.push(Pending { frame: frame.clone(), buffer, wait_after });
        state.backend_data.queue_redraw();
    }

    fn destroyed(
        state: &mut State<Bd>,
        _client: smithay::reexports::wayland_server::backend::ClientId,
        frame: &ZwlrScreencopyFrameV1,
        _: &FrameData,
    ) {
        state.protocols.screencopy.pending.retain(|p| &p.frame != frame);
    }
}

/// Answers every capture that is ready. Called after each event-loop turn.
pub fn fulfill<Bd: Backend + 'static>(state: &mut State<Bd>) {
    if state.protocols.screencopy.pending.is_empty() {
        return;
    }
    let rendered = state.protocols.screencopy.frames_rendered;
    let all = std::mem::take(&mut state.protocols.screencopy.pending);
    let (ready, waiting): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|p| p.wait_after.is_none_or(|after| rendered > after));
    state.protocols.screencopy.pending = waiting;

    for pending in ready {
        let Some(data) = pending.frame.data::<FrameData>() else {
            continue;
        };
        let Some(output) = data.output.clone() else {
            pending.frame.failed();
            continue;
        };
        let cursor_location = state.pointer.current_location()
            - state.space.output_geometry(&output).map_or_else(Default::default, |g| g.loc).to_f64();
        let request = CaptureRequest {
            output: &output,
            space: &state.space,
            wm: &state.wm,
            cursor: data.overlay_cursor.then_some((&state.cursor_status, cursor_location)),
            window: None,
        };
        // Into a client dmabuf (rendered on the GPU), or through memory into shm.
        if let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(&pending.buffer) {
            let mut dmabuf = dmabuf.clone();
            match state.backend_data.capture_into_dmabuf(request, &mut dmabuf) {
                Ok(()) => {
                    pending.frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
                    if pending.frame.version() >= 2 && pending.wait_after.is_some() {
                        let (_, _, w, h) = data.region;
                        pending.frame.damage(0, 0, w as u32, h as u32);
                    }
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                    let secs = now.as_secs();
                    pending.frame.ready((secs >> 32) as u32, secs as u32, now.subsec_nanos());
                    tracing::debug!("screencopy: dmabuf frame delivered");
                }
                Err(err) => {
                    tracing::debug!("screencopy: dmabuf capture failed: {err}");
                    pending.frame.failed();
                }
            }
            continue;
        }
        let captured = match state.backend_data.capture_output(request) {
            Ok(captured) => captured,
            Err(err) => {
                tracing::debug!("screencopy: capture failed: {err}");
                pending.frame.failed();
                continue;
            }
        };
        if copy_into(&pending.buffer, &captured, data.region) {
            pending.frame.flags(zwlr_screencopy_frame_v1::Flags::empty());
            if pending.frame.version() >= 2 && pending.wait_after.is_some() {
                let (_, _, w, h) = data.region;
                pending.frame.damage(0, 0, w as u32, h as u32);
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();
            let secs = now.as_secs();
            pending.frame.ready((secs >> 32) as u32, secs as u32, now.subsec_nanos());
            tracing::debug!("screencopy: frame delivered");
        } else {
            pending.frame.failed();
        }
    }
}

/// Copies `region` of `captured` into the client's shm `buffer`.
pub(crate) fn copy_into(buffer: &WlBuffer, captured: &CapturedFrame, region: (i32, i32, i32, i32)) -> bool {
    let (rx, ry, rw, rh) = region;
    if rx < 0 || ry < 0 || rx + rw > captured.width || ry + rh > captured.height {
        return false;
    }
    let result = smithay::wayland::shm::with_buffer_contents_mut(buffer, |ptr, len, meta| {
        if meta.width != rw || meta.height != rh || (meta.stride * rh) as usize > len {
            return false;
        }
        for row in 0..rh {
            let src_start = (((ry + row) * captured.width + rx) * 4) as usize;
            let src = &captured.data[src_start..src_start + (rw * 4) as usize];
            // SAFETY: `ptr..ptr+len` is the client's shm pool mapping;
            // the bounds check above keeps every row's `stride`-sized slot
            // inside it, and rows are written one at a time with no other
            // reference into that memory alive (Smithay documents that the
            // client may mutate it concurrently, which for a plain byte
            // copy only means it may see a partly-updated frame — the same
            // as with any compositor).
            unsafe {
                std::ptr::copy_nonoverlapping(src.as_ptr(), ptr.add((row * meta.stride) as usize), src.len());
            }
        }
        true
    });
    result.unwrap_or(false)
}
