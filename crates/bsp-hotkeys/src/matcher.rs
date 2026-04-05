//! A chord-chain state machine: feeds one key/button event at a time
//! and reports whether a chain advanced, completed, or nothing matched.
//!
//! bspwm's sxhkd: `src/types.c` `find_hotkey()`/`match_chord()`. Kept
//! pure and X11-free: `grab_chord()`/`ungrab()` (X11 passive key grabs
//! — Wayland has no equivalent, a compositor must instead choose for
//! itself, based on [`Outcome`], whether to forward an event to the
//! focused client), the `timeout`/`alarm()`-driven auto-abort (a timer
//! is `bsp-compositor`'s event loop's job — call [`Matcher::abort_chain`]
//! when it fires), `status_fifo` reporting, and the configurable
//! `abort_chord` "escape this chain" key (not implemented; every
//! `abort_chain` call must come from the caller for now) are all left
//! out. `hk->cycle` (a workaround for X11 key-repeat delivering
//! duplicate presses for identical chains) is dropped too — a
//! Wayland compositor controls repeat delivery itself and does not
//! need it.

use std::collections::HashSet;

use crate::binding::{Chord, Key, Modifier};

/// One step of live input, ready to match against parsed chords.
///
/// bspwm: the `keysym`/`button`/`modfield`/`event_type` parameters to
/// `find_hotkey()`. `modifiers` is the *set* of symbolic modifiers
/// currently held, already resolved against the live keymap — this
/// crate has no keymap of its own (`docs/bsp-hotkeys.md` Scope), so
/// resolving e.g. "this keycode is currently held and means `Alt`" is
/// `bsp-compositor`'s job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    /// The keysym or button this event is for.
    pub key: Key,
    /// Every modifier currently held, as a set (order and repeats
    /// don't matter — a chord's own modifier list is compared as a
    /// set too, matching bspwm's bitmask equality).
    pub modifiers: HashSet<Modifier>,
    /// `true` for a press, `false` for a release.
    pub pressed: bool,
}

/// What feeding one [`KeyEvent`] did.
///
/// bspwm's `find_hotkey()` only ever returns the matched `hotkey_t*` or
/// `NULL`, leaning on its own `chained` global for callers (the X11
/// event loop) to tell "nothing matched" apart from "a chain is now in
/// progress, don't forward this to the client". [`Matcher`] exposes
/// that distinction directly instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing matched, and no chain was in progress: forward the
    /// event to the focused client as usual.
    Pass,
    /// A chain started or advanced, but has not completed: swallow the
    /// event rather than forwarding it.
    Continue,
    /// The chain at this index (into the `Vec` passed to
    /// [`Matcher::new`]) just completed.
    Fire {
        /// The completed chain's index.
        index: usize,
    },
}

struct ChainState {
    chords: Vec<Chord>,
    /// Index into `chords` of the next chord this chain expects.
    /// bspwm: `chain_t.state`, a pointer into the chord linked list;
    /// `chords[0]` is `chain_t.head`, `chords[chords.len() - 1]` is
    /// `chain_t.tail`.
    cursor: usize,
}

/// Tracks every configured chain's progress and the matcher's overall
/// `chained`/`locked` mode.
///
/// bspwm: the `chained`/`locked` file-scope globals in `src/sxhkd.c`,
/// plus every `hotkey_t`'s own `chain_t`.
pub struct Matcher {
    chains: Vec<ChainState>,
    chained: bool,
    locked: bool,
}

impl Matcher {
    /// Builds a matcher over `chains` (typically each one a
    /// [`crate::binding::parse_chain`] result); a chain's position in
    /// this `Vec` is the `index` [`Outcome::Fire`] reports back.
    #[must_use]
    pub fn new(chains: Vec<Vec<Chord>>) -> Self {
        let chains = chains
            .into_iter()
            .map(|chords| ChainState { chords, cursor: 0 })
            .collect();
        Self {
            chains,
            chained: false,
            locked: false,
        }
    }

    /// Resets every chain to its head and leaves `chained`/`locked`
    /// mode.
    ///
    /// bspwm: `src/types.c` `abort_chain()`, minus `ungrab()`/`grab()`
    /// (see the module doc comment) and the `status_fifo` report.
    pub fn abort_chain(&mut self) {
        for chain in &mut self.chains {
            chain.cursor = 0;
        }
        self.chained = false;
        self.locked = false;
    }

    /// Feeds one input event, advancing/resetting every chain's cursor
    /// and reporting the result.
    ///
    /// bspwm: `src/types.c` `find_hotkey()`.
    pub fn feed(&mut self, event: &KeyEvent) -> Outcome {
        let mut num_active = 0usize;
        let mut num_locked = 0usize;
        let mut result = None;

        for idx in 0..self.chains.len() {
            if self.chains[idx].chords.is_empty() {
                continue;
            }
            let tail = self.chains[idx].chords.len() - 1;
            let cursor = self.chains[idx].cursor;
            // Once chained, a chain that hasn't started yet (still at
            // its head) is not considered — otherwise a lone,
            // unrelated single-chord hotkey would fire mid-chain.
            // Once locked, only a chain already sitting at its tail
            // (a "stays open" chord, `Chord::lock_chain`) is
            // considered.
            if (self.chained && cursor == 0) || (self.locked && cursor != tail) {
                continue;
            }

            if chord_matches(&self.chains[idx].chords[cursor], event) {
                if self.chains[idx].chords[cursor].lock_chain {
                    num_locked += 1;
                }
                if cursor == tail {
                    result = Some(idx);
                    break;
                }
                self.chains[idx].cursor += 1;
                num_active += 1;
            } else if self.chained {
                let chord = &self.chains[idx].chords[cursor];
                if !self.locked && same_event_type(chord, event) {
                    self.chains[idx].cursor = 0;
                } else {
                    num_active += 1;
                }
            }
        }

        if let Some(idx) = result {
            if self.chained && !self.locked {
                self.abort_chain();
            }
            return Outcome::Fire { index: idx };
        }

        if num_locked > 0 {
            self.locked = true;
        }

        if !self.chained {
            if num_active > 0 {
                self.chained = true;
                Outcome::Continue
            } else {
                Outcome::Pass
            }
        } else if num_active == 0 {
            // bspwm: `abort_chain(); return find_hotkey(...);` — every
            // chain just reset to head, so this re-evaluates `event`
            // as if nothing had been in progress; terminates in at
            // most one extra call, since `self.chained` is now false.
            //
            // A real, non-obvious consequence: the very keypress that
            // breaks a chain is retried fresh, not just swallowed — if
            // it also happens to match some other complete single-chord
            // hotkey (one the skip condition above was ignoring only
            // because a chain was in progress), that hotkey fires
            // immediately on this same keypress, matching bspwm.
            self.abort_chain();
            self.feed(event)
        } else {
            Outcome::Continue
        }
    }
}

fn same_event_type(chord: &Chord, event: &KeyEvent) -> bool {
    chord.on_release != event.pressed
}

fn chord_matches(chord: &Chord, event: &KeyEvent) -> bool {
    same_event_type(chord, event)
        && chord.key == Some(event.key)
        && modifiers_match(&chord.modifiers, &event.modifiers)
}

/// bspwm: `match_chord()`'s `c->modfield == XCB_MOD_MASK_ANY ||
/// c->modfield == modfield` — an exact bitmask match, not "at least
/// these modifiers": holding one extra, unrelated modifier breaks an
/// otherwise-matching chord. `any` is a wildcard only when it is the
/// chord's *entire* modifier list; combined with any other modifier it
/// just becomes one more required (and practically unmatchable, since
/// nothing ever "holds" the wildcard) modifier — a real, if obscure,
/// bspwm quirk, reproduced rather than smoothed over.
fn modifiers_match(chord_modifiers: &[Modifier], held: &HashSet<Modifier>) -> bool {
    if chord_modifiers.len() == 1 && chord_modifiers[0] == Modifier::Any {
        return true;
    }
    let chord_set: HashSet<Modifier> = chord_modifiers.iter().copied().collect();
    chord_set == *held
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::parse_chain;

    fn mods(names: &[Modifier]) -> HashSet<Modifier> {
        names.iter().copied().collect()
    }

    fn press(chain: &str, held: &[Modifier]) -> KeyEvent {
        let chords = parse_chain(chain).unwrap();
        KeyEvent {
            key: chords[0].key.unwrap(),
            modifiers: mods(held),
            pressed: true,
        }
    }

    #[test]
    fn a_single_chord_fires_immediately_on_an_exact_match() {
        let chains = vec![parse_chain("super + w").unwrap()];
        let mut m = Matcher::new(chains);
        let event = press("super + w", &[Modifier::Super]);
        assert_eq!(m.feed(&event), Outcome::Fire { index: 0 });
    }

    #[test]
    fn an_extra_held_modifier_breaks_an_exact_match() {
        // bspwm requires the modifier state to match exactly.
        let chains = vec![parse_chain("super + w").unwrap()];
        let mut m = Matcher::new(chains);
        let event = press("super + w", &[Modifier::Super, Modifier::Shift]);
        assert_eq!(m.feed(&event), Outcome::Pass);
    }

    #[test]
    fn any_modifier_matches_regardless_of_what_is_held() {
        let chains = vec![parse_chain("any + w").unwrap()];
        let mut m = Matcher::new(chains);
        let event = press("super + w", &[Modifier::Super, Modifier::Shift]);
        assert_eq!(m.feed(&event), Outcome::Fire { index: 0 });
    }

    #[test]
    fn a_multi_chord_chain_advances_then_fires() {
        let chains = vec![parse_chain("super + w ; h").unwrap()];
        let mut m = Matcher::new(chains);
        let first = press("super + w", &[Modifier::Super]);
        assert_eq!(m.feed(&first), Outcome::Continue);
        let second = press("h", &[]);
        assert_eq!(m.feed(&second), Outcome::Fire { index: 0 });
    }

    #[test]
    fn a_non_matching_event_aborts_an_in_progress_chain() {
        let chains = vec![parse_chain("super + w ; h").unwrap()];
        let mut m = Matcher::new(chains);
        m.feed(&press("super + w", &[Modifier::Super]));
        let unrelated = press("x", &[]);
        assert_eq!(m.feed(&unrelated), Outcome::Pass);
    }

    #[test]
    fn an_unstarted_single_chord_hotkey_is_ignored_while_chained() {
        // A 3-chord chain so its second step (`y`) advances rather than
        // completes it, and a single-chord hotkey on that exact same
        // `y` — which would fire immediately on its own.
        let chains = vec![
            parse_chain("super + w ; y ; z").unwrap(),
            parse_chain("y").unwrap(),
        ];
        let mut m = Matcher::new(chains);
        m.feed(&press("super + w", &[Modifier::Super]));
        // `y` advances chain 0 (to expect `z`) rather than firing
        // chain 1, which is skipped entirely while chained.
        assert_eq!(m.feed(&press("y", &[])), Outcome::Continue);
        assert_eq!(m.feed(&press("z", &[])), Outcome::Fire { index: 0 });
    }

    #[test]
    fn a_locked_chord_keeps_firing_without_replaying_the_earlier_steps() {
        // "super + r : h" — the `:` sets the FIRST chord's lock_chain.
        let chains = vec![parse_chain("super + r : h").unwrap()];
        let mut m = Matcher::new(chains);
        m.feed(&press("super + r", &[Modifier::Super]));
        assert_eq!(m.feed(&press("h", &[])), Outcome::Fire { index: 0 });
        // Locked: fires again on `h` alone, no need to repeat `super + r`.
        assert_eq!(m.feed(&press("h", &[])), Outcome::Fire { index: 0 });
    }

    #[test]
    fn abort_chain_resets_progress() {
        let chains = vec![parse_chain("super + w ; h").unwrap()];
        let mut m = Matcher::new(chains);
        m.feed(&press("super + w", &[Modifier::Super]));
        m.abort_chain();
        // Back to needing `super + w` again, not `h`.
        assert_eq!(m.feed(&press("h", &[])), Outcome::Pass);
        assert_eq!(
            m.feed(&press("super + w", &[Modifier::Super])),
            Outcome::Continue
        );
    }

    #[test]
    fn release_bound_chord_does_not_match_a_press() {
        let chains = vec![parse_chain("super + @w").unwrap()];
        let mut m = Matcher::new(chains);
        assert_eq!(
            m.feed(&press("super + w", &[Modifier::Super])),
            Outcome::Pass
        );
    }

    #[test]
    fn release_bound_chord_matches_a_release() {
        let chords = parse_chain("super + @w").unwrap();
        let key = chords[0].key.unwrap();
        let chains = vec![chords];
        let mut m = Matcher::new(chains);
        let event = KeyEvent {
            key,
            modifiers: mods(&[Modifier::Super]),
            pressed: false,
        };
        assert_eq!(m.feed(&event), Outcome::Fire { index: 0 });
    }

    #[test]
    fn an_empty_chain_never_matches_and_never_panics() {
        let chains = vec![Vec::new()];
        let mut m = Matcher::new(chains);
        assert_eq!(
            m.feed(&press("super + w", &[Modifier::Super])),
            Outcome::Pass
        );
    }
}
