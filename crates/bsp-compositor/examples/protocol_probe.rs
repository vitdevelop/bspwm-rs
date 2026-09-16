//! A tiny Wayland client for live-testing `bspwm-rs`'s protocols from
//! inside the QEMU test VM, where the usual tools (`wayland-info`,
//! `wlr-randr` …) are not always installed. It prints what the compositor
//! advertises and what each protocol reports:
//!
//! - `protocol_probe globals` — every global, with its version;
//! - `protocol_probe xdg-output` — every output's logical position and
//!   size, name and description (`zxdg_output_v1`);
//! - `protocol_probe toplevels` — the toplevel list (`ext_foreign_toplevel_list_v1`):
//!   identifier, title, app id, closed;
//! - `protocol_probe gamma` — asks for gamma control of the first output and
//!   prints the ramp size (or `failed`), then loads a dimmed ramp;
//! - `protocol_probe vpointer X Y [click]` — warps a `zwlr_virtual_pointer_v1`
//!   to fraction `X`,`Y` (0-1000) of the layout and optionally clicks;
//! - `protocol_probe power on|off` — sets the first output's power mode
//!   (`wlr-output-power-management`) and prints the events;
//! - `protocol_probe extcopy` — captures the first output through
//!   `ext-image-copy-capture-v1` into a shm buffer and prints the size and a
//!   pixel histogram summary;
//! - `protocol_probe sandbox` — creates a `wp_security_context_v1` socket, connects
//!   through it and prints which privileged protocols that sandboxed client
//!   can see (expected: none) next to what the normal connection sees;
//! - `protocol_probe vrel DX DY` — a relative virtual-pointer motion;
//! - `protocol_probe idle MS` — asks for an idle notification after `MS`
//!   milliseconds without input and prints `idled`/`resumed` until killed.
//!
//! Run against a compositor with `WAYLAND_DISPLAY` set as usual.

use wayland_client::protocol::{wl_buffer, wl_output, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool};
use wayland_protocols::ext::image_capture_source::v1::client::{ext_image_capture_source_v1, ext_output_image_capture_source_manager_v1};
use wayland_protocols::ext::image_copy_capture::v1::client::{ext_image_copy_capture_frame_v1, ext_image_copy_capture_manager_v1, ext_image_copy_capture_session_v1};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::{ext_foreign_toplevel_handle_v1, ext_foreign_toplevel_list_v1};
use wayland_protocols::ext::idle_notify::v1::client::{ext_idle_notification_v1, ext_idle_notifier_v1};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols::wp::security_context::v1::client::{wp_security_context_manager_v1, wp_security_context_v1};
use wayland_protocols_wlr::output_power_management::v1::client::{zwlr_output_power_manager_v1, zwlr_output_power_v1};
use wayland_protocols_wlr::gamma_control::v1::client::{zwlr_gamma_control_manager_v1, zwlr_gamma_control_v1};
use wayland_protocols_wlr::virtual_pointer::v1::client::{zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1};

#[derive(Default)]
struct Probe {
    globals: Vec<(u32, String, u32)>,
    outputs: Vec<(u32, wl_output::WlOutput)>,
    xdg_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    xdg_events: Vec<String>,
    seat: Option<wl_seat::WlSeat>,
    idle_notifier: Option<ext_idle_notifier_v1::ExtIdleNotifierV1>,
    toplevel_list: Option<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1>,
    log: Vec<String>,
    shm: Option<wl_shm::WlShm>,
    source_manager: Option<ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1>,
    copy_manager: Option<ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1>,
    session_size: Option<(u32, u32)>,
    session_done: bool,
    frame_ready: bool,
    frame_failed: bool,
    power_manager: Option<zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1>,
    security_manager: Option<wp_security_context_manager_v1::WpSecurityContextManagerV1>,
    gamma_manager: Option<zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1>,
    vp_manager: Option<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_output" => {
                    let output = registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, ());
                    state.outputs.push((name, output));
                }
                "zxdg_output_manager_v1" => {
                    state.xdg_manager =
                        Some(registry.bind::<zxdg_output_manager_v1::ZxdgOutputManagerV1, _, _>(name, version.min(3), qh, ()));
                }
                "zwlr_output_power_manager_v1" => state.power_manager = Some(registry.bind(name, 1, qh, ())),
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "ext_output_image_capture_source_manager_v1" => state.source_manager = Some(registry.bind(name, 1, qh, ())),
                "ext_image_copy_capture_manager_v1" => state.copy_manager = Some(registry.bind(name, 1, qh, ())),
                "wp_security_context_manager_v1" => {
                    state.security_manager = Some(registry.bind(name, 1, qh, ()));
                }
                "zwlr_gamma_control_manager_v1" => {
                    state.gamma_manager = Some(registry.bind(name, 1, qh, ()));
                }
                "zwlr_virtual_pointer_manager_v1" => {
                    state.vp_manager = Some(registry.bind(name, version.min(2), qh, ()));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(5), qh, ()));
                }
                "ext_idle_notifier_v1" => {
                    state.idle_notifier =
                        Some(registry.bind::<ext_idle_notifier_v1::ExtIdleNotifierV1, _, _>(name, 1, qh, ()));
                }
                "ext_foreign_toplevel_list_v1" => {
                    state.toplevel_list =
                        Some(registry.bind::<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, _, _>(name, 1, qh, ()));
                }
                _ => {}
            }
            state.globals.push((name, interface, version));
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for Probe {
    fn event(_: &mut Self, _: &wl_output::WlOutput, _: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

macro_rules! ignore_events {
    ($($ty:ty),* $(,)?) => {$(
        impl Dispatch<$ty, ()> for Probe {
            fn event(_: &mut Self, _: &$ty, _: <$ty as wayland_client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
ignore_events!(
    zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1,
);

impl Dispatch<ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1, ()> for Probe {
    fn event(state: &mut Self, _: &ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1, event: ext_image_copy_capture_session_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            ext_image_copy_capture_session_v1::Event::BufferSize { width, height } => state.session_size = Some((width, height)),
            ext_image_copy_capture_session_v1::Event::Done => state.session_done = true,
            ext_image_copy_capture_session_v1::Event::Stopped => println!("session stopped"),
            _ => {}
        }
    }
}

impl Dispatch<ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1, ()> for Probe {
    fn event(state: &mut Self, _: &ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1, event: ext_image_copy_capture_frame_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            ext_image_copy_capture_frame_v1::Event::Ready => state.frame_ready = true,
            ext_image_copy_capture_frame_v1::Event::Failed { reason } => {
                println!("frame failed: {reason:?}");
                state.frame_failed = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_output_power_v1::ZwlrOutputPowerV1, ()> for Probe {
    fn event(_: &mut Self, _: &zwlr_output_power_v1::ZwlrOutputPowerV1, event: zwlr_output_power_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwlr_output_power_v1::Event::Mode { mode } => println!("power mode {mode:?}"),
            zwlr_output_power_v1::Event::Failed => println!("power failed"),
            _ => {}
        }
    }
}

impl Dispatch<wp_security_context_manager_v1::WpSecurityContextManagerV1, ()> for Probe {
    fn event(_: &mut Self, _: &wp_security_context_manager_v1::WpSecurityContextManagerV1, _: wp_security_context_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wp_security_context_v1::WpSecurityContextV1, ()> for Probe {
    fn event(_: &mut Self, _: &wp_security_context_v1::WpSecurityContextV1, _: wp_security_context_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1, ()> for Probe {
    fn event(_: &mut Self, _: &zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1, _: zwlr_gamma_control_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<zwlr_gamma_control_v1::ZwlrGammaControlV1, ()> for Probe {
    fn event(state: &mut Self, control: &zwlr_gamma_control_v1::ZwlrGammaControlV1, event: zwlr_gamma_control_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwlr_gamma_control_v1::Event::GammaSize { size } => {
                println!("gamma_size {size}");
                // A dimmed (x0.5) linear ramp: R, G, B tables back to back.
                let mut bytes = Vec::new();
                for _channel in 0..3 {
                    for i in 0..size {
                        let v = ((i as u64 * 65535 / (size as u64 - 1)) / 2) as u16;
                        bytes.extend_from_slice(&v.to_ne_bytes());
                    }
                }
                let path = format!("{}/gamma-{}.bin", std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into()), std::process::id());
                if std::fs::write(&path, &bytes).is_ok() {
                    if let Ok(file) = std::fs::File::open(&path) {
                        use std::os::fd::AsFd;
                        control.set_gamma(file.as_fd());
                        println!("gamma ramp sent");
                    }
                    let _ = std::fs::remove_file(&path);
                }
                state.log.push("gamma_size".into());
            }
            zwlr_gamma_control_v1::Event::Failed => {
                println!("gamma failed");
                state.log.push("failed".into());
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, ()> for Probe {
    fn event(_: &mut Self, _: &zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1, _: zwlr_virtual_pointer_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1, ()> for Probe {
    fn event(_: &mut Self, _: &zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1, _: zwlr_virtual_pointer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_seat::WlSeat, ()> for Probe {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ext_idle_notifier_v1::ExtIdleNotifierV1, ()> for Probe {
    fn event(_: &mut Self, _: &ext_idle_notifier_v1::ExtIdleNotifierV1, _: ext_idle_notifier_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ext_idle_notification_v1::ExtIdleNotificationV1, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &ext_idle_notification_v1::ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let line = match event {
            ext_idle_notification_v1::Event::Idled => "idled",
            ext_idle_notification_v1::Event::Resumed => "resumed",
            _ => return,
        };
        println!("{line}");
        state.log.push(line.to_string());
    }
}

impl Dispatch<ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, ()> for Probe {
    fn event(
        _: &mut Self,
        _: &ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1,
        _: ext_foreign_toplevel_list_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    wayland_client::event_created_child!(Probe, ext_foreign_toplevel_list_v1::ExtForeignToplevelListV1, [
        ext_foreign_toplevel_list_v1::EVT_TOPLEVEL_OPCODE => (ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1, ()> for Probe {
    fn event(
        state: &mut Self,
        _: &ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
        event: ext_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let line = match event {
            ext_foreign_toplevel_handle_v1::Event::Identifier { identifier } => format!("toplevel identifier {identifier}"),
            ext_foreign_toplevel_handle_v1::Event::Title { title } => format!("toplevel title {title}"),
            ext_foreign_toplevel_handle_v1::Event::AppId { app_id } => format!("toplevel app_id {app_id}"),
            ext_foreign_toplevel_handle_v1::Event::Done => "toplevel done".to_string(),
            ext_foreign_toplevel_handle_v1::Event::Closed => "toplevel closed".to_string(),
            _ => return,
        };
        state.log.push(line);
    }
}

impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for Probe {
    fn event(
        _: &mut Self,
        _: &zxdg_output_manager_v1::ZxdgOutputManagerV1,
        _: zxdg_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for Probe {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let line = match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => format!("output {id}: logical_position {x},{y}"),
            zxdg_output_v1::Event::LogicalSize { width, height } => format!("output {id}: logical_size {width}x{height}"),
            zxdg_output_v1::Event::Name { name } => format!("output {id}: name {name}"),
            zxdg_output_v1::Event::Description { description } => format!("output {id}: description {description}"),
            zxdg_output_v1::Event::Done => format!("output {id}: done"),
            _ => return,
        };
        state.xdg_events.push(line);
    }
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "globals".into());
    let conn = match Connection::connect_to_env() {
        Ok(conn) => conn,
        Err(err) => {
            eprintln!("cannot connect to a Wayland server: {err}");
            std::process::exit(1);
        }
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut probe = Probe::default();
    if queue.roundtrip(&mut probe).is_err() || queue.roundtrip(&mut probe).is_err() {
        eprintln!("roundtrip failed");
        std::process::exit(1);
    }

    match mode.as_str() {
        "globals" => {
            for (name, interface, version) in &probe.globals {
                println!("{name}\t{interface}\tv{version}");
            }
        }
        "xdg-output" => {
            let Some(manager) = probe.xdg_manager.clone() else {
                println!("zxdg_output_manager_v1 is not advertised");
                std::process::exit(2);
            };
            for (name, output) in probe.outputs.clone() {
                let _ = manager.get_xdg_output(&output, &qh, name);
            }
            if queue.roundtrip(&mut probe).is_err() || queue.roundtrip(&mut probe).is_err() {
                eprintln!("roundtrip failed");
                std::process::exit(1);
            }
            for line in &probe.xdg_events {
                println!("{line}");
            }
        }
        "toplevels" => {
            if probe.toplevel_list.is_none() {
                println!("ext_foreign_toplevel_list_v1 is not advertised");
                std::process::exit(2);
            }
            if queue.roundtrip(&mut probe).is_err() || queue.roundtrip(&mut probe).is_err() {
                eprintln!("roundtrip failed");
                std::process::exit(1);
            }
            for line in &probe.log {
                println!("{line}");
            }
        }
        "gamma" => {
            let (Some(manager), Some((_, output))) = (probe.gamma_manager.clone(), probe.outputs.first().cloned()) else {
                println!("zwlr_gamma_control_manager_v1 is not advertised");
                std::process::exit(2);
            };
            let _control = manager.get_gamma_control(&output, &qh, ());
            for _ in 0..3 {
                if queue.roundtrip(&mut probe).is_err() {
                    eprintln!("roundtrip failed");
                    std::process::exit(1);
                }
            }
            // Hold the control briefly so the ramp stays applied while observed.
            std::thread::sleep(std::time::Duration::from_secs(4));
        }
        "vpointer" => {
            let x: u32 = std::env::args().nth(2).and_then(|v| v.parse().ok()).unwrap_or(500);
            let y: u32 = std::env::args().nth(3).and_then(|v| v.parse().ok()).unwrap_or(500);
            let click = std::env::args().nth(4).as_deref() == Some("click");
            let (Some(manager), Some(seat)) = (probe.vp_manager.clone(), probe.seat.clone()) else {
                println!("zwlr_virtual_pointer_manager_v1 is not advertised");
                std::process::exit(2);
            };
            let vp = manager.create_virtual_pointer(Some(&seat), &qh, ());
            vp.motion_absolute(0, x, y, 1000, 1000);
            vp.frame();
            if click {
                vp.button(1, 0x110, wl_pointer::ButtonState::Pressed);
                vp.frame();
                vp.button(2, 0x110, wl_pointer::ButtonState::Released);
                vp.frame();
            }
            let _ = queue.roundtrip(&mut probe);
            println!("virtual pointer moved to {x},{y}{}", if click { " and clicked" } else { "" });
        }
        "power" => {
            let want = std::env::args().nth(2).unwrap_or_else(|| "off".into());
            let (Some(manager), Some((_, output))) = (probe.power_manager.clone(), probe.outputs.first().cloned()) else {
                println!("zwlr_output_power_manager_v1 is not advertised");
                std::process::exit(2);
            };
            let power = manager.get_output_power(&output, &qh, ());
            let _ = queue.roundtrip(&mut probe);
            power.set_mode(if want == "on" { zwlr_output_power_v1::Mode::On } else { zwlr_output_power_v1::Mode::Off });
            for _ in 0..3 {
                let _ = queue.roundtrip(&mut probe);
            }
            // Hold the object so the state is observable, then release.
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        "extcopy" => {
            use std::io::Read;
            use std::os::fd::AsFd;
            let (Some(sources), Some(manager), Some(shm), Some((_, output))) =
                (probe.source_manager.clone(), probe.copy_manager.clone(), probe.shm.clone(), probe.outputs.first().cloned())
            else {
                println!("ext-image-copy-capture / output source / wl_shm is not advertised");
                std::process::exit(2);
            };
            let source = sources.create_source(&output, &qh, ());
            let session = manager.create_session(&source, ext_image_copy_capture_manager_v1::Options::PaintCursors, &qh, ());
            for _ in 0..3 {
                if queue.roundtrip(&mut probe).is_err() {
                    eprintln!("roundtrip failed");
                    std::process::exit(1);
                }
            }
            let Some((w, h)) = probe.session_size.filter(|_| probe.session_done) else {
                println!("session gave no buffer size");
                std::process::exit(2);
            };
            println!("session buffer {w}x{h}");
            let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
            let path = format!("{dir}/extcopy-{}.shm", std::process::id());
            let mut file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path).expect("shm file");
            std::io::Write::write_all(&mut file, &vec![0u8; (w * h * 4) as usize]).expect("size the file");
            let pool = shm.create_pool(file.as_fd(), (w * h * 4) as i32, &qh, ());
            let buffer = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888, &qh, ());
            let frame = session.create_frame(&qh, ());
            frame.attach_buffer(&buffer);
            frame.capture();
            for _ in 0..5 {
                if queue.roundtrip(&mut probe).is_err() || probe.frame_ready || probe.frame_failed {
                    break;
                }
            }
            println!("frame ready: {}", probe.frame_ready);
            if probe.frame_ready {
                let mut bytes = Vec::new();
                let _ = std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0));
                let _ = file.read_to_end(&mut bytes);
                let bg = bytes.chunks_exact(4).filter(|p| p[0] == 0x14 && p[1] == 0x14 && p[2] == 0x14).count();
                println!("captured {} pixels, {} are the compositor background (0x141414)", bytes.len() / 4, bg);
            }
            let _ = std::fs::remove_file(&path);
        }
        "sandbox" => {
            use std::os::fd::AsFd;
            const PRIVILEGED: &[&str] = &[
                "zwlr_screencopy_manager_v1",
                "zwlr_virtual_pointer_manager_v1",
                "zwlr_output_manager_v1",
                "zwlr_gamma_control_manager_v1",
                "zwlr_foreign_toplevel_manager_v1",
                "ext_session_lock_manager_v1",
                "zwlr_layer_shell_v1",
                "zwp_virtual_keyboard_manager_v1",
                "zwlr_data_control_manager_v1",
                "wp_security_context_manager_v1",
            ];
            let Some(manager) = probe.security_manager.clone() else {
                println!("wp_security_context_manager_v1 is not advertised");
                std::process::exit(2);
            };
            let visible = |globals: &[(u32, String, u32)]| -> Vec<String> {
                PRIVILEGED.iter().filter(|p| globals.iter().any(|(_, i, _)| i == *p)).map(|p| p.to_string()).collect()
            };
            println!("normal client sees {} of {} privileged protocols", visible(&probe.globals).len(), PRIVILEGED.len());

            let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
            let path = format!("{dir}/probe-sandbox-{}.sock", std::process::id());
            let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind the sandbox socket");
            let (close_read, close_write) = std::io::pipe().expect("pipe");
            let context = manager.create_listener(listener.as_fd(), close_read.as_fd(), &qh, ());
            context.set_sandbox_engine("probe".into());
            context.set_app_id("org.example.probe".into());
            context.commit();
            if queue.roundtrip(&mut probe).is_err() {
                eprintln!("roundtrip failed");
                std::process::exit(1);
            }
            // Connect through the new socket, as the sandboxed app would.
            let stream = std::os::unix::net::UnixStream::connect(&path).expect("connect to the sandbox socket");
            let sandbox_conn = Connection::from_socket(stream).expect("wayland connection");
            let mut sandbox_queue = sandbox_conn.new_event_queue();
            let sandbox_qh = sandbox_queue.handle();
            let _r = sandbox_conn.display().get_registry(&sandbox_qh, ());
            let mut sandboxed = Probe::default();
            if sandbox_queue.roundtrip(&mut sandboxed).is_err() || sandbox_queue.roundtrip(&mut sandboxed).is_err() {
                eprintln!("sandboxed roundtrip failed");
                std::process::exit(1);
            }
            let seen = visible(&sandboxed.globals);
            println!("sandboxed client sees {} globals in total, {} privileged: {:?}", sandboxed.globals.len(), seen.len(), seen);
            drop(close_write);
            let _ = std::fs::remove_file(&path);
        }
        "vrel" => {
            let dx: f64 = std::env::args().nth(2).and_then(|v| v.parse().ok()).unwrap_or(10.0);
            let dy: f64 = std::env::args().nth(3).and_then(|v| v.parse().ok()).unwrap_or(0.0);
            let (Some(manager), Some(seat)) = (probe.vp_manager.clone(), probe.seat.clone()) else {
                println!("zwlr_virtual_pointer_manager_v1 is not advertised");
                std::process::exit(2);
            };
            let vp = manager.create_virtual_pointer(Some(&seat), &qh, ());
            vp.motion(0, dx, dy);
            vp.frame();
            let _ = queue.roundtrip(&mut probe);
            println!("virtual pointer moved by {dx},{dy}");
        }
        "idle" => {
            let ms: u32 = std::env::args().nth(2).and_then(|v| v.parse().ok()).unwrap_or(3000);
            let (Some(notifier), Some(seat)) = (probe.idle_notifier.clone(), probe.seat.clone()) else {
                println!("ext_idle_notifier_v1 is not advertised");
                std::process::exit(2);
            };
            let _notification = notifier.get_idle_notification(ms, &seat, &qh, ());
            println!("waiting for idle after {ms} ms");
            // Runs until killed (`timeout SECONDS protocol_probe idle MS`).
            while queue.blocking_dispatch(&mut probe).is_ok() {}
        }
        other => {
            eprintln!("unknown mode '{other}' (globals, xdg-output, toplevels, idle)");
            std::process::exit(2);
        }
    }
}
