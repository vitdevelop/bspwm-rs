//! Node, desktop and monitor selectors: `[REFERENCE#]DESCRIPTOR(.MODIFIER)*`.
//!
//! bspwm: `src/query.c` (`node_from_desc()`, `desktop_from_desc()`,
//! `monitor_from_desc()`, `*_matches()`), `src/parse.c`
//! (`parse_*_modifiers()`), and `doc/bspwm.1.asciidoc`'s Selectors section,
//! which is the grammar this module's parser follows.
//!
//! Only *parsing* every selector form is complete; *resolving* a parsed
//! selector against live state ([`resolve_node`]/[`resolve_desktop`]/
//! [`resolve_monitor`]) covers the structural subset
//! that needs no pointer, EWMH-primary or focus-history state — none of
//! which exist yet (`docs/bsp-ipc.md`, scope). A selector that
//! parses but names an unsupported descriptor or modifier resolves to
//! [`ResolveError::Unsupported`].

use crate::value::CycleDir;
use bsp_core::id::{DesktopId, MonitorId};
use bsp_core::node::{ClientState, Layer};
use bsp_core::tree::{Direction, SplitType};

/// Node flags a `.modifier` can test (`[!](hidden|sticky|private|locked|marked|urgent)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeFlag {
    /// `hidden`.
    Hidden,
    /// `sticky`.
    Sticky,
    /// `private`.
    Private,
    /// `locked`.
    Locked,
    /// `marked`.
    Marked,
    /// `urgent`.
    Urgent,
}

// ---- Descriptors -----------------------------------------------------

/// A node selector's descriptor: `DIR|CYCLE_DIR|PATH|any|first_ancestor|
/// last|newest|older|newer|focused|pointed|biggest|smallest|<node_id>`.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeDescriptor {
    /// A compass direction.
    Dir(Direction),
    /// `next`/`prev` in depth-first in-order traversal.
    Cycle(CycleDir),
    /// `@[DESKTOP_SEL:][[/]JUMP](/JUMP)*`.
    Path(Path),
    /// `any`.
    Any,
    /// `first_ancestor`.
    FirstAncestor,
    /// `last`.
    Last,
    /// `newest`.
    Newest,
    /// `older`.
    Older,
    /// `newer`.
    Newer,
    /// `focused`.
    Focused,
    /// `pointed`.
    Pointed,
    /// `biggest`.
    Biggest,
    /// `smallest`.
    Smallest,
    /// A literal node id: the stable, never-reused id `bsp-ipc` mints for
    /// each node (bspwm's `node_t.id`, generated via `xcb_generate_id()`),
    /// *not* `bsp-core`'s arena [`bsp_core::id::NodeId`], which is reused
    /// after a node is freed and is only ever meaningful within one
    /// [`bsp_core::tree::Tree`] — see `crate::registry`.
    Id(u32),
}

/// A path selector's jump list: `PATH := @[DESKTOP_SEL:][[/]JUMP](/JUMP)*`.
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
    /// An optional desktop to start the path from.
    pub desktop: Option<Box<DesktopSelector>>,
    /// `true` if the path starts with `/` (from the desktop's root rather
    /// than its focused node).
    pub from_root: bool,
    /// The jumps to make, in order.
    pub jumps: Vec<Jump>,
}

/// One step in a [`Path`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Jump {
    /// `first`/`1`: the first child.
    First,
    /// `second`/`2`: the second child.
    Second,
    /// `brother`: the sibling.
    Brother,
    /// `parent`: the parent.
    Parent,
    /// A compass direction: the node holding that edge.
    Dir(Direction),
}

/// A desktop selector's descriptor.
#[derive(Debug, Clone, PartialEq)]
pub enum DesktopDescriptor {
    /// `next`/`prev`.
    Cycle(CycleDir),
    /// `any`.
    Any,
    /// `last`.
    Last,
    /// `newest`.
    Newest,
    /// `older`.
    Older,
    /// `newer`.
    Newer,
    /// `focused`.
    Focused,
    /// `[MONITOR_SEL:]^<n>`.
    Nth {
        /// An optional monitor to index within.
        monitor: Option<Box<MonitorSelector>>,
        /// The 1-based index.
        n: u16,
    },
    /// A literal desktop id.
    Id(DesktopId),
    /// A desktop name (bare, or `%name` for a name that would otherwise
    /// collide with a keyword or a number).
    Name(String),
}

/// A monitor selector's descriptor.
#[derive(Debug, Clone, PartialEq)]
pub enum MonitorDescriptor {
    /// A compass direction.
    Dir(Direction),
    /// `next`/`prev`.
    Cycle(CycleDir),
    /// `any`.
    Any,
    /// `last`.
    Last,
    /// `newest`.
    Newest,
    /// `older`.
    Older,
    /// `newer`.
    Newer,
    /// `focused`.
    Focused,
    /// `pointed`.
    Pointed,
    /// `primary`.
    Primary,
    /// `^<n>`.
    Nth(u16),
    /// A literal monitor id.
    Id(MonitorId),
    /// A monitor name (bare, or `%name`).
    Name(String),
}

// ---- Modifiers ---------------------------------------------------------

/// `.modifier` constraints on a node selector. Every field is `None`
/// (unconstrained), `Some(true)` (`.modifier`) or `Some(false)`
/// (`.!modifier`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeModifiers {
    /// `focused`.
    pub focused: Option<bool>,
    /// `active`.
    pub active: Option<bool>,
    /// `automatic` (in automatic, not manual/preselected, insertion mode).
    pub automatic: Option<bool>,
    /// `local` (in the reference desktop).
    pub local: Option<bool>,
    /// `leaf`.
    pub leaf: Option<bool>,
    /// `window` (holds a client).
    pub window: Option<bool>,
    /// `same_class` as the reference window.
    pub same_class: Option<bool>,
    /// `descendant_of` the reference node.
    pub descendant_of: Option<bool>,
    /// `ancestor_of` the reference node.
    pub ancestor_of: Option<bool>,
    /// A required (or excluded) [`ClientState`].
    pub state: Option<(ClientState, bool)>,
    /// A required (or excluded) [`Layer`].
    pub layer: Option<(Layer, bool)>,
    /// A required (or excluded) [`SplitType`].
    pub split_type: Option<(SplitType, bool)>,
    /// A required (or excluded) [`NodeFlag`].
    pub flags: Vec<(NodeFlag, bool)>,
}

/// `.modifier` constraints on a desktop selector.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopModifiers {
    /// `focused`.
    pub focused: Option<bool>,
    /// `active`.
    pub active: Option<bool>,
    /// `occupied`.
    pub occupied: Option<bool>,
    /// `urgent`.
    pub urgent: Option<bool>,
    /// `local` (on the reference monitor).
    pub local: Option<bool>,
    /// `tiled`/`monocle`, the effective layout.
    pub layout: Option<(bsp_core::tree::Layout, bool)>,
    /// `user_tiled`/`user_monocle`, the user-requested layout.
    pub user_layout: Option<(bsp_core::tree::Layout, bool)>,
}

/// `.modifier` constraints on a monitor selector.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MonitorModifiers {
    /// `focused`.
    pub focused: Option<bool>,
    /// `occupied` (focused desktop is occupied).
    pub occupied: Option<bool>,
}

// ---- Selectors -----------------------------------------------------------

/// A full node selector: `[REFERENCE#]DESCRIPTOR(.MODIFIER)*`.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeSelector {
    /// The selector its descriptor is relative to (default: `focused`).
    pub reference: Option<Box<NodeSelector>>,
    /// What node(s) to consider.
    pub descriptor: NodeDescriptor,
    /// Constraints every candidate must satisfy.
    pub modifiers: NodeModifiers,
}

/// A full desktop selector.
#[derive(Debug, Clone, PartialEq)]
pub struct DesktopSelector {
    /// The selector its descriptor is relative to.
    pub reference: Option<Box<DesktopSelector>>,
    /// What desktop(s) to consider.
    pub descriptor: DesktopDescriptor,
    /// Constraints every candidate must satisfy.
    pub modifiers: DesktopModifiers,
}

/// A full monitor selector.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorSelector {
    /// The selector its descriptor is relative to.
    pub reference: Option<Box<MonitorSelector>>,
    /// What monitor(s) to consider.
    pub descriptor: MonitorDescriptor,
    /// Constraints every candidate must satisfy.
    pub modifiers: MonitorModifiers,
}

/// Splits `s` at its rightmost occurrence of `sep`, matching bspwm's
/// `strrchr`-based, right-to-left stripping in `parse_*_modifiers()`
/// (`src/parse.c`) and the recursive `[REFERENCE#]` grammar: `#`, `:` and
/// `.` cannot appear inside a name (`doc/bspwm.1.asciidoc`, Selectors), so
/// the rightmost split is always the outermost one.
fn rsplit_once_ext(s: &str, sep: char) -> (Option<&str>, &str) {
    match s.rfind(sep) {
        Some(i) => (Some(&s[..i]), &s[i + sep.len_utf8()..]),
        None => (None, s),
    }
}

/// Pops every trailing `.modifier` token off `s` (right to left, as bspwm's
/// `parse_*_modifiers()` does), calling `apply` with each token with its
/// leading `!` (if any) stripped and the polarity as a `bool`. Returns the
/// remaining `[REFERENCE#]DESCRIPTOR` prefix, or `None` if `apply` rejected
/// a token (an unrecognized modifier).
fn strip_modifiers(mut s: &str, mut apply: impl FnMut(&str, bool) -> bool) -> Option<&str> {
    while let Some(dot) = s.rfind('.') {
        let tok = &s[dot + 1..];
        let (name, positive) = match tok.strip_prefix('!') {
            Some(rest) => (rest, false),
            None => (tok, true),
        };
        if !apply(name, positive) {
            return None;
        }
        s = &s[..dot];
    }
    Some(s)
}

impl NodeSelector {
    /// Parses a full node selector string.
    pub fn parse(s: &str) -> Option<NodeSelector> {
        let mut modifiers = NodeModifiers::default();
        let rest = strip_modifiers(s, |name, positive| {
            match name {
                "focused" => modifiers.focused = Some(positive),
                "active" => modifiers.active = Some(positive),
                "automatic" => modifiers.automatic = Some(positive),
                "local" => modifiers.local = Some(positive),
                "leaf" => modifiers.leaf = Some(positive),
                "window" => modifiers.window = Some(positive),
                "same_class" => modifiers.same_class = Some(positive),
                "descendant_of" => modifiers.descendant_of = Some(positive),
                "ancestor_of" => modifiers.ancestor_of = Some(positive),
                "tiled" => modifiers.state = Some((ClientState::Tiled, positive)),
                "pseudo_tiled" => modifiers.state = Some((ClientState::PseudoTiled, positive)),
                "floating" => modifiers.state = Some((ClientState::Floating, positive)),
                "fullscreen" => modifiers.state = Some((ClientState::Fullscreen, positive)),
                "below" => modifiers.layer = Some((Layer::Below, positive)),
                "normal" => modifiers.layer = Some((Layer::Normal, positive)),
                "above" => modifiers.layer = Some((Layer::Above, positive)),
                "horizontal" => modifiers.split_type = Some((SplitType::Horizontal, positive)),
                "vertical" => modifiers.split_type = Some((SplitType::Vertical, positive)),
                "hidden" => modifiers.flags.push((NodeFlag::Hidden, positive)),
                "sticky" => modifiers.flags.push((NodeFlag::Sticky, positive)),
                "private" => modifiers.flags.push((NodeFlag::Private, positive)),
                "locked" => modifiers.flags.push((NodeFlag::Locked, positive)),
                "marked" => modifiers.flags.push((NodeFlag::Marked, positive)),
                "urgent" => modifiers.flags.push((NodeFlag::Urgent, positive)),
                _ => return false,
            }
            true
        })?;

        let (reference, descriptor_str) = rsplit_once_ext(rest, '#');
        let reference = match reference {
            Some(r) => Some(Box::new(NodeSelector::parse(r)?)),
            None => None,
        };
        let descriptor = parse_node_descriptor(descriptor_str)?;
        // `strip_modifiers` pops right to left; restore left-to-right
        // order (cosmetic — a set of AND'd constraints, order is
        // otherwise meaningless).
        modifiers.flags.reverse();

        Some(NodeSelector {
            reference,
            descriptor,
            modifiers,
        })
    }
}

fn parse_node_descriptor(s: &str) -> Option<NodeDescriptor> {
    if let Some(path) = s.strip_prefix('@') {
        return parse_path(path).map(NodeDescriptor::Path);
    }
    Some(match s {
        "north" => NodeDescriptor::Dir(Direction::North),
        "west" => NodeDescriptor::Dir(Direction::West),
        "south" => NodeDescriptor::Dir(Direction::South),
        "east" => NodeDescriptor::Dir(Direction::East),
        "next" => NodeDescriptor::Cycle(CycleDir::Next),
        "prev" => NodeDescriptor::Cycle(CycleDir::Prev),
        "any" => NodeDescriptor::Any,
        "first_ancestor" => NodeDescriptor::FirstAncestor,
        "last" => NodeDescriptor::Last,
        "newest" => NodeDescriptor::Newest,
        "older" => NodeDescriptor::Older,
        "newer" => NodeDescriptor::Newer,
        "focused" => NodeDescriptor::Focused,
        "pointed" => NodeDescriptor::Pointed,
        "biggest" => NodeDescriptor::Biggest,
        "smallest" => NodeDescriptor::Smallest,
        _ => NodeDescriptor::Id(crate::value::parse_id(s)?),
    })
}

fn parse_path(s: &str) -> Option<Path> {
    let (desktop_str, rest) = match s.split_once(':') {
        Some((d, r)) => (Some(d), r),
        None => (None, s),
    };
    let desktop = match desktop_str {
        Some(d) => Some(Box::new(DesktopSelector::parse(d)?)),
        None => None,
    };
    let (from_root, jumps_str) = match rest.strip_prefix('/') {
        Some(r) => (true, r),
        None => (false, rest),
    };
    let mut jumps = Vec::new();
    if !jumps_str.is_empty() {
        for part in jumps_str.split('/') {
            jumps.push(match part {
                "first" | "1" => Jump::First,
                "second" | "2" => Jump::Second,
                "brother" => Jump::Brother,
                "parent" => Jump::Parent,
                "north" => Jump::Dir(Direction::North),
                "west" => Jump::Dir(Direction::West),
                "south" => Jump::Dir(Direction::South),
                "east" => Jump::Dir(Direction::East),
                _ => return None,
            });
        }
    }
    Some(Path {
        desktop,
        from_root,
        jumps,
    })
}

impl DesktopSelector {
    /// Parses a full desktop selector string.
    pub fn parse(s: &str) -> Option<DesktopSelector> {
        let mut modifiers = DesktopModifiers::default();
        let rest = strip_modifiers(s, |name, positive| {
            match name {
                "focused" => modifiers.focused = Some(positive),
                "active" => modifiers.active = Some(positive),
                "occupied" => modifiers.occupied = Some(positive),
                "urgent" => modifiers.urgent = Some(positive),
                "local" => modifiers.local = Some(positive),
                "tiled" => modifiers.layout = Some((bsp_core::tree::Layout::Tiled, positive)),
                "monocle" => modifiers.layout = Some((bsp_core::tree::Layout::Monocle, positive)),
                "user_tiled" => {
                    modifiers.user_layout = Some((bsp_core::tree::Layout::Tiled, positive))
                }
                "user_monocle" => {
                    modifiers.user_layout = Some((bsp_core::tree::Layout::Monocle, positive))
                }
                _ => return false,
            }
            true
        })?;

        let (reference, descriptor_str) = rsplit_once_ext(rest, '#');
        let reference = match reference {
            Some(r) => Some(Box::new(DesktopSelector::parse(r)?)),
            None => None,
        };
        let descriptor = parse_desktop_descriptor(descriptor_str)?;

        Some(DesktopSelector {
            reference,
            descriptor,
            modifiers,
        })
    }
}

fn parse_desktop_descriptor(s: &str) -> Option<DesktopDescriptor> {
    Some(match s {
        "next" => DesktopDescriptor::Cycle(CycleDir::Next),
        "prev" => DesktopDescriptor::Cycle(CycleDir::Prev),
        "any" => DesktopDescriptor::Any,
        "last" => DesktopDescriptor::Last,
        "newest" => DesktopDescriptor::Newest,
        "older" => DesktopDescriptor::Older,
        "newer" => DesktopDescriptor::Newer,
        "focused" => DesktopDescriptor::Focused,
        _ => {
            if let Some(name) = s.strip_prefix('%') {
                return Some(DesktopDescriptor::Name(name.to_string()));
            }
            // `[MONITOR_SEL:]^<n>` — split on the *last* ':' so a monitor
            // reference that itself contains a chained `#` reference still
            // parses (monitor names cannot contain ':', doc/bspwm.1.asciidoc).
            if let Some(colon) = s.rfind(':') {
                let (mon, n) = (&s[..colon], &s[colon + 1..]);
                if let Some(n) = crate::value::parse_index(n) {
                    let monitor = Some(Box::new(MonitorSelector::parse(mon)?));
                    return Some(DesktopDescriptor::Nth { monitor, n });
                }
            }
            if let Some(n) = crate::value::parse_index(s) {
                DesktopDescriptor::Nth { monitor: None, n }
            } else if let Some(id) = crate::value::parse_id(s) {
                DesktopDescriptor::Id(bsp_core::id::DesktopId(id))
            } else {
                DesktopDescriptor::Name(s.to_string())
            }
        }
    })
}

impl MonitorSelector {
    /// Parses a full monitor selector string.
    pub fn parse(s: &str) -> Option<MonitorSelector> {
        let mut modifiers = MonitorModifiers::default();
        let rest = strip_modifiers(s, |name, positive| {
            match name {
                "focused" => modifiers.focused = Some(positive),
                "occupied" => modifiers.occupied = Some(positive),
                _ => return false,
            }
            true
        })?;

        let (reference, descriptor_str) = rsplit_once_ext(rest, '#');
        let reference = match reference {
            Some(r) => Some(Box::new(MonitorSelector::parse(r)?)),
            None => None,
        };
        let descriptor = parse_monitor_descriptor(descriptor_str)?;

        Some(MonitorSelector {
            reference,
            descriptor,
            modifiers,
        })
    }
}

fn parse_monitor_descriptor(s: &str) -> Option<MonitorDescriptor> {
    Some(match s {
        "north" => MonitorDescriptor::Dir(Direction::North),
        "west" => MonitorDescriptor::Dir(Direction::West),
        "south" => MonitorDescriptor::Dir(Direction::South),
        "east" => MonitorDescriptor::Dir(Direction::East),
        "next" => MonitorDescriptor::Cycle(CycleDir::Next),
        "prev" => MonitorDescriptor::Cycle(CycleDir::Prev),
        "any" => MonitorDescriptor::Any,
        "last" => MonitorDescriptor::Last,
        "newest" => MonitorDescriptor::Newest,
        "older" => MonitorDescriptor::Older,
        "newer" => MonitorDescriptor::Newer,
        "focused" => MonitorDescriptor::Focused,
        "pointed" => MonitorDescriptor::Pointed,
        "primary" => MonitorDescriptor::Primary,
        _ => {
            if let Some(name) = s.strip_prefix('%') {
                return Some(MonitorDescriptor::Name(name.to_string()));
            }
            if let Some(n) = crate::value::parse_index(s) {
                MonitorDescriptor::Nth(n)
            } else if let Some(id) = crate::value::parse_id(s) {
                MonitorDescriptor::Id(bsp_core::id::MonitorId(id))
            } else {
                MonitorDescriptor::Name(s.to_string())
            }
        }
    })
}

/// Why resolving a selector (`resolve_node`/`resolve_desktop`/
/// `resolve_monitor`) could not produce a match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No node matched.
    NoMatch,
    /// The selector names a descriptor or modifier that needs state this
    /// build does not track yet (focus history, the pointer, or the X11
    /// "primary monitor" concept) — see `docs/bsp-ipc.md`, scope.
    Unsupported(&'static str),
}

pub(crate) mod resolve_impl;
pub use resolve_impl::{resolve_desktop, resolve_monitor, resolve_node, Ctx};

#[cfg(test)]
mod tests {
    use super::*;
    use bsp_core::tree::Direction as D;

    #[test]
    fn parses_bare_descriptor_with_default_modifiers() {
        let sel = NodeSelector::parse("focused").unwrap();
        assert_eq!(sel.descriptor, NodeDescriptor::Focused);
        assert_eq!(sel.modifiers, NodeModifiers::default());
        assert!(sel.reference.is_none());
    }

    #[test]
    fn parses_direction_descriptor() {
        assert_eq!(
            NodeSelector::parse("west").unwrap().descriptor,
            NodeDescriptor::Dir(D::West)
        );
    }

    #[test]
    fn parses_modifiers_in_any_order() {
        let sel = NodeSelector::parse("any.floating.!local").unwrap();
        assert_eq!(sel.descriptor, NodeDescriptor::Any);
        assert_eq!(sel.modifiers.state, Some((ClientState::Floating, true)));
        assert_eq!(sel.modifiers.local, Some(false));
    }

    #[test]
    fn parses_flag_modifier_list() {
        let sel = NodeSelector::parse("focused.sticky.!marked").unwrap();
        assert_eq!(
            sel.modifiers.flags,
            vec![(NodeFlag::Sticky, true), (NodeFlag::Marked, false)]
        );
    }

    #[test]
    fn parses_reference_hash_descriptor() {
        let sel = NodeSelector::parse("west#focused").unwrap();
        assert_eq!(sel.descriptor, NodeDescriptor::Focused);
        let reference = sel.reference.unwrap();
        assert_eq!(reference.descriptor, NodeDescriptor::Dir(D::West));
    }

    #[test]
    fn parses_chained_references() {
        let sel = NodeSelector::parse("west#east#focused").unwrap();
        assert_eq!(sel.descriptor, NodeDescriptor::Focused);
        let mid = sel.reference.unwrap();
        assert_eq!(mid.descriptor, NodeDescriptor::Dir(D::East));
        let inner = mid.reference.unwrap();
        assert_eq!(inner.descriptor, NodeDescriptor::Dir(D::West));
    }

    #[test]
    fn parses_node_id() {
        let sel = NodeSelector::parse("0x00000005").unwrap();
        assert_eq!(sel.descriptor, NodeDescriptor::Id(5));
    }

    #[test]
    fn parses_path_with_jumps() {
        let sel = NodeSelector::parse("@/first/parent/east").unwrap();
        match sel.descriptor {
            NodeDescriptor::Path(p) => {
                assert!(p.from_root);
                assert!(p.desktop.is_none());
                assert_eq!(p.jumps, vec![Jump::First, Jump::Parent, Jump::Dir(D::East)]);
            }
            other => panic!("expected Path, got {other:?}"),
        }
    }

    #[test]
    fn parses_path_with_desktop_prefix() {
        let sel = NodeSelector::parse("@focused:/1").unwrap();
        match sel.descriptor {
            NodeDescriptor::Path(p) => {
                assert!(p.desktop.is_some());
                assert_eq!(p.jumps, vec![Jump::First]);
            }
            other => panic!("expected Path, got {other:?}"),
        }
    }

    #[test]
    fn rejects_unknown_modifier() {
        assert!(NodeSelector::parse("focused.bogus").is_none());
    }

    #[test]
    fn desktop_selector_parses_nth_with_monitor_prefix() {
        let sel = DesktopSelector::parse("eDP-1:^2").unwrap();
        match sel.descriptor {
            DesktopDescriptor::Nth { monitor, n } => {
                assert_eq!(n, 2);
                assert!(monitor.is_some());
            }
            other => panic!("expected Nth, got {other:?}"),
        }
    }

    #[test]
    fn desktop_selector_parses_percent_escaped_name() {
        let sel = DesktopSelector::parse("%3").unwrap();
        assert_eq!(sel.descriptor, DesktopDescriptor::Name("3".to_string()));
    }

    #[test]
    fn monitor_selector_parses_name() {
        let sel = MonitorSelector::parse("eDP-1").unwrap();
        assert_eq!(sel.descriptor, MonitorDescriptor::Name("eDP-1".to_string()));
    }

    #[test]
    fn monitor_selector_parses_primary() {
        assert_eq!(
            MonitorSelector::parse("primary").unwrap().descriptor,
            MonitorDescriptor::Primary
        );
    }
}
