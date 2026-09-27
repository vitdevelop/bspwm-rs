#!/bin/sh
# Records the real bspwm's transcripts for bspwm-rs's golden tests
# (crates/bsp-ipc/tests/golden.rs). Needs bspwm, Xvfb and xprop; run it in the
# QEMU test VM, never on a desktop session:
#
#   record.sh GOLDEN_RECORD SCENARIO_DIR OUT_DIR
#
# GOLDEN_RECORD is `cargo build -p bsp-compositor --example golden_record`'s
# binary. Each scenario gets a fresh Xvfb (1920x1080) and a bspwm without a
# bspwmrc, so every transcript starts from bspwm's own initial state.
set -eu
record=$1
scenarios=$2
out=$3
export DISPLAY=:7
for scenario in "$scenarios"/*.scn; do
	name=$(basename "$scenario" .scn)
	Xvfb "$DISPLAY" -screen 0 1920x1080x24 -nolisten tcp >/dev/null 2>&1 &
	xvfb=$!
	for _ in $(seq 100); do xprop -root >/dev/null 2>&1 && break; sleep 0.1; done
	bspwm -c /dev/null >/dev/null 2>&1 &
	wm=$!
	for _ in $(seq 100); do bspc query -M >/dev/null 2>&1 && break; sleep 0.1; done
	"$record" "$scenario" > "$out/$name.out"
	echo "recorded $name ($(wc -l < "$out/$name.out") lines)"
	kill "$wm" 2>/dev/null || true
	wait "$wm" 2>/dev/null || true
	kill "$xvfb" 2>/dev/null || true
	wait "$xvfb" 2>/dev/null || true
done
