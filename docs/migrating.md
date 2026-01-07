# Migrating from bspwm to bspwm-rs

Written as steps land; this file lists only what an existing bspwm setup has to change.

## Checklist

| Area | On bspwm (X11) | On bspwm-rs (Wayland) | Status |
| --- | --- | --- | --- |
| Hotkeys | sxhkd daemon | Bundled: the same `sxhkdrc` is read by `bspwm-rs` | Hotkeys |
| Client | `bspc` | `bspc-rs` (stock `bspc` also works with `BSPWM_SOCKET` set) | The IPC |
| Monitors | `xrandr` in `bspwmrc` | `bspc-rs output …`, or kanshi / wlr-randr | The hardware backend |
| Keyboard layout | `setxkbmap` | `bspc-rs input keyboard layout …` | The hardware backend |
| Key repeat | `xset r rate` | `bspc-rs input keyboard repeat …` | The hardware backend |
| Touchpad | `xinput set-prop` | `bspc-rs input touchpad …` | The hardware backend |
| Bar | polybar | waybar (layer-shell) | The protocols |
| Compositor | picom | Not needed; `bspwm-rs` composites | — |
| Restart | `bspc wm -r` | Live reload; a restart kills all clients | Hotkeys |
| Window tools | `xdo`, `xprop`, `wmctrl` | XWayland windows only | XWayland |
| Screenshots, sharing | maim, flameshot | xdg-desktop-portal-wlr (grim, slurp) | The protocols |

## Notes

- Keep bspwm on X11 installed and selectable in the display manager until `bspwm-rs` has run for a few weeks without crashes.
- A `bspwmrc` shared between both can guard the Wayland-only commands with a check on `XDG_SESSION_TYPE`.
