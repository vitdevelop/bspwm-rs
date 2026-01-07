# bspwm-rs

A Wayland compositor with exact bspwm behavior, written in Rust on Smithay.

`docs/design.md` is the specification: read it first, especially **Development rules**, then continue with the roadmap. `docs/bsp-compositor.md` has the current status.

## Documents

| File | Holds |
| --- | --- |
| [docs/design.md](docs/design.md) | Goal, decisions, architecture, performance budget, compatibility with bspwm 0.9.12, roadmap, and the rules the implementation must follow |
| [docs/bsp-core.md](docs/bsp-core.md) | The pure-logic crate: tree, desktops, monitors, rules |
| [docs/bsp-ipc.md](docs/bsp-ipc.md) | The `bspc`-compatible control socket |
| [docs/bsp-hotkeys.md](docs/bsp-hotkeys.md) | The bundled sxhkd replacement |
| [docs/bsp-compositor.md](docs/bsp-compositor.md) | The Smithay layer: rendering, input, outputs, protocols |
| [docs/migrating.md](docs/migrating.md) | What an existing bspwm setup has to change |
| [CHANGELOG.md](CHANGELOG.md) | Every function added, changed or removed |

## Keeping the docs true

Every commit that adds, changes or removes a public function also updates the Public functions table in that crate's file and adds a row to `CHANGELOG.md`. A design decision changes `docs/design.md`.
