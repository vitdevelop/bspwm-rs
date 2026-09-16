//! `wlr-export-dmabuf-unstable-v1`: hands a client a dmabuf of an output's
//! current contents (`wf-recorder --dmabuf`, older OBS plugins). It is
//! deprecated in favour of `ext-image-copy-capture` (`crate::ext_capture`),
//! whose dmabuf sessions do the same, but its clients are still around.
//!
//! The compositor renders the output into a freshly allocated dmabuf (the
//! render is [`Backend::capture_into_dmabuf`], the same as the other
//! capture protocols) and sends the client its plane file descriptors; the
//! buffer is a private copy, but the generated bindings can only announce
//! the frame as `transient` ("may change"), which just tells clients to copy it promptly. Only
//! privileged clients see the global.

use std::os::fd::AsFd;

use smithay::backend::allocator::Buffer;
use smithay::output::Output;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};
use wayland_protocols_wlr::export_dmabuf::v1::server::{
    zwlr_export_dmabuf_frame_v1::{self, CancelReason, Flags, ZwlrExportDmabufFrameV1},
    zwlr_export_dmabuf_manager_v1::{self, ZwlrExportDmabufManagerV1},
};

use crate::screencopy::CaptureRequest;
use crate::state::{Backend, State};

/// User data of a frame: what was asked for.
pub struct FrameData {
    output: Option<Output>,
    overlay_cursor: bool,
}

/// Per-protocol state.
#[derive(Default)]
pub struct ExportDmabuf {
    pending: Vec<ZwlrExportDmabufFrameV1>,
}

impl<Bd: Backend + 'static> GlobalDispatch<ZwlrExportDmabufManagerV1, (), State<Bd>> for State<Bd> {
    fn can_view(client: Client, _: &()) -> bool {
        crate::state::is_privileged(&client)
    }

    fn bind(_: &mut State<Bd>, _: &DisplayHandle, _: &Client, resource: New<ZwlrExportDmabufManagerV1>, _: &(), data_init: &mut DataInit<'_, State<Bd>>) {
        data_init.init(resource, ());
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrExportDmabufManagerV1, (), State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _: &Client,
        _: &ZwlrExportDmabufManagerV1,
        request: zwlr_export_dmabuf_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        if let zwlr_export_dmabuf_manager_v1::Request::CaptureOutput { frame, overlay_cursor, output } = request {
            let frame = data_init.init(frame, FrameData { output: Output::from_resource(&output), overlay_cursor: overlay_cursor != 0 });
            state.protocols.export_dmabuf.pending.push(frame);
            state.backend_data.queue_redraw();
        }
    }
}

impl<Bd: Backend + 'static> Dispatch<ZwlrExportDmabufFrameV1, FrameData, State<Bd>> for State<Bd> {
    fn request(_: &mut State<Bd>, _: &Client, _: &ZwlrExportDmabufFrameV1, _: zwlr_export_dmabuf_frame_v1::Request, _: &FrameData, _: &DisplayHandle, _: &mut DataInit<'_, State<Bd>>) {
        // Only `destroy`.
    }

    fn destroyed(state: &mut State<Bd>, _: smithay::reexports::wayland_server::backend::ClientId, frame: &ZwlrExportDmabufFrameV1, _: &FrameData) {
        state.protocols.export_dmabuf.pending.retain(|f| f != frame);
    }
}

/// Answers every requested export. Called after each event-loop turn.
pub fn fulfill<Bd: Backend + 'static>(state: &mut State<Bd>) {
    if state.protocols.export_dmabuf.pending.is_empty() {
        return;
    }
    for frame in std::mem::take(&mut state.protocols.export_dmabuf.pending) {
        let Some(data) = frame.data::<FrameData>() else {
            continue;
        };
        let Some(output) = data.output.clone() else {
            frame.cancel(CancelReason::Permanent);
            continue;
        };
        let Some(mode) = output.current_mode() else {
            frame.cancel(CancelReason::Temporary);
            continue;
        };
        let mut dmabuf = match state.backend_data.allocate_capture_buffer(mode.size.w, mode.size.h) {
            Ok(dmabuf) => dmabuf,
            Err(err) => {
                tracing::debug!("wlr-export-dmabuf: {err}");
                frame.cancel(CancelReason::Permanent);
                continue;
            }
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
        if let Err(err) = state.backend_data.capture_into_dmabuf(request, &mut dmabuf) {
            tracing::debug!("wlr-export-dmabuf: capture failed: {err}");
            frame.cancel(CancelReason::Temporary);
            continue;
        }
        let format = dmabuf.format();
        let modifier = u64::from(format.modifier);
        let planes = dmabuf.num_planes() as u32;
        frame.frame(
            mode.size.w as u32,
            mode.size.h as u32,
            0,
            0,
            0,
            // The generated binding has no way to say "no flags"; `transient`
            // (the object may change later) is only a hint to copy promptly.
            Flags::Transient,
            format.code as u32,
            (modifier >> 32) as u32,
            modifier as u32,
            planes,
        );
        let offsets: Vec<u32> = dmabuf.offsets().collect();
        let strides: Vec<u32> = dmabuf.strides().collect();
        for (index, handle) in dmabuf.handles().enumerate() {
            // The size of a dmabuf's memory is where its end is.
            let size = smithay::reexports::rustix::fs::seek(handle.as_fd(), smithay::reexports::rustix::fs::SeekFrom::End(0)).unwrap_or(0) as u32;
            frame.object(
                index as u32,
                handle.as_fd(),
                size,
                offsets.get(index).copied().unwrap_or(0),
                strides.get(index).copied().unwrap_or(0),
                index as u32,
            );
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let secs = now.as_secs();
        frame.ready((secs >> 32) as u32, secs as u32, now.subsec_nanos());
        tracing::debug!("wlr-export-dmabuf: frame delivered");
    }
}
