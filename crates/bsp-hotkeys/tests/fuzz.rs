//! Property-based fuzzing of the sxhkdrc parser (design.md, hardening):
//! arbitrary file contents must never panic, and brace expansion must
//! stay bounded.

use bsp_hotkeys::{config, dispatch, expand, lexer};
use proptest::prelude::*;

const PIECES: &[&str] = &[
    "super", "alt", "ctrl", "shift", "mod4", "hyper", "{", "}", "{a,b,c}", "{1-9}", "{,shift + }",
    "{,}", "{a-", "-}", "_", "@", "~", ":", ";", "+", " + ", "Return", "space", "XF86AudioMute",
    "button1", "\n", "\n\t", "\\\n", "#", "# comment\n", "bspc node -f {west,east}", "bspc desktop -f ^{1-9}",
    "\\", "\\{", "\u{0}", "é", "  ", "{{a,b},{c,d}}", "}{", "{a,b", "a,b}",
];

fn arb_text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![
            8 => prop::sample::select(PIECES).prop_map(String::from),
            1 => ".{0,8}",
        ],
        0..24,
    )
    .prop_map(|v| v.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn lexer_never_panics(s in arb_text()) {
        let _ = lexer::parse(&s);
    }

    #[test]
    fn expand_never_panics(chain in arb_text(), cmd in arb_text()) {
        let _ = expand::expand(&chain, &cmd);
    }

    #[test]
    fn load_never_panics(s in arb_text()) {
        let _ = config::load(&s);
    }

    #[test]
    fn classify_never_panics(s in arb_text()) {
        let _ = dispatch::classify(&s);
    }

    #[test]
    fn parse_chain_never_panics(s in arb_text()) {
        let _ = bsp_hotkeys::binding::parse_chain(&s);
    }
}
