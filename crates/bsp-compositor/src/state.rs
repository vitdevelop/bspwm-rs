//! The compositor's global state: every Smithay protocol global, the
//! Wayland display, and the `bsp-core`/`bsp-ipc` state the rest of this
//! crate wires Wayland activity to.
//!
//! Scoped to what the nested compositor needs (`docs/design.md` roadmap: "winit backend,
//! xdg-shell, tiling through the core, focus, borders"): the compositor,
//! shm, output, seat and xdg-shell globals only. Data-device (clipboard),
//! xdg-decoration, layer-shell, presentation-time and every other
//! protocol in `docs/bsp-compositor.md`'s module table are later work
//! and not wired up yet — a client that needs one simply doesn't see that
//! global advertised.

use std::sync::Arc;
use std::time::Instant;

use smithay::desktop::{PopupManager, Space, Window};
use smithay::utils::{Clock, Monotonic};
use smithay::input::keyboard::LedState;
use smithay::input::pointer::{CursorImageStatus, PointerHandle};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::{Client, DisplayHandle};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgShellState};
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{delegate_compositor, delegate_output, delegate_seat, delegate_shm};

use crate::adapter::WindowAdapter;

/// The status `bspc quit STATUS` asked the process to exit with.
pub static EXIT_STATUS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

/// What each backend (`crate::winit_backend::WinitData`, `crate::udev_backend::DrmData`)
/// must provide so the rest of this crate can stay generic over `State<Bd>`
/// — everything else (protocol globals, `bsp-core`/`bsp-ipc` state,
/// hotkeys) is identical regardless of which backend is running.
///
/// Modeled directly on Smithay's own reference compositor, anvil
/// (`anvil/src/state.rs` `trait Backend`) — kept just as thin: anvil's
/// own `AnvilState<BackendData: Backend>` threads this generic through
/// nearly its entire codebase precisely because every method here is
/// mechanical, compiler-checked boilerplate (one `impl<Bd: Backend>
/// SomeHandler for AnvilState<Bd>` per Smithay protocol trait), not
/// manual per-call-site branching — the same reasoning applies here.
pub trait Backend {
    /// Whether this backend's pointer motion events carry a real
    /// relative-motion delta (real hardware, via `libinput`) rather than
    /// only absolute positions (the nested winit backend). Declared for
    /// interface parity with anvil's own `Backend` trait; nothing in this
    /// crate consults it (`crate::udev_backend` handles relative motion
    /// itself).
    #[allow(dead_code)]
    const HAS_RELATIVE_MOTION: bool = false;
    /// Whether this backend's input can deliver gesture events (real
    /// `libinput`, not the nested winit backend); `crate::devices`
    /// forwards them. Same status as `HAS_RELATIVE_MOTION` above.
    #[allow(dead_code)]
    const HAS_GESTURES: bool = false;

    /// The seat name Smithay's input stack reports to clients. Only
    /// called by `crate::udev_backend` today (the nested backend names
    /// its seat directly, `winit_backend::OUTPUT_NAME`) — hence the
    /// `cfg_attr`, so a `nested`-only build does not warn.
    #[cfg_attr(not(feature = "real"), allow(dead_code))]
    fn seat_name(&self) -> String;
    /// Called when an output's buffers need re-submitting from scratch
    /// (e.g. after a VT switch resume) — a no-op unless the backend
    /// tracks its own damage/buffer-age state outside `bsp-compositor`'s
    /// own damage tracking. Only called by `crate::udev_backend` today.
    #[cfg_attr(not(feature = "real"), allow(dead_code))]
    fn reset_buffers(&mut self, output: &smithay::output::Output);
    /// Called after every client commit: lets the backend import the
    /// surface's new buffer into its renderer ahead of the next frame —
    /// only the multi-GPU DRM backend does anything (so the copy to a
    /// secondary GPU is not paid for inside the frame), the rest no-op.
    fn early_import(&mut self, surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface);
    /// Forwards the keyboard's LED state to real hardware — a no-op for
    /// the nested backend's virtual keyboard, which has no LEDs.
    fn update_led_state(&mut self, led_state: LedState);
    /// Entries per channel of `output`'s hardware gamma LUT, if it has one
    /// (`wlr-gamma-control`). The default (nested backend) has none.
    fn gamma_size(&mut self, output: &smithay::output::Output) -> Option<u32> {
        let _ = output;
        None
    }
    /// Loads a gamma ramp (`3 * size` values: red, green, blue) into
    /// `output`'s LUT, or with `None` restores the identity ramp.
    fn set_gamma(&mut self, output: &smithay::output::Output, ramp: Option<&[u16]>) -> Result<(), String> {
        let _ = (output, ramp);
        Err("no hardware gamma LUT".to_string())
    }
    /// Whether `output`'s display is powered on, if the backend can tell
    /// (`wlr-output-power-management`); `None` = no DPMS support.
    fn output_power(&mut self, output: &smithay::output::Output) -> Option<bool> {
        let _ = output;
        None
    }
    /// Powers `output`'s display on or off (DPMS). The default refuses.
    fn set_output_power(&mut self, output: &smithay::output::Output, on: bool) -> Result<(), String> {
        let _ = (output, on);
        Err("no display power control".to_string())
    }
    /// Renders `request.output` off-screen into memory (`wlr-screencopy`).
    /// The default (nested backend) cannot.
    fn capture_output(&mut self, request: crate::screencopy::CaptureRequest<'_>) -> Result<crate::screencopy::CapturedFrame, String> {
        let _ = request;
        Err("screen capture is not supported by this backend".to_string())
    }
    /// Which `linux-dmabuf` formats a capture can be rendered into, and
    /// on which device; `None` = only shm capture (the default).
    fn capture_dmabuf_caps(&mut self) -> Option<crate::screencopy::DmabufCaps> {
        None
    }
    /// Like [`Backend::capture_output`], but rendered straight into a
    /// client's dmabuf on the GPU.
    fn capture_into_dmabuf(
        &mut self,
        request: crate::screencopy::CaptureRequest<'_>,
        dmabuf: &mut smithay::backend::allocator::dmabuf::Dmabuf,
    ) -> Result<(), String> {
        let _ = (request, dmabuf);
        Err("dmabuf capture is not supported by this backend".to_string())
    }
    /// Allocates a buffer of `width`×`height` a capture can be rendered
    /// into and handed to a client (`wlr-export-dmabuf`).
    fn allocate_capture_buffer(&mut self, width: i32, height: i32) -> Result<smithay::backend::allocator::dmabuf::Dmabuf, String> {
        let _ = (width, height);
        Err("dmabuf capture is not supported by this backend".to_string())
    }
    /// Something visible may have changed (a client committed, the
    /// pointer moved, a command re-tiled): the backend should render a
    /// new frame soon. The DRM backend renders on demand rather than
    /// polling (idle = zero wakeups); the nested backend redraws every
    /// event-loop turn anyway, so the default is a no-op.
    fn queue_redraw(&mut self) {}
    /// Imports a client's `linux-dmabuf` buffer into the renderer so it
    /// can be drawn later; `false` refuses it. Only backends that
    /// advertise the `zwp_linux_dmabuf_v1` global are ever asked, so the
    /// default (the nested backend, which does not advertise it) refuses.
    fn dmabuf_imported(&mut self, dmabuf: &smithay::backend::allocator::dmabuf::Dmabuf) -> bool {
        let _ = dmabuf;
        false
    }
    /// Switches `output` to `mode` on real hardware (`bspc output -m`),
    /// returning the Wayland-side mode that is now current. `Err`'s text
    /// becomes the command's failure reply. The default rejects: the
    /// nested backend's one window has no modes to switch between (and
    /// exposes none, so `bspc output` never validates a request for one).
    fn set_output_mode(&mut self, output: &smithay::output::Output, mode: bsp_ipc::command::OutputMode) -> Result<smithay::output::Mode, String> {
        let _ = (output, mode);
        Err("output: mode switching is not supported by this backend.\n".to_string())
    }
    /// Sets pointer acceleration (`bspc input DEVICE -a`) on the named
    /// device. The default rejects, like [`Backend::set_output_mode`].
    fn set_pointer_accel(&mut self, device: &str, accel: f64) -> Result<(), String> {
        let _ = (device, accel);
        Err("input: pointer acceleration is not supported by this backend.\n".to_string())
    }
}

/// Makes `SIGINT`/`SIGHUP`/`SIGTERM` stop the main loop, as bspwm does
/// (`src/bspwm.c` `sig_handler()`: those three set `running = false`) —
/// and, here, lets the `Drop`s that remove the Wayland and control
/// sockets run, so an ordinary `kill` does not leave a stale
/// `wayland-N` behind (which made the next start pick `wayland-N+1`).
pub fn init_quit_signals<Bd: Backend + 'static>(state: &mut State<Bd>) {
    use smithay::reexports::calloop::signals::{Signal, Signals};
    match Signals::new(&[Signal::SIGINT, Signal::SIGHUP, Signal::SIGTERM]) {
        Ok(signals) => {
            if let Err(err) = state.handle.insert_source(signals, |event, _, state| {
                tracing::info!(signal = ?event.signal(), "quitting");
                state.running = false;
            }) {
                tracing::warn!("failed to register the quit signal handler: {err}");
            }
        }
        Err(err) => tracing::warn!("failed to set up quit signal handling: {err}"),
    }
}

/// Per-client state Smithay asks every client to carry.
#[derive(Default)]
pub struct ClientState {
    /// Connected through a `wp_security_context_v1` socket (a sandboxed
    /// app): privileged protocols are hidden from it.
    pub sandboxed: bool,
    /// Per-client compositor bookkeeping (required by `CompositorHandler`).
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// The compositor's whole state, threaded through every Smithay callback.
///
/// Generic over which backend is running (`Bd`, `crate::winit_backend::WinitData`
/// or `crate::udev_backend::DrmData`) — see [`Backend`]'s own doc comment
/// for why a generic rather than an enum.
pub struct State<Bd: Backend + 'static> {
    /// Handle to the Wayland display, for inserting new clients.
    pub display_handle: DisplayHandle,
    /// Set to `false` to stop the main loop (`bspc wm --restart`/emergency quit).
    pub running: bool,
    /// The calloop event loop handle, for registering each newly accepted
    /// IPC connection's file descriptor (`crate::ipc`).
    pub handle: LoopHandle<'static, State<Bd>>,
    /// Monotonic clock reference for input event timestamps.
    pub start_time: Instant,
    /// The same monotonic time, as Smithay's own `Time<Monotonic>` type —
    /// the DRM backend's vblank-driven render loop needs it for frame
    /// timing/presentation feedback (`crate::udev_backend`); the nested
    /// backend has no use for it yet but it costs nothing to keep on
    /// every backend rather than making it backend-specific.
    #[cfg_attr(not(feature = "real"), allow(dead_code))]
    pub clock: Clock<Monotonic>,

    /// Every mapped window, arranged by `bsp-core`.
    pub space: Space<Window>,
    /// Popup (e.g. menu, tooltip) tracking.
    pub popups: PopupManager,
    /// Popups that asked for a grab (`xdg_popup.grab`): a click outside them
    /// dismisses them (`crate::shell::dismiss_grabbed_popups`).
    pub grabbed_popups: Vec<smithay::wayland::shell::xdg::PopupSurface>,
    /// Toplevels whose role was just created but have not yet reached
    /// their first `commit` (`crate::shell::new_toplevel`/`on_commit`):
    /// rule matching needs `app_id`/`title`, which are only reliably set
    /// by then, so mapping itself waits until then too.
    pub pending_toplevels: Vec<ToplevelSurface>,

    // Smithay protocol globals.
    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    /// XWayland: display sockets, the running server and its window manager (`crate::xwayland`).
    pub xwayland: crate::xwayland::XWaylandState,
    /// The the protocols protocol globals (`crate::protocols`).
    pub protocols: crate::protocols::Protocols<Bd>,
    /// `zwp_linux_dmabuf_v1` bookkeeping; its global is created by
    /// backends that can import dmabufs (`crate::udev_backend`).
    pub dmabuf_state: smithay::wayland::dmabuf::DmabufState,
    pub seat_state: SeatState<State<Bd>>,
    pub xdg_shell_state: XdgShellState,

    /// The one seat this compositor exposes (bspwm itself only ever
    /// tracks one input focus at a time; multi-seat is not a bspwm
    /// concept, `docs/design.md` Compatibility).
    pub seat: Seat<State<Bd>>,
    pub pointer: PointerHandle<State<Bd>>,
    pub cursor_status: CursorImageStatus,

    /// The `bsp-core` window manager state: monitors, desktops, trees.
    pub wm: bsp_core::wm::Wm,
    /// Stable wire node ids, shared with `bsp-ipc`'s executor once the
    /// control socket is wired in.
    pub registry: bsp_ipc::registry::NodeRegistry,
    /// `WindowId` ↔ Smithay `Window` map and class/instance lookup; also
    /// `bsp-ipc`'s `Adapter`.
    pub adapter: WindowAdapter,
    /// Open `subscribe`d control-socket connections (`crate::ipc`).
    pub subscribers: bsp_ipc::server::Subscribers,

    /// Every hotkey loaded from sxhkdrc (`crate::hotkeys::init`), in the
    /// same order `hotkey_matcher` was built from — a `Fire { index }`
    /// outcome indexes into this `Vec`.
    pub hotkeys: Vec<bsp_hotkeys::config::LoadedHotkey>,
    /// The chord-chain state machine driven by every keyboard event
    /// (`crate::hotkeys::process_key`).
    pub hotkey_matcher: bsp_hotkeys::matcher::Matcher,
    /// `bspc config hotkeys_inline_bspc` (`docs/bsp-hotkeys.md` "Binding
    /// execution"): `true` runs a [`bsp_hotkeys::dispatch::Dispatch::InlineBspc`]
    /// binding straight through `bsp-ipc`'s executor in-process; `false`
    /// forces every binding through a shell instead. Lives here rather
    /// than in `bsp_core::wm::Wm`'s `Settings` because it affects nothing
    /// in the tree engine — `bsp-core`'s own settings module doc comment
    /// reserves exactly this kind of compositor-only setting for
    /// `bsp-compositor`.
    pub hotkeys_inline_bspc: bool,
    /// `bspc config pointer_modifier`/`pointer_action1..3`/
    /// `click_to_focus`/`pointer_motion_interval`/`swallow_first_click`
    /// (`crate::pointer_action`) — compositor-local for the same
    /// reason as `hotkeys_inline_bspc` above.
    pub pointer_settings: crate::pointer_action::PointerSettings,
    /// Buttons whose press was swallowed (`swallow_first_click`, a drag binding):
    /// their release is not forwarded either, or the client would see a release
    /// with no press.
    pub swallowed_buttons: std::collections::HashSet<u32>,
    /// `sync_wayland_from_core` is wanted but the caller holds the pointer's lock
    /// (a drag grab callback), so it runs after the event-loop turn
    /// (`shell::run_deferred_sync`).
    pub sync_pending: bool,
    /// Replies to `bspc` clients that read slowly, still being written.
    pub pending_replies: Vec<crate::ipc::PendingReply>,
    /// Whether the timer that revisits queued IPC output is armed.
    pub ipc_flush_armed: bool,
    /// When `extras::periodic_syncs` last ran, and whether a timer is already
    /// armed for the next run.
    pub last_periodic_sync: Option<std::time::Instant>,
    /// See `last_periodic_sync`.
    pub periodic_timer_armed: bool,

    /// The backend: `crate::winit_backend::WinitData` (nested, cargo
    /// feature `nested`) or `crate::udev_backend::DrmData` (real
    /// hardware, cargo feature `real`) — mutually exclusive builds, see
    /// `main.rs`.
    pub backend_data: Bd,
}

impl<Bd: Backend + 'static> State<Bd> {
    /// Asks for `sync_wayland_from_core` to run once the current event-loop turn
    /// is over, outside any Smithay lock (`shell::run_deferred_sync`). The way
    /// for code that cannot sync now (a pointer-grab callback, a commit) to get
    /// the tree reconciled with the surfaces; several requests make one sync.
    pub fn request_sync(&mut self) {
        self.sync_pending = true;
    }

    /// Creates the compositor state and every protocol global, and starts
    /// listening on a Wayland socket.
    pub fn new(
        display_handle: DisplayHandle,
        handle: LoopHandle<'static, State<Bd>>,
        backend_data: Bd,
        wm: bsp_core::wm::Wm,
        hotkeys: Vec<bsp_hotkeys::config::LoadedHotkey>,
    ) -> Self {
        let protocols = crate::protocols::Protocols::new(&display_handle, handle.clone());
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, Vec::new());
        let mut seat_state = SeatState::new();
        // Only fullscreen: bspwm has no maximize, minimize or window menu, and a
        // client that is told they exist draws buttons for them that do nothing.
        let xdg_shell_state = XdgShellState::new_with_capabilities::<Self>(
            &display_handle,
            [smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::WmCapabilities::Fullscreen],
        );

        let mut seat = seat_state.new_wl_seat(&display_handle, "seat0");
        let pointer = seat.add_pointer();
        seat.add_touch();
        // Without a keyboard every keyboard path degrades to a no-op
        // (each looks the keyboard up and returns if absent).
        if let Err(err) = seat.add_keyboard(Default::default(), 200, 25) {
            tracing::error!("failed to initialize the keyboard: {err}");
        }

        let hotkey_matcher = crate::hotkeys::build_matcher(&hotkeys);

        State {
            display_handle,
            running: true,
            handle,
            start_time: Instant::now(),
            clock: Clock::new(),
            space: Space::default(),
            popups: PopupManager::default(),
            grabbed_popups: Vec::new(),
            pending_toplevels: Vec::new(),
            compositor_state,
            shm_state,
            protocols,
            xwayland: crate::xwayland::XWaylandState::default(),
            dmabuf_state: smithay::wayland::dmabuf::DmabufState::new(),
            seat_state,
            xdg_shell_state,
            seat,
            pointer,
            cursor_status: CursorImageStatus::default_named(),
            wm,
            registry: bsp_ipc::registry::NodeRegistry::new(),
            adapter: WindowAdapter::new(),
            subscribers: bsp_ipc::server::Subscribers::new(),
            hotkeys,
            hotkey_matcher,
            hotkeys_inline_bspc: true,
            pointer_settings: crate::pointer_action::PointerSettings::default(),
            swallowed_buttons: std::collections::HashSet::new(),
            sync_pending: false,
            pending_replies: Vec::new(),
            ipc_flush_armed: false,
            last_periodic_sync: None,
            periodic_timer_armed: false,
            backend_data,
        }
    }
}

impl<Bd: Backend + 'static> BufferHandler for State<Bd> {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
    }
}

impl<Bd: Backend + 'static> CompositorHandler for State<Bd> {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        // Every client is created through `insert_client`, which attaches
        // a `ClientState`; a foreign one gets a shared default instead of
        // a panic.
        static FALLBACK: std::sync::OnceLock<CompositorClientState> = std::sync::OnceLock::new();
        match client.get_data::<ClientState>() {
            Some(data) => &data.compositor_state,
            None => FALLBACK.get_or_init(CompositorClientState::default),
        }
    }

    fn commit(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        crate::shell::on_commit(self, surface);
        crate::layers::on_commit(self, surface);
        self.backend_data.early_import(surface);
        self.backend_data.queue_redraw();
    }
}
delegate_compositor!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> ShmHandler for State<Bd> {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
delegate_shm!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> smithay::wayland::dmabuf::DmabufHandler for State<Bd> {
    fn dmabuf_state(&mut self) -> &mut smithay::wayland::dmabuf::DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &smithay::wayland::dmabuf::DmabufGlobal,
        dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: smithay::wayland::dmabuf::ImportNotifier,
    ) {
        if self.backend_data.dmabuf_imported(&dmabuf) {
            // A failed `successful` only means the client went away.
            let _ = notifier.successful::<State<Bd>>();
        } else {
            notifier.failed();
        }
    }
}
smithay::delegate_dmabuf!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> OutputHandler for State<Bd> {}
delegate_output!(@<Bd: Backend + 'static> State<Bd>);

impl<Bd: Backend + 'static> SelectionHandler for State<Bd> {
    type SelectionUserData = ();

    /// A Wayland client set the clipboard/primary selection: tell X11 clients.
    fn new_selection(
        &mut self,
        ty: smithay::wayland::selection::SelectionTarget,
        source: Option<smithay::wayland::selection::SelectionSource>,
        _seat: Seat<Self>,
    ) {
        self.selection_to_xwm(ty, source.map(|s| s.mime_types()));
    }

    /// A Wayland client wants an X11 client's selection contents.
    fn send_selection(
        &mut self,
        ty: smithay::wayland::selection::SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &Self::SelectionUserData,
    ) {
        self.selection_from_xwm(ty, mime_type, fd);
    }
}

impl<Bd: Backend + 'static> SeatHandler for State<Bd> {
    type KeyboardFocus = crate::focus::FocusTarget;
    type PointerFocus = smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
    type TouchFocus = smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State<Bd>> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        tracing::debug!(
            image = match &image {
                CursorImageStatus::Hidden => "hidden".to_string(),
                CursorImageStatus::Named(icon) => format!("named {}", icon.name()),
                CursorImageStatus::Surface(_) => "surface".to_string(),
            },
            "cursor image requested"
        );
        self.cursor_status = image;
        self.backend_data.queue_redraw();
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&Self::KeyboardFocus>) {
        use smithay::wayland::seat::WaylandFocus;
        let surface = focused.and_then(|target| target.wl_surface()).map(|s| s.into_owned());
        crate::protocols::keyboard_focus_changed(self, seat, surface.as_ref());
        self.activate_x11_focus(surface.as_ref());
        self.update_activated(surface.as_ref());
    }

    fn led_state_changed(&mut self, _seat: &Seat<Self>, led_state: LedState) {
        self.backend_data.update_led_state(led_state);
    }
}
delegate_seat!(@<Bd: Backend + 'static> State<Bd>);

/// Exports what desktop portals and session tooling look for, so programs
/// started from `bspwmrc` inherit it: `XDG_CURRENT_DESKTOP=bspwm-rs` (how
/// `xdg-desktop-portal` picks a portal backend — see
/// `contrib/portals/bspwm-rs-portals.conf`) and `XDG_SESSION_TYPE=wayland`.
/// Both are only set if the environment did not already say otherwise (a
/// console login's `XDG_SESSION_TYPE=tty` counts as "unset").
pub fn export_session_env() {
    for (key, value) in [("XDG_CURRENT_DESKTOP", "bspwm-rs"), ("XDG_SESSION_TYPE", "wayland")] {
        // A console login (logind) says `XDG_SESSION_TYPE=tty`, which is not
        // what a Wayland compositor is: treat it like unset.
        let unset = match std::env::var(key) {
            Err(_) => true,
            Ok(current) => key == "XDG_SESSION_TYPE" && current == "tty",
        };
        if unset {
            // SAFETY: `set_var` is unsound only if another thread reads or
            // writes the environment concurrently; this runs once at
            // startup, before the event loop and before any child process
            // (`bspwmrc`) or helper thread exists.
            unsafe {
                std::env::set_var(key, value);
            }
        }
    }
}

/// Whether `client` may use privileged protocols (screen capture, input
/// injection, window lists, output configuration, session lock …): any
/// client except one that came in through a security-context socket.
pub fn is_privileged(client: &Client) -> bool {
    !client.get_data::<ClientState>().is_some_and(|c| c.sandboxed)
}

/// Registers a new Wayland client connection with the display.
pub fn insert_client(display_handle: &DisplayHandle, stream: std::os::unix::net::UnixStream) {
    let mut display_handle = display_handle.clone();
    if let Err(err) = display_handle.insert_client(stream, Arc::new(ClientState::default())) {
        tracing::warn!("failed to add Wayland client: {err}");
    }
}
