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

use crate::state::{Backend, State};

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
pub fn reload<Bd: Backend + 'static>(state: &mut State<Bd>) {
    state.hotkeys = init();
    canonicalize_virtual_modifiers(state);
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
pub fn filter<Bd: Backend + 'static>(
    state: &mut State<Bd>,
    mods: &ModifiersState,
    keysym: KeysymHandle<'_>,
    pressed: bool,
) -> FilterResult<()> {
    // The focused client asked for shortcuts to be suspended
    // (`zwp_keyboard_shortcuts_inhibit`): every key goes to it. The
    // emergency quit key and VT switching are checked before this filter
    // runs, so they still work.
    if state.protocols.session_lock.locked || crate::protocols::shortcuts_inhibited(state) {
        return FilterResult::Forward;
    }
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
/// against — the live-held half of full modifier coverage
/// (`docs/design.md`'s hotkeys row); [`canonicalize_virtual_modifiers`]
/// below is the other half.
///
/// Only ever populates the eight *real* X11/xkb modifiers (`Shift`/
/// `Control`/`Lock`/`Mod1`..`Mod5`) — never `Modifier::Alt`/`Super`/
/// `ModeSwitch`/`Hyper`/`Meta` — because on any real keymap, that is
/// *all there structurally is*: XKB has exactly eight real modifiers,
/// full stop, and every "friendly" name (`alt`, `super`, `mode_switch`,
/// and, on essentially every stock keymap, `hyper`/`meta` too) is
/// always an alias for one of them, never independently-held state.
/// `Mod1`..`Mod5` are read straight off the *fields* Smithay's own
/// `ModifiersState::update_with`
/// (`smithay-0.7.0/src/input/keyboard/modifiers_state.rs`) already
/// computes from exactly that real modifier's fixed xkb name (`xkb::
/// MOD_NAME_ALT`/`_NUM`/`_MOD3`/`_LOGO`/`_ISO_LEVEL3_SHIFT` are
/// literally `"Mod1"`/`"Mod2"`/`"Mod3"`/`"Mod4"`/`"Mod5"`) — `alt` and
/// `mod1` are one and the same query, not two independent conditions.
///
/// A chord still written `alt`/`super`/`mode_switch`/`hyper`/`meta`
/// matches this real-bit-only held set through
/// [`canonicalize_virtual_modifiers`] instead of a symmetric query
/// here: it rewrites a loaded chord's modifiers to the real bit they
/// name, once, when the chord is loaded — see its own doc comment for
/// why querying `hyper`/`meta` here too (this function's first,
/// live-tested-and-reverted shape) is actively wrong, not just
/// redundant.
fn resolve_modifiers(mods: &ModifiersState) -> HashSet<Modifier> {
    let mut set = HashSet::new();
    if mods.shift {
        set.insert(Modifier::Shift);
    }
    if mods.ctrl {
        set.insert(Modifier::Control);
    }
    if mods.caps_lock {
        set.insert(Modifier::Lock);
    }
    if mods.alt {
        set.insert(Modifier::Mod1);
    }
    if mods.num_lock {
        set.insert(Modifier::Mod2);
    }
    if mods.iso_level5_shift {
        set.insert(Modifier::Mod3);
    }
    if mods.logo {
        set.insert(Modifier::Mod4);
    }
    if mods.iso_level3_shift {
        set.insert(Modifier::Mod5);
    }
    set
}

/// Rewrites every loaded hotkey's `Hyper`/`Meta` chord modifiers (if
/// any) in place, to whichever real `Shift`/`Control`/`Lock`/`Mod1`..
/// `Mod5` bit the live keymap actually binds them to, then rebuilds
/// `state.hotkey_matcher` to match — called once after `state.hotkeys`
/// is (re)loaded ([`State::new`](crate::state::State::new), after
/// `seat.add_keyboard`; [`reload`]), not per key event.
///
/// This is the Wayland/xkbcommon equivalent of sxhkd's own *parse-time*
/// `modfield_from_keysym(Hyper_L)`/`modfield_from_keysym(Meta_L)`
/// resolution (`src/parse.c` `parse_modifier()`) — just performed once
/// the keymap is available instead of at parse time, since
/// `bsp-hotkeys` has no keymap of its own (`docs/bsp-hotkeys.md`).
///
/// Necessary, not a style choice: a first attempt at this function
/// resolved `hyper`/`meta` the same way as [`resolve_modifiers`] above
/// — a fresh `mod_name_is_active` query on every key event, inserting
/// `Modifier::Hyper`/`Meta` into the held set directly. Live-testing
/// caught a real bug that approach introduced: on this environment's
/// (and, per `xkbcli compile-keymap`'s stock `pc` ruleset output,
/// essentially every default Linux keymap's) `modifier_map Mod1 {
/// <LALT>, <RALT>, <ALT>, <META> }`, physical `Alt` asserts the virtual
/// `Meta` modifier too — so the held set became `{Mod1, Meta}` while an
/// existing, previously-working `alt + x` chord's modifiers canonicalize
/// to just `{Mod1}`; `bsp-hotkeys::matcher::modifiers_match`'s exact-set
/// comparison (a real bspwm quirk, reproduced deliberately, not
/// smoothed over) correctly judged that a mismatch and broke a binding
/// that had nothing to do with `meta` at all. Real sxhkd never hits
/// this: `modfield_from_keysym` resolves `meta` to a bit *once*, at
/// parse time, so its `modfield` bitmask has no way to carry an "extra"
/// bit a chord's own text never named — this function restores that
/// same one-time-resolution property instead of tracking `hyper`/`meta`
/// as live, independently-held state.
pub fn canonicalize_virtual_modifiers<Bd: Backend + 'static>(state: &mut State<Bd>) {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    let hyper = keyboard.with_xkb_state(state, |ctx| real_bit_for(ctx.xkb(), "Hyper"));
    let meta = keyboard.with_xkb_state(state, |ctx| real_bit_for(ctx.xkb(), "Meta"));
    for hotkey in &mut state.hotkeys {
        for chord in &mut hotkey.chords {
            for m in &mut chord.modifiers {
                let real = match *m {
                    Modifier::Hyper => hyper,
                    Modifier::Meta => meta,
                    _ => None,
                };
                if let Some(real) = real {
                    *m = real;
                }
            }
        }
    }
    state.hotkey_matcher = build_matcher(&state.hotkeys);
}

/// Which real `Shift`/`Control`/`Lock`/`Mod1`..`Mod5` modifier (if any)
/// the live keymap's virtual modifier `name` (e.g. `"Hyper"`) actually
/// depends on — `None` if the keymap doesn't define `name` at all, or
/// (never observed in practice, but structurally possible on an exotic
/// keymap) it depends on more than one real bit at once, which this
/// crate's single-`Modifier` chord slots can't represent. Shared by
/// [`canonicalize_virtual_modifiers`] and
/// `crate::pointer_action::canonicalize_modifier`.
///
/// A virtual modifier's real-bit dependency is *not* exposed as a
/// direct query anywhere in `libxkbcommon`'s public API — unlike a real
/// modifier, it gets its own index into the keymap's modifier table
/// (`Keymap::mod_get_index`), entirely separate from the real
/// modifiers' fixed indices 0..8, so comparing indices for equality
/// (this function's first, live-tested-and-reverted shape) never once
/// matches, even where `mod_name_is_active` proves at runtime that
/// holding a real modifier *does* activate it. So this probes for the
/// answer the same way the keymap resolves it internally: build one
/// throwaway [`xkb::State`] from the same [`Keymap`], assert each real
/// modifier alone in turn (`State::update_mask`), and check whether
/// that alone was enough to activate `name`
/// (`State::mod_index_is_active`) — exactly the computation
/// `mod_name_is_active` performs against the *live* state, just run
/// once per real modifier against a synthetic one instead of the seat's
/// actual held keys.
pub(crate) fn real_bit_for(
    xkb: &std::sync::Mutex<smithay::input::keyboard::Xkb>,
    name: &str,
) -> Option<Modifier> {
    let Ok(guard) = xkb.lock() else {
        return None;
    };
    // SAFETY: `state()` only requires that its returned reference not
    // outlive the `Xkb` it borrows from; the reference (and the
    // `Keymap`/scratch `State` derived from it) are used only inside
    // this function and never stored, returned, or cloned, so none of
    // them can outlive `xkb` (dropped at the end of this function, once
    // `guard` goes out of scope).
    let state = unsafe { guard.state() };
    let keymap = state.get_keymap();
    let vidx = keymap.mod_get_index(name);
    if vidx == xkbcommon::xkb::MOD_INVALID {
        return None;
    }
    const REAL: [(&str, Modifier); 8] = [
        ("Shift", Modifier::Shift),
        ("Lock", Modifier::Lock),
        ("Control", Modifier::Control),
        ("Mod1", Modifier::Mod1),
        ("Mod2", Modifier::Mod2),
        ("Mod3", Modifier::Mod3),
        ("Mod4", Modifier::Mod4),
        ("Mod5", Modifier::Mod5),
    ];
    let mut scratch = xkbcommon::xkb::State::new(&keymap);
    for (real_name, m) in REAL {
        let ridx = keymap.mod_get_index(real_name);
        if ridx == xkbcommon::xkb::MOD_INVALID {
            continue;
        }
        scratch.update_mask(1 << ridx, 0, 0, 0, 0, 0);
        let active = scratch.mod_index_is_active(vidx, xkbcommon::xkb::STATE_MODS_EFFECTIVE);
        scratch.update_mask(0, 0, 0, 0, 0, 0);
        if active {
            return Some(m);
        }
    }
    None
}

fn run<Bd: Backend + 'static>(state: &mut State<Bd>, index: usize) {
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
fn run_inline<Bd: Backend + 'static>(state: &mut State<Bd>, tokens: &[String]) {
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
                .or_else(|| crate::xwayland::try_config(state, &command))
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
