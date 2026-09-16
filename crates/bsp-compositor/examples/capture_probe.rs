//! A Wayland client that live-tests the screen-capture protocols' *dmabuf*
//! and *toplevel* paths (DRM backend only, in the QEMU test VM):
//!
//! ```text
//! capture_probe wlr-dmabuf                 wlr-screencopy into a GBM dmabuf
//! capture_probe ext-dmabuf                 ext-image-copy-capture of the output into a dmabuf
//! capture_probe ext-toplevel [APP_ID]      ext-image-copy-capture of one toplevel, into shm
//! capture_probe ext-toplevel-dmabuf [APP_ID]
//! capture_probe export                     wlr-export-dmabuf of the output
//! ```
//!
//! Every mode that fills a dmabuf also captures the same thing into shm
//! and prints how many bytes differ (`match` when none), which is how
//! orientation and format mistakes show. `--resized-wait SECS` (toplevel
//! modes) waits that long for the session to announce a new size and captures again.

use std::io::Write;
use std::os::fd::{AsFd, OwnedFd};
use std::time::{Duration, Instant};

use smithay::reexports::gbm;
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{event_created_child, Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{ext_foreign_toplevel_handle_v1, ext_foreign_toplevel_list_v1};
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1, ext_image_capture_source_v1, ext_output_image_capture_source_manager_v1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1, ext_image_copy_capture_manager_v1, ext_image_copy_capture_session_v1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1};
use wayland_protocols_wlr::export_dmabuf::v1::client::{zwlr_export_dmabuf_frame_v1, zwlr_export_dmabuf_manager_v1};
use wayland_protocols_wlr::screencopy::v1::client::{zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1};

const FOURCC_ARGB8888: u32 = 0x3432_5241;

#[derive(Default)]
struct App {
    output: Option<wl_output::WlOutput>,
    shm: Option<wl_shm::WlShm>,
    dmabuf: Option<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
    screencopy: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    ext_copy: Option<ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1>,
    output_source: Option<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1>,
    toplevel_source: Option<ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1>,
    toplevel_list: Option<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1>,
    export: Option<zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1>,
    toplevels: Vec<(ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, String)>,

    // wlr-screencopy frame
    wlr_shm: Option<(u32, u32, u32)>,
    wlr_dmabuf: Option<(u32, u32, u32)>,
    wlr_done: bool,
    ready: bool,
    failed: Option<String>,

    // ext-image-copy-capture session
    ext_size: (u32, u32),
    ext_dmabuf_formats: Vec<(u32, Vec<u64>)>,
    ext_dmabuf_device: bool,
    ext_session_done: bool,
    ext_sizes_seen: u32,

    // wlr-export-dmabuf frame
    export_info: Option<(u32, u32, u32, u64, u32)>,
    export_objects: Vec<(OwnedFd, u32, u32, u32)>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(app: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        let wl_registry::Event::Global { name, interface, version } = event else {
            return;
        };
        match interface.as_str() {
            "wl_output" if app.output.is_none() => app.output = Some(registry.bind(name, version.min(3), qh, ())),
            "wl_shm" => app.shm = Some(registry.bind(name, 1, qh, ())),
            "zwp_linux_dmabuf_v1" => app.dmabuf = Some(registry.bind(name, version.min(3), qh, ())),
            "zwlr_screencopy_manager_v1" => app.screencopy = Some(registry.bind(name, version.min(3), qh, ())),
            "ext_image_copy_capture_manager_v1" => app.ext_copy = Some(registry.bind(name, 1, qh, ())),
            "ext_output_image_capture_source_manager_v1" => app.output_source = Some(registry.bind(name, 1, qh, ())),
            "ext_foreign_toplevel_image_capture_source_manager_v1" => app.toplevel_source = Some(registry.bind(name, 1, qh, ())),
            "ext_foreign_toplevel_list_v1" => app.toplevel_list = Some(registry.bind(name, 1, qh, ())),
            "zwlr_export_dmabuf_manager_v1" => app.export = Some(registry.bind(name, 1, qh, ())),
            _ => {}
        }
    }
}

macro_rules! ignore_events {
    ($($ty:ty),* $(,)?) => {$(
        impl Dispatch<$ty, ()> for App {
            fn event(_: &mut Self, _: &$ty, _: <$ty as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(
    wl_output::WlOutput,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1,
    zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
);

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for App {
    fn event(app: &mut Self, _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, event: zwlr_screencopy_frame_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer { format, width, height, stride } => {
                app.wlr_shm = Some((width, height, stride));
                let _ = format;
            }
            Event::LinuxDmabuf { format, width, height } => {
                if app.wlr_dmabuf.is_none() || format == FOURCC_ARGB8888 {
                    app.wlr_dmabuf = Some((format, width, height));
                }
            }
            Event::BufferDone => app.wlr_done = true,
            Event::Ready { .. } => app.ready = true,
            Event::Failed => app.failed = Some("failed".to_string()),
            _ => {}
        }
    }
}

impl Dispatch<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1, ()> for App {
    fn event(app: &mut Self, _: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1, event: ext_image_copy_capture_session_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                app.ext_size = (width, height);
                app.ext_sizes_seen += 1;
                app.ext_dmabuf_formats.clear();
            }
            Event::DmabufDevice { .. } => app.ext_dmabuf_device = true,
            Event::DmabufFormat { format, modifiers } => {
                let mods = modifiers.chunks_exact(8).map(|c| u64::from_ne_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])).collect();
                app.ext_dmabuf_formats.push((format, mods));
            }
            Event::Done => app.ext_session_done = true,
            Event::Stopped => app.failed = Some("session stopped".to_string()),
            _ => {}
        }
    }
}

impl Dispatch<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1, ()> for App {
    fn event(app: &mut Self, _: &ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1, event: ext_image_copy_capture_frame_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Ready => app.ready = true,
            Event::Failed { reason } => app.failed = Some(format!("failed: {reason:?}")),
            _ => {}
        }
    }
}

impl Dispatch<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, ()> for App {
    fn event(app: &mut Self, _: &ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, event: ext_foreign_toplevel_list_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let ext_foreign_toplevel_list_v1::Event::Toplevel { toplevel } = event {
            app.toplevels.push((toplevel, String::new()));
        }
    }
    event_created_child!(App, ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()> for App {
    fn event(app: &mut Self, handle: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, event: ext_foreign_toplevel_handle_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let ext_foreign_toplevel_handle_v1::Event::AppId { app_id } = event {
            if let Some(entry) = app.toplevels.iter_mut().find(|(h, _)| h == handle) {
                entry.1 = app_id;
            }
        }
    }
}

impl Dispatch<zwlr_export_dmabuf_frame_v1::ZwlrExportDmabufFrameV1, ()> for App {
    fn event(app: &mut Self, _: &zwlr_export_dmabuf_frame_v1::ZwlrExportDmabufFrameV1, event: zwlr_export_dmabuf_frame_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        use zwlr_export_dmabuf_frame_v1::Event;
        match event {
            Event::Frame { width, height, format, mod_high, mod_low, num_objects, .. } => {
                app.export_info = Some((width, height, format, ((mod_high as u64) << 32) | mod_low as u64, num_objects));
            }
            Event::Object { fd, size, offset, stride, .. } => app.export_objects.push((fd, size, offset, stride)),
            Event::Ready { .. } => app.ready = true,
            Event::Cancel { reason } => app.failed = Some(format!("cancelled: {reason:?}")),
            _ => {}
        }
    }
}

/// A shm buffer backed by a file we can read back afterwards.
struct ShmTarget {
    buffer: wl_buffer::WlBuffer,
    path: String,
}

fn shm_target(app: &App, qh: &QueueHandle<App>, w: u32, h: u32, stride: u32) -> ShmTarget {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let path = format!("{dir}/capture_probe-{}-{w}x{h}.shm", std::process::id());
    let mut file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).expect("shm file");
    file.write_all(&vec![0u8; (stride * h) as usize]).expect("shm file size");
    let shm = app.shm.clone().expect("wl_shm");
    let pool = shm.create_pool(file.as_fd(), (stride * h) as i32, qh, ());
    let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, wl_shm::Format::Argb8888, qh, ());
    ShmTarget { buffer, path }
}

/// A GBM linear ARGB8888 buffer imported as a `wl_buffer`.
struct DmabufTarget {
    buffer: wl_buffer::WlBuffer,
    bo: gbm::BufferObject<()>,
}

fn dmabuf_target(app: &App, qh: &QueueHandle<App>, gbm: &gbm::Device<std::fs::File>, w: u32, h: u32, format: u32) -> Option<DmabufTarget> {
    let bo = gbm
        .create_buffer_object::<()>(w, h, gbm::Format::try_from(format).ok()?, gbm::BufferObjectFlags::RENDERING | gbm::BufferObjectFlags::LINEAR)
        .map_err(|err| eprintln!("cannot allocate a GBM buffer: {err}"))
        .ok()?;
    let fd = bo.fd().ok()?;
    let modifier: u64 = bo.modifier().into();
    let params = app.dmabuf.as_ref()?.create_params(qh, ());
    params.add(fd.as_fd(), 0, bo.offset(0), bo.stride(), (modifier >> 32) as u32, modifier as u32);
    let buffer = params.create_immed(w as i32, h as i32, format, zwp_linux_buffer_params_v1::Flags::empty(), qh, ());
    Some(DmabufTarget { buffer, bo })
}

fn read_dmabuf(target: &DmabufTarget, w: u32, h: u32) -> Vec<u8> {
    target
        .bo
        .map(0, 0, w, h, |m| {
            let mut out = Vec::with_capacity((w * h * 4) as usize);
            for row in 0..h as usize {
                let start = row * m.stride() as usize;
                out.extend_from_slice(&m.buffer()[start..start + (w * 4) as usize]);
            }
            out
        })
        .unwrap_or_default()
}

fn compare(label: &str, shm: &[u8], dmabuf: &[u8]) {
    if shm.len() != dmabuf.len() || shm.is_empty() {
        println!("{label}: sizes differ ({} vs {} bytes)", shm.len(), dmabuf.len());
        return;
    }
    let differing = shm.iter().zip(dmabuf).filter(|(a, b)| a != b).count();
    let non_background = dmabuf.chunks_exact(4).filter(|p| p[..3] != [0x14, 0x14, 0x14] && p[..3] != [0, 0, 0]).count();
    if differing == 0 {
        println!("{label}: match ({} bytes, {non_background} non-background pixels)", shm.len());
    } else {
        println!("{label}: {differing} of {} bytes differ", shm.len());
    }
}

fn wait(app: &mut App, queue: &mut wayland_client::EventQueue<App>, secs: u64, done: impl Fn(&App) -> bool) -> bool {
    let start = Instant::now();
    while !done(app) && start.elapsed() < Duration::from_secs(secs) {
        if queue.roundtrip(app).is_err() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    done(app)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    let mut app_id: Option<String> = None;
    let mut resized_wait: Option<u64> = None;
    while let Some(arg) = args.next() {
        if arg == "--resized-wait" {
            resized_wait = args.next().and_then(|v| v.parse().ok());
        } else {
            app_id = Some(arg);
        }
    }
    let conn = Connection::connect_to_env().expect("wayland connection");
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut app = App::default();
    queue.roundtrip(&mut app).expect("roundtrip");
    queue.roundtrip(&mut app).expect("roundtrip");
    let card = std::env::var("CAPTURE_RENDER_NODE").unwrap_or_else(|_| "/dev/dri/renderD128".into());
    let gbm = std::fs::OpenOptions::new().read(true).write(true).open(&card).ok().and_then(|f| gbm::Device::new(f).ok());
    let Some(output) = app.output.clone() else {
        eprintln!("no wl_output");
        std::process::exit(1);
    };

    match mode.as_str() {
        "wlr-dmabuf" => {
            let manager = app.screencopy.clone().expect("zwlr_screencopy_manager_v1");
            let frame = manager.capture_output(0, &output, &qh, ());
            assert!(wait(&mut app, &mut queue, 5, |a| a.wlr_done), "no buffer_done");
            let (Some((w, h, stride)), Some((format, dw, dh))) = (app.wlr_shm, app.wlr_dmabuf) else {
                println!("wlr-screencopy offered no linux_dmabuf event");
                std::process::exit(1);
            };
            println!("offered: shm {w}x{h} stride {stride}, dmabuf format {format:#x} {dw}x{dh}");
            let Some(gbm) = &gbm else {
                println!("no GBM device");
                std::process::exit(1);
            };
            let Some(target) = dmabuf_target(&app, &qh, gbm, dw, dh, format) else {
                println!("cannot make a dmabuf");
                std::process::exit(1);
            };
            frame.copy(&target.buffer);
            if !wait(&mut app, &mut queue, 5, |a| a.ready || a.failed.is_some()) || app.failed.is_some() {
                println!("dmabuf copy: {}", app.failed.clone().unwrap_or_else(|| "timeout".into()));
                std::process::exit(1);
            }
            println!("dmabuf copy ready");
            let dmabuf_pixels = read_dmabuf(&target, dw, dh);
            // The same frame into shm.
            app.ready = false;
            app.wlr_done = false;
            let frame = manager.capture_output(0, &output, &qh, ());
            assert!(wait(&mut app, &mut queue, 5, |a| a.wlr_done));
            let shm = shm_target(&app, &qh, w, h, stride);
            frame.copy(&shm.buffer);
            assert!(wait(&mut app, &mut queue, 5, |a| a.ready), "shm copy did not finish");
            compare("wlr dmabuf vs shm", &std::fs::read(&shm.path).unwrap_or_default(), &dmabuf_pixels);
            let _ = std::fs::remove_file(&shm.path);
        }
        "ext-dmabuf" | "ext-toplevel" | "ext-toplevel-dmabuf" => {
            let copy = app.ext_copy.clone().expect("ext_image_copy_capture_manager_v1");
            let source = if mode == "ext-dmabuf" {
                app.output_source.clone().expect("output source manager").create_source(&output, &qh, ())
            } else {
                let list = app.toplevel_list.clone().expect("ext_foreign_toplevel_list_v1");
                let _ = list;
                let _ = queue.roundtrip(&mut app);
                let handle = app
                    .toplevels
                    .iter()
                    .find(|(_, id)| app_id.as_ref().is_none_or(|want| want == id))
                    .map(|(h, _)| h.clone());
                let Some(handle) = handle else {
                    println!("no toplevel found ({} listed)", app.toplevels.len());
                    std::process::exit(1);
                };
                app.toplevel_source.clone().expect("toplevel source manager").create_source(&handle, &qh, ())
            };
            let session = copy.create_session(&source, ext_image_copy_capture_manager_v1::Options::empty(), &qh, ());
            if !wait(&mut app, &mut queue, 5, |a| a.ext_session_done || a.failed.is_some()) || app.failed.is_some() {
                println!("session: {}", app.failed.clone().unwrap_or_else(|| "timeout".into()));
                std::process::exit(1);
            }
            let want_dmabuf = mode != "ext-toplevel";
            for round in 0..2 {
                let (w, h) = app.ext_size;
                println!("session buffer size {w}x{h}, dmabuf device advertised: {}, formats: {}", app.ext_dmabuf_device, app.ext_dmabuf_formats.len());
                app.ready = false;
                app.failed = None;
                let frame = session.create_frame(&qh, ());
                let mut dmabuf_pixels = None;
                let shm = shm_target(&app, &qh, w, h, w * 4);
                if want_dmabuf {
                    let format = app.ext_dmabuf_formats.iter().find(|(f, _)| *f == FOURCC_ARGB8888).or(app.ext_dmabuf_formats.first()).map(|(f, _)| *f);
                    let (Some(format), Some(gbm)) = (format, &gbm) else {
                        println!("no dmabuf format offered");
                        std::process::exit(1);
                    };
                    let Some(target) = dmabuf_target(&app, &qh, gbm, w, h, format) else {
                        println!("cannot make a dmabuf");
                        std::process::exit(1);
                    };
                    frame.attach_buffer(&target.buffer);
                    frame.damage_buffer(0, 0, w as i32, h as i32);
                    frame.capture();
                    if !wait(&mut app, &mut queue, 5, |a| a.ready || a.failed.is_some()) || app.failed.is_some() {
                        println!("dmabuf capture: {}", app.failed.clone().unwrap_or_else(|| "timeout".into()));
                        // A resize between rounds is reported as buffer constraints, then works.
                        if round == 0 && resized_wait.is_some() {
                            continue;
                        }
                        std::process::exit(1);
                    }
                    println!("dmabuf capture ready");
                    dmabuf_pixels = Some(read_dmabuf(&target, w, h));
                    app.ready = false;
                    frame.destroy();
                }
                let frame = if want_dmabuf { session.create_frame(&qh, ()) } else { frame };
                frame.attach_buffer(&shm.buffer);
                frame.damage_buffer(0, 0, w as i32, h as i32);
                frame.capture();
                if !wait(&mut app, &mut queue, 5, |a| a.ready || a.failed.is_some()) || app.failed.is_some() {
                    println!("shm capture: {}", app.failed.clone().unwrap_or_else(|| "timeout".into()));
                    if round == 0 && resized_wait.is_some() {
                        continue;
                    }
                    std::process::exit(1);
                }
                println!("shm capture ready");
                frame.destroy();
                let shm_pixels = std::fs::read(&shm.path).unwrap_or_default();
                let _ = std::fs::remove_file(&shm.path);
                match dmabuf_pixels {
                    Some(pixels) => compare("ext dmabuf vs shm", &shm_pixels, &pixels),
                    None => {
                        let non_background = shm_pixels.chunks_exact(4).filter(|p| p[..3] != [0x14, 0x14, 0x14] && p[..3] != [0, 0, 0]).count();
                        println!("toplevel shm capture: {} pixels, {non_background} not background", shm_pixels.len() / 4);
                    }
                }
                let Some(secs) = resized_wait.filter(|_| round == 0) else {
                    break;
                };
                println!("waiting {secs}s, then capturing with the old size...");
                std::thread::sleep(Duration::from_secs(secs));
                let before = app.ext_sizes_seen;
                app.ext_session_done = false;
                app.ready = false;
                app.failed = None;
                let stale = session.create_frame(&qh, ());
                let old = shm_target(&app, &qh, w, h, w * 4);
                stale.attach_buffer(&old.buffer);
                stale.damage_buffer(0, 0, w as i32, h as i32);
                stale.capture();
                wait(&mut app, &mut queue, 5, |a| a.ready || a.failed.is_some());
                println!("capture with the old size: {}", if app.ready { "ready (size did not change)".to_string() } else { app.failed.clone().unwrap_or_else(|| "timeout".into()) });
                stale.destroy();
                let _ = std::fs::remove_file(&old.path);
                if !wait(&mut app, &mut queue, 5, |a| a.ext_sizes_seen > before && a.ext_session_done) {
                    println!("no new size announced");
                    break;
                }
                app.failed = None;
            }
        }
        "export" => {
            let manager = app.export.clone().expect("zwlr_export_dmabuf_manager_v1");
            let _frame = manager.capture_output(0, &output, &qh, ());
            if !wait(&mut app, &mut queue, 5, |a| a.ready || a.failed.is_some()) || app.failed.is_some() {
                println!("export: {}", app.failed.clone().unwrap_or_else(|| "timeout".into()));
                std::process::exit(1);
            }
            let (w, h, format, modifier, num) = app.export_info.expect("frame event");
            println!("export frame {w}x{h} format {format:#x} modifier {modifier:#x} objects {num} (received {})", app.export_objects.len());
            let Some(gbm) = &gbm else {
                println!("no GBM device");
                std::process::exit(1);
            };
            // Import the exported fd through GBM and read it.
            let (fd, size, offset, stride) = app.export_objects.remove(0);
            let imported = gbm.import_buffer_object_from_dma_buf::<()>(fd.as_fd(), w, h, stride, gbm::Format::try_from(format).expect("format"), gbm::BufferObjectFlags::RENDERING);
            match imported {
                Ok(bo) => {
                    let pixels = bo.map(0, 0, w, h, |m| {
                        let mut out = Vec::new();
                        for row in 0..h as usize {
                            let start = row * m.stride() as usize;
                            out.extend_from_slice(&m.buffer()[start..start + (w * 4) as usize]);
                        }
                        out
                    });
                    let pixels = pixels.unwrap_or_default();
                    let non_background = pixels.chunks_exact(4).filter(|p| p[..3] != [0x14, 0x14, 0x14] && p[..3] != [0, 0, 0]).count();
                    println!("export object: size {size} offset {offset} stride {stride}; {} pixels, {non_background} not background", pixels.len() / 4);
                }
                Err(err) => println!("cannot import the exported dmabuf: {err}"),
            }
        }
        other => {
            eprintln!("unknown mode {other:?}");
            std::process::exit(2);
        }
    }
    let _ = WEnum::Value(0u32);
}
