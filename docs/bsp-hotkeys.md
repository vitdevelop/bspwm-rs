# bsp-hotkeys

The bundled replacement for sxhkd: it reads your existing `sxhkdrc` and matches key presses inside the compositor, since Wayland clients cannot grab keys globally.

## Scope

- sxhkdrc syntax: modifiers, keysyms, `{a,b,c}` expansion, `{1-9}` ranges, `_` for an empty choice, comments, line continuation.
- Chords (`super + w ; h`), key release bindings (`@`), and replay (`~`).
- Pointer bindings for moving and resizing floating windows, as bspwm's `pointer_action` settings do.
- Commands run through `sh -c`, like sxhkd; commands that are plain `bspc` calls go straight to `bsp-core`, skipping the process spawn.
- Config reload on `SIGUSR1`, matching sxhkd.

## Binding execution

Decision: a binding that is one `bspc` call with only literal arguments runs in-process; everything else runs through `sh -c`, as sxhkd does.

| Command shape | Path | Example |
| --- | --- | --- |
| Single `bspc` call, literal arguments | Parsed once at load, sent straight to `bsp-core` | `bspc node -f west` |
| Any shell feature: `$VAR`, `$(…)`, pipes, `&&`, `\|\|`, `;`, redirects | `sh -c`, spawned | `bspc node -f west \|\| bspc monitor -f west` |
| Anything that is not `bspc` | `sh -c`, spawned | `alacritty` |

- Why: skips spawning `sh` and `bspc` on every press of frequent keys, and applies commands in key-press order with no process races.
- Equivalence: the in-process path must produce the same state change, events and `subscribe` output as the socket path; a test runs each binding both ways and compares.
- Off switch: `bspc config hotkeys_inline_bspc false` forces every binding through the shell. Default is `true`.

## Modules (planned)

| Module | Holds |
| --- | --- |
| `lexer` | Tokens of an sxhkdrc file |
| `expand` | Brace and range expansion into concrete bindings |
| `binding` | `Binding { chord: Vec<Keystroke>, command, on_release, replay }` |
| `matcher` | Chord state machine fed by key events; returns the command to run or `Pass` |

## Public functions

| Function | Signature | Behavior |
| --- | --- | --- |
| — | — | Filled from hotkeys |
