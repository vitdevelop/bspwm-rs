//! `bsp-hotkeys`: the bundled sxhkdrc-compatible parser and chord matcher.
//!
//! Hotkeys (`docs/design.md` roadmap) is in progress. Done so far: the
//! pure grammar, matching sxhkd's own (`github.com/baskerville/sxhkd`,
//! tag `0.6.2`) `src/parse.c` byte for byte — line grouping ([`lexer`]),
//! `{}`/range expansion ([`expand`]), and parsing an expanded chain
//! string into modifier/keysym chords ([`binding`], using `xkbcommon`
//! for keysym *name* lookup only — the one dependency `docs/design.md`
//! allows this crate beyond the standard library). Not started: the
//! chord-matching state machine, pointer bindings, and wiring any of it
//! into `bsp-compositor`.
#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod binding;
pub mod expand;
pub mod lexer;
mod token;
