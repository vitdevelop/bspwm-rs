# bsp-hotkeys

The bundled replacement for sxhkd: it reads your existing `sxhkdrc` and matches key presses inside the compositor, since Wayland clients cannot grab keys globally.

## Scope

- sxhkdrc syntax: modifiers, keysyms, `{a,b,c}` expansion, `{1-9}` ranges, `_` for an empty choice, comments, line continuation.
- Chords (`super + w ; h`), key release bindings (`@`), and replay (`~`).
- Pointer bindings for moving and resizing floating windows, as bspwm's `pointer_action` settings do.
- Commands run through `sh -c`, like sxhkd; commands that are plain `bspc` calls go straight to `bsp-ipc`'s command parser, skipping the process spawn.
- Config reload on `SIGUSR1`, matching sxhkd.

## Binding execution

Decision: a binding that is one `bspc` call with only literal arguments runs in-process; everything else runs through `sh -c`, as sxhkd does. `dispatch::classify` (below) implements the classification; actually running the in-process path is left to `bsp-compositor` (`bsp-ipc::command::parse` then `exec::execute`, not `bsp-core` directly — `bsp-core` has no command grammar of its own, that's `bsp-ipc`'s job).

| Command shape | Path | Example |
| --- | --- | --- |
| Single `bspc` call, literal arguments | Parsed once at load, sent straight to `bsp-ipc::command::parse` | `bspc node -f west` |
| Any shell feature: `$VAR`, `$(…)`, pipes, `&&`, `\|\|`, `;`, redirects | `sh -c`, spawned | `bspc node -f west \|\| bspc monitor -f west` |
| Anything that is not `bspc` | `sh -c`, spawned | `alacritty` |

- Why: skips spawning `sh` and `bspc` on every press of frequent keys, and applies commands in key-press order with no process races.
- Equivalence: the in-process path must produce the same state change, events and `subscribe` output as the socket path; a test runs each binding both ways and compares. Not implemented yet — needs a real `bsp-compositor` integration to compare against, not just this crate's classification.
- Off switch: `bspc config hotkeys_inline_bspc false` forces every binding through the shell. Default is `true`. Implemented and live-verified in `bsp-compositor` (`docs/bsp-compositor.md` Hotkeys and config progress) — `dispatch::classify` itself has no such switch (it has no access to `bsp-core::Settings`) and stays unaware of it by design; honoring it is `bsp-compositor`'s job, checking the setting before acting on a `Dispatch::InlineBspc` result. `LoadedHotkey::command` (`config::load`'s output type) keeps the binding's expanded command text verbatim alongside `dispatch` for exactly this: a caller forcing the shell path for an otherwise-`InlineBspc` binding needs the *original* text, not a re-join of `dispatch`'s already-split tokens (lossy for a token that itself contained a literal quote character).

## Modules

| Module | Holds |
| --- | --- |
| `lexer` | Groups an sxhkdrc file's lines into raw (chain, command) pairs |
| `expand` | `{}`/range expansion of a raw pair into concrete (chain, command) pairs |
| `binding` | Parses an expanded chain string into `Chord`s: modifiers, keysym/button, press/release/replay/lock-chain |
| `token` (private) | The `get_token` splitter `expand` and `binding` both need, on different separators |
| `matcher` | Chord-chain state machine fed by key/button events; reports `Pass`/`Continue`/`Fire { index }` |
| `dispatch` | Classifies a command as an in-process `bspc` call or a shell command (`Dispatch::InlineBspc`/`Shell`) |
| `config` | Loads a whole sxhkdrc file in one call (`config::load`), plus its path resolution (`config::resolve_path`) |

## Hotkeys and config progress

`lexer`, `expand`, `binding` and `matcher` are done and fully unit-tested against sxhkd 0.6.2's own source (`github.com/baskerville/sxhkd`, `src/parse.c`/`src/types.c`): `load_config()`'s line-grouping state machine (comment/chain/command classification, indentation trimming, `\` line continuation), `process_hotkey()`/`extract_chunks()`/`render_next()`'s `{}`/range expansion, `parse_chain()`/`parse_modifier()`/`parse_button()`'s chord parsing, and `find_hotkey()`/`match_chord()`'s chain-matching state machine — reproduced field for field and branch for branch, including easy-to-miss bspwm quirks kept rather than "fixed":

- The *leftmost* `{}` group in a string is the fastest-changing one when several appear together (the reverse of a typical odometer).
- A `\`-continued line's own first character still decides whether it continues a chain or a command, so an indented continuation silently misclassifies.
- `:` (versus `;`) between two chords in a chain sets the *preceding* chord's `lock_chain`, not the next one's.
- A chord name's `~` (replay) and `@` (release) prefixes must appear in that order; `@~w` tries to parse `~w` as a keysym name and fails, it does not parse as a released, replayed `w`.
- A chord's modifiers must match the held set *exactly* — holding one extra, unrelated modifier breaks an otherwise-matching chord; `any` is a wildcard only when it is a chord's entire modifier list.
- When an event breaks an in-progress chain, that same event is retried immediately against every hotkey from scratch (not just swallowed) — if it happens to match some other, unrelated single-chord hotkey that was only being ignored because a chain was in progress, that hotkey fires on this same keypress.

Chord parsing resolves keysym *names* via `xkbcommon::xkb::keysym_from_name` (sxhkd keeps its own name table, `nks_dict`; this crate's one allowed dependency beyond the standard library is `xkbcommon`, precisely for this). `alt`/`super`/`hyper`/`meta`/`mode_switch` are kept as symbolic `Modifier` variants rather than resolved to a bitmask, since bspwm itself resolves them *dynamically* against the current keyboard mapping (`modfield_from_keysym()`) — there is no fixed bit to hardcode, and doing so needs a live keymap that belongs to `bsp-compositor`'s seat, not this crate. `matcher` is kept free of X11-specific side effects for the same reason: no passive key grabs (Wayland has none), no `alarm()`-driven chain timeout (a timer is `bsp-compositor`'s event loop's job), no `status_fifo` reporting, and no configurable "abort this chain" key yet.

`Modifier::canonical` (`binding.rs`) collapses `Alt`/`Super`/`ModeSwitch` onto the real `Mod1`/`Mod4`/`Mod5` bit they name on essentially every keymap in practical use — the same real-bit identity `alt`/`super`/`mode_switch` and `mod1`/`mod4`/`mod5` have on X11 itself (sxhkd's own `modfield_from_keysym(Alt_L)`/`(Super_L)` and its hardcoded `mode_switch` → `XCB_MOD_MASK_5`, `src/parse.c` `parse_modifier()`). `matcher::modifiers_match` runs both a chord's modifiers and the live held set through it before comparing, so a chord written `mod1 + x` matches a held set `bsp-compositor` resolved as `alt` (or vice versa) — without it, the exact-set match below would treat them as unrelated modifiers. `hyper`/`meta` are *not* covered by `canonical`, since which real bit (if any) they alias to is genuinely keymap-dependent, not a fixed constant; resolving them is `bsp-compositor::hotkeys::canonicalize_virtual_modifiers`'s job instead (`docs/bsp-compositor.md` Hotkeys and config progress), rewriting a loaded chord's `Hyper`/`Meta` modifiers to whichever real bit the live keymap backs them with, once, when sxhkdrc is loaded.

`dispatch::classify` is this project's own extension, not a ported bspwm behavior (sxhkd always spawns `sh -c`, no exceptions) — see "Binding execution" above for its rules. It only classifies and tokenizes; it stays free of `bsp-ipc` on purpose, so actually dispatching a `Dispatch::InlineBspc` (or honoring `hotkeys_inline_bspc`) is left to `bsp-compositor`. `config::load` ties `lexer`/`expand`/`binding`/`dispatch` together into one call over a whole file; `config::resolve_path` matches sxhkd's own `$XDG_CONFIG_HOME/sxhkd/sxhkdrc` (else `$HOME/.config/sxhkd/sxhkdrc`) lookup (`src/sxhkd.c` `main()`).

**Wired into `bsp-compositor`** (`crate::hotkeys`, `docs/bsp-compositor.md` Nested compositor / Hotkeys progress) and live-verified: sxhkdrc is read at startup, every keyboard event is matched, and a completed chain dispatches either inline or via a spawned shell, confirmed against a running compositor and a real client for all three paths — `Dispatch::Shell`, `Dispatch::InlineBspc`, and a chord using an explicit `shift` modifier. That live test caught two real bugs, both fixed:

- `dispatch::classify`'s `InlineBspc` tokens include the literal leading word `bspc` (it is a tokenized command line); `bsp_ipc::command::parse` expects only the arguments after that, the same way the real `bspc` binary strips its own `argv[0]`. Fixed in `bsp-compositor`, not here — `dispatch::classify` itself is unchanged and correct, this was purely a call-site mismatch.
- Matching a chord's keysym against Smithay's shift-*resolved* symbol (`KeysymHandle::modified_sym()`) made any chord with an explicit `shift` modifier unmatchable: holding Shift turns the incoming symbol from `a` into `A`, which nothing parsed from the sxhkdrc text `a` would ever equal. Fixed by matching on `KeysymHandle::raw_syms()`'s level-0 symbol instead (Smithay's equivalent of bspwm's `parse_event()` always reading column 0), letting the live modifier state and the fixed base symbol act as two independent conditions — exactly `match_chord()`'s own design. Neither of these was a `bsp-hotkeys` bug: this crate's own 64 unit tests, including several exercising `shift`-modified chords with real `xkbcommon` keysym resolution, all passed throughout — the mismatch only existed in how `bsp-compositor` fed it live keyboard state.

**`SIGUSR1` config reload is wired in and live-verified** (`hotkeys::reload`, `docs/bsp-compositor.md` Hotkeys and config progress): re-reading sxhkdrc from scratch and rebuilding the matcher, confirmed by swapping sxhkdrc mid-run and checking the old binding stopped firing while the new one worked. `bspwmrc` is also read and run at startup now (`crate::bspwmrc`, same doc).

**`bspc config hotkeys_inline_bspc` is wired in and live-verified** (`ipc::try_hotkeys_inline_bspc`, `docs/bsp-compositor.md` Hotkeys and config progress): a compositor-local `bool` on `State`, since `bsp-core::Settings`' own module doc comment reserves this kind of compositor-only, non-tree-engine setting for `bsp-compositor` (`docs/bsp-core.md`). Confirmed live over the socket (default `true`, set/get round-trips, an invalid value rejected) and via real key presses in both states: with the flag `true`, a hotkey bound to `bspc config border_width 7` changed `border_width` in-process, no shell spawned; with the flag set `false`, the identical key press left `border_width` unchanged, since forcing the binding through `sh -c "bspc config border_width 7"` fails silently (no real `bspc` binary on `PATH` in this environment) — the expected, distinguishing behavior of the off switch.

**Pointer bindings (`bspc config pointer_modifier`/`pointer_action1..3`/`click_to_focus`) are wired in and live-verified** — entirely in `bsp-compositor` (`crate::pointer_action`, `docs/bsp-compositor.md` Hotkeys and config progress), not this crate: bspwm's `pointer_action` system is X11 passive-grab-driven, not an sxhkdrc concept, so it doesn't touch `bsp-hotkeys`' chord grammar or matcher at all. `binding::Key::Button`/`binding::parse_button` remain genuinely unwired — an sxhkdrc line binding a command to a bare pointer button press (sxhkd's own, separate button-chord feature) still does nothing.

## Public functions

| Function | Signature | Behavior |
| --- | --- | --- |
| `lexer::parse` | `fn(&str) -> Vec<RawBinding>` | Groups a whole sxhkdrc file's lines into raw (chain, command) pairs, before expansion |
| `expand::expand` | `fn(&str, &str) -> Vec<ExpandedBinding>` | Expands every `{}`/range sequence in one raw pair, zipping the chain and command sides index for index |
| `binding::parse_chain` | `fn(&str) -> Result<Vec<Chord>, UnknownName>` | Parses an expanded chain string into its chords: modifiers, keysym/button, press/release/replay/lock-chain |
| `Modifier::canonical` | `fn(self) -> Modifier` | Collapses `Alt`/`Super`/`ModeSwitch` onto the real `Mod1`/`Mod4`/`Mod5` bit they name, for comparing a chord's modifiers against a live held set resolved either way |
| `Matcher::new` | `fn(Vec<Vec<Chord>>) -> Matcher` | Builds a matcher over every configured chain |
| `Matcher::feed` | `fn(&mut self, &KeyEvent) -> Outcome` | Advances every chain's progress against one input event; reports `Pass`/`Continue`/`Fire { index }` |
| `Matcher::abort_chain` | `fn(&mut self)` | Resets every chain to its head and leaves chained/locked mode; the caller drives this from a timeout timer |
| `dispatch::classify` | `fn(&str) -> Dispatch` | Classifies a command as an in-process `bspc` call (tokenized) or a shell command |
| `config::load` | `fn(&str) -> Vec<LoadedHotkey>` | Loads every hotkey in a whole sxhkdrc file's contents |
| `config::resolve_path` | `fn(Option<&str>, Option<&str>) -> Option<PathBuf>` | Resolves the sxhkdrc path from `XDG_CONFIG_HOME`/`HOME` |

## Fuzzing

`tests/fuzz.rs` (proptest) feeds sxhkdrc-shaped random text (brace groups, ranges, escapes, NULs, unicode) to `lexer::parse`, `expand::expand`, `config::load`, `dispatch::classify` and `binding::parse_chain`, asserting no panics. It found one: a NUL in a key name panicked inside `xkbcommon`, now an `UnknownName` error. A single huge range such as `{a-\u{10ffff}}` paired with a constant other side expands to about a million bindings, exactly as sxhkd would.
