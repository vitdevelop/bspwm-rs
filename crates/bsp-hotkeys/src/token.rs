//! A small, generic token splitter shared by [`crate::expand`] (splitting
//! a `{}` sequence's raw comma list) and [`crate::binding`] (splitting a
//! chain into chords, and a chord into modifier/keysym names).
//!
//! bspwm's sxhkd: `src/parse.c` `get_token()`.

/// Splits the next token off the front of `src` at the first run of one
/// or more characters in `sep`, honoring `\`-escaping: `\` before a
/// separator or another `\` drops the backslash but keeps the following
/// character literally (escaping it from being treated as a
/// separator); `\` before anything else keeps both characters.
/// Consecutive separator characters count as a single boundary.
///
/// Returns `(token, consumed_separators, rest)`. `consumed_separators`
/// is every separator character actually consumed between this token
/// and the next (bspwm's `ign` output parameter — `binding`'s outer
/// chain-to-chords split uses it to detect a `:` among `;`/`:` and set
/// a chord's `lock_chain`; every other caller ignores it).
///
/// bspwm: `src/parse.c` `get_token()`. Returns owned `String`s rather
/// than writing into caller-sized buffers and returning a pointer,
/// since this build has no fixed `MAXLEN` to size those buffers to.
pub(crate) fn get_token(src: &str, sep: &[char]) -> (String, String, String) {
    let chars: Vec<char> = src.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut inhibit = false;
    let mut found = false;
    let mut dst = String::new();
    let mut ign = String::new();
    while i < len && !found {
        let c = chars[i];
        if inhibit {
            dst.push(c);
            inhibit = false;
        } else if c == '\\' {
            inhibit = true;
            let next = chars.get(i + 1).copied();
            if next != Some('\\') && !next.is_some_and(|n| sep.contains(&n)) {
                dst.push(c);
            }
        } else if sep.contains(&c) {
            if !dst.is_empty() {
                found = true;
            }
            while i < len && sep.contains(&chars[i]) {
                ign.push(chars[i]);
                i += 1;
            }
            i -= 1;
        } else {
            dst.push(c);
        }
        i += 1;
    }
    let rest: String = chars[i..].iter().collect();
    (dst, ign, rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_separators_and_skips_runs_of_them() {
        let (tok, ign, rest) = get_token("a,,b", &[',']);
        assert_eq!(tok, "a");
        assert_eq!(ign, ",,");
        assert_eq!(rest, "b");
    }

    #[test]
    fn no_separator_present_consumes_everything() {
        let (tok, ign, rest) = get_token("only", &[',']);
        assert_eq!(tok, "only");
        assert_eq!(ign, "");
        assert_eq!(rest, "");
    }

    #[test]
    fn backslash_escapes_a_separator() {
        let (tok, _, rest) = get_token(r"a\,b,c", &[',']);
        assert_eq!(tok, "a,b");
        assert_eq!(rest, "c");
    }

    #[test]
    fn ign_records_every_consumed_separator_character() {
        let (tok, ign, rest) = get_token("a;:b", &[';', ':']);
        assert_eq!(tok, "a");
        assert_eq!(ign, ";:");
        assert_eq!(rest, "b");
    }
}
