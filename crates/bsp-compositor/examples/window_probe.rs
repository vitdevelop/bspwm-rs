//! A minimal Wayland *window* client for live-testing the protocols that
//! need a surface (run it in the QEMU test VM against `bspwm-rs`):
//!
//! ```text
//! window_probe [--title T] [--color RRGGBB] [--inhibit-idle]
//!              [--inhibit-shortcuts] [--viewport] [--cursor SHAPE]
//!              [--activate-self-after SECS] [--vkbd] [--decoration]
//!              [--fractional] [--single-pixel] [--lock-pointer] [--lock SECS]
//!              [--presentation] [--tearing] [--text-input] [--exit-after SECS]
//!              [--modal] [--alpha PERCENT] [--fifo] [--commit-timing] [--tag T]
//!              [--icon NAME] [--bell] [--export] [--drag] [--gestures] [--tablet]
//! ```
//!
//! It maps a toplevel showing a solid colour (a 64×64 shm buffer, scaled to
//! the configured size when `--viewport` is given), and prints what it
//! observes: `configured WxH` (then `states [...]`), `wm_capabilities [...]`,
//! `keyboard enter`, `pointer enter`,
//! `shortcuts inhibitor active`, `activation token …`. `--cursor` sets a
//! `wp_cursor_shape` on the first pointer enter, `--activate-self-after`
//! asks `xdg_activation_v1` (with the keyboard-enter serial) to activate this
//! window after a delay — used to test focus stealing from a *background*
//! window. `--vkbd` creates a `zwp_virtual_keyboard_v1` (re-uploading the
//! seat's own keymap) and types the `A` key through it; the window prints
//! `key N pressed|released` for every key it receives. `--decoration` requests
//! `xdg_decoration` and prints the mode it is given; `--fractional` prints the
//! `wp_fractional_scale` preferred scale; `--single-pixel` shows the colour
//! through a `wp_single_pixel_buffer` instead of an shm buffer; `--lock-pointer`
//! locks the pointer on enter and prints `relative motion`. `--lock SECS`
//! locks the session with `ext_session_lock_v1` (a solid purple lock surface
//! per output), holds it for `SECS` seconds, then unlocks. `--presentation`
//! asks `wp_presentation` for feedback on its first frame and prints
//! `presented seq=… refresh=… ns`. `--tearing` sets
//! `wp_tearing_control` async; `--text-input` enables a `zwp_text_input_v3`
//! (nothing answers without an input method engine — this only proves the
//! protocol is served without errors). `--modal` marks the toplevel a modal
//! `xdg_dialog_v1`; `--alpha` sets `wp_alpha_modifier_v1` opacity (percent);
//! `--fifo` and `--commit-timing` commit three frames through `wp_fifo_v1` /
//! `wp_commit_timer_v1` and print when each callback fires; `--tag` and
//! `--icon` set an `xdg_toplevel_tag` and an icon name; `--bell` rings the
//! `xdg_system_bell`; `--export` exports the toplevel through `xdg_foreign_v2`
//! and prints the handle; `--drag` starts a drag with an
//! `xdg_toplevel_drag_v1` on the first button press (the toplevel follows the
//! pointer); `--gestures` and `--tablet` bind `zwp_pointer_gestures_v1` /
//! `zwp_tablet_manager_v2` and print the events they deliver.

use std::io::Write;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use wayland_client::protocol::{wl_output, wl_buffer, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::{wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1};
use wayland_protocols::wp::idle_inhibit::zv1::client::{zwp_idle_inhibit_manager_v1, zwp_idle_inhibitor_v1};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1, zwp_keyboard_shortcuts_inhibitor_v1,
};
use wayland_protocols::wp::fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1};
use wayland_protocols::wp::pointer_constraints::zv1::client::{zwp_locked_pointer_v1, zwp_pointer_constraints_v1};
use wayland_protocols::wp::relative_pointer::zv1::client::{zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1;
use wayland_protocols::ext::session_lock::v1::client::{ext_session_lock_manager_v1, ext_session_lock_surface_v1, ext_session_lock_v1};
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};
use wayland_protocols::wp::tearing_control::v1::client::{wp_tearing_control_manager_v1, wp_tearing_control_v1};
use wayland_protocols::wp::text_input::zv3::client::{zwp_text_input_manager_v3, zwp_text_input_v3};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::decoration::zv1::client::{zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};
use wayland_protocols::wp::alpha_modifier::v1::client::{wp_alpha_modifier_surface_v1, wp_alpha_modifier_v1};
use wayland_protocols::wp::commit_timing::v1::client::{wp_commit_timer_v1, wp_commit_timing_manager_v1};
use wayland_protocols::wp::fifo::v1::client::{wp_fifo_manager_v1, wp_fifo_v1};
use wayland_protocols::wp::pointer_gestures::zv1::client::{
    zwp_pointer_gesture_hold_v1, zwp_pointer_gesture_pinch_v1, zwp_pointer_gesture_swipe_v1, zwp_pointer_gestures_v1,
};
use wayland_protocols::wp::tablet::zv2::client::{zwp_tablet_manager_v2, zwp_tablet_seat_v2, zwp_tablet_tool_v2, zwp_tablet_v2};
use wayland_protocols::xdg::dialog::v1::client::{xdg_dialog_v1, xdg_wm_dialog_v1};
use wayland_protocols::xdg::foreign::zv2::client::{zxdg_exported_v2, zxdg_exporter_v2};
use wayland_protocols::xdg::system_bell::v1::client::xdg_system_bell_v1;
use wayland_protocols::xdg::toplevel_drag::v1::client::{xdg_toplevel_drag_manager_v1, xdg_toplevel_drag_v1};
use wayland_protocols::xdg::toplevel_icon::v1::client::{xdg_toplevel_icon_manager_v1, xdg_toplevel_icon_v1};
use wayland_protocols::xdg::toplevel_tag::v1::client::xdg_toplevel_tag_manager_v1;

#[derive(Default)]
struct App {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    seat: Option<wl_seat::WlSeat>,
    idle_inhibit: Option<zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1>,
    shortcuts: Option<zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    cursor_shape: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
    activation: Option<xdg_activation_v1::XdgActivationV1>,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard_serial: Option<u32>,
    surface: Option<wl_surface::WlSurface>,
    viewport: Option<wp_viewport::WpViewport>,
    size: (i32, i32),
    want_viewport: bool,
    cursor: Option<String>,
    cursor_set: bool,
    activation_token: Option<String>,
    vkbd_manager: Option<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1>,
    keymap: Option<(std::os::fd::OwnedFd, u32)>,
    decoration_manager: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    fractional_manager: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    single_pixel: Option<wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1>,
    constraints: Option<zwp_pointer_constraints_v1::ZwpPointerConstraintsV1>,
    relative: Option<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1>,
    lock_pointer: bool,
    locked: bool,
    outputs: Vec<wl_output::WlOutput>,
    lock_manager: Option<ext_session_lock_manager_v1::ExtSessionLockManagerV1>,
    lock_files: u32,
    presentation: Option<wp_presentation::WpPresentation>,
    tearing: Option<wp_tearing_control_manager_v1::WpTearingControlManagerV1>,
    text_input: Option<zwp_text_input_manager_v3::ZwpTextInputManagerV3>,
    alpha: Option<wp_alpha_modifier_v1::WpAlphaModifierV1>,
    fifo: Option<wp_fifo_manager_v1::WpFifoManagerV1>,
    commit_timing: Option<wp_commit_timing_manager_v1::WpCommitTimingManagerV1>,
    dialog: Option<xdg_wm_dialog_v1::XdgWmDialogV1>,
    exporter: Option<zxdg_exporter_v2::ZxdgExporterV2>,
    icon_manager: Option<xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1>,
    tag_manager: Option<xdg_toplevel_tag_manager_v1::XdgToplevelTagManagerV1>,
    bell: Option<xdg_system_bell_v1::XdgSystemBellV1>,
    drag_manager: Option<xdg_toplevel_drag_manager_v1::XdgToplevelDragManagerV1>,
    data_manager: Option<wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager>,
    gestures: Option<zwp_pointer_gestures_v1::ZwpPointerGesturesV1>,
    tablet_manager: Option<zwp_tablet_manager_v2::ZwpTabletManagerV2>,
    want_drag: bool,
    drag_started: bool,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    frames: u32,
}

impl Dispatch<wl_registry::WlRegistry, ()> for App {
    fn event(app: &mut Self, registry: &wl_registry::WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        let wl_registry::Event::Global { name, interface, version } = event else {
            return;
        };
        match interface.as_str() {
            "wl_compositor" => app.compositor = Some(registry.bind(name, version.min(4), qh, ())),
            "wl_shm" => app.shm = Some(registry.bind(name, 1, qh, ())),
            "xdg_wm_base" => app.wm_base = Some(registry.bind(name, version.min(6), qh, ())),
            "wl_seat" if app.seat.is_none() => app.seat = Some(registry.bind(name, version.min(5), qh, ())),
            "zwp_idle_inhibit_manager_v1" => app.idle_inhibit = Some(registry.bind(name, 1, qh, ())),
            "zwp_keyboard_shortcuts_inhibit_manager_v1" => app.shortcuts = Some(registry.bind(name, 1, qh, ())),
            "wp_viewporter" => app.viewporter = Some(registry.bind(name, 1, qh, ())),
            "wp_cursor_shape_manager_v1" => app.cursor_shape = Some(registry.bind(name, 1, qh, ())),
            "xdg_activation_v1" => app.activation = Some(registry.bind(name, 1, qh, ())),
            "wp_tearing_control_manager_v1" => app.tearing = Some(registry.bind(name, 1, qh, ())),
            "zwp_text_input_manager_v3" => app.text_input = Some(registry.bind(name, 1, qh, ())),
            "wp_presentation" => app.presentation = Some(registry.bind(name, 1, qh, ())),
            "wl_output" => app.outputs.push(registry.bind(name, version.min(3), qh, ())),
            "ext_session_lock_manager_v1" => app.lock_manager = Some(registry.bind(name, 1, qh, ())),
            "zxdg_decoration_manager_v1" => app.decoration_manager = Some(registry.bind(name, 1, qh, ())),
            "wp_fractional_scale_manager_v1" => app.fractional_manager = Some(registry.bind(name, 1, qh, ())),
            "wp_single_pixel_buffer_manager_v1" => app.single_pixel = Some(registry.bind(name, 1, qh, ())),
            "zwp_pointer_constraints_v1" => app.constraints = Some(registry.bind(name, 1, qh, ())),
            "zwp_relative_pointer_manager_v1" => app.relative = Some(registry.bind(name, 1, qh, ())),
            "zwp_virtual_keyboard_manager_v1" => app.vkbd_manager = Some(registry.bind(name, 1, qh, ())),
            "wp_alpha_modifier_v1" => app.alpha = Some(registry.bind(name, 1, qh, ())),
            "wp_fifo_manager_v1" => app.fifo = Some(registry.bind(name, 1, qh, ())),
            "wp_commit_timing_manager_v1" => app.commit_timing = Some(registry.bind(name, 1, qh, ())),
            "xdg_wm_dialog_v1" => app.dialog = Some(registry.bind(name, 1, qh, ())),
            "zxdg_exporter_v2" => app.exporter = Some(registry.bind(name, 1, qh, ())),
            "xdg_toplevel_icon_manager_v1" => app.icon_manager = Some(registry.bind(name, 1, qh, ())),
            "xdg_toplevel_tag_manager_v1" => app.tag_manager = Some(registry.bind(name, 1, qh, ())),
            "xdg_system_bell_v1" => app.bell = Some(registry.bind(name, 1, qh, ())),
            "xdg_toplevel_drag_manager_v1" => app.drag_manager = Some(registry.bind(name, 1, qh, ())),
            "wl_data_device_manager" => app.data_manager = Some(registry.bind(name, version.min(3), qh, ())),
            "zwp_pointer_gestures_v1" => app.gestures = Some(registry.bind(name, version.min(3), qh, ())),
            "zwp_tablet_manager_v2" => app.tablet_manager = Some(registry.bind(name, 1, qh, ())),
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
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    wl_surface::WlSurface,
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1,
    zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    wp_viewporter::WpViewporter,
    wp_viewport::WpViewport,
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
    wp_cursor_shape_device_v1::WpCursorShapeDeviceV1,
    xdg_activation_v1::XdgActivationV1,
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    wl_output::WlOutput,
    wp_presentation::WpPresentation,
    wp_tearing_control_manager_v1::WpTearingControlManagerV1,
    wp_tearing_control_v1::WpTearingControlV1,
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    ext_session_lock_manager_v1::ExtSessionLockManagerV1,
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1,
    zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
    zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
    wp_alpha_modifier_v1::WpAlphaModifierV1,
    wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1,
    wp_fifo_manager_v1::WpFifoManagerV1,
    wp_fifo_v1::WpFifoV1,
    wp_commit_timing_manager_v1::WpCommitTimingManagerV1,
    wp_commit_timer_v1::WpCommitTimerV1,
    xdg_wm_dialog_v1::XdgWmDialogV1,
    xdg_dialog_v1::XdgDialogV1,
    zxdg_exporter_v2::ZxdgExporterV2,
    xdg_toplevel_icon_manager_v1::XdgToplevelIconManagerV1,
    xdg_toplevel_icon_v1::XdgToplevelIconV1,
    xdg_toplevel_tag_manager_v1::XdgToplevelTagManagerV1,
    xdg_system_bell_v1::XdgSystemBellV1,
    xdg_toplevel_drag_manager_v1::XdgToplevelDragManagerV1,
    xdg_toplevel_drag_v1::XdgToplevelDragV1,
    wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager,
    zwp_pointer_gestures_v1::ZwpPointerGesturesV1,
    zwp_tablet_manager_v2::ZwpTabletManagerV2,
);

impl Dispatch<wayland_client::protocol::wl_callback::WlCallback, ()> for App {
    fn event(app: &mut Self, _: &wayland_client::protocol::wl_callback::WlCallback, event: wayland_client::protocol::wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wayland_client::protocol::wl_callback::Event::Done { .. } = event {
            app.frames += 1;
            println!("frame callback {} at {} ms", app.frames, START.with(|s| s.elapsed().as_millis()));
        }
    }
}

thread_local! {
    static START: Instant = Instant::now();
}

impl Dispatch<wayland_client::protocol::wl_touch::WlTouch, ()> for App {
    fn event(_: &mut Self, _: &wayland_client::protocol::wl_touch::WlTouch, event: wayland_client::protocol::wl_touch::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wayland_client::protocol::wl_touch::Event::Down { id, x, y, .. } => println!("touch down {id} at {x:.0},{y:.0}"),
            wayland_client::protocol::wl_touch::Event::Motion { id, x, y, .. } => println!("touch motion {id} at {x:.0},{y:.0}"),
            wayland_client::protocol::wl_touch::Event::Up { id, .. } => println!("touch up {id}"),
            _ => {}
        }
    }
}

impl Dispatch<zxdg_exported_v2::ZxdgExportedV2, ()> for App {
    fn event(_: &mut Self, _: &zxdg_exported_v2::ZxdgExportedV2, event: zxdg_exported_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let zxdg_exported_v2::Event::Handle { handle } = event {
            println!("exported handle {}", if handle.is_empty() { "EMPTY" } else { "received" });
        }
    }
}

impl Dispatch<wayland_client::protocol::wl_data_device::WlDataDevice, ()> for App {
    fn event(_: &mut Self, _: &wayland_client::protocol::wl_data_device::WlDataDevice, _: wayland_client::protocol::wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    wayland_client::event_created_child!(App, wayland_client::protocol::wl_data_device::WlDataDevice, [
        wayland_client::protocol::wl_data_device::EVT_DATA_OFFER_OPCODE => (wayland_client::protocol::wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wayland_client::protocol::wl_data_source::WlDataSource, ()> for App {
    fn event(_: &mut Self, _: &wayland_client::protocol::wl_data_source::WlDataSource, event: wayland_client::protocol::wl_data_source::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wayland_client::protocol::wl_data_source::Event::DndDropPerformed => println!("drag dropped"),
            wayland_client::protocol::wl_data_source::Event::Cancelled => println!("drag cancelled"),
            _ => {}
        }
    }
}

impl Dispatch<wayland_client::protocol::wl_data_offer::WlDataOffer, ()> for App {
    fn event(_: &mut Self, _: &wayland_client::protocol::wl_data_offer::WlDataOffer, _: wayland_client::protocol::wl_data_offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<zwp_pointer_gesture_swipe_v1::ZwpPointerGestureSwipeV1, ()> for App {
    fn event(_: &mut Self, _: &zwp_pointer_gesture_swipe_v1::ZwpPointerGestureSwipeV1, event: zwp_pointer_gesture_swipe_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        println!("swipe gesture {event:?}");
    }
}
impl Dispatch<zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1, ()> for App {
    fn event(_: &mut Self, _: &zwp_pointer_gesture_pinch_v1::ZwpPointerGesturePinchV1, event: zwp_pointer_gesture_pinch_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        println!("pinch gesture {event:?}");
    }
}
impl Dispatch<zwp_pointer_gesture_hold_v1::ZwpPointerGestureHoldV1, ()> for App {
    fn event(_: &mut Self, _: &zwp_pointer_gesture_hold_v1::ZwpPointerGestureHoldV1, event: zwp_pointer_gesture_hold_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        println!("hold gesture {event:?}");
    }
}

impl Dispatch<zwp_tablet_seat_v2::ZwpTabletSeatV2, ()> for App {
    fn event(_: &mut Self, _: &zwp_tablet_seat_v2::ZwpTabletSeatV2, event: zwp_tablet_seat_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwp_tablet_seat_v2::Event::TabletAdded { .. } => println!("tablet added"),
            zwp_tablet_seat_v2::Event::ToolAdded { .. } => println!("tablet tool added"),
            _ => {}
        }
    }
    wayland_client::event_created_child!(App, zwp_tablet_seat_v2::ZwpTabletSeatV2, [
        zwp_tablet_seat_v2::EVT_TABLET_ADDED_OPCODE => (zwp_tablet_v2::ZwpTabletV2, ()),
        zwp_tablet_seat_v2::EVT_TOOL_ADDED_OPCODE => (zwp_tablet_tool_v2::ZwpTabletToolV2, ()),
        zwp_tablet_seat_v2::EVT_PAD_ADDED_OPCODE => (wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_v2::ZwpTabletPadV2, ()),
    ]);
}
impl Dispatch<zwp_tablet_v2::ZwpTabletV2, ()> for App {
    fn event(_: &mut Self, _: &zwp_tablet_v2::ZwpTabletV2, _: zwp_tablet_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<zwp_tablet_tool_v2::ZwpTabletToolV2, ()> for App {
    fn event(_: &mut Self, _: &zwp_tablet_tool_v2::ZwpTabletToolV2, event: zwp_tablet_tool_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        println!("tablet tool {event:?}");
    }
}
impl Dispatch<wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_v2::ZwpTabletPadV2, ()> for App {
    fn event(_: &mut Self, _: &wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_v2::ZwpTabletPadV2, _: wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
    wayland_client::event_created_child!(App, wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_v2::ZwpTabletPadV2, [
        wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_v2::EVT_GROUP_OPCODE => (wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, ()),
    ]);
}
impl Dispatch<wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, ()> for App {
    fn event(_: &mut Self, _: &wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2, _: wayland_protocols::wp::tablet::zv2::client::zwp_tablet_pad_group_v2::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, ()> for App {
    fn event(_: &mut Self, _: &wp_presentation_feedback::WpPresentationFeedback, event: wp_presentation_feedback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wp_presentation_feedback::Event::Presented { seq_lo, refresh, .. } => println!("presented seq={seq_lo} refresh={refresh} ns"),
            wp_presentation_feedback::Event::Discarded => println!("presentation discarded"),
            _ => {}
        }
    }
}

impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for App {
    fn event(_: &mut Self, _: &zwp_text_input_v3::ZwpTextInputV3, event: zwp_text_input_v3::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let zwp_text_input_v3::Event::Enter { .. } = event {
            println!("text input enter");
        }
    }
}

impl Dispatch<ext_session_lock_v1::ExtSessionLockV1, ()> for App {
    fn event(_: &mut Self, _: &ext_session_lock_v1::ExtSessionLockV1, event: ext_session_lock_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            ext_session_lock_v1::Event::Locked => println!("session locked"),
            ext_session_lock_v1::Event::Finished => println!("lock finished (denied or ended)"),
            _ => {}
        }
    }
}

impl Dispatch<ext_session_lock_surface_v1::ExtSessionLockSurfaceV1, wl_surface::WlSurface> for App {
    fn event(app: &mut Self, lock_surface: &ext_session_lock_surface_v1::ExtSessionLockSurfaceV1, event: ext_session_lock_surface_v1::Event, surface: &wl_surface::WlSurface, _: &Connection, qh: &QueueHandle<Self>) {
        if let ext_session_lock_surface_v1::Event::Configure { serial, width, height } = event {
            lock_surface.ack_configure(serial);
            println!("lock surface configured {width}x{height}");
            let Some(shm) = app.shm.clone() else { return };
            let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
            app.lock_files += 1;
            let path = format!("{dir}/lock-{}-{}.shm", std::process::id(), app.lock_files);
            let pixels: Vec<u8> = (0..width * height).flat_map(|_| 0xff8822aau32.to_le_bytes()).collect();
            if let Ok(mut file) = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path) {
                if file.write_all(&pixels).is_ok() {
                    let pool = shm.create_pool(file.as_fd(), pixels.len() as i32, qh, ());
                    let buffer = pool.create_buffer(0, width as i32, height as i32, width as i32 * 4, wl_shm::Format::Argb8888, qh, ());
                    surface.attach(Some(&buffer), 0, 0);
                    surface.damage(0, 0, width as i32, height as i32);
                    surface.commit();
                }
            }
            let _ = std::fs::remove_file(&path);
        }
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()> for App {
    fn event(_: &mut Self, _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, event: zxdg_toplevel_decoration_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let zxdg_toplevel_decoration_v1::Event::Configure { mode } = event {
            println!("decoration mode {mode:?}");
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for App {
    fn event(_: &mut Self, _: &wp_fractional_scale_v1::WpFractionalScaleV1, event: wp_fractional_scale_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            println!("preferred scale {:.3}", scale as f64 / 120.0);
        }
    }
}

impl Dispatch<zwp_locked_pointer_v1::ZwpLockedPointerV1, ()> for App {
    fn event(app: &mut Self, _: &zwp_locked_pointer_v1::ZwpLockedPointerV1, event: zwp_locked_pointer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            zwp_locked_pointer_v1::Event::Locked => {
                app.locked = true;
                println!("pointer locked");
            }
            zwp_locked_pointer_v1::Event::Unlocked => println!("pointer unlocked"),
            _ => {}
        }
    }
}

impl Dispatch<zwp_relative_pointer_v1::ZwpRelativePointerV1, ()> for App {
    fn event(_: &mut Self, _: &zwp_relative_pointer_v1::ZwpRelativePointerV1, event: zwp_relative_pointer_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion { dx, dy, .. } = event {
            println!("relative motion {dx:.0},{dy:.0}");
        }
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(_: &mut Self, wm_base: &xdg_wm_base::XdgWmBase, event: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(app: &mut Self, xdg: &xdg_surface::XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            if app.want_viewport {
                if let (Some(viewport), (w, h)) = (&app.viewport, app.size) {
                    if w > 0 && h > 0 {
                        viewport.set_destination(w, h);
                    }
                }
            }
            if let Some(surface) = &app.surface {
                surface.commit();
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for App {
    fn event(app: &mut Self, _: &xdg_toplevel::XdgToplevel, event: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            xdg_toplevel::Event::Configure { width, height, states } => {
                app.size = (width, height);
                println!("configured {width}x{height}");
                let names: Vec<&str> = states
                    .chunks_exact(4)
                    .map(|c| match u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) {
                        1 => "maximized",
                        2 => "fullscreen",
                        3 => "resizing",
                        4 => "activated",
                        5..=8 => "tiled",
                        9 => "suspended",
                        _ => "other",
                    })
                    .collect();
                println!("states {names:?}");
            }
            xdg_toplevel::Event::WmCapabilities { capabilities } => {
                let names: Vec<&str> = capabilities
                    .chunks_exact(4)
                    .map(|c| match u32::from_ne_bytes([c[0], c[1], c[2], c[3]]) {
                        1 => "window_menu",
                        2 => "maximize",
                        3 => "fullscreen",
                        4 => "minimize",
                        _ => "other",
                    })
                    .collect();
                println!("wm_capabilities {names:?}");
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for App {
    fn event(app: &mut Self, seat: &wl_seat::WlSeat, event: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event {
            if caps.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, ());
            }
            if caps.contains(wl_seat::Capability::Pointer) && app.pointer.is_none() {
                app.pointer = Some(seat.get_pointer(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Touch) {
                seat.get_touch(qh, ());
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for App {
    fn event(app: &mut Self, _: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_keyboard::Event::Enter { serial, .. } => {
                app.keyboard_serial = Some(serial);
                println!("keyboard enter");
            }
            wl_keyboard::Event::Leave { .. } => println!("keyboard leave"),
            wl_keyboard::Event::Keymap { fd, size, .. } => app.keymap = Some((fd, size)),
            wl_keyboard::Event::Key { key, state, .. } => {
                let what = if matches!(state, WEnum::Value(wl_keyboard::KeyState::Pressed)) { "pressed" } else { "released" };
                println!("key {key} {what}");
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for App {
    fn event(app: &mut Self, pointer: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        match &event {
            wl_pointer::Event::Motion { surface_x, surface_y, .. } => println!("pointer motion {surface_x:.0},{surface_y:.0}"),
            wl_pointer::Event::Button { button, state, serial, .. } => {
                let pressed = matches!(state, WEnum::Value(wl_pointer::ButtonState::Pressed));
                let what = if pressed { "pressed" } else { "released" };
                println!("pointer button {button} {what}");
                if pressed && app.want_drag && !app.drag_started {
                    if let (Some(manager), Some(drag_manager), Some(seat), Some(surface), Some(toplevel)) =
                        (app.data_manager.clone(), app.drag_manager.clone(), app.seat.clone(), app.surface.clone(), app.toplevel.clone())
                    {
                        app.drag_started = true;
                        let source = manager.create_data_source(qh, ());
                        source.offer("text/plain".to_string());
                        let drag = drag_manager.get_xdg_toplevel_drag(&source, qh, ());
                        drag.attach(&toplevel, 10, 10);
                        let device = manager.get_data_device(&seat, qh, ());
                        device.start_drag(Some(&source), &surface, None, *serial);
                        println!("toplevel drag started");
                    }
                }
            }
            _ => {}
        }
        if let wl_pointer::Event::Enter { serial, .. } = event {
            println!("pointer enter");
            if app.lock_pointer && !app.locked {
                if let (Some(constraints), Some(surface), Some(relative)) = (app.constraints.clone(), app.surface.clone(), app.relative.clone()) {
                    let _ = constraints.lock_pointer(&surface, pointer, None, zwp_pointer_constraints_v1::Lifetime::Persistent, qh, ());
                    let _ = relative.get_relative_pointer(pointer, qh, ());
                }
            }
            if let (Some(shape), Some(manager), false) = (app.cursor.clone(), &app.cursor_shape, app.cursor_set) {
                use wp_cursor_shape_device_v1::Shape;
                let shape = match shape.as_str() {
                    "crosshair" => Shape::Crosshair,
                    "text" => Shape::Text,
                    "pointer" => Shape::Pointer,
                    "wait" => Shape::Wait,
                    _ => Shape::Default,
                };
                let device = manager.get_pointer(pointer, qh, ());
                device.set_shape(serial, shape);
                app.cursor_set = true;
                println!("cursor shape set");
            }
        }
    }
}

impl Dispatch<zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
        event: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Active => println!("shortcuts inhibitor active"),
            zwp_keyboard_shortcuts_inhibitor_v1::Event::Inactive => println!("shortcuts inhibitor inactive"),
            _ => {}
        }
    }
}

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for App {
    fn event(app: &mut Self, _: &xdg_activation_token_v1::XdgActivationTokenV1, event: xdg_activation_token_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            println!("activation token {}", if token.is_empty() { "EMPTY" } else { "received" });
            app.activation_token = Some(token);
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut title = "window_probe".to_string();
    let mut color: u32 = 0xff2266cc;
    let (mut inhibit_idle, mut inhibit_shortcuts, mut viewport) = (false, false, false);
    let mut cursor = None;
    let mut activate_after: Option<u64> = None;
    let mut vkbd = false;
    let mut lock_secs: Option<u64> = None;
    let mut want_presentation = false;
    let (mut want_tearing, mut want_text_input) = (false, false);
    let (mut want_decoration, mut want_fractional, mut want_single_pixel, mut want_lock) = (false, false, false, false);
    let mut exit_after: Option<u64> = None;
    let (mut modal, mut want_fifo, mut want_timing, mut want_bell, mut want_export, mut want_drag) = (false, false, false, false, false, false);
    let (mut want_gestures, mut want_tablet) = (false, false);
    let mut want_fullscreen = false;
    let (mut alpha, mut tag, mut icon): (Option<u32>, Option<String>, Option<String>) = (None, None, None);
    START.with(|_| ());
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--title" => title = args.next().unwrap_or_default(),
            "--color" => color = 0xff000000 | u32::from_str_radix(&args.next().unwrap_or_default(), 16).unwrap_or(0x2266cc),
            "--inhibit-idle" => inhibit_idle = true,
            "--inhibit-shortcuts" => inhibit_shortcuts = true,
            "--viewport" => viewport = true,
            "--cursor" => cursor = args.next(),
            "--activate-self-after" => activate_after = args.next().and_then(|v| v.parse().ok()),
            "--vkbd" => vkbd = true,
            "--tearing" => want_tearing = true,
            "--text-input" => want_text_input = true,
            "--presentation" => want_presentation = true,
            "--lock" => lock_secs = args.next().and_then(|v| v.parse().ok()),
            "--decoration" => want_decoration = true,
            "--fractional" => want_fractional = true,
            "--single-pixel" => want_single_pixel = true,
            "--lock-pointer" => want_lock = true,
            "--exit-after" => exit_after = args.next().and_then(|v| v.parse().ok()),
            "--modal" => modal = true,
            "--fullscreen" => want_fullscreen = true,
            "--alpha" => alpha = args.next().and_then(|v| v.parse().ok()),
            "--fifo" => want_fifo = true,
            "--commit-timing" => want_timing = true,
            "--tag" => tag = args.next(),
            "--icon" => icon = args.next(),
            "--bell" => want_bell = true,
            "--export" => want_export = true,
            "--drag" => want_drag = true,
            "--gestures" => want_gestures = true,
            "--tablet" => want_tablet = true,
            other => {
                eprintln!("unknown option {other}");
                std::process::exit(2);
            }
        }
    }

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
    let mut app = App { want_viewport: viewport, cursor, lock_pointer: want_lock, want_drag, ..Default::default() };
    if queue.roundtrip(&mut app).is_err() || queue.roundtrip(&mut app).is_err() {
        eprintln!("roundtrip failed");
        std::process::exit(1);
    }
    let (Some(compositor), Some(shm), Some(wm_base)) = (app.compositor.clone(), app.shm.clone(), app.wm_base.clone()) else {
        eprintln!("missing wl_compositor / wl_shm / xdg_wm_base");
        std::process::exit(1);
    };

    // A 64x64 solid-colour buffer in a plain file (the compositor mmaps it).
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    let path = format!("{dir}/window_probe-{}.shm", std::process::id());
    let pixels: Vec<u8> = (0..64 * 64).flat_map(|_| color.to_le_bytes()).collect();
    let mut file = match std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&path) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("cannot create {path}: {err}");
            std::process::exit(1);
        }
    };
    if file.write_all(&pixels).is_err() {
        eprintln!("cannot write the shm file");
        std::process::exit(1);
    }
    let pool = shm.create_pool(file.as_fd(), pixels.len() as i32, &qh, ());
    let buffer = pool.create_buffer(0, 64, 64, 64 * 4, wl_shm::Format::Argb8888, &qh, ());
    let _ = std::fs::remove_file(&path);

    let surface = compositor.create_surface(&qh, ());
    app.surface = Some(surface.clone());
    if viewport {
        if let Some(viewporter) = &app.viewporter {
            app.viewport = Some(viewporter.get_viewport(&surface, &qh, ()));
        } else {
            println!("wp_viewporter is not advertised");
        }
    }
    if want_fractional {
        match &app.fractional_manager {
            Some(manager) => {
                let _ = manager.get_fractional_scale(&surface, &qh, ());
            }
            None => println!("wp_fractional_scale_manager_v1 is not advertised"),
        }
    }
    let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    app.toplevel = Some(toplevel.clone());
    if want_decoration {
        match &app.decoration_manager {
            Some(manager) => {
                let deco = manager.get_toplevel_decoration(&toplevel, &qh, ());
                deco.set_mode(zxdg_toplevel_decoration_v1::Mode::ClientSide);
            }
            None => println!("zxdg_decoration_manager_v1 is not advertised"),
        }
    }
    toplevel.set_title(title.clone());
    toplevel.set_app_id(title.clone());
    if want_fullscreen {
        toplevel.set_fullscreen(None);
    }
    if modal {
        match &app.dialog {
            Some(manager) => {
                manager.get_xdg_dialog(&toplevel, &qh, ()).set_modal();
                println!("modal dialog requested");
            }
            None => println!("xdg_wm_dialog_v1 is not advertised"),
        }
    }
    if let Some(tag) = &tag {
        match &app.tag_manager {
            Some(manager) => {
                manager.set_toplevel_tag(&toplevel, tag.clone());
                manager.set_toplevel_description(&toplevel, format!("{tag} window"));
                println!("toplevel tag set");
            }
            None => println!("xdg_toplevel_tag_manager_v1 is not advertised"),
        }
    }
    if let Some(name) = &icon {
        match &app.icon_manager {
            Some(manager) => {
                let icon = manager.create_icon(&qh, ());
                icon.set_name(name.clone());
                manager.set_icon(&toplevel, Some(&icon));
                println!("toplevel icon set");
            }
            None => println!("xdg_toplevel_icon_manager_v1 is not advertised"),
        }
    }
    if let Some(percent) = alpha {
        match &app.alpha {
            Some(manager) => {
                let modifier = manager.get_surface(&surface, &qh, ());
                modifier.set_multiplier((u32::MAX as f64 * percent as f64 / 100.0) as u32);
                println!("alpha {percent}% set");
            }
            None => println!("wp_alpha_modifier_v1 is not advertised"),
        }
    }
    if want_export {
        match &app.exporter {
            Some(exporter) => {
                let _ = exporter.export_toplevel(&surface, &qh, ());
            }
            None => println!("zxdg_exporter_v2 is not advertised"),
        }
    }
    if inhibit_idle {
        match (&app.idle_inhibit, ()) {
            (Some(manager), ()) => {
                let _ = manager.create_inhibitor(&surface, &qh, ());
                println!("idle inhibitor created");
            }
            (None, ()) => println!("zwp_idle_inhibit_manager_v1 is not advertised"),
        }
    }
    if inhibit_shortcuts {
        match (&app.shortcuts, &app.seat) {
            (Some(manager), Some(seat)) => {
                let _ = manager.inhibit_shortcuts(&surface, seat, &qh, ());
            }
            _ => println!("keyboard shortcuts inhibit is not advertised"),
        }
    }
    surface.commit();
    // First configure arrives after the initial commit; attach once acked.
    let _ = queue.roundtrip(&mut app);
    if want_single_pixel {
        match &app.single_pixel {
            Some(manager) => {
                let c = |shift: u32| ((color >> shift) & 0xff) * 0x0101_0101;
                let single = manager.create_u32_rgba_buffer(c(16), c(8), c(0), u32::MAX, &qh, ());
                surface.attach(Some(&single), 0, 0);
                surface.damage(0, 0, 64, 64);
                if let Some(viewport) = &app.viewport {
                    viewport.set_destination(200, 200);
                }
                println!("single pixel buffer attached");
            }
            None => println!("wp_single_pixel_buffer_manager_v1 is not advertised"),
        }
    } else {
        surface.attach(Some(&buffer), 0, 0);
        surface.damage(0, 0, 64, 64);
    }
    surface.commit();
    if want_presentation {
        match &app.presentation {
            Some(presentation) => {
                let _ = presentation.feedback(&surface, &qh, ());
                surface.damage(0, 0, 64, 64);
                surface.commit();
            }
            None => println!("wp_presentation is not advertised"),
        }
    }
    if want_tearing {
        match &app.tearing {
            Some(manager) => {
                let control = manager.get_tearing_control(&surface, &qh, ());
                control.set_presentation_hint(wp_tearing_control_v1::PresentationHint::Async);
                surface.commit();
                println!("tearing hint set");
            }
            None => println!("wp_tearing_control_manager_v1 is not advertised"),
        }
    }
    if want_text_input {
        match (&app.text_input, &app.seat) {
            (Some(manager), Some(seat)) => {
                let input = manager.get_text_input(seat, &qh, ());
                input.enable();
                input.commit();
                println!("text input enabled");
            }
            _ => println!("zwp_text_input_manager_v3 is not advertised"),
        }
    }
    println!("mapped {title}");
    if want_bell {
        match &app.bell {
            Some(bell) => {
                bell.ring(Some(&surface));
                println!("bell rung");
            }
            None => println!("xdg_system_bell_v1 is not advertised"),
        }
    }
    if want_gestures {
        match (&app.gestures, &app.pointer) {
            (Some(gestures), Some(pointer)) => {
                let _ = gestures.get_swipe_gesture(pointer, &qh, ());
                let _ = gestures.get_pinch_gesture(pointer, &qh, ());
                let _ = gestures.get_hold_gesture(pointer, &qh, ());
                println!("gesture objects created");
            }
            _ => println!("zwp_pointer_gestures_v1 or a pointer is missing"),
        }
    }
    if want_tablet {
        match (&app.tablet_manager, &app.seat) {
            (Some(manager), Some(seat)) => {
                let _ = manager.get_tablet_seat(seat, &qh, ());
                println!("tablet seat requested");
            }
            _ => println!("zwp_tablet_manager_v2 is missing"),
        }
    }
    if want_fifo {
        match &app.fifo {
            Some(manager) => {
                let fifo = manager.get_fifo(&surface, &qh, ());
                // Each commit sets a barrier and waits on the previous one: the
                // compositor may release only one commit per presented frame.
                for _ in 0..4 {
                    fifo.wait_barrier();
                    fifo.set_barrier();
                    surface.frame(&qh, ());
                    surface.damage(0, 0, 64, 64);
                    surface.commit();
                }
                println!("fifo commits sent");
            }
            None => println!("wp_fifo_manager_v1 is not advertised"),
        }
    }
    if want_timing {
        match &app.commit_timing {
            Some(manager) => {
                let timer = manager.get_timer(&surface, &qh, ());
                let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
                let target = now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64 + 800_000_000;
                let (secs, nanos) = (target / 1_000_000_000, target % 1_000_000_000);
                timer.set_timestamp((secs >> 32) as u32, secs as u32, nanos as u32);
                surface.frame(&qh, ());
                surface.damage(0, 0, 64, 64);
                surface.commit();
                println!("commit with a timestamp 800 ms ahead sent at {} ms", START.with(|s| s.elapsed().as_millis()));
            }
            None => println!("wp_commit_timing_manager_v1 is not advertised"),
        }
    }

    // Non-blocking reads, so the timers below fire while the compositor is quiet.
    if let Ok(flags) = rustix::fs::fcntl_getfl(conn.backend().poll_fd()) {
        let _ = rustix::fs::fcntl_setfl(conn.backend().poll_fd(), flags | rustix::fs::OFlags::NONBLOCK);
    }
    let start = Instant::now();
    let mut lock_object = None;
    if let Some(_secs) = lock_secs {
        match &app.lock_manager {
            Some(manager) => {
                let lock = manager.lock(&qh, ());
                for output in app.outputs.clone() {
                    let lock_surface_wl = compositor.create_surface(&qh, ());
                    let _ = lock.get_lock_surface(&lock_surface_wl, &output, &qh, lock_surface_wl.clone());
                }
                lock_object = Some(lock);
            }
            None => println!("ext_session_lock_manager_v1 is not advertised"),
        }
    }
    let mut activated = false;
    let mut typed = false;
    loop {
        if queue.flush().is_err() {
            break;
        }
        if let Some(guard) = queue.prepare_read() {
            let _ = guard.read();
        }
        if queue.dispatch_pending(&mut app).is_err() {
            break;
        }
        if vkbd && !typed && app.keyboard_serial.is_some() && start.elapsed() > Duration::from_secs(2) {
            typed = true;
            match (app.vkbd_manager.clone(), app.seat.clone(), app.keymap.take()) {
                (Some(manager), Some(seat), Some((fd, size))) => {
                    let vk = manager.create_virtual_keyboard(&seat, &qh, ());
                    vk.keymap(1, fd.as_fd(), size);
                    vk.key(0, 30, 1);
                    vk.key(10, 30, 0);
                    println!("virtual keyboard typed A");
                }
                _ => println!("cannot type: no virtual keyboard manager / seat / keymap"),
            }
        }
        if let (Some(lock), Some(secs)) = (&lock_object, lock_secs) {
            if start.elapsed() > Duration::from_secs(secs) {
                lock.unlock_and_destroy();
                println!("unlocked");
                lock_object = None;
            }
        }
        if let Some(secs) = exit_after {
            if start.elapsed() > Duration::from_secs(secs) {
                break;
            }
        }
        if let (Some(secs), false) = (activate_after, activated) {
            if start.elapsed() > Duration::from_secs(secs) {
                activated = true;
                match (app.activation.clone(), app.keyboard_serial, app.seat.clone()) {
                    (Some(activation), Some(serial), Some(seat)) => {
                        let token = activation.get_activation_token(&qh, ());
                        token.set_serial(serial, &seat);
                        token.set_app_id(title.clone());
                        token.commit();
                        println!("activation token requested");
                        let _ = queue.roundtrip(&mut app);
                        if let Some(token) = app.activation_token.clone().filter(|t| !t.is_empty()) {
                            activation.activate(token, &surface);
                            println!("activation requested");
                        }
                    }
                    _ => println!("cannot activate: no xdg_activation_v1 / keyboard serial / seat"),
                }
            }
        }
        std::thread::sleep(Duration::from_millis(if want_fifo || want_timing { 2 } else { 50 }));
    }
}
