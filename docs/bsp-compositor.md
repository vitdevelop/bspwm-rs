# bsp-compositor

The only crate that uses Smithay: it turns Wayland and hardware activity into `bsp-core` events and applies the effects the core returns. Builds the `bspwm-rs` binary.

## Modules

| Module | Holds |
| --- | --- |
| `main` | Startup, `tracing` init |
| `state` | `State` (every Smithay protocol global, `bsp-core::wm::Wm`, `bsp-ipc::registry::NodeRegistry`, the `WindowAdapter`, `bsp_ipc::server::Subscribers`); `CompositorHandler`/`ShmHandler`/`SeatHandler`/`OutputHandler`/`BufferHandler` impls |
| `shell` | `XdgShellHandler`: maps a new `xdg_toplevel` to a `bsp-core` client node (insert, arrange, configure the size back), unmaps it on `toplevel_destroyed`, and `sync_wayland_from_core` — reconciles the `Space`/client surfaces with `bsp-core`'s tree after any command that may have re-arranged it |
| `input` | Forwards winit keyboard/pointer events to the Wayland seat; click-to-focus |
| `render` | Per-output damage-tracked rendering: client surfaces plus border strips |
| `adapter` | `WindowId` ↔ Smithay `Window` map; implements `bsp_ipc::adapter::Adapter` (class/instance lookup, close/kill) |
| `ipc` | Binds `bsp-ipc`'s control socket and drives its `Listener`/`Connection`/`Subscribers` from this crate's calloop event loop, running every request through `bsp_ipc::exec::execute` |
| `winit_backend` | Nested development backend (cargo feature `nested`, on by default; no other backend exists yet, so it is currently required — see Nested compositor progress) |
| `backend/udev` (planned) | DRM/KMS outputs, libseat session, udev hotplug, multi-GPU renderer — the hardware backend |
| `shell/layer` (planned) | wlr-layer-shell for bars, launchers and wallpapers — the protocols |
| `xwayland` (planned) | Lazy XWayland start and Smithay's X11 window manager — XWayland |
| `protocols` (planned) | xdg-output, foreign-toplevel, ext-workspace, screencopy, session lock, idle, data device, fractional scale, viewporter, dmabuf, pointer constraints, output management, gamma control, virtual keyboard and pointer, input method, xdg-activation, shortcuts inhibit, tearing control, security context — the protocols |

## Nested compositor progress

Delivered and actually run inside a live Wayland session (Hyprland, nested — `docs/design.md` roadmap, the nested compositor: "winit backend, xdg-shell, tiling through the core, focus, borders"): the winit backend opens a real window, exposes a Wayland socket, and a real client (`alacritty`) connecting to it gets mapped as a `bsp-core` client node, tiled (arranged, sized via `xdg_toplevel`'s `configure`), focused, bordered, and cleanly unmapped on exit. Confirmed via `tracing` logs (`mapped a new window window=0x80000000 rect=Rect { x: 6, y: 6, width: 924, height: 1020 }` / `unmapped a window`) and the process staying stable afterward.

**`bsp-ipc`'s control socket is wired in and confirmed against the same live client**: `bspc-rs query -M --names`, `wm -g`, `query -T` and `node -t floating`/`node -t tiled` all round-tripped correctly against the running compositor — including `query -T`'s JSON showing the real `className`/`instanceName` ("Alacritty") picked up from `app_id_changed`, and the floating/tiled state transition actually resizing and repositioning the live window on screen via `shell::sync_wayland_from_core`. This — and the mapping/unmapping above — is the closest thing to an automated test this crate can have without a virtual display in CI: `WindowAdapter` and every `XdgShellHandler`/`CompositorHandler` impl need a live `smithay::desktop::Window`, which needs a real `ToplevelSurface`, which needs a running Wayland client.

One bspwm behavior gap found and fixed by this live testing: bspwm seeds a new client's `floating_rectangle` from its real on-screen geometry at map time (`src/window.c` `initialize_floating_rectangle()`, an `xcb_get_geometry` call before the window is tiled) — no Wayland equivalent exists, since an xdg-shell client has no on-screen geometry before its first `configure`. `map_new_toplevel` seeds it from the tiled slot just computed instead; without this, `bspc node -t floating` collapsed the window to 0×0 (clamped to 1×1) instead of keeping its size.

Deliberately out of scope for this pass, each documented at its own call site with a `docs/bsp-compositor.md` scope reference rather than silently approximated:

- **Rules**: every mapped window lands tiled, unconditionally — `bsp-core::rules` is not consulted yet. `manage=off`, forced floating/fullscreen state, a target monitor/desktop from a rule, are all hotkeys+ work (rules need `bsp-ipc`'s parsed `RuleConsequence`/`RuleTarget`, wired to real window class/instance names, which only `app_id_changed` populates after the fact right now).
- **`bsp-hotkeys`**: not wired in. Every key is forwarded to the focused client unconditionally; the emergency keys `docs/design.md`'s Reliability section promises (Ctrl+Alt+F1–F12, Ctrl+Alt+Shift+Escape) are not implemented.
- **`bspwmrc`**: not read or run at startup.
- **`subscribe`'s events over the socket**: `Subscribers::broadcast_event`/`broadcast_report` are wired to fire after every executed command, but this has not yet been exercised with a live `bspc-rs subscribe` client (only request/reply commands have been live-tested so far).
- **`quit`'s exit status**: `bspc-rs quit <n>` stops the compositor but `<n>` is not threaded through to the process's own exit code yet.
- **Popup grabs**: a popup positions and shows correctly but cannot yet take an implicit pointer/keyboard grab that dismisses it on an outside click (`XdgShellHandler::grab` is a no-op).
- **xdg-decoration**: not offered as a global yet, so a client that insists on server-side decoration has no way to ask for it (most clients fall back to their own CSD, or none, without it).
- **Move/resize via pointer drag**: bspwm is a tiling WM and does not support this either (outside `state=floating`), so it is not a gap relative to bspwm, but noted since Smithay's `XdgShellHandler::move_request`/`resize_request` are left at their no-op defaults. `bspc node -v/-z` (programmatic move/resize) is also still unimplemented in `bsp-ipc::exec` itself (`docs/bsp-ipc.md`).
- **LED state, tablet, touch, gestures, relative pointer, pointer constraints, data device (clipboard), fractional scale, presentation-time**: no globals for any of these yet; a client that needs one simply does not see it advertised.

## Border rendering

A node's border is four separate solid-color strips (top/bottom/left/right) framing its `tiled_rectangle`, not one rectangle drawn behind the surface: `smithay::desktop::space::render_output`'s `custom_elements` argument always draws on top of window content (elements earlier in its internal render list are topmost, and there is no ready-made way to interleave a custom element behind one specific window's surface without writing a full custom combining `RenderElement` — see `render.rs`'s module doc comment). Four non-overlapping strips sidestep the ordering problem entirely rather than fighting it. Colors are hardcoded (`FOCUSED_BORDER_COLOR`/`NORMAL_BORDER_COLOR`) since `bsp-core::Settings` has no color fields yet (colors are an X11/pointer-adjacent concern, `docs/bsp-core.md`).

## Renderer abstraction

Decision: drawing code never names a concrete renderer, so OpenGL ES, Vulkan and Pixman are interchangeable and chosen at runtime. **Not yet implemented**: `render.rs` currently names `GlesRenderer` concretely (the nested compositor only needed one working renderer to prove the pipeline); making it generic over Smithay's renderer traits, and choosing among GLES/Pixman/Vulkan at runtime, is deferred to when a second renderer actually exists to switch to (real hardware without working GLES, the hardware backend).

- **Selection** (planned): `BSPWM_RENDERER=gles|vulkan|pixman`. Default order is OpenGL ES, then Pixman if GL setup fails.
- **Vulkan**: opt-in until Smithay's Vulkan rendering support is complete enough for daily use; its status is checked before the hardware backend.
- **Cargo features**: `renderer-gles` (default, in use), `renderer-pixman` (declared on the `smithay` dependency, not yet used by `render.rs`), `renderer-vulkan` (not yet declared).
- **Testing**: CI builds every feature combination. Pixman needs no GPU, so rendering is tested headlessly against reference images, and GPU renderers are checked on real hardware. Not implemented yet — no CI workflow exists for this crate (only `bsp-core`'s dependency-isolation check runs today, `.github/workflows/ci.yml`).

| Renderer | Role | Needs |
| --- | --- | --- |
| OpenGL ES | Default, in use | Mesa or NVIDIA 555+ |
| Vulkan | Optional, opt-in | Vulkan driver; Smithay support to be verified |
| Pixman | Fallback, CI tests | CPU only — declared as a `smithay` feature, not wired into `render.rs` yet |

## Public functions

| Function | Signature | Behavior |
| --- | --- | --- |
| `winit_backend::run` | `fn()` | Opens the nested window, creates the Wayland socket, binds the control socket, runs the main loop until closed |
| `State::new` | `fn(DisplayHandle, LoopHandle<State>, WinitData, bsp_core::wm::Wm) -> State` | Creates every protocol global and the one seat |
| `State::window_for_surface` | `fn(&self, &WlSurface) -> Option<Window>` | The mapped window showing a surface, if any |
| `insert_client` | `fn(&DisplayHandle, UnixStream)` | Registers a new Wayland client connection |
| `input::process_input_event` | `fn<B>(&mut State, InputEvent<B>, &Output)` | Forwards one winit input event to the seat |
| `input::focus_node` | `fn(&mut State, usize, usize, NodeId)` | Sets the seat's keyboard focus to a `bsp-core` node's client surface |
| `render::render_frame` | `fn(...) -> Result<RenderOutputResult, ...>` | Renders one damage-tracked frame: client surfaces plus border strips |
| `WindowAdapter::insert`/`remove`/`window`/`id_of`/`set_app_id` | `fn(&mut self/&self, ...) -> ...` | The `WindowId` ↔ `Window` map and class/instance bookkeeping |
| `WindowAdapter`'s `Adapter` impl | `window_class`/`close_window`/`kill_window` | `bsp-ipc::exec`'s window-system interface, backed by real `xdg_toplevel::send_close` |
| `shell::sync_wayland_from_core` | `fn(&mut State)` | Reconciles every mapped client's `Space` position and `xdg_toplevel` size with `bsp-core`'s tree; called after every executed IPC command and after a nested-window resize |
| `ipc::init` | `fn(&mut State)` | Binds the control socket and registers it (and every accepted connection) with the event loop |
