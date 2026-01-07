//! `bspc rule` entries and matching a client's class/instance/title
//! against them.
//!
//! bspwm: `src/types.h` `rule_t`, `src/rule.c`. Applying a matched rule's
//! consequence to a real window (`_apply_class`, `_apply_hints`, the
//! `external_rules_command` hook) is left for later steps: the core only
//! needs the pure matching bspwm's `apply_rules()` does before handing off
//! to those X11-specific appliers.

use crate::tree::Direction;
use crate::{node::ClientState, node::Layer};

/// The wildcard pattern that matches any string.
///
/// bspwm: `src/rule.h` `MATCH_ANY`.
pub const MATCH_ANY: &str = "*";

/// One `bspc rule` entry.
///
/// bspwm: `src/types.h` `rule_t`. `effect`, bspwm's raw
/// `key1=value1 key2=value2 ...` string, is replaced here by the already
/// parsed [`RuleConsequence`] (bspwm parses it lazily, once a window
/// actually matches; `bsp-ipc`, in the IPC, is where that parsing belongs
/// since it owns the command grammar).
#[derive(Debug, Clone)]
pub struct Rule {
    /// Pattern matched against the window's class name, or [`MATCH_ANY`].
    pub class_name: String,
    /// Pattern matched against the window's instance name, or
    /// [`MATCH_ANY`].
    pub instance_name: String,
    /// Pattern matched against the window's title, or [`MATCH_ANY`].
    pub name: String,
    /// What to do to a window this rule matches.
    pub consequence: RuleConsequence,
    /// If `true`, this rule is removed after it matches once.
    pub one_shot: bool,
}

impl Rule {
    /// `true` if `class`, `instance` and `title` all satisfy this rule's
    /// patterns.
    ///
    /// bspwm matches each field with plain string equality, not a glob:
    /// the only wildcard is the literal pattern `"*"`, matched with
    /// `streq(pattern, MATCH_ANY)` before ever comparing to the window's
    /// value (`src/rule.c` `apply_rules()`). A pattern of `"Foo*"` matches
    /// the literal string `"Foo*"` and nothing else.
    pub fn matches(&self, class: &str, instance: &str, title: &str) -> bool {
        Self::field_matches(&self.class_name, class)
            && Self::field_matches(&self.instance_name, instance)
            && Self::field_matches(&self.name, title)
    }

    fn field_matches(pattern: &str, value: &str) -> bool {
        pattern == MATCH_ANY || pattern == value
    }
}

/// What a matched rule does to a window.
///
/// bspwm: `src/types.h` `rule_consequence_t`. `monitor_desc`/
/// `desktop_desc`/`node_desc` (raw selector strings) and `honor_size_hints`
/// are left for `bsp-ipc`, which owns selector parsing;
/// `manage`/`focus`/`border`/`center`/`follow` (all plain bools in bspwm)
/// are included since they need no parsing.
#[derive(Debug, Clone, Default)]
pub struct RuleConsequence {
    /// Force a split direction for the window's insertion.
    pub split_dir: Option<Direction>,
    /// Force a split ratio for the window's insertion.
    pub split_ratio: Option<f64>,
    /// Force a stacking layer.
    pub layer: Option<Layer>,
    /// Force a tiling state.
    pub state: Option<ClientState>,
    /// Force the hidden flag.
    pub hidden: Option<bool>,
    /// Force the sticky flag.
    pub sticky: Option<bool>,
    /// Force the private flag.
    pub private: Option<bool>,
    /// Force the locked flag.
    pub locked: Option<bool>,
    /// Force the marked flag.
    pub marked: Option<bool>,
    /// Center the window (only meaningful together with `state:
    /// Floating`).
    pub center: bool,
    /// Focus the desktop the window is inserted into.
    pub follow: bool,
    /// `false` rejects the window outright (bspwm: `manage`).
    pub manage: bool,
    /// Focus the window once it is managed.
    pub focus: bool,
    /// Draw a border around the window.
    pub border: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(class: &str, instance: &str, name: &str) -> Rule {
        Rule {
            class_name: class.to_string(),
            instance_name: instance.to_string(),
            name: name.to_string(),
            consequence: RuleConsequence::default(),
            one_shot: false,
        }
    }

    #[test]
    fn exact_match_on_every_field() {
        let r = rule("Firefox", "Navigator", "Mozilla Firefox");
        assert!(r.matches("Firefox", "Navigator", "Mozilla Firefox"));
        assert!(!r.matches("firefox", "Navigator", "Mozilla Firefox"));
    }

    #[test]
    fn wildcard_field_matches_anything() {
        let r = rule("Firefox", MATCH_ANY, MATCH_ANY);
        assert!(r.matches("Firefox", "anything", "any title"));
        assert!(!r.matches("Chromium", "anything", "any title"));
    }

    #[test]
    fn wildcard_is_not_a_glob() {
        // bspwm: src/rule.c apply_rules() compares with streq(), so a
        // pattern of "Foo*" is the literal string "Foo*", not a prefix
        // match.
        let r = rule("Foo*", MATCH_ANY, MATCH_ANY);
        assert!(!r.matches("Foobar", "x", "y"));
        assert!(r.matches("Foo*", "x", "y"));
    }
}
