//! Loads a whole sxhkdrc file's worth of hotkeys in one call, tying
//! [`crate::lexer`], [`crate::expand`], [`crate::binding`] and
//! [`crate::dispatch`] together.
//!
//! bspwm's sxhkd: `src/parse.c` `load_config()`, which does the same
//! job as one big imperative loop reading straight from the file
//! (`docs/design.md` Architecture: reading the file is `bsp-compositor`'s
//! job, this crate stays I/O-free and takes the contents as a `&str`).

use std::path::PathBuf;

use crate::binding::{self, Chord};
use crate::dispatch::{self, Dispatch};
use crate::expand;
use crate::lexer;

/// Resolves the sxhkdrc path sxhkd itself would use, given
/// `XDG_CONFIG_HOME` and `HOME` (`None` for an unset variable): `$XDG_
/// CONFIG_HOME/sxhkd/sxhkdrc` if set, else `$HOME/.config/sxhkd/
/// sxhkdrc`. `None` only when neither variable is set — matches sxhkd's
/// own fallback order, but a real `HOME`-less environment is not one
/// sxhkd itself handles gracefully either (it calls `getenv("HOME")`
/// unchecked, `src/sxhkd.c`).
///
/// bspwm's sxhkd: `src/sxhkd.c` `main()`'s config-path block,
/// `CONFIG_HOME_ENV`/`CONFIG_PATH` (`src/sxhkd.h`). Takes the two
/// variables as parameters rather than reading the environment itself,
/// so it can be tested without touching real process state — the
/// actual `std::env::var` calls belong to `bsp-compositor`, the I/O
/// boundary (mirrors `bsp_ipc::wire::resolve_socket_path`).
#[must_use]
pub fn resolve_path(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    if let Some(config_home) = xdg_config_home {
        return Some(PathBuf::from(config_home).join("sxhkd/sxhkdrc"));
    }
    home.map(|h| PathBuf::from(h).join(".config/sxhkd/sxhkdrc"))
}

/// One fully loaded hotkey: its chord chain, ready for
/// [`crate::matcher::Matcher`], and where its command should run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedHotkey {
    /// The chord chain a `Matcher` should track for this hotkey.
    pub chords: Vec<Chord>,
    /// Where (and, for [`Dispatch::InlineBspc`], how) to run its
    /// command.
    pub dispatch: Dispatch,
}

/// Loads every hotkey in `contents` (a whole sxhkdrc file's text),
/// running each raw binding through `{}`/range expansion and then
/// chord parsing.
///
/// A binding whose chain names an unrecognized modifier, keysym or
/// button is skipped rather than failing the whole file — bspwm itself
/// only ever `warn()`s and moves on (`src/parse.c` `parse_chain()`
/// returning `false`, checked by `process_hotkey()`); this crate has no
/// logging of its own (`docs/design.md` Architecture: it returns plain
/// data), so a skipped binding is simply absent from the result. A
/// caller that wants to warn the user can re-derive which one failed by
/// calling [`crate::binding::parse_chain`] itself on any chain string
/// missing from [`ExpandedBinding`](expand::ExpandedBinding)'s output
/// mapped 1:1 against this function's result.
#[must_use]
pub fn load(contents: &str) -> Vec<LoadedHotkey> {
    let mut hotkeys = Vec::new();
    for raw in lexer::parse(contents) {
        for expanded in expand::expand(&raw.chain, &raw.command) {
            if let Ok(chords) = binding::parse_chain(&expanded.chain) {
                hotkeys.push(LoadedHotkey {
                    chords,
                    dispatch: dispatch::classify(&expanded.command),
                });
            }
        }
    }
    hotkeys
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::{Key, Modifier};

    #[test]
    fn resolve_path_prefers_xdg_config_home() {
        assert_eq!(
            resolve_path(Some("/x"), Some("/home/u")),
            Some(PathBuf::from("/x/sxhkd/sxhkdrc"))
        );
    }

    #[test]
    fn resolve_path_falls_back_to_home_dot_config() {
        assert_eq!(
            resolve_path(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.config/sxhkd/sxhkdrc"))
        );
    }

    #[test]
    fn resolve_path_is_none_with_neither_variable() {
        assert_eq!(resolve_path(None, None), None);
    }

    #[test]
    fn loads_a_simple_sxhkdrc() {
        let hotkeys = load("super + Return\n\talacritty\n");
        assert_eq!(hotkeys.len(), 1);
        assert_eq!(hotkeys[0].chords[0].modifiers, vec![Modifier::Super]);
        assert_eq!(
            hotkeys[0].dispatch,
            Dispatch::Shell("alacritty".to_string())
        );
    }

    #[test]
    fn expands_braces_into_one_hotkey_per_combination() {
        let hotkeys = load("super + {h,j,k,l}\n\tbspc node -f {west,south,north,east}\n");
        assert_eq!(hotkeys.len(), 4);
        assert_eq!(
            hotkeys[0].dispatch,
            Dispatch::InlineBspc(vec![
                "bspc".to_string(),
                "node".to_string(),
                "-f".to_string(),
                "west".to_string(),
            ])
        );
    }

    #[test]
    fn an_unknown_keysym_silently_drops_that_one_hotkey() {
        let hotkeys = load("super + Return\n\talacritty\nsuper + not_a_real_key\n\tfoo\n");
        assert_eq!(hotkeys.len(), 1);
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let hotkeys = load("# a comment\n\nsuper + Return\n\talacritty\n\n");
        assert_eq!(hotkeys.len(), 1);
    }

    #[test]
    fn a_chain_of_chords_keeps_every_chord() {
        let hotkeys = load("super + w ; h\n\tbspc node -f west\n");
        assert_eq!(hotkeys[0].chords.len(), 2);
        assert!(matches!(hotkeys[0].chords[1].key, Some(Key::Keysym(_))));
    }
}
