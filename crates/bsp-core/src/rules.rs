//! `bspc rule` entries and matching a client's class/instance/title
//! against them.
//!
//! bspwm: `src/types.h` `rule_t`, `src/rule.c`. [`match_rules`] reproduces
//! `apply_rules()`'s matching-and-merging loop in full — still pure, since
//! it only ever produces a [`RuleConsequence`], not a mutated window.
//! Applying that consequence to a real window (inserting a tree node,
//! configuring a `Window`) needs an adapter, so it belongs to
//! `bsp-compositor`; `_apply_class`/`_apply_hints`/the
//! `external_rules_command` hook (populating `class`/`instance`/`title`
//! from the window itself before matching) belong there too.

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
    /// The `key=value ...` effect string this rule was added with, kept
    /// verbatim for `rule --list` (bspwm: `rule_t.effect`, `src/rule.h`,
    /// printed back unparsed by `list_rules()`). Opaque to `bsp-core`: it
    /// is not reparsed here, only carried.
    pub effect_raw: String,
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

/// Matches `class`/`instance`/`title` against every rule in `rules`, in
/// order, merging each match's consequence onto one accumulator.
///
/// bspwm: `src/rule.c` `apply_rules()`'s loop. It merges *every* matching
/// rule's effect into one `rule_consequence_t`, but stops immediately
/// after removing a one-shot match — even if a later rule would also
/// match — reproduced here as-is (a real bspwm quirk, not "fixed",
/// since this crate's job is to match bspwm's behavior).
pub fn match_rules(
    rules: &mut Vec<Rule>,
    class: &str,
    instance: &str,
    title: &str,
) -> RuleConsequence {
    let mut consequence = RuleConsequence::default();
    let mut i = 0;
    while i < rules.len() {
        if rules[i].matches(class, instance, title) {
            consequence.merge(&rules[i].consequence);
            if rules[i].one_shot {
                rules.remove(i);
                break;
            }
        }
        i += 1;
    }
    consequence
}

/// What a matched rule does to a window.
///
/// bspwm: `src/types.h` `rule_consequence_t`. `monitor_desc`/
/// `desktop_desc`/`node_desc` (raw selector strings) and `honor_size_hints`
/// are left for `bsp-ipc`, which owns selector parsing;
/// `manage`/`focus`/`border`/`center`/`follow` (all plain bools in bspwm)
/// are included since they need no parsing.
///
/// `manage`/`focus`/`border` are `Option<bool>`, not plain `bool` like
/// bspwm's C struct: `None` means "not mentioned by this rule", distinct
/// from an explicit `false`. bspwm can get away with a plain `bool` because
/// `make_rule_consequence()` (`src/rule.c`) pre-seeds a *fresh*, per-window
/// accumulator with `manage = focus = border = true` before merging in any
/// matched rule's `key=value` tokens, and `parse_key_value()` only ever
/// writes a field a token actually names — so an unmentioned field keeps
/// that `true` default. A `Rule`'s own stored `consequence`, here, has no
/// such per-window accumulator to fall back on: it must be able to say "I
/// don't touch this field" so [`RuleConsequence::merge`] can tell that
/// apart from "I explicitly turn this off".
#[derive(Debug, Clone, Default, PartialEq)]
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
    /// `Some(false)` rejects the window outright (bspwm: `manage`);
    /// unmentioned (`None`) defaults to `true`, like every window that
    /// matches no rule at all.
    pub manage: Option<bool>,
    /// Focus the window once it is managed; unmentioned defaults to
    /// `true`.
    pub focus: Option<bool>,
    /// Draw a border around the window; unmentioned defaults to `true`.
    pub border: Option<bool>,
}

impl RuleConsequence {
    /// Merges `other` on top of `self`: every field `other` sets
    /// overwrites `self`'s, and every field `other` leaves unset is kept
    /// as-is. `center`/`follow` are plain `bool`s that only ever turn a
    /// flag on (bspwm never has a rule turn them back off relative to an
    /// earlier match), so `other`'s `true` wins but its `false` does not
    /// clear a `true` already set.
    ///
    /// Mirrors applying successive matching rules' `key=value` tokens onto
    /// one shared accumulator (bspwm: `src/rule.c` `apply_rules()`'s loop
    /// over `rule_head`, each iteration calling `parse_keys_values()` on
    /// the same `csq`).
    pub fn merge(&mut self, other: &RuleConsequence) {
        macro_rules! take_some {
            ($field:ident) => {
                if other.$field.is_some() {
                    self.$field = other.$field;
                }
            };
        }
        take_some!(split_dir);
        take_some!(split_ratio);
        take_some!(layer);
        take_some!(state);
        take_some!(hidden);
        take_some!(sticky);
        take_some!(private);
        take_some!(locked);
        take_some!(marked);
        take_some!(manage);
        take_some!(focus);
        take_some!(border);
        self.center |= other.center;
        self.follow |= other.follow;
    }

    /// Whether a window this consequence applies to should be managed
    /// (inserted into the tree) at all. `true` unless a rule explicitly
    /// set `manage=off` (bspwm: `make_rule_consequence()`'s `manage =
    /// true` default, `src/rule.c`).
    pub fn should_manage(&self) -> bool {
        self.manage.unwrap_or(true)
    }

    /// Whether a window this consequence applies to should be focused
    /// once managed. `true` unless a rule explicitly set `focus=off`.
    pub fn should_focus(&self) -> bool {
        self.focus.unwrap_or(true)
    }

    /// Whether a window this consequence applies to should be bordered.
    /// `true` unless a rule explicitly set `border=off`.
    pub fn should_border(&self) -> bool {
        self.border.unwrap_or(true)
    }
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
            effect_raw: String::new(),
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

    #[test]
    fn unset_manage_focus_border_default_to_true() {
        let c = RuleConsequence::default();
        assert!(c.should_manage());
        assert!(c.should_focus());
        assert!(c.should_border());
    }

    #[test]
    fn explicit_false_overrides_the_true_default() {
        let mut c = RuleConsequence::default();
        c.merge(&RuleConsequence {
            manage: Some(false),
            ..Default::default()
        });
        assert!(!c.should_manage());
        // Unmentioned fields are untouched by the merge.
        assert!(c.should_focus());
        assert!(c.should_border());
    }

    #[test]
    fn merge_lets_a_later_rule_overwrite_an_earlier_ones_field() {
        let mut c = RuleConsequence {
            state: Some(ClientState::Floating),
            ..Default::default()
        };
        c.merge(&RuleConsequence {
            state: Some(ClientState::Tiled),
            follow: true,
            ..Default::default()
        });
        assert_eq!(c.state, Some(ClientState::Tiled));
        assert!(c.follow);
    }

    fn rule_with(class: &str, one_shot: bool, consequence: RuleConsequence) -> Rule {
        Rule {
            class_name: class.to_string(),
            instance_name: MATCH_ANY.to_string(),
            name: MATCH_ANY.to_string(),
            consequence,
            one_shot,
            effect_raw: String::new(),
        }
    }

    #[test]
    fn match_rules_merges_every_matching_rule_in_order() {
        let mut rules = vec![
            rule_with(
                "Firefox",
                false,
                RuleConsequence {
                    state: Some(ClientState::Floating),
                    ..Default::default()
                },
            ),
            rule_with(
                "Firefox",
                false,
                RuleConsequence {
                    sticky: Some(true),
                    ..Default::default()
                },
            ),
            rule_with(
                "Chromium",
                false,
                RuleConsequence {
                    sticky: Some(false),
                    ..Default::default()
                },
            ),
        ];
        let c = match_rules(&mut rules, "Firefox", "x", "y");
        assert_eq!(c.state, Some(ClientState::Floating));
        assert_eq!(c.sticky, Some(true));
        assert_eq!(rules.len(), 3, "no rule here is one-shot");
    }

    #[test]
    fn match_rules_removes_a_one_shot_match_and_stops_there() {
        // bspwm: `apply_rules()` breaks out of the loop right after
        // removing a one-shot match, so a later rule that would also
        // match never gets a chance to.
        let mut rules = vec![
            rule_with(
                "Firefox",
                true,
                RuleConsequence {
                    state: Some(ClientState::Floating),
                    ..Default::default()
                },
            ),
            rule_with(
                MATCH_ANY,
                false,
                RuleConsequence {
                    sticky: Some(true),
                    ..Default::default()
                },
            ),
        ];
        let c = match_rules(&mut rules, "Firefox", "x", "y");
        assert_eq!(c.state, Some(ClientState::Floating));
        assert_eq!(c.sticky, None, "the wildcard rule never ran");
        assert_eq!(rules.len(), 1, "the one-shot rule was removed");
    }

    #[test]
    fn match_rules_ignores_non_matching_rules() {
        let mut rules = vec![rule_with(
            "Chromium",
            false,
            RuleConsequence {
                state: Some(ClientState::Floating),
                ..Default::default()
            },
        )];
        let c = match_rules(&mut rules, "Firefox", "x", "y");
        assert_eq!(c.state, None);
        assert_eq!(rules.len(), 1);
    }
}
