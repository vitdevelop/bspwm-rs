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
| `matcher` | Chord-chain state machine fed by key/button events; reports `Pass`/`Continue`/`Fire { index }` |

## Hotkeys progress

`lexer`, `expand`, `binding` and `matcher` are done and fully unit-tested against sxhkd 0.6.2's own source (`github.com/baskerville/sxhkd`, `src/parse.c`/`src/types.c`): `load_config()`'s line-grouping state machine (comment/chain/command classification, indentation trimming, `\` line continuation), `process_hotkey()`/`extract_chunks()`/`render_next()`'s `{}`/range expansion, `parse_chain()`/`parse_modifier()`/`parse_button()`'s chord parsing, and `find_hotkey()`/`match_chord()`'s chain-matching state machine — reproduced field for field and branch for branch, including easy-to-miss bspwm quirks kept rather than "fixed":

- The *leftmost* `{}` group in a string is the fastest-changing one when several appear together (the reverse of a typical odometer).
- A `\`-continued line's own first character still decides whether it continues a chain or a command, so an indented continuation silently misclassifies.
- `:` (versus `;`) between two chords in a chain sets the *preceding* chord's `lock_chain`, not the next one's.
- A chord name's `~` (replay) and `@` (release) prefixes must appear in that order; `@~w` tries to parse `~w` as a keysym name and fails, it does not parse as a released, replayed `w`.
- A chord's modifiers must match the held set *exactly* — holding one extra, unrelated modifier breaks an otherwise-matching chord; `any` is a wildcard only when it is a chord's entire modifier list.
- When an event breaks an in-progress chain, that same event is retried immediately against every hotkey from scratch (not just swallowed) — if it happens to match some other, unrelated single-chord hotkey that was only being ignored because a chain was in progress, that hotkey fires on this same keypress.

Chord parsing resolves keysym *names* via `xkbcommon::xkb::keysym_from_name` (sxhkd keeps its own name table, `nks_dict`; this crate's one allowed dependency beyond the standard library is `xkbcommon`, precisely for this). `alt`/`super`/`hyper`/`meta`/`mode_switch` are kept as symbolic `Modifier` variants rather than resolved to a bitmask, since bspwm itself resolves them *dynamically* against the current keyboard mapping (`modfield_from_keysym()`) — there is no fixed bit to hardcode, and doing so needs a live keymap that belongs to `bsp-compositor`'s seat, not this crate. `matcher` is kept free of X11-specific side effects for the same reason: no passive key grabs (Wayland has none), no `alarm()`-driven chain timeout (a timer is `bsp-compositor`'s event loop's job), no `status_fifo` reporting, and no configurable "abort this chain" key yet.

Not started: pointer bindings, the in-process-`bspc`-vs-`sh -c` execution split above, `SIGUSR1` config reload, and wiring any of it into `bsp-compositor` (`bspwmrc`/sxhkdrc are not read at startup yet, `docs/bsp-compositor.md`'s Nested compositor progress).

## Public functions

| Function | Signature | Behavior |
| --- | --- | --- |
| `lexer::parse` | `fn(&str) -> Vec<RawBinding>` | Groups a whole sxhkdrc file's lines into raw (chain, command) pairs, before expansion |
| `expand::expand` | `fn(&str, &str) -> Vec<ExpandedBinding>` | Expands every `{}`/range sequence in one raw pair, zipping the chain and command sides index for index |
| `binding::parse_chain` | `fn(&str) -> Result<Vec<Chord>, UnknownName>` | Parses an expanded chain string into its chords: modifiers, keysym/button, press/release/replay/lock-chain |
| `Matcher::new` | `fn(Vec<Vec<Chord>>) -> Matcher` | Builds a matcher over every configured chain |
| `Matcher::feed` | `fn(&mut self, &KeyEvent) -> Outcome` | Advances every chain's progress against one input event; reports `Pass`/`Continue`/`Fire { index }` |
| `Matcher::abort_chain` | `fn(&mut self)` | Resets every chain to its head and leaves chained/locked mode; the caller drives this from a timeout timer |
