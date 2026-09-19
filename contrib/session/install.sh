#!/bin/sh
# Installs the session config into ~/.config. Never overwrites an existing file
# unless --force is given. --no-launcher skips ~/.local/bin/bspwm-rs-session.
#
# Run from a source checkout it also installs the launcher into ~/.local/bin;
# the Arch package ships this script as `bspwm-rs-setup` next to the configs in
# /usr/share/bspwm-rs/session and the launcher in /usr/bin.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
SRC=$HERE
[ -d "$SRC/waybar" ] || SRC=/usr/share/bspwm-rs/session
REPO=$(cd "$HERE/../.." 2>/dev/null && pwd || echo /usr)
CFG="${XDG_CONFIG_HOME:-$HOME/.config}"
force=0; launcher=1
for a in "$@"; do
	case "$a" in
	--force) force=1 ;;
	--no-launcher) launcher=0 ;;
	esac
done

put() { # put <mode> <src> <dst>
	if [ -e "$3" ] && [ "$force" = 0 ]; then echo "exists, kept: $3"; return; fi
	mkdir -p "$(dirname "$3")"
	sed "s|@REPO@|$REPO|g" "$2" > "$3"; chmod "$1" "$3"; echo "installed:   $3"
}
put 755 "$SRC/bspwmrc"                "$CFG/bspwm/bspwmrc"
put 644 "$SRC/waybar/config.jsonc"    "$CFG/bspwm-rs/waybar/config.jsonc"
put 644 "$SRC/waybar/style.css"       "$CFG/bspwm-rs/waybar/style.css"
# only from a source checkout: a packaged install has the launcher in /usr/bin
[ "$launcher" = 1 ] && [ -f "$SRC/bspwm-rs-session" ] && put 755 "$SRC/bspwm-rs-session" "$HOME/.local/bin/bspwm-rs-session"
# ~/.config/sxhkd/sxhkdrc (the hotkeys) and rofi are the user's own files
# the nvidia module in the waybar config runs this script
[ -e "$CFG/waybar/nvidia-data.sh" ] || echo "note: waybar's custom/nvidia expects $CFG/waybar/nvidia-data.sh"
exit 0
