## Bspwm-rs

*bspwm-rs* is a tiling Wayland compositor that represents windows as the leaves of a full binary tree, written in Rust on [Smithay](https://github.com/Smithay/smithay). It aims at exact [bspwm](https://github.com/baskerville/bspwm) 0.9.12 behavior: the same tree, the same `bspc` commands over the same kind of control socket, the same `bspwmrc` and `sxhkdrc`.

It only responds to Wayland events and to the messages it receives on a dedicated socket.

`bspc-rs` is a program that writes messages on bspwm-rs's socket. Stock `bspc` works too when `BSPWM_SOCKET` points at it.

bspwm-rs ships its own sxhkd replacement (a built-in matcher for `sxhkdrc`), because a Wayland client cannot grab keys. Bindings that only run `bspc` are executed inside the compositor without spawning a process.

The window tree, its operations and the `bspc` grammar are separate crates with no Wayland dependency, so they are tested without a display.

| Crate | Role |
| --- | --- |
| `bsp-core` | The tree, desktops, monitors, rules, focus history |
| `bsp-ipc` | The `bspc` protocol: parsing, selectors, the executor, reports and events |
| `bsp-hotkeys` | The `sxhkdrc` parser and chord matcher |
| `bsp-compositor` | The compositor (`bspwm-rs`): rendering, input, outputs, protocols, XWayland |
| `bspc-rs` | The command-line client |

## Status

Implemented and tested in a QEMU VM (virtio-gpu, software rendering):

- every `bspc` domain, `subscribe`, `query` and `wm -d`, with `node`, `desktop` and `monitor` selectors, rules, focus history and border colors;
- a DRM/KMS backend with libseat, GBM/EGL, multiple GPUs, hotplug, VT switching, cursor drawing, and `bspc output`/`bspc input`;
- the Wayland protocols bars, launchers, screen capture and lock screens need (layer-shell, foreign-toplevel, ext-workspace, screencopy, session lock, idle, output management, gamma, virtual input, and more);
- XWayland, started on the first X11 client and stopped when unused.

Not done: `_NET_ACTIVE_WINDOW` and `_NET_WM_STATE` requests from X11 clients other than fullscreen are not passed on by Smithay, `bspc wm -l` is refused (clients do not survive a compositor restart; `wm -r` reloads live), and input-method composition is missing. `docs/migrating.md` lists every difference from bspwm.

## Installation

### Arch Linux

`packaging/arch/PKGBUILD` builds a `bspwm-rs-git` package from this checkout (the working tree, so uncommitted changes are included).

1. Install the build tools, if they are not there yet:

   ```
   sudo pacman -S --needed base-devel git rust
   ```

   `rustup` works instead of the `rust` package; then run `rustup default stable` once and add `-d` to the `makepkg` commands below (the package depends on `cargo`, which rustup does not register with pacman).

2. Build, test and install:

   ```
   cd packaging/arch
   makepkg -si
   ```

   `makepkg` runs the PKGBUILD's `build()`, then `check()` (the whole test suite, with the compositor built for the DRM backend that is packaged), then `package()`. A failing test stops the build before anything is packaged. `makepkg -si --nocheck` skips the tests. `-s` installs missing dependencies from the repositories, `-i` installs the package afterwards.

3. Optional programs used by the default session config (waybar, rofi, alacritty, swaybg, kanshi, gammastep, dunst, …) are listed as optional dependencies: `pacman -Qi bspwm-rs-git` shows them. `xorg-xwayland` is needed for X11 applications.

The package installs `bspwm-rs`, `bspc-rs`, the `bspwm-rs-session` launcher (with `bspc` on its `PATH`), `bspwm-rs-setup` and the default configuration it copies into `~/.config` on first run, and a `bspwm-rs` entry in `/usr/share/wayland-sessions`, so greetd, GDM or SDDM offer it as a session.

To update, pull or edit the tree and run `makepkg -si` again; the version is taken from `Cargo.toml` and the git revision. Build files stay in `packaging/arch/src` and `packaging/arch/pkg`; `makepkg -C` starts from a clean build. To remove it: `sudo pacman -R bspwm-rs-git`.

### Other systems

From source:

```
cargo build --release -p bsp-compositor -p bspc-rs --no-default-features --features real
```

The `real` feature is the DRM/KMS backend that runs on a text console. The default `nested` feature runs inside another Wayland compositor and is meant for development.

## Running

From a display manager, choose the *bspwm-rs* session. From a text console (for example Ctrl+Alt+F3), run `bspwm-rs-session`. To try the configuration from a source checkout first, run `contrib/session/install.sh` and then `contrib/session/bspwm-rs-session`.

`Ctrl+Alt+Shift+Escape` quits the compositor and `Ctrl+Alt+F1`–`F12` switch consoles; neither depends on `sxhkdrc`.

## Configuration

The configuration is done through `bspc-rs`, as with bspwm. `~/.config/bspwm/bspwmrc` is executed when the compositor starts (and again on `bspc wm -r`, which reloads live), and hotkeys are read from `~/.config/sxhkd/sxhkdrc`. Example, from `contrib/session/bspwmrc`:

```
bspc monitor -d 1 2 3 4 5 6 7 8 9 10

bspc config border_width 1
bspc config window_gap   10
bspc config focused_border_color '#93a1a1'

bspc rule -a firefox desktop='^3' follow=on
```

Things bspwm did through X11 tools have their own commands:

```
bspc input keyboard -r 66 350       # key repeat, instead of xset r rate
bspc output HDMI-A-1 -m 1920x1080@60 # instead of xrandr
```

The keyboard layout comes from `XKB_DEFAULT_LAYOUT` and `XKB_DEFAULT_OPTIONS`, which `bspwm-rs-session` reads from `~/.config/bspwm-rs/session.env`. `docs/migrating.md` lists everything an existing bspwm setup has to change.

## Documents

| File | Holds |
| --- | --- |
| [docs/design.md](docs/design.md) | Goal, decisions, architecture, performance budget, compatibility with bspwm 0.9.12, roadmap, and the rules the implementation must follow |
| [docs/bsp-core.md](docs/bsp-core.md) | The pure-logic crate: tree, desktops, monitors, rules, focus history |
| [docs/bsp-ipc.md](docs/bsp-ipc.md) | The `bspc`-compatible control socket |
| [docs/bsp-hotkeys.md](docs/bsp-hotkeys.md) | The bundled sxhkd replacement |
| [docs/bsp-compositor.md](docs/bsp-compositor.md) | The Smithay layer: rendering, input, outputs, protocols |
| [docs/migrating.md](docs/migrating.md) | What an existing bspwm setup has to change |
| [CHANGELOG.md](CHANGELOG.md) | Every function added, changed or removed |

## Contributing

Read `docs/design.md` first, especially *Development rules*. Every commit that adds, changes or removes a public function also updates the Public functions table in that crate's file and adds a row to `CHANGELOG.md`. A design decision changes `docs/design.md`.

## Other resources

- [bspwm](https://github.com/baskerville/bspwm), the window manager this reproduces, and its [wiki](https://github.com/baskerville/bspwm/wiki)
- [sxhkd](https://github.com/baskerville/sxhkd), whose `sxhkdrc` format `bsp-hotkeys` reads
- [Smithay](https://github.com/Smithay/smithay), the compositor library
