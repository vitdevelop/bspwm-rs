//! Expands `{...}` sequences (comma lists, `a-z` ranges, `_` as an
//! explicit no-op branch) in a [`crate::lexer::RawBinding`]'s chain and
//! command text into concrete pairs, zipping the two sides index for
//! index rather than taking their independent cross product.
//!
//! bspwm's sxhkd: `src/parse.c` `process_hotkey()` (the driving loop),
//! `extract_chunks()` (splitting text into literal/sequence chunks) and
//! `render_next()` (stepping every sequence chunk's cursor by exactly
//! one combination per call, `crate::token::get_token`'s `SEQ_SEP`
//! splitting the raw comma list).

use crate::token::get_token;

/// One fully expanded (chain, command) pair, ready for a chord parser
/// (not yet implemented, `docs/bsp-hotkeys.md`'s `binding` module).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedBinding {
    /// The expanded chord chain text.
    pub chain: String,
    /// The expanded command text.
    pub command: String,
}

/// Expands every `{...}` sequence in `chain` and `command`, zipping the
/// two expansions index for index (bspwm: `process_hotkey()`'s loop
/// calls `render_next()` on both sides once per iteration, not as an
/// independent cross product between them). A side with no `{}` at all
/// stays constant across every emitted pair.
pub fn expand(chain: &str, command: &str) -> Vec<ExpandedBinding> {
    let mut chain_chunks = extract_chunks(chain);
    let mut command_chunks = extract_chunks(command);

    // bspwm: `process_hotkey()`'s `CHECKCHUNK` macro. A side with
    // exactly one, non-sequence chunk is detached from `render_next`
    // entirely and used verbatim on every iteration, rather than being
    // "expanded" into a one-element cycle that would immediately
    // terminate the whole zip.
    let chain_const = as_constant(&chain_chunks);
    let command_const = as_constant(&command_chunks);

    let mut current_chain = match &chain_const {
        Some(text) => text.clone(),
        None => render_next(&mut chain_chunks).unwrap_or_default(),
    };
    let mut current_command = match &command_const {
        Some(text) => text.clone(),
        None => render_next(&mut command_chunks).unwrap_or_default(),
    };

    let mut bindings = Vec::new();
    loop {
        let chain_ready = chain_const.is_some() || !current_chain.is_empty();
        let command_ready = command_const.is_some() || !current_command.is_empty();
        if !chain_ready || !command_ready {
            break;
        }
        bindings.push(ExpandedBinding {
            chain: current_chain.clone(),
            command: current_command.clone(),
        });

        // Both sides constant: nothing left to iterate (bspwm:
        // `process_hotkey()`'s `if (hk_chunks == NULL && cm_chunks ==
        // NULL) break;`, checked right after emitting one binding).
        if chain_const.is_some() && command_const.is_some() {
            break;
        }
        if chain_const.is_none() {
            current_chain = render_next(&mut chain_chunks).unwrap_or_default();
        }
        if command_const.is_none() {
            current_command = render_next(&mut command_chunks).unwrap_or_default();
        }
    }
    bindings
}

/// `Some(text)` if `chunks` is a single literal chunk (or empty, which
/// renders as an empty string) — a side with no `{}` at all, which never
/// changes across iterations. `None` otherwise (needs `render_next`).
fn as_constant(chunks: &[Chunk]) -> Option<String> {
    match chunks {
        [] => Some(String::new()),
        [Chunk::Literal(text)] => Some(text.clone()),
        _ => None,
    }
}

/// One piece of a chain/command string, split at `{`/`}` boundaries.
///
/// bspwm: `chunk_t` (`src/parse.h`), minus `next` (a `Vec` holds the
/// list here) and `repr` (bspwm's own `subscribe`-status debug field).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Chunk {
    /// Verbatim text, emitted unchanged on every call.
    Literal(String),
    /// A `{...}` group's raw, not-yet-split comma list, plus its
    /// odometer cursor state.
    Sequence(SequenceChunk),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SequenceChunk {
    /// The raw text between `{` and `}`, comma-separated, never
    /// mutated after creation.
    text: String,
    /// The current comma-separated item (bspwm: `chunk_t.item`).
    item: String,
    /// The unconsumed remainder of `text` after `item`; `None` means no
    /// item has been pulled yet (bspwm: `chunk_t.advance == NULL`).
    advance: Option<String>,
    /// A char-range's current/max character, as `u32` (so `range_max`
    /// can validly be less than any real char, matching bspwm's `1 > 0`
    /// sentinel meaning "no active range"). bspwm: `chunk_t.range_cur`/
    /// `range_max` (`char`s there; `char` doesn't have a clean "no
    /// range" sentinel in Rust the way `1 > 0` does in C, hence `u32`).
    range_cur: u32,
    range_max: u32,
}

impl SequenceChunk {
    fn new(text: String) -> Self {
        Self {
            text,
            item: String::new(),
            advance: None,
            range_cur: 1,
            range_max: 0,
        }
    }

    /// Advances this chunk's cursor by exactly one step if `*incr` is
    /// still `false` (nothing has advanced yet this call) and this
    /// chunk itself has more to give; sets `*incr = true` whenever it
    /// consumes that "one advance" budget. Never touches `*incr` when
    /// it only wraps back around to its first item, letting a
    /// left-adjacent — checked next, since chunks are visited in the
    /// order they appear in the original string — chunk claim the
    /// budget instead.
    ///
    /// This makes the *leftmost* `{}` group in a string the
    /// fastest-changing one and the rightmost the slowest, the reverse
    /// of a typical positional-number odometer but exactly bspwm's
    /// behavior (`render_next()`, reproduced field-for-field, branch
    /// for branch).
    fn step(&mut self, incr: &mut bool) {
        if !*incr {
            if self.range_cur < self.range_max {
                self.range_cur += 1;
                *incr = true;
            } else {
                self.range_cur = 1;
                self.range_max = 0;
            }
        }
        match &self.advance {
            None => {
                *incr = true;
                let (item, _, rest) = get_token(&self.text, &[',']);
                self.item = item;
                self.advance = Some(rest);
            }
            Some(rest) if !*incr && self.range_cur > self.range_max => {
                if rest.is_empty() {
                    let (item, _, rest) = get_token(&self.text, &[',']);
                    self.item = item;
                    self.advance = Some(rest);
                } else {
                    let (item, _, rest) = get_token(rest, &[',']);
                    self.item = item;
                    self.advance = Some(rest);
                    *incr = true;
                }
            }
            _ => {}
        }
        if self.range_cur > self.range_max {
            let chars: Vec<char> = self.item.chars().collect();
            if chars.len() == 3 && chars[1] == '-' {
                self.range_cur = chars[0] as u32;
                self.range_max = chars[2] as u32;
            }
        }
    }

    /// This chunk's contribution to the current call's output, or
    /// `None` if it is the `_` (`SEQ_NONE`) no-op placeholder.
    fn current(&self) -> Option<String> {
        if self.range_cur <= self.range_max {
            Some(char::from_u32(self.range_cur).unwrap_or('?').to_string())
        } else if self.item == "_" {
            None
        } else {
            Some(self.item.clone())
        }
    }
}

/// Splits `text` into literal and `{}`-sequence chunks, in order.
///
/// bspwm: `src/parse.c` `extract_chunks()`. `\{`, `\}` and, inside a
/// sequence, `\\` escape to a literal character rather than opening/
/// closing a sequence or escaping the following character; everywhere
/// else `\` plus any other character keeps both characters verbatim
/// (this module has no other use for `\`, since the comma separator
/// `get_token`-escapes elsewhere is handled by [`get_token`], not here).
fn extract_chunks(s: &str) -> Vec<Chunk> {
    let mut chunks = Vec::new();
    let mut buf = String::new();
    let mut in_sequence = false;
    let mut inhibit = false;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if inhibit {
            buf.push(c);
            inhibit = false;
        } else if c == '\\' {
            inhibit = true;
            let next = chars.get(i + 1).copied();
            let escapes_delimiter = matches!(next, Some('{') | Some('}'));
            let escapes_backslash = next == Some('\\') && !in_sequence;
            if !escapes_delimiter && !escapes_backslash {
                buf.push(c);
            }
        } else if c == '{' {
            if !buf.is_empty() {
                chunks.push(finish_chunk(std::mem::take(&mut buf), in_sequence));
            }
            in_sequence = true;
        } else if c == '}' {
            if !buf.is_empty() {
                chunks.push(finish_chunk(std::mem::take(&mut buf), in_sequence));
            }
            in_sequence = false;
        } else {
            buf.push(c);
        }
        i += 1;
    }
    if !buf.is_empty() {
        chunks.push(finish_chunk(buf, in_sequence));
    }
    chunks
}

fn finish_chunk(text: String, sequence: bool) -> Chunk {
    if sequence {
        Chunk::Sequence(SequenceChunk::new(text))
    } else {
        Chunk::Literal(text)
    }
}

/// Steps every chunk in `chunks` by exactly one combination and renders
/// the result, or `None` once every sequence chunk in the list has
/// cycled back to its start in the same call (bspwm: `render_next()`,
/// `dest[0] = '\0'` when `!incr`).
fn render_next(chunks: &mut [Chunk]) -> Option<String> {
    let mut incr = false;
    let mut out = String::new();
    for chunk in chunks.iter_mut() {
        match chunk {
            Chunk::Sequence(seq) => {
                seq.step(&mut incr);
                if let Some(text) = seq.current() {
                    out.push_str(&text);
                }
            }
            Chunk::Literal(text) => out.push_str(text),
        }
    }
    if incr {
        Some(out)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_braces_on_either_side_yields_one_unchanged_pair() {
        let out = expand("super + w", "firefox");
        assert_eq!(
            out,
            vec![ExpandedBinding {
                chain: "super + w".to_string(),
                command: "firefox".to_string(),
            }]
        );
    }

    #[test]
    fn a_comma_list_zips_index_for_index_with_the_other_side() {
        let out = expand("super + {h,j,k,l}", "bspc node -f {west,south,north,east}");
        let pairs: Vec<(&str, &str)> = out
            .iter()
            .map(|b| (b.chain.as_str(), b.command.as_str()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("super + h", "bspc node -f west"),
                ("super + j", "bspc node -f south"),
                ("super + k", "bspc node -f north"),
                ("super + l", "bspc node -f east"),
            ]
        );
    }

    #[test]
    fn one_constant_side_stays_fixed_across_every_expansion() {
        let out = expand("super + {h,j,k,l}", "bspc node -p west");
        assert_eq!(out.len(), 4);
        assert!(out.iter().all(|b| b.command == "bspc node -p west"));
    }

    #[test]
    fn two_groups_on_one_side_produce_their_full_cartesian_product() {
        // bspwm's odometer increments the LEFTMOST group fastest (the
        // reverse of typical positional-number nesting): {a,b} cycles
        // once per call, {1,2,3} only once {a,b} wraps.
        let out = expand("{a,b} + {1,2,3}", "cmd");
        let chains: Vec<&str> = out.iter().map(|b| b.chain.as_str()).collect();
        assert_eq!(
            chains,
            vec!["a + 1", "b + 1", "a + 2", "b + 2", "a + 3", "b + 3"]
        );
    }

    #[test]
    fn a_char_range_expands_one_character_at_a_time() {
        let out = expand("super + {a-c}", "cmd");
        let chains: Vec<&str> = out.iter().map(|b| b.chain.as_str()).collect();
        assert_eq!(chains, vec!["super + a", "super + b", "super + c"]);
    }

    #[test]
    fn underscore_is_a_no_op_branch() {
        let out = expand("{_,shift +} w", "cmd");
        let chains: Vec<&str> = out.iter().map(|b| b.chain.as_str()).collect();
        assert_eq!(chains, vec![" w", "shift + w"]);
    }

    #[test]
    fn escaped_braces_are_literal_and_not_a_sequence() {
        let out = expand(r"echo \{literal\}", "cmd");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].chain, "echo {literal}");
    }

    #[test]
    fn mismatched_group_counts_stop_at_the_shorter_side() {
        // bspwm quirk, reproduced rather than "fixed" (`docs/bsp-hotkeys.md`):
        // once either side's own `render_next` call fails to advance,
        // the whole zip stops, even if the other side had more left.
        let out = expand("{a,b,c}", "{x,y}");
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn both_sides_constant_yields_exactly_one_pair() {
        let out = expand("super + w", "firefox");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn empty_strings_expand_to_one_empty_pair() {
        let out = expand("", "");
        assert_eq!(
            out,
            vec![ExpandedBinding {
                chain: String::new(),
                command: String::new(),
            }]
        );
    }
}
