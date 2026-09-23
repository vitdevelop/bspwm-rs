# Migrating from bspwm to bspwm-rs

Written as features land; this file lists only what an existing bspwm setup has to change.

## Checklist

| Area | On bspwm (X11) | On bspwm-rs (Wayland) | Status |
| --- | --- | --- | --- |
| Hotkeys | sxhkd daemon | Bundled: the same `sxhkdrc` is read by `bspwm-rs` | Hotkeys and config |
| Client | `bspc` | `bspc-rs` (stock `bspc` also works with `BSPWM_SOCKET` set) | IPC |
| Monitors | `xrandr` in `bspwmrc` | `bspc-rs output …`, or kanshi / wlr-randr | Hardware backend |
| Keyboard layout | `setxkbmap` | `bspc-rs input keyboard layout …` | Hardware backend |
| Key repeat | `xset r rate` | `bspc-rs input keyboard repeat …` | Hardware backend |
| Touchpad | `xinput set-prop` | `bspc-rs input touchpad …` | Hardware backend |
| Bar | polybar | waybar (layer-shell) | Protocols |
| Compositor | picom | Not needed; `bspwm-rs` composites | — |
| Restart | `bspc wm -r` | Live reload; a restart kills all clients | Hotkeys and config |
| Window tools | `xdo`, `xprop`, `wmctrl` | XWayland windows only | XWayland |
| Screenshots, sharing | maim, flameshot | xdg-desktop-portal-wlr (grim, slurp) | Protocols |
| Virtual screen | none (Xvfb, a dummy `xrandr` output) | `bspc-rs output --create-headless [WxH@HZ]`, `output HEADLESS-1 --remove`; stream with `wayvnc HEADLESS-1` | Hardware backend |

## Notes

- Keep bspwm on X11 installed and selectable in the display manager until `bspwm-rs` has run for a few weeks without crashes.
- A `bspwmrc` shared between both can guard the Wayland-only commands with a check on `XDG_SESSION_TYPE`.

## Where bspwm-rs differs from bspwm

What still behaves differently once a `bspwmrc` and `sxhkdrc` are in place. Everything not listed here is meant to match bspwm 0.9.12 (report a difference as a bug).

**Settings and rules**

- These settings are accepted and stored, and change nothing (there is nothing on Wayland for them to change): `mapping_events_count`, `remove_disabled_monitors`, `merge_overlapping_monitors`.
- An unplugged output keeps its monitor (desktops and windows) unwired until an output of the same name returns, as in bspwm; `remove_unplugged_monitors true` moves its desktops to the last wired monitor and removes it.
- `external_rules_command` runs asynchronously and the window stays pending until it closes its output, as in bspwm (the script may call `bspc`). It is killed after two seconds and the window is then managed without its answer. A Wayland window's first argument (the window id) is `0`.
- `pointer_follows_focus` and `pointer_follows_monitor` move the pointer after a `bspc` command, a hotkey, a new window or a closed one changed the focus, not after a click (the pointer is already there).
- `focus_follows_pointer` reacts to real pointer motion only, as bspwm's `motion_notify()`: a window appearing under a resting pointer (a desktop switch, a closed window) does not take the focus.
- `ignore_ewmh_focus` covers `xdg_activation_v1` requests; an X11 `_NET_ACTIVE_WINDOW` is not passed on by Smithay, so it is never honoured either way.
- An activation request without a valid recent input serial marks the window urgent instead of focusing it.
- A window's class and instance for `bspc rule` are its `app_id` (both) and title (name) for a native Wayland window.

**Windows and focus**

- `bspc node -k` disconnects a Wayland client (all its windows go, as an X11 client's would) and uses `XKillClient` for an X11 window. The node leaves the tree when the window is destroyed.
- `bspc node -f next` and `prev` walk splits as well as windows, as in bspwm; use `next.window` to skip splits.
- `honor_size_hints` (setting, rule and `config -n`) applies bspwm's `apply_size_hints()`. A Wayland window only has a minimum and a maximum size (xdg `set_min_size`/`set_max_size`); an X11 window has all of `WM_NORMAL_HINTS`.
- `sticky` works as in bspwm (it follows the monitor's shown desktop through focus, `desktop -a`, `-s` and `-m`).
- Stacking, borders and gaps follow bspwm's stack levels and `apply_layout()`; a border frames the rectangle the window is given (after its size hints), not the size a client commits, so a client that draws smaller than it was told shows a gap inside its border.

**Pointer, keyboard, outputs**

- The pointer stays on an output: with outputs of different sizes it is pulled to the nearest one.
- Output names are DRM connector names (`HDMI-A-1`, `eDP-1`); there is no primary output, so the `primary` monitor descriptor matches nothing (bspwm's is the RandR primary).
- Screen capture, virtual input, gamma control and output management are offered only to clients the compositor started itself or that come through the portal (`docs/design.md`, Privileged protocols); an X11 tool or a Flatpak app cannot use them.

**IPC**

- `subscribe` lines carry the `status_prefix` (`W` by default) and follow bspwm's formats; `node_stack`, `node_add` and the other events are sent for changes the compositor makes itself (a click, a new window), not just for `bspc` commands.
- A `bspc` reply is written whole, without blocking the compositor (a reader stuck for 2 s is dropped); a `subscribe` reader that falls more than 1 MiB behind is dropped.
- Each instance has its own control socket: the first binds `$XDG_RUNTIME_DIR/bspwm-rs-socket`, a second one (another VT) `bspwm-rs-$WAYLAND_DISPLAY-socket`, and each exports `BSPWM_SOCKET` to what it starts.
- `wm -l` (load a `wm -d` dump) is refused: bspwm uses it when `wm -r` re-execs the window manager with the same X windows, but a Wayland compositor's clients do not survive a restart, and `wm -r` reloads the configuration live instead. `wm -o` has nothing to adopt for the same reason. `wm -a NAME WxH+X+Y` adds a monitor that no output shows (unwired), which an output of that name shows once it appears.

**X11 windows**

- X11 windows get the EWMH state bspwm writes: `_NET_WM_STATE` (fullscreen, sticky, hidden, above, below, demands attention, plus `_NET_WM_STATE_FOCUSED`), `_NET_WM_DESKTOP`, and the root's `_NET_NUMBER_OF_DESKTOPS`, `_NET_DESKTOP_NAMES` and `_NET_CURRENT_DESKTOP` (desktops numbered across every monitor, as bspwm does).
- The ICCCM urgency hint marks the node urgent. `_NET_WM_STATE` change requests other than fullscreen (sticky, above, below, demands attention) and `_NET_CURRENT_DESKTOP` requests are not passed on by Smithay, so they are not honoured.
- An X11 window on a desktop that is not shown is unmapped in X (as bspwm does), which ends any pointer or keyboard grab it holds.

## A ready-made session config

`contrib/session/` holds a complete config for running `bspwm-rs` from a text console, built from a real bspwm setup (the old `bspwmrc` from a dotfiles history) plus the Wayland extras a Hyprland setup had:

| File | Installed as | Contents |
| --- | --- | --- |
| `bspwmrc` | `~/.config/bspwm/bspwmrc` | desktops 1–10, gaps, monocle options, rules, keyboard repeat, autostart (`swaybg`, `waybar`, `kanshi`, `gammastep`, `dunst`, `cliphist`), each started once so `bspc wm -r` does not duplicate them |
| `waybar/` | `~/.config/bspwm-rs/waybar/` | a copy of the waybar config with `hyprland/workspaces` replaced by `ext/workspaces`; the original waybar config is untouched |
| `bspwm-rs-session` | `~/.local/bin/` | the launcher: puts `bspc` on `PATH`, sets the keyboard layout through `XKB_DEFAULT_LAYOUT`/`XKB_DEFAULT_OPTIONS` (edit or pre-set them; `altwin:swap_alt_win` makes `super` the physical Alt key), logs to `~/.local/state/bspwm-rs.log` |

`sh contrib/session/install.sh` installs them and never overwrites an existing file (`--force` does). Then, from a text console (Ctrl+Alt+F3), run `bspwm-rs-session`. Ctrl+Alt+Shift+Escape quits; Ctrl+Alt+F1–F12 switch consoles. The launcher refuses to start inside a graphical session.

Focus history (`last`, `older`, `newer`, `newest`) and the border colors (`focused_border_color`, `active_border_color`, `normal_border_color`) are implemented. Preselection feedback is drawn (`presel_feedback`, `presel_feedback_color`): an opaque rectangle over the part of the node the next window will take.

## Installing on Arch with greetd

`packaging/arch/PKGBUILD` builds the working tree into a package (`cd packaging/arch && makepkg -si`; add `-d` if `cargo` comes from rustup). It installs `/usr/bin/bspwm-rs`, `bspc-rs`, `bspwm-rs-session` and `bspwm-rs-setup`, the default config in `/usr/share/bspwm-rs/session`, the portal config, and `/usr/share/wayland-sessions/bspwm-rs.desktop`.

greetd greeters that list `/usr/share/wayland-sessions` (regreet, gtkgreet, tuigreet) then offer **bspwm-rs** next to the other sessions. The session runs `systemd-cat -t bspwm-rs bspwm-rs-session`, so the compositor's log is in the journal (`journalctl -t bspwm-rs`). On the first start the launcher runs `bspwm-rs-setup`, which copies `bspwmrc`, `sxhkdrc` and the waybar config into `~/.config` without overwriting anything (`bspwm-rs-setup --force` resets them). Keyboard layout and options can be changed in `~/.config/bspwm-rs/session.env` (`XKB_DEFAULT_LAYOUT=…`, `XKB_DEFAULT_OPTIONS=…`). regreet with `skip_selection = true` starts the last chosen session; pick bspwm-rs once from its session menu.
