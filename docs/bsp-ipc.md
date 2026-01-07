# bsp-ipc

The control socket: it speaks bspwm's wire protocol so the stock `bspc` binary, `bspwmrc` and existing scripts work unchanged.

## Protocol

- Socket path from `BSPWM_SOCKET`; when unset, a default under `XDG_RUNTIME_DIR` (bspwm's X11 display-based name does not apply on Wayland).
- A request is the `bspc` arguments joined by NUL bytes; the reply is text, and a failure reply starts with bspwm's failure marker byte so `bspc` exits non-zero.
- `subscribe` keeps the connection open and streams report lines and events.
- The exact byte format will be checked against bspwm's source before the IPC closes.

## Modules (planned)

| Module | Holds |
| --- | --- |
| `wire` | Reading and writing NUL-separated requests and replies |
| `parse` | `bspc` argument grammar into a `Command` for `bsp-core`: node, desktop, monitor, query, wm, rule, config, subscribe, quit |
| `selector` | Node, desktop and monitor selectors and descriptors (`focused`, `older`, `.local.!floating`, and the rest) |
| `report` | `wm -d` JSON dump, `query` output, and subscribe report lines |
| `server` | Non-blocking listener driven by the compositor's event loop |

## Public functions

| Function | Signature | Behavior |
| --- | --- | --- |
| — | — | Filled from the IPC |
