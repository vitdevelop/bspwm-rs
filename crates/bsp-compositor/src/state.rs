//! The compositor's global state: every Smithay protocol global, the
//! Wayland display, and the `bsp-core`/`bsp-ipc` state the rest of this
//! crate wires Wayland activity to.
//!
//! Scoped to what the nested compositor needs (`docs/design.md` roadmap: "winit backend,
//! xdg-shell, tiling through the core, focus, borders"): the compositor,
//! shm, output, seat and xdg-shell globals only. Data-device (clipboard),
//! xdg-decoration, layer-shell, presentation-time and every other
//! protocol in `docs/bsp-compositor.md`'s module table are later steps
//! and not wired up yet — a client that needs one simply doesn't see that
//! global advertised.

use std::sync::Arc;
use std::time::Instant;

use smithay::desktop::{PopupManager, Space, Window};
use smithay::input::keyboard::LedState;
use smithay::input::pointer::{CursorImageStatus, PointerHandle};
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::{Client, DisplayHandle};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{CompositorClientState, CompositorHandler, CompositorState};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{delegate_compositor, delegate_output, delegate_seat, delegate_shm};

use crate::adapter::WindowAdapter;
use crate::winit_backend::WinitData;

/// Per-client state Smithay asks every client to carry.
#[derive(Default)]
pub struct ClientState {
    /// Per-client compositor bookkeeping (required by `CompositorHandler`).
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

/// The compositor's whole state, threaded through every Smithay callback.
pub struct State {
    /// Handle to the Wayland display, for inserting new clients.
    pub display_handle: DisplayHandle,
    /// Set to `false` to stop the main loop (`bspc wm --restart`/emergency quit).
    pub running: bool,
    /// Monotonic clock reference for input event timestamps.
    pub start_time: Instant,

    /// Every mapped window, arranged by `bsp-core`.
    pub space: Space<Window>,
    /// Popup (e.g. menu, tooltip) tracking.
    pub popups: PopupManager,

    // Smithay protocol globals.
    pub compositor_state: CompositorState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<State>,
    pub xdg_shell_state: XdgShellState,

    /// The one seat this compositor exposes (bspwm itself only ever
    /// tracks one input focus at a time; multi-seat is not a bspwm
    /// concept, `docs/design.md` Compatibility).
    pub seat: Seat<State>,
    pub pointer: PointerHandle<State>,
    pub cursor_status: CursorImageStatus,

    /// The `bsp-core` window manager state: monitors, desktops, trees.
    pub wm: bsp_core::wm::Wm,
    /// Stable wire node ids, shared with `bsp-ipc`'s executor once the
    /// control socket is wired in.
    pub registry: bsp_ipc::registry::NodeRegistry,
    /// `WindowId` ↔ Smithay `Window` map and class/instance lookup; also
    /// `bsp-ipc`'s `Adapter`.
    pub adapter: WindowAdapter,

    /// The backend (currently only the nested winit one exists).
    pub backend_data: WinitData,
}

impl State {
    /// Creates the compositor state and every protocol global, and starts
    /// listening on a Wayland socket.
    pub fn new(
        display_handle: DisplayHandle,
        backend_data: WinitData,
        wm: bsp_core::wm::Wm,
    ) -> Self {
        let compositor_state = CompositorState::new::<Self>(&display_handle);
        let shm_state = ShmState::new::<Self>(&display_handle, Vec::new());
        let mut seat_state = SeatState::new();
        let xdg_shell_state = XdgShellState::new::<Self>(&display_handle);

        let mut seat = seat_state.new_wl_seat(&display_handle, "seat0");
        let pointer = seat.add_pointer();
        seat.add_keyboard(Default::default(), 200, 25)
            .expect("failed to initialize the keyboard");

        State {
            display_handle,
            running: true,
            start_time: Instant::now(),
            space: Space::default(),
            popups: PopupManager::default(),
            compositor_state,
            shm_state,
            seat_state,
            xdg_shell_state,
            seat,
            pointer,
            cursor_status: CursorImageStatus::default_named(),
            wm,
            registry: bsp_ipc::registry::NodeRegistry::new(),
            adapter: WindowAdapter::new(),
            backend_data,
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(
        &mut self,
        _buffer: &smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    ) {
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        crate::shell::on_commit(self, surface);
    }
}
delegate_compositor!(State);

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
delegate_shm!(State);

impl OutputHandler for State {}
delegate_output!(State);

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl SeatHandler for State {
    type KeyboardFocus = smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
    type PointerFocus = smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
    type TouchFocus = smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        self.cursor_status = image;
    }

    // No LED state to forward yet — the nested backend's virtual
    // keyboard has no LEDs; a real keyboard will need this.
    fn led_state_changed(&mut self, _seat: &Seat<Self>, _led_state: LedState) {}
}
delegate_seat!(State);

/// Registers a new Wayland client connection with the display.
pub fn insert_client(display_handle: &DisplayHandle, stream: std::os::unix::net::UnixStream) {
    let mut display_handle = display_handle.clone();
    if let Err(err) = display_handle.insert_client(stream, Arc::new(ClientState::default())) {
        tracing::warn!("failed to add Wayland client: {err}");
    }
}
