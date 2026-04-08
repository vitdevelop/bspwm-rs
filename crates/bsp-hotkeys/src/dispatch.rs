//! Classifies a binding's already-expanded command text
//! ([`crate::expand::ExpandedBinding::command`]): a plain `bspc` call
//! with only literal arguments can skip a process spawn entirely and go
//! straight into `bsp-ipc`'s command parser; anything else needs a real
//! shell.
//!
//! bspwm has no equivalent here — sxhkd always spawns `sh -c` for every
//! single binding, no exceptions (`src/helpers.c` `run()`). This module
//! implements `docs/bsp-hotkeys.md`'s own "Binding execution" decision
//! instead, not a ported bspwm behavior: skip spawning `sh` and `bspc`
//! on every press of a frequent key, and apply commands in key-press
//! order with no process races.
//!
//! This crate only classifies and tokenizes; it does not depend on
//! `bsp-ipc` and does not dispatch anything itself. Actually running
//! the in-process path (handing [`Dispatch::InlineBspc`]'s tokens to
//! `bsp_ipc::command::parse`) is `bsp-compositor`'s job, the one crate
//! that already depends on both (`docs/design.md` Architecture: this
//! crate's only dependency is `xkbcommon`).

/// Where a binding's command should run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    /// A `bspc` call with only literal arguments, already split into
    /// words — ready for `bsp_ipc::command::parse`.
    InlineBspc(Vec<String>),
    /// Needs a real shell: not a `bspc` call, uses a shell feature, or
    /// the tokenizer could not confidently split it (an unbalanced
    /// quote) — kept as the original, unmodified text for `sh -c`.
    Shell(String),
}

/// Classifies `command`.
///
/// Eligible for [`Dispatch::InlineBspc`] only when *every* condition
/// holds: the first word is exactly `bspc` (not a path to it, and not
/// `bspc-rs` — matching what a real, shared `sxhkdrc` actually
/// contains, `docs/migrating.md`), no *unquoted* shell metacharacter
/// appears anywhere in the rest, and every quote is balanced. A
/// metacharacter inside a quoted argument (`bspc rule -a "Firefox:*:*"
/// …`) does not disqualify it — a real shell would treat it as inert
/// there too.
pub fn classify(command: &str) -> Dispatch {
    if !starts_with_bare_bspc(command) {
        return Dispatch::Shell(command.to_string());
    }
    match tokenize(command) {
        Some(tokens) => Dispatch::InlineBspc(tokens),
        None => Dispatch::Shell(command.to_string()),
    }
}

fn starts_with_bare_bspc(command: &str) -> bool {
    command
        .split_whitespace()
        .next()
        .is_some_and(|first| first == "bspc")
}

/// Splits `s` on unquoted whitespace, honoring single and double
/// quotes (stripped from the output, no escaping inside them — bspc
/// arguments never need it), and bails to `None` ("needs a shell") the
/// moment an unquoted shell metacharacter or an unbalanced quote is
/// found. Deliberately conservative: a missed optimization is harmless
/// (the shell path still works), a wrong split is not.
fn tokenize(s: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;

    for c in s.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    current.push(c);
                }
            }
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                in_token = true;
            }
            None if c.is_whitespace() => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
            }
            None if is_shell_metacharacter(c) => return None,
            None => {
                current.push(c);
                in_token = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_token {
        tokens.push(current);
    }
    Some(tokens)
}

/// Characters that change a POSIX shell's behavior when unquoted:
/// variable/command substitution (`$`, `` ` ``), subshells (`(`/`)`),
/// background/sequencing/piping (`&`/`;`/`|`), redirects (`<`/`>`),
/// multiple commands (newline), home-directory expansion (`~`),
/// globbing (`*`/`?`/`[`), and escaping (`\`) — anything that could
/// make the literally-typed argument mean something other than itself.
fn is_shell_metacharacter(c: char) -> bool {
    matches!(
        c,
        '$' | '`' | '(' | ')' | '&' | '|' | ';' | '<' | '>' | '\n' | '~' | '*' | '?' | '[' | '\\'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_literal_bspc_call_is_split_for_inline_dispatch() {
        assert_eq!(
            classify("bspc node -f west"),
            Dispatch::InlineBspc(vec![
                "bspc".to_string(),
                "node".to_string(),
                "-f".to_string(),
                "west".to_string(),
            ])
        );
    }

    #[test]
    fn a_quoted_argument_with_spaces_becomes_one_token() {
        assert_eq!(
            classify(r#"bspc rule -a "Firefox:*:*" state=floating"#),
            Dispatch::InlineBspc(vec![
                "bspc".to_string(),
                "rule".to_string(),
                "-a".to_string(),
                "Firefox:*:*".to_string(),
                "state=floating".to_string(),
            ])
        );
    }

    #[test]
    fn a_metacharacter_inside_quotes_does_not_force_a_shell() {
        // The `*` above is inside double quotes; a real shell would not
        // glob it there either.
        assert!(matches!(
            classify(r#"bspc rule -a "Firefox:*:*" state=floating"#),
            Dispatch::InlineBspc(_)
        ));
    }

    #[test]
    fn an_unquoted_shell_feature_falls_back_to_a_shell() {
        let cmd = "bspc node -f west || bspc monitor -f west";
        assert_eq!(classify(cmd), Dispatch::Shell(cmd.to_string()));
    }

    #[test]
    fn anything_that_is_not_bspc_falls_back_to_a_shell() {
        assert_eq!(
            classify("alacritty"),
            Dispatch::Shell("alacritty".to_string())
        );
    }

    #[test]
    fn bspc_rs_is_not_treated_as_bspc() {
        // Real sxhkdrc files say `bspc`, matching stock bspwm — not the
        // path to this project's own client (`docs/migrating.md`).
        let cmd = "bspc-rs node -f west";
        assert_eq!(classify(cmd), Dispatch::Shell(cmd.to_string()));
    }

    #[test]
    fn an_unbalanced_quote_safely_falls_back_to_a_shell() {
        let cmd = r#"bspc rule -a "Firefox state=floating"#;
        assert_eq!(classify(cmd), Dispatch::Shell(cmd.to_string()));
    }

    #[test]
    fn single_quotes_work_like_double_quotes() {
        assert_eq!(
            classify("bspc rule -a 'Firefox:*:*'"),
            Dispatch::InlineBspc(vec![
                "bspc".to_string(),
                "rule".to_string(),
                "-a".to_string(),
                "Firefox:*:*".to_string(),
            ])
        );
    }

    #[test]
    fn empty_command_falls_back_to_a_shell() {
        assert_eq!(classify(""), Dispatch::Shell(String::new()));
    }

    #[test]
    fn dollar_variable_expansion_falls_back_to_a_shell() {
        let cmd = "bspc node -f $DIR";
        assert_eq!(classify(cmd), Dispatch::Shell(cmd.to_string()));
    }
}
