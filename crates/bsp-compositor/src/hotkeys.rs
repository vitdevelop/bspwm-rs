//! Loads sxhkdrc at startup and drives `bsp-hotkeys`' chord matcher from
//! real keyboard events, dispatching a completed chain either straight
//! into `bsp-ipc`'s executor (`bsp_hotkeys::dispatch::Dispatch::InlineBspc`)
//! or as a spawned `sh -c` (`Dispatch::Shell`) — `docs/bsp-hotkeys.md`'s
//! "Binding execution".
//!
//! bspwm's sxhkd is a separate process reading X11 key-grab events; here
//! it is this module, reading every keyboard event Smithay's seat
//! delivers (`crate::input::process_input_event`) before deciding
//! whether to forward it to the focused client.

use std::collections::HashSet;

use smithay::input::keyboard::{FilterResult, KeysymHandle, ModifiersState};

use bsp_hotkeys::binding::{Key, Modifier};
use bsp_hotkeys::config::LoadedHotkey;
use bsp_hotkeys::dispatch::Dispatch;
use bsp_hotkeys::matcher::{KeyEvent, Outcome};

use crate::state::State;

/// Reads and parses sxhkdrc (`$XDG_CONFIG_HOME/sxhkd/sxhkdrc`, or
/// `$HOME/.config/sxhkd/sxhkdrc`), returning every hotkey it names, in
/// file order. Returns an empty `Vec` (not an error) if the path can't
/// be resolved or the file doesn't exist — bspwm-rs still runs with no
/// hotkeys configured, same as sxhkd would with a missing config (it
/// just never matches anything).
///
/// bspwm's sxhkd: `src/sxhkd.c` `main()`'s config-path resolution plus
/// `load_config()`.
#[must_use]
pub fn init() -> Vec<LoadedHotkey> {
    let xdg_config_home = std::env::var("XDG_CONFIG_HOME").ok();
    let home = std::env::var("HOME").ok();
    let Some(path) = bsp_hotkeys::config::resolve_path(xdg_config_home.as_deref(), home.as_deref())
    else {
        tracing::warn!("no XDG_CONFIG_HOME/HOME: sxhkdrc disabled");
        return Vec::new();
    };
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(err) => {
            tracing::info!(path = %path.display(), "no sxhkdrc loaded: {err}");
            return Vec::new();
        }
    };
    let hotkeys = bsp_hotkeys::config::load(&contents);
    tracing::info!(path = %path.display(), count = hotkeys.len(), "loaded sxhkdrc");
    hotkeys
}

/// Re-reads sxhkdrc from scratch and replaces `state.hotkeys`/
/// `state.hotkey_matcher` outright — not merged with the old set.
///
/// bspwm's sxhkd: `SIGUSR1` (`src/sxhkd.c` `hold()` sets `reload`,
/// consumed by the main loop's `reload_cmd()`: `cleanup()` (frees every
/// existing hotkey) then `load_config()` again). Rebuilding a fresh
/// `Matcher` here has the same "old hotkeys are gone" effect and also
/// resets `chained`/`locked` mode, which sxhkd's own `cleanup()` leaves
/// dangling if a chain happened to be mid-progress at reload time — a
/// safe improvement, not a deviation worth its own `docs/design.md`
/// row (internal state, not observable protocol behavior).
pub fn reload(state: &mut State) {
    state.hotkeys = init();
    state.hotkey_matcher = build_matcher(&state.hotkeys);
}

/// Builds a fresh [`bsp_hotkeys::matcher::Matcher`] over `hotkeys`'
/// chains, in order — shared by `State::new` and [`reload`] so both
/// build it the same way.
#[must_use]
pub fn build_matcher(hotkeys: &[LoadedHotkey]) -> bsp_hotkeys::matcher::Matcher {
    bsp_hotkeys::matcher::Matcher::new(hotkeys.iter().map(|h| h.chords.clone()).collect())
}

/// A `KeyboardHandle::input` filter: feeds `bsp-hotkeys`' matcher and
/// either intercepts the event (a chain advanced, completed, or the
/// matcher is otherwise handling it) or forwards it to the focused
/// client as usual.
///
/// bspwm's sxhkd: matches `filter`'s role in Smithay's own `input()` to
/// `find_hotkey()`'s call site in `src/sxhkd.c`'s X11 event loop.
///
/// Matches on `keysym.raw_syms()` — the key's level-0 (unshifted) symbol
/// for the active layout — not `modified_sym()` (which would fold the
/// currently-held Shift/AltGr into the symbol itself). This is
/// deliberate, not an oversight: bspwm's sxhkd always reads column 0
/// too (`parse_event()`: `xcb_key_symbols_get_keysym(symbols, keycode,
/// 0)`), letting `match_chord()` compare that fixed symbol *and* the
/// live modifier state as two independent conditions — exactly what
/// `mods` (below) is for. Matching on the shift-resolved symbol instead
/// would make a config line like `super + shift + a` unmatchable: the
/// held Shift would turn the incoming symbol into `A`, which nothing
/// parsed from the literal chord text `a` would ever equal (found live,
/// testing this very binding).
pub fn filter(
    state: &mut State,
    mods: &ModifiersState,
    keysym: KeysymHandle<'_>,
    pressed: bool,
) -> FilterResult<()> {
    let sym = keysym
        .raw_syms()
        .first()
        .copied()
        .unwrap_or(xkbcommon::xkb::Keysym::NoSymbol);
    if sym == xkbcommon::xkb::Keysym::NoSymbol {
        return FilterResult::Forward;
    }
    let event = KeyEvent {
        key: Key::Keysym(sym),
        modifiers: resolve_modifiers(mods),
        pressed,
    };
    match state.hotkey_matcher.feed(&event) {
        Outcome::Pass => FilterResult::Forward,
        Outcome::Continue => FilterResult::Intercept(()),
        Outcome::Fire { index } => {
            run(state, index);
            FilterResult::Intercept(())
        }
    }
}

/// Maps Smithay's `ModifiersState` (the seat's live, resolved keyboard
/// modifier state) to the symbolic set `bsp-hotkeys`' `Chord`s compare
/// against.
///
/// Not implemented: `Modifier::Hyper`/`Meta`/`ModeSwitch`/`Mod1..Mod5`
/// (beyond what `ctrl`/`alt`/`shift`/`super`/`lock` already cover) —
/// `ModifiersState` only exposes the modifiers listed below, plus
/// `num_lock` and `iso_level3_shift`/`iso_level5_shift`, which have no
/// `bsp_hotkeys::binding::Modifier` counterpart yet either. A chord
/// naming one of those never matches, the same as any other unmatched
/// chord — not a crash, just a hotkey that silently never fires
/// (`docs/bsp-hotkeys.md`'s own note on `Modifier` staying symbolic:
/// resolving them needs the live keymap this function is the one place
/// that has).
fn resolve_modifiers(mods: &ModifiersState) -> HashSet<Modifier> {
    let mut set = HashSet::new();
    if mods.shift {
        set.insert(Modifier::Shift);
    }
    if mods.ctrl {
        set.insert(Modifier::Control);
    }
    if mods.alt {
        set.insert(Modifier::Alt);
    }
    if mods.logo {
        set.insert(Modifier::Super);
    }
    if mods.caps_lock {
        set.insert(Modifier::Lock);
    }
    set
}

fn run(state: &mut State, index: usize) {
    let Some(hotkey) = state.hotkeys.get(index) else {
        return;
    };
    // `bspc config hotkeys_inline_bspc false` (`docs/bsp-hotkeys.md`'s
    // "Binding execution" off switch): forces every binding through a
    // shell, even one `dispatch::classify` judged eligible for the
    // in-process fast path — `hotkey.command` (kept verbatim alongside
    // `dispatch` for exactly this) is what gets run instead of
    // re-joining `InlineBspc`'s already-split tokens.
    if !state.hotkeys_inline_bspc {
        run_shell(&hotkey.command);
        return;
    }
    match hotkey.dispatch.clone() {
        Dispatch::InlineBspc(tokens) => run_inline(state, &tokens),
        Dispatch::Shell(command) => run_shell(&command),
    }
}

/// Parses and runs an in-process `bspc` binding exactly as the control
/// socket would (`crate::ipc::execute_and_broadcast`), satisfying
/// `docs/bsp-hotkeys.md`'s "Binding execution" equivalence requirement
/// by construction — both paths call the same function. `quit`/
/// `subscribe` are not handled by `exec::execute` at all (the control
/// socket special-cases them, `crate::ipc`); `subscribe` bound to a key
/// makes no sense (nothing is listening) and is silently ignored,
/// `quit` is handled here directly.
///
/// `tokens` is `dispatch::classify`'s `InlineBspc` payload, which
/// starts with the literal word `bspc` (it is a tokenized *command
/// line*, `bspc node -f west` in full) — skipped here since
/// `bsp_ipc::command::parse` expects only the arguments *after* that,
/// the same way the real `bspc` binary strips its own `argv[0]` before
/// putting the rest on the wire (`docs/bsp-ipc.md`).
fn run_inline(state: &mut State, tokens: &[String]) {
    let args = &tokens[1..];
    match bsp_ipc::command::parse(args) {
        Ok(bsp_ipc::command::Command::Quit(_status)) => {
            state.running = false;
        }
        Ok(bsp_ipc::command::Command::Subscribe { .. }) => {
            tracing::warn!("a hotkey bound to `subscribe` has nothing to subscribe; ignored");
        }
        Ok(command) => {
            let _ = crate::ipc::try_hotkeys_inline_bspc(state, &command)
                .or_else(|| crate::pointer_action::try_config(state, &command))
                .unwrap_or_else(|| crate::ipc::execute_and_broadcast(state, &command));
        }
        Err(err) => {
            tracing::warn!(command = ?tokens, "hotkey's inline bspc call failed to parse: {}", err.message);
        }
    }
}

/// Spawns `sh -c command`, detached. bspwm's sxhkd: `src/helpers.c`
/// `run()`/`spawn()`/`execute()`, minus the double-fork-plus-`setsid()`
/// dance those use to fully detach from the parent — `std::process::
/// Command::spawn` already returns without waiting, which is enough to
/// not block the compositor; the child is simply left to `wait(2)` for
/// itself. Not implemented: reaping the child when it exits (it becomes
/// a zombie until this process exits or happens to check on it), a
/// known simplification rather than adding `SIGCHLD`-ignoring `unsafe`
/// FFI for this first pass.
fn run_shell(command: &str) {
    match std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .spawn()
    {
        Ok(_child) => {}
        Err(err) => tracing::warn!(command, "failed to spawn hotkey command: {err}"),
    }
}
