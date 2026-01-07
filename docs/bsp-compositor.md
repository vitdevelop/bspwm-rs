# bsp-compositor

The only crate that uses Smithay: it turns Wayland and hardware activity into `bsp-core` events and applies the effects the core returns. Builds the `bspwm-rs` binary.

## Modules (planned)

| Module | Holds |
| --- | --- |
| `main` | Startup, calloop event loop, runs `bspwmrc` |
| `adapter` | `WindowId` to Smithay window map; `Event` building and `Effect` application |
| `backend/udev` | DRM/KMS outputs, libseat session, udev hotplug, multi-GPU renderer |
| `backend/winit` | Nested development backend (cargo feature `nested`, not in release builds) |
| `render` | Per-output damage tracking, border rectangles, direct scanout |
| `input` | libinput devices, xkbcommon keymap, hands keys to `bsp-hotkeys` before clients |
| `shell/xdg` | xdg-shell toplevels and popups, xdg-decoration forced to server side |
| `shell/layer` | wlr-layer-shell for bars, launchers and wallpapers |
| `xwayland` | Lazy XWayland start and Smithay's X11 window manager |
| `protocols` | xdg-output, foreign-toplevel, ext-workspace, screencopy, session lock, idle, data device, fractional scale, viewporter, dmabuf, pointer constraints, output management, gamma control, virtual keyboard and pointer, input method, xdg-activation, shortcuts inhibit, tearing control, security context |

## Renderer abstraction

Decision: drawing code never names a concrete renderer, so OpenGL ES, Vulkan and Pixman are interchangeable and chosen at runtime.

- **Generic drawing**: `render` is generic over Smithay's renderer traits (`R: Renderer + ImportAll + ImportDma`). It only draws client surfaces, solid-color border and presel rectangles, and clears, all limited to damaged regions.
- **Concrete types in one place**: only `backend/*` constructs `GlesRenderer`, a Vulkan renderer or `PixmanRenderer`, then hands them to `render`. Multi-GPU wraps whichever renderer is chosen.
- **Selection**: `BSPWM_RENDERER=gles|vulkan|pixman`. Default order is OpenGL ES, then Pixman if GL setup fails.
- **Vulkan**: opt-in until Smithay's Vulkan rendering support is complete enough for daily use; its status is checked before the hardware backend.
- **Cargo features**: `renderer-gles` (default), `renderer-pixman` (default, always present as fallback), `renderer-vulkan` (optional).
- **Testing**: CI builds every feature combination. Pixman needs no GPU, so rendering is tested headlessly against reference images, and GPU renderers are checked on real hardware.

| Renderer | Role | Needs |
| --- | --- | --- |
| OpenGL ES | Default | Mesa or NVIDIA 555+ |
| Vulkan | Optional, opt-in | Vulkan driver; Smithay support to be verified |
| Pixman | Fallback, CI tests | CPU only |

## Public functions

| Function | Signature | Behavior |
| --- | --- | --- |
| — | — | Filled from the nested compositor |
