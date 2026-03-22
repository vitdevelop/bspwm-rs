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

## Modules

| Module | Holds |
| --- | --- |
| `lexer` | Groups an sxhkdrc file's lines into raw (chain, command) pairs |
| `expand` | `{}`/range expansion of a raw pair into concrete (chain, command) pairs |
| `binding` | Parses an expanded chain string into `Chord`s: modifiers, keysym/button, press/release/replay/lock-chain |
| `token` (private) | The `get_token` splitter `expand` and `binding` both need, on different separators |
| `matcher` (planned) | Chord state machine fed by key events; returns the command to run or `Pass` |

## Hotkeys progress

`lexer`, `expand` and `binding` are done and fully unit-tested against sxhkd 0.6.2's own source (`github.com/baskerville/sxhkd`, `src/parse.c`/`src/types.c`): `load_config()`'s line-grouping state machine (comment/chain/command classification, indentation trimming, `\` line continuation), `process_hotkey()`/`extract_chunks()`/`render_next()`'s `{}`/range expansion, and `parse_chain()`/`parse_modifier()`/`parse_button()`'s chord parsing — reproduced field for field and branch for branch, including easy-to-miss bspwm quirks kept rather than "fixed":

- The *leftmost* `{}` group in a string is the fastest-changing one when several appear together (the reverse of a typical odometer).
- A `\`-continued line's own first character still decides whether it continues a chain or a command, so an indented continuation silently misclassifies.
- `:` (versus `;`) between two chords in a chain sets the *preceding* chord's `lock_chain`, not the next one's.
- A chord name's `~` (replay) and `@` (release) prefixes must appear in that order; `@~w` tries to parse `~w` as a keysym name and fails, it does not parse as a released, replayed `w`.

Chord parsing resolves keysym *names* via `xkbcommon::xkb::keysym_from_name` (sxhkd keeps its own name table, `nks_dict`; this crate's one allowed dependency beyond the standard library is `xkbcommon`, precisely for this). `alt`/`super`/`hyper`/`meta`/`mode_switch` are kept as symbolic `Modifier` variants rather than resolved to a bitmask, since bspwm itself resolves them *dynamically* against the current keyboard mapping (`modfield_from_keysym()`) — there is no fixed bit to hardcode, and doing so needs a live keymap that belongs to `bsp-compositor`'s seat, not this crate.

Not started: the chord/state-machine matcher, pointer bindings, the in-process-`bspc`-vs-`sh -c` execution split above, `SIGUSR1` config reload, and wiring any of it into `bsp-compositor` (`bspwmrc`/sxhkdrc are not read at startup yet, `docs/bsp-compositor.md`'s Nested compositor progress).

## Public functions

| Function | Signature | Behavior |
| --- | --- | --- |
| `lexer::parse` | `fn(&str) -> Vec<RawBinding>` | Groups a whole sxhkdrc file's lines into raw (chain, command) pairs, before expansion |
| `expand::expand` | `fn(&str, &str) -> Vec<ExpandedBinding>` | Expands every `{}`/range sequence in one raw pair, zipping the chain and command sides index for index |
| `binding::parse_chain` | `fn(&str) -> Result<Vec<Chord>, UnknownName>` | Parses an expanded chain string into its chords: modifiers, keysym/button, press/release/replay/lock-chain |
