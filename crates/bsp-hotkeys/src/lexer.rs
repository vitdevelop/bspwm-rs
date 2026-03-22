//! Groups an sxhkdrc file's lines into raw (chain, command) pairs, before
//! any `{}`/range expansion ([`crate::expand`]) or chord parsing.
//!
//! bspwm's sxhkd: `src/parse.c` `load_config()`, minus the `fopen`/`fgets`
//! I/O (this module takes the whole file's contents as an already-read
//! `&str`; reading the file is `bsp-compositor`'s job) and minus
//! `process_hotkey()`'s call into it (kept in `expand`/`binding`, which
//! own what happens to a grouped pair next).

/// One hotkey definition block, still in raw (un-expanded) form: every
/// `{...}` group in `chain`/`command` is exactly as the user wrote it.
///
/// bspwm: the `hotkey`/`command` local buffers in `load_config()`, the
/// moment right before `process_hotkey()` is called on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawBinding {
    /// The chord chain text (left of the line, no leading whitespace):
    /// e.g. `super + {h,j,k,l}`, or a chain of chords joined by `;`/`:`.
    pub chain: String,
    /// The command text (from an indented line): a shell command, or a
    /// literal `bspc` call.
    pub command: String,
}

/// Groups `contents` (a whole sxhkdrc file) into raw (chain, command)
/// pairs.
///
/// bspwm's grammar, reproduced exactly:
/// - A line is a comment, and ignored entirely, only if its very first
///   character (before any trimming) is `#`.
/// - A line whose first character is *not* whitespace is a chain line;
///   a line whose first character *is* whitespace is a command line
///   (the leading whitespace itself is only a marker — indentation
///   depth is not preserved, and does not need to be consistent).
/// - Trailing whitespace on every line, and leading whitespace on a
///   command line, is trimmed before use.
/// - A line ending (after trimming) in `\` continues its own category's
///   buffer onto the next physical line, verbatim (including the `\`
///   itself: whether that produces an escaped separator or a literal
///   backslash in the final chain/command text is for `expand`'s
///   `get_token` to decide, exactly as in bspwm) — it does *not* yet
///   trigger emitting a binding. The continuation line's own first
///   character still decides its category, same as any other line: an
///   indented continuation of a chain line would misclassify as a
///   command line instead (a real bspwm/sxhkd gotcha, reproduced here
///   rather than smoothed over).
/// - A non-continued chain line *replaces* the current chain buffer
///   (not append); likewise for a command line and the command buffer.
/// - A non-continued command line, if both buffers are non-empty at
///   that point, emits one [`RawBinding`] and clears both buffers.
///
/// bspwm: `src/parse.c` `load_config()`, `src/helpers.c` `lgraph()`/
/// `rgraph()` (leading/trailing-whitespace trimming, reproduced here by
/// `str::trim`, which trims the same set of whitespace `isgraph()`'s
/// complement does for any config file bspwm/sxhkd ever reads).
pub fn parse(contents: &str) -> Vec<RawBinding> {
    let mut bindings = Vec::new();
    let mut chain = String::new();
    let mut command = String::new();
    // Whether the buffer currently being written to is mid-continuation
    // (the previous physical line of the same category ended in `\`).
    // bspwm: the shared `offset` variable in `load_config()`, which a
    // non-continued line of *either* category resets to 0 — meaning a
    // non-continued line always fully replaces its own buffer.
    let mut continuing = false;

    for line in contents.lines() {
        if line.is_empty() {
            continue;
        }
        let first = line.chars().next().expect("checked non-empty above");
        if first == '#' {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let is_chain_line = !first.is_whitespace();

        let target = if is_chain_line {
            &mut chain
        } else {
            &mut command
        };
        if continuing {
            target.push_str(trimmed);
        } else {
            target.clear();
            target.push_str(trimmed);
        }

        if trimmed.ends_with('\\') {
            continuing = true;
            continue;
        }
        continuing = false;

        if !is_chain_line && !chain.is_empty() && !command.is_empty() {
            bindings.push(RawBinding {
                chain: std::mem::take(&mut chain),
                command: std::mem::take(&mut command),
            });
        }
    }
    bindings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chain_line_then_a_command_line_produces_one_binding() {
        let out = parse("super + w\n\tfirefox\n");
        assert_eq!(
            out,
            vec![RawBinding {
                chain: "super + w".to_string(),
                command: "firefox".to_string(),
            }]
        );
    }

    #[test]
    fn indentation_and_trailing_whitespace_are_trimmed() {
        let out = parse("super + w  \n    bspc node -f west   \n");
        assert_eq!(out[0].command, "bspc node -f west");
    }

    #[test]
    fn comment_lines_are_ignored_only_at_column_zero() {
        // bspwm: `load_config()` checks `buf[0] == START_COMMENT` before
        // any trimming — an indented `#` is NOT a comment, it is a
        // (nonsensical, but not special-cased) command line.
        let out = parse("# a real comment\nsuper + w\n\tfirefox\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chain, "super + w");
    }

    #[test]
    fn blank_lines_between_blocks_are_ignored() {
        let out = parse("super + w\n\tfirefox\n\n\nsuper + shift + q\n\tbspc node -c\n");
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].command, "bspc node -c");
    }

    #[test]
    fn a_second_chain_line_with_no_intervening_command_replaces_the_first() {
        // No `\` continuation between the two chain lines: the second
        // one's write starts at offset 0, replacing the first entirely
        // (bspwm: `load_config()`'s shared `offset` reset).
        let out = parse("super + w\nsuper + e\n\tfirefox\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chain, "super + e");
    }

    #[test]
    fn trailing_backslash_continues_a_chain_across_lines() {
        // The continuation line must itself start at column 0 (no
        // leading whitespace): bspwm classifies chain-vs-command by
        // every physical line's own first character, continuation
        // lines included, so an indented continuation would
        // misclassify as a command line instead (a real bspwm/sxhkd
        // gotcha, not a simplification made here).
        let out = parse("super + \\\nw\n\tfirefox\n");
        // The continuation concatenates verbatim, backslash included —
        // matching bspwm exactly; downstream (`expand`'s `get_token`)
        // is what gives that embedded `\` any meaning.
        assert_eq!(out[0].chain, "super + \\w");
    }

    #[test]
    fn trailing_backslash_continues_a_command_across_lines() {
        let out = parse("super + w\n\tbspc node -f west \\\n\t|| bspc monitor -f west\n");
        assert_eq!(
            out[0].command,
            "bspc node -f west \\|| bspc monitor -f west"
        );
    }

    #[test]
    fn two_consecutive_command_lines_with_no_chain_are_both_dropped() {
        // After the first pair is emitted, both buffers are empty; a
        // second command line has no chain to pair with, so it is
        // silently discarded rather than attached to a stale chain.
        let out = parse("super + w\n\tfirefox\n\talacritty\n");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].command, "firefox");
    }

    #[test]
    fn multiple_independent_blocks_each_produce_a_binding() {
        let out = parse(
            "super + w\n\tfirefox\nsuper + shift + q\n\tbspc node -c\nsuper + Return\n\talacritty\n",
        );
        assert_eq!(out.len(), 3);
        assert_eq!(out[2].chain, "super + Return");
        assert_eq!(out[2].command, "alacritty");
    }

    #[test]
    fn empty_input_produces_no_bindings() {
        assert_eq!(parse(""), Vec::new());
    }
}
