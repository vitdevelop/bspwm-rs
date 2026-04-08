//! `bsp-hotkeys`: the bundled sxhkdrc-compatible parser and chord matcher.
//!
//! Hotkeys (`docs/design.md` roadmap) is in progress. Done so far: the
//! pure grammar and matcher, matching sxhkd's own
//! (`github.com/baskerville/sxhkd`, tag `0.6.2`) `src/parse.c`/
//! `src/types.c` byte for byte — line grouping ([`lexer`]), `{}`/range
//! expansion ([`expand`]), parsing an expanded chain string into
//! modifier/keysym chords ([`binding`], using `xkbcommon` for keysym
//! *name* lookup only — the one dependency `docs/design.md` allows this
//! crate beyond the standard library), and the chord-chain state
//! machine ([`matcher`]) — plus [`dispatch`], this project's own
//! extension classifying a command as an in-process `bspc` call or a
//! spawned shell command (`docs/bsp-hotkeys.md`'s "Binding execution").
//! Not started: pointer bindings, `bspwmrc`/sxhkdrc reload, and wiring
//! any of it into `bsp-compositor`.
#![deny(missing_docs)]
#![forbid(unsafe_code)]

pub mod binding;
pub mod dispatch;
pub mod expand;
pub mod lexer;
pub mod matcher;
mod token;
