//! Wayland protocols beyond the core shell (`docs/design.md` roadmap,
//! the protocols), grouped so `State` carries one field for them all.
//!
//! bspwm needs none of these itself — X11 has selections, RandR and the
//! rest built in — so each protocol here has a stated reason: it is what
//! Wayland clients need to behave as they do under X11 (`docs/bsp-compositor.md`,
//! Protocols progress).

use std::collections::HashMap;

use smithay::input::Seat;
use smithay::reexports::calloop::LoopHandle;
use smithay::wayland::cursor_shape::CursorShapeManagerState;
use smithay::wayland::foreign_toplevel_list::{ForeignToplevelHandle, ForeignToplevelListHandler, ForeignToplevelListState};
use smithay::wayland::idle_inhibit::{IdleInhibitHandler, IdleInhibitManagerState};
use smithay::wayland::idle_notify::{IdleNotifierHandler, IdleNotifierState};
use smithay::wayland::keyboard_shortcuts_inhibit::{
    KeyboardShortcutsInhibitHandler, KeyboardShortcutsInhibitState, KeyboardShortcutsInhibitor,
};
use smithay::wayland::viewporter::ViewporterState;
use smithay::wayland::virtual_keyboard::VirtualKeyboardManagerState;
use smithay::wayland::xdg_activation::{XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData};
use bsp_core::id::WindowId;
use wayland_protocols_wlr::screencopy::v1::server::zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1;
use wayland_protocols_wlr::gamma_control::v1::server::zwlr_gamma_control_manager_v1::ZwlrGammaControlManagerV1;
use wayland_protocols_wlr::output_management::v1::server::zwlr_output_manager_v1::ZwlrOutputManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::server::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols::ext::workspace::v1::server::ext_workspace_manager_v1::ExtWorkspaceManagerV1;
use wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Resource, DisplayHandle};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::shell::wlr_layer::WlrLayerShellState;
use smithay::wayland::selection::data_device::{
    set_data_device_focus, ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::primary_selection::{
    set_primary_focus, PrimarySelectionHandler, PrimarySelectionState,
};
use smithay::wayland::selection::wlr_data_control::{DataControlHandler, DataControlState};
use smithay::{delegate_data_control, delegate_data_device, delegate_primary_selection};

use crate::state::{Backend, State};

/// The state of every the protocols protocol global.
pub struct Protocols<Bd: Backend + 'static> {
    /// `wl_data_device_manager`: the clipboard and drag-and-drop.
    pub data_device: DataDeviceState,
    /// `zwp_primary_selection_device_manager_v1`: the middle-click selection.
    pub primary_selection: PrimarySelectionState,
    /// `zwlr_data_control_manager_v1`: clipboard managers and `wl-copy`/`wl-paste`.
    pub data_control: DataControlState,
    /// The `zwlr_foreign_toplevel_manager_v1` global.
    pub _taskbar_global: smithay::reexports::wayland_server::backend::GlobalId,
    /// The `zwlr_virtual_pointer_manager_v1` global (`crate::virtual_pointer`).
    pub _virtual_pointer_global: smithay::reexports::wayland_server::backend::GlobalId,
    /// The `zwlr_output_manager_v1` global.
    pub _output_management_global: smithay::reexports::wayland_server::backend::GlobalId,
    /// The `zwlr_gamma_control_manager_v1` global.
    pub _gamma_global: smithay::reexports::wayland_server::backend::GlobalId,
    /// The `zwlr_screencopy_manager_v1` global.
    pub _screencopy_global: smithay::reexports::wayland_server::backend::GlobalId,
    /// The `ext_workspace_manager_v1` global.
    pub _workspace_global: smithay::reexports::wayland_server::backend::GlobalId,
    /// `zwlr_layer_shell_v1`: panels, bars, wallpapers (`crate::layers`).
    pub layer_shell: WlrLayerShellState,
    /// `xdg_activation_v1`: "please focus this window" requests.
    pub activation: XdgActivationState,
    /// `ext_idle_notifier_v1`: idle timeouts (screen lockers, `swayidle`).
    pub idle_notifier: IdleNotifierState<State<Bd>>,
    /// `zwp_idle_inhibit_manager_v1`: video players asking not to idle.
    pub _idle_inhibit: IdleInhibitManagerState,
    /// Surfaces currently inhibiting idle.
    pub idle_inhibitors: Vec<WlSurface>,
    /// `zwp_keyboard_shortcuts_inhibit_manager_v1`: remote-desktop and VM
    /// windows asking for compositor shortcuts to be suspended.
    pub shortcuts_inhibit: KeyboardShortcutsInhibitState,
    /// Active shortcut inhibitors (`crate::hotkeys::filter` honours them).
    pub shortcut_inhibitors: Vec<KeyboardShortcutsInhibitor>,
    /// `zwp_virtual_keyboard_manager_v1`: on-screen keyboards, `wtype`.
    pub _virtual_keyboard: VirtualKeyboardManagerState,
    /// `wp_cursor_shape_manager_v1`: named cursors without a cursor surface.
    pub _cursor_shape: CursorShapeManagerState,
    /// `wp_viewporter`: client-side scaling and cropping.
    pub _viewporter: ViewporterState,
    /// `ext_foreign_toplevel_list_v1`: lists toplevels for taskbars.
    pub foreign_toplevels: ForeignToplevelListState,
    /// `zwlr_foreign_toplevel_manager_v1` (`crate::taskbar`).
    pub taskbar: crate::taskbar::Taskbar,
    /// `ext_workspace_manager_v1` (`crate::workspaces`).
    pub workspaces: crate::workspaces::Workspaces,
    /// `zwlr_output_manager_v1` (`crate::output_management`).
    pub output_management: crate::output_management::OutputManagement,
    /// `zwlr_gamma_control_manager_v1` bookkeeping (`crate::gamma`).
    pub gamma: crate::gamma::Gamma,
    /// `zwlr_screencopy_manager_v1` (`crate::screencopy`).
    pub screencopy: crate::screencopy::Screencopy,
    /// `xdg_decoration_v1`: clients are told to leave decorations to us (we draw none).
    pub _decoration: smithay::wayland::shell::xdg::decoration::XdgDecorationState,
    /// `wp_fractional_scale_v1`: tells clients each surface's output scale.
    pub _fractional_scale: smithay::wayland::fractional_scale::FractionalScaleManagerState,
    /// `wp_single_pixel_buffer_v1`: solid-colour buffers without shm.
    pub _single_pixel: smithay::wayland::single_pixel_buffer::SinglePixelBufferState,
    /// `wp_content_type_v1`: content hints (accepted, not acted on).
    pub _content_type: smithay::wayland::content_type::ContentTypeState,
    /// `ext_session_lock_manager_v1` (`crate::session_lock`).
    pub _session_lock_manager: smithay::wayland::session_lock::SessionLockManagerState,
    /// Lock state.
    pub session_lock: crate::session_lock::SessionLock,
    /// `wp_security_context_manager_v1`: sandboxed apps (Flatpak) get their own restricted socket.
    pub _security_context: smithay::wayland::security_context::SecurityContextState,
    /// `wp_tearing_control_manager_v1` (`crate::tearing`).
    pub _tearing_global: smithay::reexports::wayland_server::backend::GlobalId,
    /// `zwp_text_input_manager_v3`: applications' text fields.
    pub _text_input: smithay::wayland::text_input::TextInputManagerState,
    /// `zwp_input_method_manager_v2`: input method engines (fcitx5, ibus).
    pub _input_method: smithay::wayland::input_method::InputMethodManagerState,
    /// `wp_presentation`: tells clients when their frames reached the screen.
    pub _presentation: smithay::wayland::presentation::PresentationState,
    /// `ext_image_copy_capture_manager_v1` (`crate::ext_capture`).
    pub ext_capture: crate::ext_capture::ExtCapture,
    /// The `ext_image_copy_capture_manager_v1`, output-source and toplevel-source globals.
    pub _ext_capture_globals: (smithay::reexports::wayland_server::backend::GlobalId, smithay::reexports::wayland_server::backend::GlobalId, smithay::reexports::wayland_server::backend::GlobalId),
    /// `zwlr_output_power_manager_v1` (`crate::output_power`).
    pub output_power: crate::output_power::OutputPower,
    /// The `zwlr_output_power_manager_v1` global.
    pub _output_power_global: smithay::reexports::wayland_server::backend::GlobalId,
    /// `xwayland_shell_v1`: how Xwayland tells us which Wayland surface is which X11 window.
    pub _xwayland_shell: smithay::wayland::xwayland_shell::XWaylandShellState,
    /// The surface whose pointer constraint is currently active (`crate::constraints`).
    pub constrained: Option<WlSurface>,
    /// `zwp_relative_pointer_manager_v1`.
    pub _relative_pointer: smithay::wayland::relative_pointer::RelativePointerManagerState,
    /// `zwp_pointer_constraints_v1`.
    pub _pointer_constraints: smithay::wayland::pointer_constraints::PointerConstraintsState,
    /// The foreign-toplevel handle of every mapped window.
    pub toplevel_handles: HashMap<WindowId, ForeignToplevelHandle>,
    /// `wl_output` management plus `zxdg_output_manager_v1` (logical
    /// position and size, output names) — kept alive for its globals.
    pub _output_manager: OutputManagerState,
    /// `xdg_toplevel_drag_manager_v1` (`crate::toplevel_drag`).
    pub toplevel_drags: crate::toplevel_drag::ToplevelDrags,
    /// The `xdg_toplevel_drag_manager_v1` global.
    pub _toplevel_drag_global: smithay::reexports::wayland_server::backend::GlobalId,
    /// `zwlr_export_dmabuf_manager_v1` (`crate::export_dmabuf`).
    pub export_dmabuf: crate::export_dmabuf::ExportDmabuf,
    /// The `zwlr_export_dmabuf_manager_v1` global.
    pub _export_dmabuf_global: smithay::reexports::wayland_server::backend::GlobalId,
    /// The smaller protocols of `crate::extras`.
    pub extras: crate::extras::Extras,
}

impl<Bd: Backend + 'static> Protocols<Bd> {
    /// Creates every global on `display`.
    pub fn new(display: &DisplayHandle, handle: LoopHandle<'static, State<Bd>>) -> Self {
        let primary_selection = PrimarySelectionState::new::<State<Bd>>(display);
        let data_control = DataControlState::new::<State<Bd>, _>(display, Some(&primary_selection), crate::state::is_privileged);
        Self {
            data_device: DataDeviceState::new::<State<Bd>>(display),
            primary_selection,
            data_control,
            _taskbar_global: display.create_global::<State<Bd>, ZwlrForeignToplevelManagerV1, _>(3, ()),
            layer_shell: WlrLayerShellState::new_with_filter::<State<Bd>, _>(display, crate::state::is_privileged),
            activation: XdgActivationState::new::<State<Bd>>(display),
            idle_notifier: IdleNotifierState::new(display, handle),
            _idle_inhibit: IdleInhibitManagerState::new::<State<Bd>>(display),
            idle_inhibitors: Vec::new(),
            shortcuts_inhibit: KeyboardShortcutsInhibitState::new::<State<Bd>>(display),
            shortcut_inhibitors: Vec::new(),
            _virtual_keyboard: VirtualKeyboardManagerState::new::<State<Bd>, _>(display, crate::state::is_privileged),
            _cursor_shape: CursorShapeManagerState::new::<State<Bd>>(display),
            _viewporter: ViewporterState::new::<State<Bd>>(display),
            foreign_toplevels: ForeignToplevelListState::new_with_filter::<State<Bd>>(display, crate::state::is_privileged),
            taskbar: crate::taskbar::Taskbar::default(),
            workspaces: crate::workspaces::Workspaces::default(),
            gamma: crate::gamma::Gamma::default(),
            constrained: None,
            _xwayland_shell: smithay::wayland::xwayland_shell::XWaylandShellState::new::<State<Bd>>(display),
            output_power: crate::output_power::OutputPower::default(),
            _output_power_global: display.create_global::<State<Bd>, wayland_protocols_wlr::output_power_management::v1::server::zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1, _>(1, ()),
            ext_capture: crate::ext_capture::ExtCapture::default(),
            _ext_capture_globals: (
                display.create_global::<State<Bd>, wayland_protocols::ext::image_copy_capture::v1::server::ext_image_copy_capture_manager_v1::ExtImageCopyCaptureManagerV1, _>(1, ()),
                display.create_global::<State<Bd>, wayland_protocols::ext::image_capture_source::v1::server::ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1, _>(1, ()),
                display.create_global::<State<Bd>, wayland_protocols::ext::image_capture_source::v1::server::ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1, _>(1, ()),
            ),
            // CLOCK_MONOTONIC (1), the clock the DRM backend timestamps with.
            _presentation: smithay::wayland::presentation::PresentationState::new::<State<Bd>>(display, 1),
            _tearing_global: display.create_global::<State<Bd>, wayland_protocols::wp::tearing_control::v1::server::wp_tearing_control_manager_v1::WpTearingControlManagerV1, _>(1, ()),
            _text_input: smithay::wayland::text_input::TextInputManagerState::new::<State<Bd>>(display),
            _input_method: smithay::wayland::input_method::InputMethodManagerState::new::<State<Bd>, _>(display, crate::state::is_privileged),
            _session_lock_manager: smithay::wayland::session_lock::SessionLockManagerState::new::<State<Bd>, _>(display, crate::state::is_privileged),
            session_lock: crate::session_lock::SessionLock::default(),
            _security_context: smithay::wayland::security_context::SecurityContextState::new::<State<Bd>, _>(display, crate::state::is_privileged),
            _decoration: smithay::wayland::shell::xdg::decoration::XdgDecorationState::new::<State<Bd>>(display),
            _fractional_scale: smithay::wayland::fractional_scale::FractionalScaleManagerState::new::<State<Bd>>(display),
            _single_pixel: smithay::wayland::single_pixel_buffer::SinglePixelBufferState::new::<State<Bd>>(display),
            _content_type: smithay::wayland::content_type::ContentTypeState::new::<State<Bd>>(display),
            _relative_pointer: smithay::wayland::relative_pointer::RelativePointerManagerState::new::<State<Bd>>(display),
            _pointer_constraints: smithay::wayland::pointer_constraints::PointerConstraintsState::new::<State<Bd>>(display),
            screencopy: crate::screencopy::Screencopy::default(),
            _screencopy_global: display.create_global::<State<Bd>, ZwlrScreencopyManagerV1, _>(3, ()),
            _gamma_global: display.create_global::<State<Bd>, ZwlrGammaControlManagerV1, _>(1, ()),
            output_management: crate::output_management::OutputManagement::default(),
            _output_management_global: display.create_global::<State<Bd>, ZwlrOutputManagerV1, _>(4, ()),
            _virtual_pointer_global: display.create_global::<State<Bd>, ZwlrVirtualPointerManagerV1, _>(2, ()),
            _workspace_global: display.create_global::<State<Bd>, ExtWorkspaceManagerV1, _>(1, ()),
            toplevel_handles: HashMap::new(),
            _output_manager: OutputManagerState::new_with_xdg_output::<State<Bd>>(display),
            toplevel_drags: crate::toplevel_drag::ToplevelDrags::default(),
            _toplevel_drag_global: display.create_global::<State<Bd>, wayland_protocols::xdg::toplevel_drag::v1::server::xdg_toplevel_drag_manager_v1::XdgToplevelDragManagerV1, _>(1, ()),
            export_dmabuf: crate::export_dmabuf::ExportDmabuf::default(),
            _export_dmabuf_global: display.create_global::<State<Bd>, wayland_protocols_wlr::export_dmabuf::v1::server::zwlr_export_dmabuf_manager_v1::ZwlrExportDmabufManagerV1, _>(1, ()),
            extras: crate::extras::Extras::new::<Bd>(display),
        }
    }
}

/// Points the clipboard and primary selection at the client that owns
/// `focused` — called whenever keyboard focus changes, so a client only
/// ever sees the selection while it has focus (the Wayland model; X11
/// has no such rule, `docs/design.md` Compatibility).
pub fn keyboard_focus_changed<Bd: Backend + 'static>(state: &State<Bd>, seat: &Seat<State<Bd>>, focused: Option<&WlSurface>) {
    let client = focused.and_then(|surface| state.display_handle.get_client(surface.id()).ok());
    set_data_device_focus(&state.display_handle, seat, client.clone());
    set_primary_focus(&state.display_handle, seat, client);
}

impl<Bd: Backend + 'static> DataDeviceHandler for State<Bd> {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.protocols.data_device
    }
}
impl<Bd: Backend + 'static> ClientDndGrabHandler for State<Bd> {
    // Both run inside Smithay's DnD pointer grab.
    fn started(&mut self, source: Option<smithay::reexports::wayland_server::protocol::wl_data_source::WlDataSource>, _icon: Option<WlSurface>, _seat: Seat<Self>) {
        let _guard = crate::pointer_action::GrabGuard::enter();
        crate::toplevel_drag::drag_started(self, source.as_ref());
    }

    fn dropped(&mut self, _target: Option<WlSurface>, _validated: bool, _seat: Seat<Self>) {
        let _guard = crate::pointer_action::GrabGuard::enter();
        crate::toplevel_drag::drag_ended(self);
    }
}
impl<Bd: Backend + 'static> ServerDndGrabHandler for State<Bd> {}
delegate_data_device!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> PrimarySelectionHandler for State<Bd> {
    fn primary_selection_state(&self) -> &PrimarySelectionState {
        &self.protocols.primary_selection
    }
}
delegate_primary_selection!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> DataControlHandler for State<Bd> {
    fn data_control_state(&self) -> &DataControlState {
        &self.protocols.data_control
    }
}
delegate_data_control!(@<Bd: Backend + 'static> State<Bd>);

// --- xdg-activation --------------------------------------------------------

/// How long an activation token stays usable (`bspwm` has no equivalent —
/// its `_NET_ACTIVE_WINDOW` handling takes any request; Wayland's token
/// scheme exists precisely so a background client cannot steal focus).
const ACTIVATION_TOKEN_LIFETIME: std::time::Duration = std::time::Duration::from_secs(10);

/// Marks an activation token whose serial proved it came from a real event.
struct FocusToken;

impl<Bd: Backend + 'static> XdgActivationHandler for State<Bd> {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.protocols.activation
    }

    /// Every token is created, but only one that carries the serial of a real
    /// event on this seat (no older than the keyboard's last focus change) may
    /// take focus; any other can only mark its window urgent. A client cannot
    /// mint a focus-stealing token out of nothing (anvil's `token_created`,
    /// cosmic-comp's urgent-only tokens).
    fn token_created(&mut self, token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        let valid = data.serial.as_ref().is_some_and(|(serial, seat)| {
            smithay::input::Seat::from_resource(seat).as_ref() == Some(&self.seat)
                && self
                    .seat
                    .get_keyboard()
                    .and_then(|keyboard| keyboard.last_enter())
                    .is_some_and(|last_enter| serial.is_no_older_than(&last_enter))
        });
        tracing::debug!(token = token.as_str(), valid, app_id = ?data.app_id, "activation token requested");
        if valid {
            data.user_data.insert_if_missing(|| FocusToken);
        }
        true
    }

    fn request_activation(&mut self, token: XdgActivationToken, token_data: XdgActivationTokenData, surface: WlSurface) {
        // `ignore_ewmh_focus`: windows may not ask for focus.
        if self.xwayland.ignore_focus {
            self.protocols.activation.remove_token(&token);
            return;
        }
        let fresh = token_data.timestamp.elapsed() < ACTIVATION_TOKEN_LIFETIME;
        // One-shot: a used or stale token is dropped either way.
        self.protocols.activation.remove_token(&token);
        if !fresh {
            tracing::debug!(token = token.as_str(), "activation request with an expired token ignored");
            return;
        }
        let Some(window) = self.window_for_surface(&surface) else {
            tracing::debug!("activation request for a surface that is not a mapped window ignored");
            return;
        };
        let Some(window_id) = self.adapter.id_of(&window) else {
            return;
        };
        let Some((mi, di, node)) = crate::input::locate_window(self, window_id) else {
            return;
        };
        if token_data.user_data.get::<FocusToken>().is_none() {
            // bspwm: an unfocused window that asks for attention is urgent
            // (`_NET_WM_STATE_DEMANDS_ATTENTION`), shown as `u` in the report.
            tracing::debug!(window = %window_id, "activation without a valid serial: marking the window urgent");
            let trg = bsp_ipc::exec::Coordinates { monitor: mi, desktop: di, node: Some(node) };
            crate::ipc::with_ops(self, |ctx, events| bsp_ipc::exec::set_urgent(ctx, trg, true, events));
            self.backend_data.queue_redraw();
            return;
        }
        // Same as bspwm's `_NET_ACTIVE_WINDOW`: focusing the node also
        // moves focus to its desktop and monitor.
        tracing::debug!(window = %window_id, "activating a window on request");
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        crate::input::set_focus(self, mi, di, node, serial);
        self.backend_data.queue_redraw();
    }
}
smithay::delegate_xdg_activation!(@<Bd: Backend + 'static> State<Bd>);

// --- idle ------------------------------------------------------------------

impl<Bd: Backend + 'static> IdleNotifierHandler for State<Bd> {
    fn idle_notifier_state(&mut self) -> &mut IdleNotifierState<Self> {
        &mut self.protocols.idle_notifier
    }
}
smithay::delegate_idle_notify!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> IdleInhibitHandler for State<Bd> {
    fn inhibit(&mut self, surface: WlSurface) {
        self.protocols.idle_inhibitors.push(surface);
        self.protocols.idle_notifier.set_is_inhibited(true);
    }

    fn uninhibit(&mut self, surface: WlSurface) {
        self.protocols.idle_inhibitors.retain(|s| s != &surface);
        let inhibited = !self.protocols.idle_inhibitors.is_empty();
        self.protocols.idle_notifier.set_is_inhibited(inhibited);
    }
}
smithay::delegate_idle_inhibit!(@<Bd: Backend + 'static> State<Bd>);

/// Drops idle inhibitors whose surface is gone and updates the idle
/// notifier. Smithay's `uninhibit` fires only when a client explicitly
/// destroys its inhibitor, so a client that exits or crashes would
/// otherwise inhibit idling forever (found by the live test). Called after
/// each event-loop turn; free when nothing inhibits.
pub fn refresh_idle_inhibit<Bd: Backend + 'static>(state: &mut State<Bd>) {
    use smithay::reexports::wayland_server::Resource;
    if state.protocols.idle_inhibitors.is_empty() {
        return;
    }
    let before = state.protocols.idle_inhibitors.len();
    state.protocols.idle_inhibitors.retain(|surface| surface.is_alive());
    if state.protocols.idle_inhibitors.len() != before {
        tracing::debug!(
            dropped = before - state.protocols.idle_inhibitors.len(),
            "dropped idle inhibitors of exited clients"
        );
        let inhibited = !state.protocols.idle_inhibitors.is_empty();
        state.protocols.idle_notifier.set_is_inhibited(inhibited);
    }
}

/// Tells idle listeners the user is active — called for every input event.
pub fn notify_activity<Bd: Backend + 'static>(state: &mut State<Bd>) {
    let seat = state.seat.clone();
    state.protocols.idle_notifier.notify_activity(&seat);
}

// --- keyboard shortcuts inhibit ---------------------------------------------

impl<Bd: Backend + 'static> KeyboardShortcutsInhibitHandler for State<Bd> {
    fn keyboard_shortcuts_inhibit_state(&mut self) -> &mut KeyboardShortcutsInhibitState {
        &mut self.protocols.shortcuts_inhibit
    }

    /// Granted immediately: an inhibitor only takes effect while its
    /// surface has keyboard focus, and the emergency quit key and VT
    /// switching are never inhibited (`crate::hotkeys::shortcuts_inhibited`
    /// is consulted by the sxhkdrc filter only).
    fn new_inhibitor(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        tracing::debug!("keyboard shortcuts inhibitor granted");
        inhibitor.activate();
        self.protocols.shortcut_inhibitors.push(inhibitor);
    }

    fn inhibitor_destroyed(&mut self, inhibitor: KeyboardShortcutsInhibitor) {
        self.protocols
            .shortcut_inhibitors
            .retain(|i| i.wl_surface() != inhibitor.wl_surface());
    }
}
smithay::delegate_keyboard_shortcuts_inhibit!(@<Bd: Backend + 'static> State<Bd>);

/// Whether the focused surface asked for compositor shortcuts to be
/// suspended, so sxhkdrc bindings must not fire.
pub fn shortcuts_inhibited<Bd: Backend + 'static>(state: &State<Bd>) -> bool {
    // An X11 client holds an active keyboard grab (`XGrabKeyboard`, passed on
    // through `zwp_xwayland_keyboard_grab_v1`): as under X, it wins over the
    // window manager's own key bindings. It is the only keyboard grab this
    // compositor ever sets.
    if state.seat.get_keyboard().is_some_and(|k| k.is_grabbed()) {
        return true;
    }
    let Some(focus) = state
        .seat
        .get_keyboard()
        .and_then(|k| k.current_focus())
        .and_then(|target| smithay::wayland::seat::WaylandFocus::wl_surface(&target).map(|s| s.into_owned()))
    else {
        return false;
    };
    state
        .protocols
        .shortcut_inhibitors
        .iter()
        .any(|i| i.is_active() && *i.wl_surface() == focus)
}

// --- virtual keyboard, cursor shape, viewporter ----------------------------

smithay::delegate_virtual_keyboard_manager!(@<Bd: Backend + 'static> State<Bd>);
// `wp_cursor_shape` also covers graphics-tablet tools, so it needs this
// handler; no tablet is supported yet, so a tool's cursor is ignored.
impl<Bd: Backend + 'static> smithay::wayland::tablet_manager::TabletSeatHandler for State<Bd> {}
smithay::delegate_cursor_shape!(@<Bd: Backend + 'static> State<Bd>);
smithay::delegate_viewporter!(@<Bd: Backend + 'static> State<Bd>);

// --- foreign toplevel list ---------------------------------------------------

impl<Bd: Backend + 'static> ForeignToplevelListHandler for State<Bd> {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelListState {
        &mut self.protocols.foreign_toplevels
    }
}
smithay::delegate_foreign_toplevel_list!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> State<Bd> {
    /// Announces a newly mapped window to `ext-foreign-toplevel-list`
    /// clients (taskbars).
    pub fn toplevel_mapped(&mut self, window: WindowId, title: &str, app_id: &str) {
        let handle = self.protocols.foreign_toplevels.new_toplevel::<State<Bd>>(title, app_id);
        self.protocols.toplevel_handles.insert(window, handle);
    }

    /// Tells `ext-foreign-toplevel-list` clients a window is gone.
    pub fn toplevel_unmapped(&mut self, window: WindowId) {
        if let Some(handle) = self.protocols.toplevel_handles.remove(&window) {
            handle.send_closed();
        }
    }

    /// Forwards a changed title and/or app id to `ext-foreign-toplevel-list` clients.
    pub fn toplevel_changed(&mut self, window: WindowId, title: &str, app_id: &str) {
        if let Some(handle) = self.protocols.toplevel_handles.get(&window) {
            handle.send_title(title);
            handle.send_app_id(app_id);
            handle.send_done();
        }
    }
}

/// Runs a `bspc` command line as if typed (`argv` without the leading
/// `bspc`), through the same path as the control socket and hotkeys:
/// protocols whose requests have a `bspc` equivalent use this so they
/// cannot drift from it. Returns whether it succeeded.
pub fn run_bspc<Bd: Backend + 'static>(state: &mut State<Bd>, argv: &[String]) -> bool {
    match bsp_ipc::command::parse(argv) {
        Ok(command) => matches!(crate::ipc::execute_and_broadcast(state, &command), bsp_ipc::wire::Reply::Ok(_)),
        Err(err) => {
            tracing::debug!(?argv, "protocol request produced an invalid bspc command: {}", err.message);
            false
        }
    }
}

// --- decorations, fractional scale, single pixel, content type -------------

/// Every toplevel is server-side decorated: bspwm draws only a plain
/// border (`crate::render`), no title bar, so clients must not draw their
/// own (GTK and Qt draw client-side decorations unless told otherwise).
impl<Bd: Backend + 'static> smithay::wayland::shell::xdg::decoration::XdgDecorationHandler for State<Bd> {
    fn new_decoration(&mut self, toplevel: smithay::wayland::shell::xdg::ToplevelSurface) {
        force_server_side(&toplevel);
    }

    fn request_mode(&mut self, toplevel: smithay::wayland::shell::xdg::ToplevelSurface, _mode: smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode) {
        force_server_side(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: smithay::wayland::shell::xdg::ToplevelSurface) {
        force_server_side(&toplevel);
    }
}
smithay::delegate_xdg_decoration!(@<Bd: Backend + 'static> State<Bd>);

fn force_server_side(toplevel: &smithay::wayland::shell::xdg::ToplevelSurface) {
    use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode;
    toplevel.with_pending_state(|state| state.decoration_mode = Some(Mode::ServerSide));
    // Before the first configure the pending state simply goes out with it.
    let configured = smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
            .and_then(|data| data.lock().ok().map(|d| d.initial_configure_sent))
            .unwrap_or(false)
    });
    if configured {
        toplevel.send_pending_configure();
    }
}

impl<Bd: Backend + 'static> smithay::wayland::fractional_scale::FractionalScaleHandler for State<Bd> {
    fn new_fractional_scale(&mut self, _surface: WlSurface) {
        update_fractional_scales(self);
    }
}
smithay::delegate_fractional_scale!(@<Bd: Backend + 'static> State<Bd>);

/// Sends every window and layer surface the scale of the output it is on
/// (deduplicated by Smithay, so calling it after any change is cheap).
pub fn update_fractional_scales<Bd: Backend + 'static>(state: &State<Bd>) {
    use smithay::wayland::compositor::with_states;
    use smithay::wayland::fractional_scale::with_fractional_scale;
    use smithay::wayland::seat::WaylandFocus;
    for window in state.space.elements() {
        let Some(surface) = window.wl_surface() else {
            continue;
        };
        let Some(output) = state.space.outputs_for_element(window).into_iter().next() else {
            continue;
        };
        let scale = output.current_scale().fractional_scale();
        with_states(&surface, |states| with_fractional_scale(states, |fs| fs.set_preferred_scale(scale)));
    }
    for output in state.space.outputs() {
        let scale = output.current_scale().fractional_scale();
        let map = smithay::desktop::layer_map_for_output(output);
        for layer in map.layers() {
            with_states(layer.wl_surface(), |states| with_fractional_scale(states, |fs| fs.set_preferred_scale(scale)));
        }
    }
}

smithay::delegate_single_pixel_buffer!(@<Bd: Backend + 'static> State<Bd>);
smithay::delegate_content_type!(@<Bd: Backend + 'static> State<Bd>);

// --- security context -------------------------------------------------------

/// A sandboxed app (Flatpak, a container) gets a socket of its own; clients
/// connecting through it are marked `sandboxed`, which hides every
/// privileged protocol (`crate::state::is_privileged`) from them —
/// screen capture, input injection, window lists, output configuration,
/// session lock, clipboard managers, layer shell. X11 has no equivalent:
/// any X client can read and inject everything.
impl<Bd: Backend + 'static> smithay::wayland::security_context::SecurityContextHandler for State<Bd> {
    fn context_created(
        &mut self,
        source: smithay::wayland::security_context::SecurityContextListenerSource,
        context: smithay::wayland::security_context::SecurityContext,
    ) {
        tracing::debug!(
            engine = ?context.sandbox_engine,
            app_id = ?context.app_id,
            "security context created; its clients will be sandboxed"
        );
        let inserted = self.handle.insert_source(source, |stream, _, state| {
            let client_state = crate::state::ClientState {
                sandboxed: true,
                ..Default::default()
            };
            if let Err(err) = state.display_handle.insert_client(stream, std::sync::Arc::new(client_state)) {
                tracing::warn!("failed to add a sandboxed client: {err}");
            }
        });
        if let Err(err) = inserted {
            tracing::warn!("failed to listen on a security context socket: {err}");
        }
    }
}
smithay::delegate_security_context!(@<Bd: Backend + 'static> State<Bd>);

// --- text input / input method -----------------------------------------------

/// Input method engines (fcitx5, ibus) talk to applications' text fields
/// through these two protocols; X11 does this via XIM. An engine's
/// candidate popup is tracked as a popup of the focused window, so it is
/// drawn and positioned with the window's other popups. Only privileged
/// (not sandboxed) clients may be an input method.
impl<Bd: Backend + 'static> smithay::wayland::input_method::InputMethodHandler for State<Bd> {
    fn new_popup(&mut self, surface: smithay::wayland::input_method::PopupSurface) {
        if let Err(err) = self.popups.track_popup(smithay::desktop::PopupKind::from(surface)) {
            tracing::warn!("failed to track an input method popup: {err}");
        }
    }

    fn dismiss_popup(&mut self, surface: smithay::wayland::input_method::PopupSurface) {
        if let Some(parent) = surface.get_parent().map(|p| p.surface.clone()) {
            let _ = smithay::desktop::PopupManager::dismiss_popup(&parent, &smithay::desktop::PopupKind::from(surface));
        }
    }

    fn popup_repositioned(&mut self, _surface: smithay::wayland::input_method::PopupSurface) {}

    fn parent_geometry(&self, parent: &WlSurface) -> smithay::utils::Rectangle<i32, smithay::utils::Logical> {
        self.window_for_surface(parent)
            .and_then(|window| self.space.element_geometry(&window))
            .unwrap_or_default()
    }
}
smithay::delegate_input_method_manager!(@<Bd: Backend + 'static> State<Bd>);
smithay::delegate_text_input_manager!(@<Bd: Backend + 'static> State<Bd>);

smithay::delegate_presentation!(@<Bd: Backend + 'static> State<Bd>);
