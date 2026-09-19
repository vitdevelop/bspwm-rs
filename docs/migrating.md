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

## Notes

- Keep bspwm on X11 installed and selectable in the display manager until `bspwm-rs` has run for a few weeks without crashes.
- A `bspwmrc` shared between both can guard the Wayland-only commands with a check on `XDG_SESSION_TYPE`.

## A ready-made session config

`contrib/session/` holds a complete config for running `bspwm-rs` from a text console, built from a real bspwm setup (the old `bspwmrc` from a dotfiles history) plus the Wayland extras a Hyprland setup had:

| File | Installed as | Contents |
| --- | --- | --- |
| `bspwmrc` | `~/.config/bspwm/bspwmrc` | desktops 1–10, gaps, monocle options, rules, keyboard repeat, autostart (`swaybg`, `waybar`, `kanshi`, `gammastep`, `dunst`, `cliphist`), each started once so `bspc wm -r` does not duplicate them |
| `waybar/` | `~/.config/bspwm-rs/waybar/` | a copy of the waybar config with `hyprland/workspaces` replaced by `ext/workspaces`; the original waybar config is untouched |
| `bspwm-rs-session` | `~/.local/bin/` | the launcher: puts `bspc` on `PATH`, sets the keyboard layout through `XKB_DEFAULT_LAYOUT`/`XKB_DEFAULT_OPTIONS` (edit or pre-set them; `altwin:swap_alt_win` makes `super` the physical Alt key), logs to `~/.local/state/bspwm-rs.log` |

`sh contrib/session/install.sh` installs them and never overwrites an existing file (`--force` does). Then, from a text console (Ctrl+Alt+F3), run `bspwm-rs-session`. Ctrl+Alt+Shift+Escape quits; Ctrl+Alt+F1–F12 switch consoles. The launcher refuses to start inside a graphical session.

Focus history (`last`, `older`, `newer`, `newest`) and the border colors (`focused_border_color`, `active_border_color`, `normal_border_color`) are implemented. `presel_feedback_color` is accepted but preselection feedback is not drawn.

## Installing on Arch with greetd

`packaging/arch/PKGBUILD` builds the working tree into a package (`cd packaging/arch && makepkg -si`; add `-d` if `cargo` comes from rustup). It installs `/usr/bin/bspwm-rs`, `bspc-rs`, `bspwm-rs-session` and `bspwm-rs-setup`, the default config in `/usr/share/bspwm-rs/session`, the portal config, and `/usr/share/wayland-sessions/bspwm-rs.desktop`.

greetd greeters that list `/usr/share/wayland-sessions` (regreet, gtkgreet, tuigreet) then offer **bspwm-rs** next to the other sessions. The session runs `systemd-cat -t bspwm-rs bspwm-rs-session`, so the compositor's log is in the journal (`journalctl -t bspwm-rs`). On the first start the launcher runs `bspwm-rs-setup`, which copies `bspwmrc`, `sxhkdrc` and the waybar config into `~/.config` without overwriting anything (`bspwm-rs-setup --force` resets them). Keyboard layout and options can be changed in `~/.config/bspwm-rs/session.env` (`XKB_DEFAULT_LAYOUT=…`, `XKB_DEFAULT_OPTIONS=…`). regreet with `skip_selection = true` starts the last chosen session; pick bspwm-rs once from its session menu.
