//! Parses an already-`{}`-expanded chain string
//! ([`crate::expand::ExpandedBinding::chain`]) into a sequence of
//! [`Chord`]s: the modifiers, keysym or pointer button, and press/
//! release/replay/lock-chain flags sxhkd's own grammar defines.
//!
//! bspwm's sxhkd: `src/parse.c` `parse_chain()`, `parse_modifier()` and
//! `parse_button()`. `parse_keysym()` is replaced by
//! `xkbcommon::xkb::keysym_from_name` — sxhkd keeps its own `nks_dict`
//! name table, but this crate's only allowed dependency beyond the
//! standard library is `xkbcommon`, precisely for this lookup
//! (`docs/design.md` Architecture).

use xkbcommon::xkb::{self, Keysym};

use crate::token::get_token;

/// A modifier key name (bspwm: `parse_modifier()`'s recognized names).
/// A chord can carry several; they combine, not replace one another.
///
/// Kept symbolic rather than resolved to a bitmask: bspwm's own
/// `shift`/`control`/`lock`/`mod1`..`mod5` are fixed X11 modifier bits,
/// but `alt`/`super`/`hyper`/`meta`/`mode_switch` are resolved
/// *dynamically* against the current keyboard mapping
/// (`modfield_from_keysym()`) — there is no fixed bit to hardcode for
/// them, on X11 or (with likely different modifier-index assignment)
/// under `xkbcommon`. Resolving a name here to a real modifier state
/// needs a live keymap, which belongs to `bsp-compositor`'s seat, not
/// this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Modifier {
    /// `shift`.
    Shift,
    /// `control`/`ctrl`.
    Control,
    /// `lock` (Caps Lock).
    Lock,
    /// `alt`.
    Alt,
    /// `super`.
    Super,
    /// `hyper`.
    Hyper,
    /// `meta`.
    Meta,
    /// `mode_switch`.
    ModeSwitch,
    /// `mod1`.
    Mod1,
    /// `mod2`.
    Mod2,
    /// `mod3`.
    Mod3,
    /// `mod4`.
    Mod4,
    /// `mod5`.
    Mod5,
    /// `any`: matches regardless of which modifiers are held.
    Any,
}

impl Modifier {
    /// Parses one modifier name, or `None` if `name` isn't one.
    ///
    /// bspwm: `src/parse.c` `parse_modifier()`.
    fn parse(name: &str) -> Option<Modifier> {
        Some(match name {
            "shift" => Modifier::Shift,
            "control" | "ctrl" => Modifier::Control,
            "alt" => Modifier::Alt,
            "super" => Modifier::Super,
            "hyper" => Modifier::Hyper,
            "meta" => Modifier::Meta,
            "mode_switch" => Modifier::ModeSwitch,
            "mod1" => Modifier::Mod1,
            "mod2" => Modifier::Mod2,
            "mod3" => Modifier::Mod3,
            "mod4" => Modifier::Mod4,
            "mod5" => Modifier::Mod5,
            "lock" => Modifier::Lock,
            "any" => Modifier::Any,
            _ => return None,
        })
    }
}

/// What a chord fires on: a key, or (`docs/bsp-hotkeys.md`'s "Pointer
/// bindings" scope item, not wired up anywhere yet) a pointer button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A keysym, resolved by name via `xkbcommon`.
    Keysym(Keysym),
    /// `buttonN` (bspwm: `parse_button()`).
    Button(u8),
}

/// One chord in a chain: bspwm's `chord_t` (`src/types.h`), minus
/// `repr` (its own `subscribe`-status debug field) and the keycode
/// fan-out `make_chord()` performs (resolving a keysym to every
/// physical key that can produce it needs a live keymap, `bsp-
/// compositor`'s job, not this crate's — see [`Modifier`]'s doc
/// comment for the same reasoning).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    /// Every modifier this chord requires.
    pub modifiers: Vec<Modifier>,
    /// The key or button, or `None` if the chord text named no key at
    /// all (a malformed chord bspwm's own parser doesn't reject
    /// either — `make_chord()` is simply handed `XCB_NO_SYMBOL`).
    pub key: Option<Key>,
    /// `@`-prefixed: fires on release rather than press.
    pub on_release: bool,
    /// `~`-prefixed: replays the event to the client afterward.
    pub replay: bool,
    /// Whether a `:` (rather than `;`) followed this chord, keeping
    /// the chain grabbed open past it (bspwm: `GRP_SEP`, detected via
    /// `parse_chain()`'s `ignored` buffer from `get_token()`).
    pub lock_chain: bool,
}

/// A chord name that is not a recognized modifier, keysym, or `buttonN`
/// pointer button.
///
/// bspwm: `parse_chain()` returning `false` after `warn("Unknown keysym
/// name: '%s'.\n", nm)`. Formatting that message is left to the caller
/// (`bsp-compositor`, once this is wired in) rather than assumed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownName(pub String);

impl std::fmt::Display for UnknownName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown keysym, modifier or button name: '{}'", self.0)
    }
}

impl std::error::Error for UnknownName {}

/// Parses an expanded chain string (`super + w`, `super + w ; h`, …)
/// into its chords, in order.
///
/// bspwm: `src/parse.c` `parse_chain()`. Chords are separated by `;`/`:`
/// (`:` additionally sets the preceding chord's `lock_chain`); within a
/// chord, names are separated by `+`/space, each optionally prefixed
/// `~` (replay) then `@` (release) — in that order; `~@w` is a replayed
/// release of `w`, `@~w` parses `~w` as a keysym name and fails.
pub fn parse_chain(chain: &str) -> Result<Vec<Chord>, UnknownName> {
    let mut chords = Vec::new();
    let mut rest = chain.to_string();
    loop {
        let (chord_text, ign, next_rest) = get_token(&rest, &[';', ':']);
        if chord_text.is_empty() {
            break;
        }
        rest = next_rest;

        let mut modifiers = Vec::new();
        let mut key = None;
        let mut replay = false;
        let mut on_release = false;

        let mut name_rest = chord_text;
        loop {
            let (name, _, next_name_rest) = get_token(&name_rest, &['+', ' ']);
            if name.is_empty() {
                break;
            }
            name_rest = next_name_rest;

            let mut n = name.as_str();
            if let Some(stripped) = n.strip_prefix('~') {
                replay = true;
                n = stripped;
            }
            if let Some(stripped) = n.strip_prefix('@') {
                on_release = true;
                n = stripped;
            }

            if let Some(m) = Modifier::parse(n) {
                modifiers.push(m);
            } else {
                let keysym = xkb::keysym_from_name(n, xkb::KEYSYM_NO_FLAGS);
                if keysym != Keysym::NoSymbol {
                    key = Some(Key::Keysym(keysym));
                } else if let Some(button) = parse_button(n) {
                    key = Some(Key::Button(button));
                } else {
                    return Err(UnknownName(n.to_string()));
                }
            }
        }

        chords.push(Chord {
            modifiers,
            key,
            on_release,
            replay,
            lock_chain: ign.contains(':'),
        });
    }
    Ok(chords)
}

/// Parses `buttonN` (`N` a `u8`). bspwm's `sscanf(name, "button%hhu",
/// …)` tolerates trailing garbage after the digits (`"button1foo"`
/// would still parse as button 1); this build requires the whole
/// remainder to be the number, a harmless tightening no real sxhkdrc
/// would notice.
fn parse_button(name: &str) -> Option<u8> {
    name.strip_prefix("button")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keysym(name: &str) -> Keysym {
        xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS)
    }

    #[test]
    fn a_single_modifier_and_keysym() {
        let chords = parse_chain("super + w").unwrap();
        assert_eq!(
            chords,
            vec![Chord {
                modifiers: vec![Modifier::Super],
                key: Some(Key::Keysym(keysym("w"))),
                on_release: false,
                replay: false,
                lock_chain: false,
            }]
        );
    }

    #[test]
    fn modifiers_accumulate() {
        let chords = parse_chain("super + shift + w").unwrap();
        assert_eq!(chords[0].modifiers, vec![Modifier::Super, Modifier::Shift]);
    }

    #[test]
    fn a_chain_of_chords_separated_by_semicolon() {
        let chords = parse_chain("super + w ; h").unwrap();
        assert_eq!(chords.len(), 2);
        assert_eq!(chords[0].key, Some(Key::Keysym(keysym("w"))));
        assert_eq!(chords[1].key, Some(Key::Keysym(keysym("h"))));
        assert_eq!(chords[0].modifiers, vec![Modifier::Super]);
        assert!(chords[1].modifiers.is_empty());
    }

    #[test]
    fn colon_locks_the_preceding_chord_not_the_next_one() {
        let chords = parse_chain("super + w : h").unwrap();
        assert!(chords[0].lock_chain);
        assert!(!chords[1].lock_chain);
    }

    #[test]
    fn release_prefix_sets_on_release() {
        let chords = parse_chain("super + @w").unwrap();
        assert!(chords[0].on_release);
        assert!(!chords[0].replay);
    }

    #[test]
    fn replay_prefix_sets_replay() {
        let chords = parse_chain("~w").unwrap();
        assert!(chords[0].replay);
        assert!(!chords[0].on_release);
    }

    #[test]
    fn replay_then_release_combine_in_that_order() {
        let chords = parse_chain("~@w").unwrap();
        assert!(chords[0].replay);
        assert!(chords[0].on_release);
    }

    #[test]
    fn release_then_replay_order_fails_to_parse() {
        // `@~w`: `@` is stripped first (release), leaving `~w`, which is
        // not a recognized modifier/keysym/button name — bspwm's own
        // ordering constraint (`parse_chain()` checks `~` before `@`),
        // reproduced rather than made more lenient.
        assert_eq!(parse_chain("@~w"), Err(UnknownName("~w".to_string())));
    }

    #[test]
    fn a_pointer_button_name() {
        let chords = parse_chain("super + button1").unwrap();
        assert_eq!(chords[0].key, Some(Key::Button(1)));
    }

    #[test]
    fn an_unknown_name_is_rejected() {
        assert_eq!(
            parse_chain("super + not_a_real_key"),
            Err(UnknownName("not_a_real_key".to_string()))
        );
    }

    #[test]
    fn any_modifier_is_recognized() {
        let chords = parse_chain("any + w").unwrap();
        assert_eq!(chords[0].modifiers, vec![Modifier::Any]);
    }
}
