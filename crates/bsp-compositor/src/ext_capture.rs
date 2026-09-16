//! `ext-image-copy-capture-v1` with `ext-image-capture-source-v1` output
//! sources: the standardized successor of `wlr-screencopy` (`crate::screencopy`),
//! used by newer `grim` and by `xdg-desktop-portal-wlr` for screen casting.
//!
//! Protocol shape: a client makes a *source* (an output, or one toplevel
//! named by its `ext-foreign-toplevel-list` handle), opens a *session* on
//! it, is told the buffer size, the shm format and the dmabuf device,
//! formats and modifiers, then repeatedly creates a *frame*, attaches a
//! matching shm or dmabuf buffer and says `capture`. The render is the
//! backend's ([`Backend::capture_output`] into shm, [`Backend::capture_into_dmabuf`]
//! on the GPU for dmabufs), the same one `wlr-screencopy` uses, so both
//! protocols show the same pixels. A toplevel source renders that window
//! alone (also when it is covered or on another desktop); when it is
//! resized the session announces the new size and the pending frame fails
//! with `buffer_constraints`. Only privileged clients (not sandboxed) see
//! these globals.
//!
//! Not implemented: cursor *sessions* (`create_pointer_cursor_session` is
//! answered with a session that is stopped at once; cursors are painted
//! into output frames with the `paint_cursors` option instead).

use std::sync::Mutex;

use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum};
use smithay::desktop::Window;
use smithay::wayland::foreign_toplevel_list::ForeignToplevelHandle;
use wayland_protocols::ext::image_capture_source::v1::server::{
    ext_foreign_toplevel_image_capture_source_manager_v1::{self, ExtForeignToplevelImageCaptureSourceManagerV1},
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::{self, ExtOutputImageCaptureSourceManagerV1},
};
use wayland_protocols::ext::image_copy_capture::v1::server::{
    ext_image_copy_capture_cursor_session_v1::{self, ExtImageCopyCaptureCursorSessionV1},
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1},
    ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};

use crate::screencopy::{copy_into, dmabuf_matches, preferred_formats, CaptureRequest, DmabufCaps, FORMAT};
use crate::state::{Backend, State};

/// What a capture source names.
#[derive(Clone)]
pub enum SourceKind {
    /// A whole output (`None`: it is gone).
    Output(Option<Output>),
    /// One toplevel, by its foreign-toplevel handle.
    Toplevel(ForeignToplevelHandle),
}

/// User data of a capture source.
pub struct SourceData(SourceKind);

/// User data of a session.
pub struct SessionData {
    source: SourceKind,
    paint_cursors: bool,
    /// A frame object exists and has not finished (only one at a time).
    frame_live: Mutex<bool>,
    /// The buffer size last announced to the client.
    announced: Mutex<(i32, i32)>,
}

/// User data of a frame.
pub struct FrameData {
    session: ExtImageCopyCaptureSessionV1,
    buffer: Mutex<Option<WlBuffer>>,
    captured: Mutex<bool>,
}

/// Per-protocol state.
#[derive(Default)]
pub struct ExtCapture {
    pending: Vec<ExtImageCopyCaptureFrameV1>,
}

/// What a session captures right now.
struct Resolved {
    /// The output whose scale and mode apply.
    output: Output,
    /// The window, for a toplevel source.
    window: Option<Window>,
    /// The buffer size in pixels.
    size: (i32, i32),
}

/// Resolves a source to what it names today: `None` once the output or
/// window is gone.
fn resolve<Bd: Backend + 'static>(state: &State<Bd>, source: &SourceKind) -> Option<Resolved> {
    match source {
        SourceKind::Output(output) => {
            let output = output.clone()?;
            let mode = output.current_mode()?;
            Some(Resolved { output, window: None, size: (mode.size.w, mode.size.h) })
        }
        SourceKind::Toplevel(handle) => {
            if handle.is_closed() {
                return None;
            }
            let id = state.protocols.toplevel_handles.iter().find(|(_, h)| h.identifier() == handle.identifier()).map(|(id, _)| *id)?;
            let window = state.adapter.window(id)?.clone();
            let output = state
                .space
                .outputs_for_element(&window)
                .into_iter()
                .next()
                .or_else(|| state.space.outputs().next().cloned())?;
            let scale = smithay::utils::Scale::from(output.current_scale().fractional_scale());
            let size = window.geometry().size.to_f64().to_physical(scale).to_i32_ceil();
            (size.w > 0 && size.h > 0).then_some(Resolved { output, window: Some(window), size: (size.w, size.h) })
        }
    }
}

/// Announces the buffer constraints of a session: size, shm format and
/// (if the backend can render into dmabufs) their device, formats and modifiers.
fn send_constraints(session: &ExtImageCopyCaptureSessionV1, size: (i32, i32), caps: Option<&DmabufCaps>) {
    session.buffer_size(size.0 as u32, size.1 as u32);
    session.shm_format(FORMAT);
    if let Some(caps) = caps {
        session.dmabuf_device(caps.device.to_ne_bytes().to_vec());
        for (code, modifiers) in preferred_formats(caps) {
            let bytes = modifiers.iter().flat_map(|m| u64::from(*m).to_ne_bytes()).collect();
            session.dmabuf_format(*code as u32, bytes);
        }
    }
    session.done();
}

impl<Bd: Backend + 'static> GlobalDispatch<ExtOutputImageCaptureSourceManagerV1, (), State<Bd>> for State<Bd> {
    fn can_view(client: Client, _: &()) -> bool {
        crate::state::is_privileged(&client)
    }

    fn bind(_: &mut State<Bd>, _: &DisplayHandle, _: &Client, resource: New<ExtOutputImageCaptureSourceManagerV1>, _: &(), data_init: &mut DataInit<'_, State<Bd>>) {
        data_init.init(resource, ());
    }
}

impl<Bd: Backend + 'static> Dispatch<ExtOutputImageCaptureSourceManagerV1, (), State<Bd>> for State<Bd> {
    fn request(
        _: &mut State<Bd>,
        _: &Client,
        _: &ExtOutputImageCaptureSourceManagerV1,
        request: ext_output_image_capture_source_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        if let ext_output_image_capture_source_manager_v1::Request::CreateSource { source, output } = request {
            data_init.init(source, SourceData(SourceKind::Output(Output::from_resource(&output))));
        }
    }
}

impl<Bd: Backend + 'static> GlobalDispatch<ExtForeignToplevelImageCaptureSourceManagerV1, (), State<Bd>> for State<Bd> {
    fn can_view(client: Client, _: &()) -> bool {
        crate::state::is_privileged(&client)
    }

    fn bind(_: &mut State<Bd>, _: &DisplayHandle, _: &Client, resource: New<ExtForeignToplevelImageCaptureSourceManagerV1>, _: &(), data_init: &mut DataInit<'_, State<Bd>>) {
        data_init.init(resource, ());
    }
}

impl<Bd: Backend + 'static> Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, (), State<Bd>> for State<Bd> {
    fn request(
        _: &mut State<Bd>,
        _: &Client,
        _: &ExtForeignToplevelImageCaptureSourceManagerV1,
        request: ext_foreign_toplevel_image_capture_source_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        if let ext_foreign_toplevel_image_capture_source_manager_v1::Request::CreateSource { source, toplevel_handle } = request {
            match ForeignToplevelHandle::from_resource(&toplevel_handle) {
                Some(handle) => {
                    data_init.init(source, SourceData(SourceKind::Toplevel(handle)));
                }
                None => {
                    // A handle of some other list: a source that names nothing.
                    data_init.init(source, SourceData(SourceKind::Output(None)));
                }
            }
        }
    }
}

impl<Bd: Backend + 'static> Dispatch<ExtImageCaptureSourceV1, SourceData, State<Bd>> for State<Bd> {
    fn request(_: &mut State<Bd>, _: &Client, _: &ExtImageCaptureSourceV1, _: <ExtImageCaptureSourceV1 as Resource>::Request, _: &SourceData, _: &DisplayHandle, _: &mut DataInit<'_, State<Bd>>) {
        // Only `destroy`.
    }
}

impl<Bd: Backend + 'static> GlobalDispatch<ExtImageCopyCaptureManagerV1, (), State<Bd>> for State<Bd> {
    fn can_view(client: Client, _: &()) -> bool {
        crate::state::is_privileged(&client)
    }

    fn bind(_: &mut State<Bd>, _: &DisplayHandle, _: &Client, resource: New<ExtImageCopyCaptureManagerV1>, _: &(), data_init: &mut DataInit<'_, State<Bd>>) {
        data_init.init(resource, ());
    }
}

impl<Bd: Backend + 'static> Dispatch<ExtImageCopyCaptureManagerV1, (), State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _: &Client,
        manager: &ExtImageCopyCaptureManagerV1,
        request: ext_image_copy_capture_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        use ext_image_copy_capture_manager_v1::Request;
        match request {
            Request::CreateSession { session, source, options } => {
                let paint_cursors = match options {
                    WEnum::Value(options) => options.contains(Options::PaintCursors),
                    WEnum::Unknown(bits) => {
                        manager.post_error(ext_image_copy_capture_manager_v1::Error::InvalidOption, format!("unknown options {bits:#x}"));
                        return;
                    }
                };
                let kind = source.data::<SourceData>().map_or(SourceKind::Output(None), |d| d.0.clone());
                let resolved = resolve(state, &kind);
                let session = data_init.init(
                    session,
                    SessionData {
                        source: kind,
                        paint_cursors,
                        frame_live: Mutex::new(false),
                        announced: Mutex::new(resolved.as_ref().map_or((0, 0), |r| r.size)),
                    },
                );
                match resolved {
                    Some(resolved) => {
                        tracing::debug!(w = resolved.size.0, h = resolved.size.1, paint_cursors, "ext-image-copy-capture: session opened");
                        let caps = state.backend_data.capture_dmabuf_caps();
                        send_constraints(&session, resolved.size, caps.as_ref());
                    }
                    None => session.stopped(),
                }
            }
            Request::CreatePointerCursorSession { session, .. } => {
                // Cursors are painted into frames (`paint_cursors`); there is no
                // separate cursor stream.
                let session = data_init.init(session, ());
                let _ = session;
            }
            _ => {}
        }
    }
}

impl<Bd: Backend + 'static> Dispatch<ExtImageCopyCaptureCursorSessionV1, (), State<Bd>> for State<Bd> {
    fn request(
        _: &mut State<Bd>,
        _: &Client,
        _: &ExtImageCopyCaptureCursorSessionV1,
        request: ext_image_copy_capture_cursor_session_v1::Request,
        _: &(),
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        if let ext_image_copy_capture_cursor_session_v1::Request::GetCaptureSession { session } = request {
            // A capture session that is over before it starts.
            let session = data_init.init(session, SessionData { source: SourceKind::Output(None), paint_cursors: false, frame_live: Mutex::new(false), announced: Mutex::new((0, 0)) });
            session.stopped();
        }
    }
}

impl<Bd: Backend + 'static> Dispatch<ExtImageCopyCaptureSessionV1, SessionData, State<Bd>> for State<Bd> {
    fn request(
        _: &mut State<Bd>,
        _: &Client,
        session: &ExtImageCopyCaptureSessionV1,
        request: ext_image_copy_capture_session_v1::Request,
        data: &SessionData,
        _: &DisplayHandle,
        data_init: &mut DataInit<'_, State<Bd>>,
    ) {
        if let ext_image_copy_capture_session_v1::Request::CreateFrame { frame } = request {
            let already = data.frame_live.lock().map(|mut live| std::mem::replace(&mut *live, true)).unwrap_or(false);
            if already {
                session.post_error(ext_image_copy_capture_session_v1::Error::DuplicateFrame, "a frame is already in flight");
                return;
            }
            data_init.init(
                frame,
                FrameData { session: session.clone(), buffer: Mutex::new(None), captured: Mutex::new(false) },
            );
        }
    }
}

impl<Bd: Backend + 'static> Dispatch<ExtImageCopyCaptureFrameV1, FrameData, State<Bd>> for State<Bd> {
    fn request(
        state: &mut State<Bd>,
        _: &Client,
        frame: &ExtImageCopyCaptureFrameV1,
        request: ext_image_copy_capture_frame_v1::Request,
        data: &FrameData,
        _: &DisplayHandle,
        _: &mut DataInit<'_, State<Bd>>,
    ) {
        use ext_image_copy_capture_frame_v1::Request;
        match request {
            Request::AttachBuffer { buffer } => {
                if let Ok(mut slot) = data.buffer.lock() {
                    *slot = Some(buffer);
                }
            }
            Request::Capture => {
                let already = data.captured.lock().map(|mut c| std::mem::replace(&mut *c, true)).unwrap_or(false);
                if already {
                    frame.post_error(ext_image_copy_capture_frame_v1::Error::AlreadyCaptured, "capture already requested");
                    return;
                }
                if data.buffer.lock().map(|b| b.is_none()).unwrap_or(true) {
                    frame.post_error(ext_image_copy_capture_frame_v1::Error::NoBuffer, "capture without an attached buffer");
                    return;
                }
                state.protocols.ext_capture.pending.push(frame.clone());
                state.backend_data.queue_redraw();
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut State<Bd>, _: smithay::reexports::wayland_server::backend::ClientId, frame: &ExtImageCopyCaptureFrameV1, data: &FrameData) {
        state.protocols.ext_capture.pending.retain(|f| f != frame);
        if let Some(session) = data.session.data::<SessionData>() {
            if let Ok(mut live) = session.frame_live.lock() {
                *live = false;
            }
        }
    }
}

/// Answers every requested capture. Called after each event-loop turn.
pub fn fulfill<Bd: Backend + 'static>(state: &mut State<Bd>) {
    if state.protocols.ext_capture.pending.is_empty() {
        return;
    }
    let frames = std::mem::take(&mut state.protocols.ext_capture.pending);
    for frame in frames {
        let Some(data) = frame.data::<FrameData>() else {
            continue;
        };
        let Some(session) = data.session.data::<SessionData>() else {
            continue;
        };
        let Some(buffer) = data.buffer.lock().ok().and_then(|b| b.clone()) else {
            frame.failed(ext_image_copy_capture_frame_v1::FailureReason::Stopped);
            continue;
        };
        let Some(resolved) = resolve(state, &session.source) else {
            // The output or window is gone: the session is over.
            data.session.stopped();
            frame.failed(ext_image_copy_capture_frame_v1::FailureReason::Stopped);
            continue;
        };
        let caps = state.backend_data.capture_dmabuf_caps();
        // A resized window (or a changed mode): tell the client the new
        // constraints; this frame cannot match them.
        let changed = session.announced.lock().map(|mut a| std::mem::replace(&mut *a, resolved.size) != resolved.size).unwrap_or(false);
        if changed {
            send_constraints(&data.session, resolved.size, caps.as_ref());
            frame.failed(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints);
            continue;
        }
        let output = resolved.output.clone();
        let cursor_location = state.pointer.current_location()
            - state.space.output_geometry(&output).map_or_else(Default::default, |g| g.loc).to_f64();
        let request = CaptureRequest {
            output: &output,
            space: &state.space,
            wm: &state.wm,
            cursor: (session.paint_cursors && resolved.window.is_none()).then_some((&state.cursor_status, cursor_location)),
            window: resolved.window.as_ref(),
        };
        let (w, h) = resolved.size;
        let delivered = if let Ok(dmabuf) = smithay::wayland::dmabuf::get_dmabuf(&buffer) {
            let matches = caps.as_ref().is_some_and(|caps| dmabuf_matches(&buffer, w, h, caps));
            if !matches {
                frame.failed(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints);
                continue;
            }
            let mut dmabuf = dmabuf.clone();
            match state.backend_data.capture_into_dmabuf(request, &mut dmabuf) {
                Ok(()) => true,
                Err(err) => {
                    tracing::debug!("ext-image-copy-capture: dmabuf capture failed: {err}");
                    frame.failed(ext_image_copy_capture_frame_v1::FailureReason::Unknown);
                    continue;
                }
            }
        } else {
            let captured = match state.backend_data.capture_output(request) {
                Ok(captured) => captured,
                Err(err) => {
                    tracing::debug!("ext-image-copy-capture: capture failed: {err}");
                    frame.failed(ext_image_copy_capture_frame_v1::FailureReason::Unknown);
                    continue;
                }
            };
            copy_into(&buffer, &captured, (0, 0, captured.width, captured.height))
        };
        if delivered {
            frame.transform(smithay::reexports::wayland_server::protocol::wl_output::Transform::Normal);
            frame.damage(0, 0, w, h);
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            let secs = now.as_secs();
            frame.presentation_time((secs >> 32) as u32, secs as u32, now.subsec_nanos());
            frame.ready();
            tracing::debug!("ext-image-copy-capture: frame delivered");
        } else {
            // The buffer does not match the announced constraints.
            frame.failed(ext_image_copy_capture_frame_v1::FailureReason::BufferConstraints);
        }
    }
}
