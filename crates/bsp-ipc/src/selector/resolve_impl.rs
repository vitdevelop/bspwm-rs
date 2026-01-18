//! Resolving a parsed selector against live [`bsp_core::wm::Wm`] state.
//!
//! bspwm: `src/query.c` (`node_from_desc()`, `desktop_from_desc()`,
//! `monitor_from_desc()`, `find_nearest_neighbor()`, `find_any_node()`,
//! `find_first_ancestor()`, `*_matches()`), `src/tree.c`
//! (`find_fence()`), `src/geometry.c`
//! (`on_dir_side()`/`boundary_distance()`).
//!
//! Every descriptor and modifier that needs no history, pointer or EWMH-
//! primary state is resolved here; the rest return
//! [`super::ResolveError::Unsupported`] (`docs/bsp-ipc.md`, scope).
//! Search descriptors (`any`, `first_ancestor`, `biggest`, `smallest`, a
//! direction, `next`/`prev`) apply modifiers *while* searching, as bspwm's
//! own `find_*` functions do (each candidate is tested with
//! `node_matches()` before it can win); a single-candidate descriptor
//! (`focused`, a literal id, a path) instead has modifiers applied once, as
//! a final accept/reject check, in [`resolve_node`]/[`resolve_desktop`]/
//! [`resolve_monitor`].

use super::{
    DesktopDescriptor, DesktopModifiers, DesktopSelector, Jump, MonitorDescriptor,
    MonitorModifiers, MonitorSelector, NodeDescriptor, NodeFlag, NodeModifiers, NodeSelector,
    ResolveError,
};
use crate::registry::NodeRegistry;
use crate::value::CycleDir;
use bsp_core::geometry::Rect;
use bsp_core::id::{DesktopId, NodeId};
use bsp_core::tree::{Direction, Tree};
use bsp_core::wm::Wm;

/// bspwm default (`src/settings.c` `load_settings()`:
/// `directional_focus_tightness = TIGHTNESS_HIGH`). Not yet a `bsp-core`
/// setting (`docs/bsp-ipc.md`, scope), so directional resolution
/// always behaves as bspwm's own default rather than a configurable one
/// (the `on_dir_side` below implements only the `TIGHTNESS_HIGH` branch of
/// bspwm's `src/geometry.c`).
const _TIGHTNESS_HIGH_IS_THE_ONLY_MODE_IMPLEMENTED: () = ();

/// A resolved location: which monitor and desktop (by index into
/// [`Wm::monitors`] and [`bsp_core::monitor::Monitor::desktops`]), and
/// optionally which node within that desktop's tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coordinates {
    /// Index into `Wm::monitors`.
    pub monitor: usize,
    /// Index into the monitor's `desktops`.
    pub desktop: usize,
    /// A node within that desktop's tree, if this coordinate names one.
    pub node: Option<NodeId>,
}

/// Borrowed state a resolution needs: the window manager and the node id
/// registry (to resolve `NodeDescriptor::Id`).
#[derive(Debug, Clone, Copy)]
pub struct Ctx<'a> {
    /// The window manager to resolve against.
    pub wm: &'a Wm,
    /// The node id registry, for resolving a selector's literal node ids.
    pub registry: &'a NodeRegistry,
}

impl<'a> Ctx<'a> {
    pub(crate) fn tree(&self, c: Coordinates) -> &'a Tree {
        &self.wm.monitors[c.monitor].desktops[c.desktop].tree
    }

    pub(crate) fn focused(&self) -> Option<Coordinates> {
        let mi = self.wm.focused_monitor?;
        let m = &self.wm.monitors[mi];
        let di = m.focused?;
        Some(Coordinates {
            monitor: mi,
            desktop: di,
            node: m.desktops[di].tree.focus,
        })
    }

    pub(crate) fn all_desktops(&self) -> impl Iterator<Item = Coordinates> + 'a {
        let wm = self.wm;
        (0..wm.monitors.len()).flat_map(move |mi| {
            (0..wm.monitors[mi].desktops.len()).map(move |di| Coordinates {
                monitor: mi,
                desktop: di,
                node: None,
            })
        })
    }

    pub(crate) fn locate_desktop(&self, desktop: DesktopId) -> Option<(usize, usize)> {
        for (mi, m) in self.wm.monitors.iter().enumerate() {
            if let Some(di) = m.desktops.iter().position(|d| d.id == desktop) {
                return Some((mi, di));
            }
        }
        None
    }
}

// ---- Node resolution ----------------------------------------------------

/// Resolves a node selector to a single coordinate, relative to
/// `reference`.
pub fn resolve_node(
    ctx: Ctx,
    reference: Coordinates,
    sel: &NodeSelector,
) -> Result<Coordinates, ResolveError> {
    let reference = match &sel.reference {
        Some(r) => resolve_node(ctx, reference, r)?,
        None => reference,
    };
    let candidate = resolve_node_descriptor(ctx, reference, &sel.descriptor, &sel.modifiers)?;
    if node_matches(ctx, candidate, reference, &sel.modifiers) {
        Ok(candidate)
    } else {
        Err(ResolveError::NoMatch)
    }
}

fn resolve_node_descriptor(
    ctx: Ctx,
    reference: Coordinates,
    d: &NodeDescriptor,
    modifiers: &NodeModifiers,
) -> Result<Coordinates, ResolveError> {
    match d {
        NodeDescriptor::Focused => ctx.focused().ok_or(ResolveError::NoMatch),
        NodeDescriptor::Dir(dir) => find_nearest_neighbor(ctx, reference, *dir, modifiers),
        NodeDescriptor::Cycle(cyc) => cycle_node(ctx, reference, *cyc, modifiers),
        NodeDescriptor::Any => find_any_node(ctx, reference, modifiers),
        NodeDescriptor::FirstAncestor => find_first_ancestor(ctx, reference, modifiers),
        NodeDescriptor::Biggest => find_extremal(ctx, modifiers, true),
        NodeDescriptor::Smallest => find_extremal(ctx, modifiers, false),
        NodeDescriptor::Id(id) => {
            let (desktop, node) = ctx.registry.lookup(*id).ok_or(ResolveError::NoMatch)?;
            let (mi, di) = ctx.locate_desktop(desktop).ok_or(ResolveError::NoMatch)?;
            Ok(Coordinates {
                monitor: mi,
                desktop: di,
                node: Some(node),
            })
        }
        NodeDescriptor::Path(path) => resolve_path(ctx, reference, path),
        NodeDescriptor::Last
        | NodeDescriptor::Newest
        | NodeDescriptor::Older
        | NodeDescriptor::Newer => Err(ResolveError::Unsupported(
            "focus history is not tracked yet",
        )),
        NodeDescriptor::Pointed => Err(ResolveError::Unsupported("no pointer state yet")),
    }
}

/// bspwm: `src/tree.c` `find_any_node()`: monitor list order, desktop list
/// order, pre-order tree traversal; the first node matching `modifiers`
/// wins.
fn find_any_node(
    ctx: Ctx,
    reference: Coordinates,
    modifiers: &NodeModifiers,
) -> Result<Coordinates, ResolveError> {
    for loc in ctx.all_desktops() {
        if let Some(c) = first_preorder_match(ctx, loc, reference, modifiers) {
            return Ok(c);
        }
    }
    Err(ResolveError::NoMatch)
}

fn first_preorder_match(
    ctx: Ctx,
    loc: Coordinates,
    reference: Coordinates,
    modifiers: &NodeModifiers,
) -> Option<Coordinates> {
    let tree = ctx.tree(loc);
    let mut f = tree.first_extrema(tree.root);
    while let Some(n) = f {
        let candidate = Coordinates {
            node: Some(n),
            ..loc
        };
        if node_matches(ctx, candidate, reference, modifiers) {
            return Some(candidate);
        }
        f = tree.next_leaf(Some(n), tree.root);
    }
    None
}

/// bspwm: `src/tree.c` `find_first_ancestor()`: walks from the reference
/// node toward the root, returning the first ancestor matching `modifiers`.
fn find_first_ancestor(
    ctx: Ctx,
    reference: Coordinates,
    modifiers: &NodeModifiers,
) -> Result<Coordinates, ResolveError> {
    let Some(start) = reference.node else {
        return Err(ResolveError::NoMatch);
    };
    let tree = ctx.tree(reference);
    let mut cur = tree.node(start).parent();
    while let Some(n) = cur {
        let candidate = Coordinates {
            node: Some(n),
            ..reference
        };
        if node_matches(ctx, candidate, reference, modifiers) {
            return Ok(candidate);
        }
        cur = tree.node(n).parent();
    }
    Err(ResolveError::NoMatch)
}

/// `biggest`/`smallest`: searched across every desktop, like `any`
/// (bspwm's own scoping for these two is not in the sources read for this
/// step; whole-`Wm` search keeps behavior consistent with `any`).
fn find_extremal(
    ctx: Ctx,
    modifiers: &NodeModifiers,
    biggest: bool,
) -> Result<Coordinates, ResolveError> {
    // `find_extremal` never restricts by reference (`local` still works,
    // it just compares against whichever coordinate is passed as
    // `reference` in the modifier check below — `biggest`/`smallest` pass
    // themselves as their own reference, matching "no particular
    // reference" for a whole-`Wm` search).
    let mut best: Option<(Coordinates, i64)> = None;
    for loc in ctx.all_desktops() {
        let tree = ctx.tree(loc);
        let mut f = tree.first_extrema(tree.root);
        while let Some(n) = f {
            let candidate = Coordinates {
                node: Some(n),
                ..loc
            };
            if node_matches(ctx, candidate, candidate, modifiers) {
                let area = tree.node_area(n);
                let better = match best {
                    None => true,
                    Some((_, a)) => {
                        if biggest {
                            area > a
                        } else {
                            area < a
                        }
                    }
                };
                if better {
                    best = Some((candidate, area));
                }
            }
            f = tree.next_leaf(Some(n), tree.root);
        }
    }
    best.map(|(c, _)| c).ok_or(ResolveError::NoMatch)
}

/// `next`/`prev`: depth-first in-order traversal within the reference
/// node's own desktop tree, wrapping past either end. Traversal across
/// desktop/monitor boundaries is not implemented (`docs/bsp-ipc.md`, Step
/// 2 scope): bspwm's own `CYCLE_DIR` node traversal scope was not
/// confirmed from source for this step, so this stays within one tree
/// rather than guess.
fn cycle_node(
    ctx: Ctx,
    reference: Coordinates,
    dir: CycleDir,
    modifiers: &NodeModifiers,
) -> Result<Coordinates, ResolveError> {
    let Some(start) = reference.node else {
        return Err(ResolveError::NoMatch);
    };
    let tree = ctx.tree(reference);
    let step = |n: NodeId| match dir {
        CycleDir::Next => tree.next_leaf(Some(n), tree.root),
        CycleDir::Prev => tree.prev_leaf(Some(n), tree.root),
    };
    let wrap = || match dir {
        CycleDir::Next => tree.first_extrema(tree.root),
        CycleDir::Prev => tree.second_extrema(tree.root),
    };

    let mut cur = start;
    loop {
        let next = step(cur).or_else(wrap);
        let Some(n) = next else {
            return Err(ResolveError::NoMatch);
        };
        if n == start {
            return Err(ResolveError::NoMatch);
        }
        let candidate = Coordinates {
            node: Some(n),
            ..reference
        };
        if node_matches(ctx, candidate, reference, modifiers) {
            return Ok(candidate);
        }
        cur = n;
    }
}

fn resolve_path(
    ctx: Ctx,
    reference: Coordinates,
    path: &super::Path,
) -> Result<Coordinates, ResolveError> {
    let base = match &path.desktop {
        Some(d) => resolve_desktop(ctx, reference, d)?,
        None => reference,
    };
    let tree = ctx.tree(base);
    let mut node = if path.from_root {
        tree.root
    } else {
        base.node.or(tree.focus).or(tree.root)
    };

    for jump in &path.jumps {
        let Some(cur) = node else {
            return Err(ResolveError::NoMatch);
        };
        node = match jump {
            Jump::First => tree.node(cur).first_child(),
            Jump::Second => tree.node(cur).second_child(),
            Jump::Brother => tree.brother(cur),
            Jump::Parent => tree.node(cur).parent(),
            Jump::Dir(dir) => find_fence(tree, cur, *dir),
        };
    }

    match node {
        Some(n) => Ok(Coordinates {
            node: Some(n),
            ..base
        }),
        None => Err(ResolveError::NoMatch),
    }
}

/// bspwm: `src/tree.c` `find_fence()`: walks up from `n` until an ancestor
/// splits in the right orientation with `n`'s side on the near edge for
/// `dir`, and returns that ancestor (the node holding the edge in `dir`).
fn find_fence(tree: &Tree, n: NodeId, dir: Direction) -> Option<NodeId> {
    use bsp_core::tree::SplitType;
    let mut cur = n;
    let mut p = tree.node(cur).parent();
    while let Some(p_id) = p {
        let pn = tree.node(p_id);
        let pr = pn.rect;
        let nr = tree.node(cur).rect;
        let matches = match dir {
            Direction::North => pn.split_type == SplitType::Horizontal && pr.y < nr.y,
            Direction::West => pn.split_type == SplitType::Vertical && pr.x < nr.x,
            Direction::South => pn.split_type == SplitType::Horizontal && pr.bottom() > nr.bottom(),
            Direction::East => pn.split_type == SplitType::Vertical && pr.right() > nr.right(),
        };
        if matches {
            return Some(p_id);
        }
        cur = p_id;
        p = tree.node(p_id).parent();
    }
    None
}

/// bspwm: `src/geometry.c` `boundary_distance()`.
fn boundary_distance(r1: Rect, r2: Rect, dir: Direction) -> i64 {
    let (r1_max_x, r1_max_y) = (r1.x + r1.width - 1, r1.y + r1.height - 1);
    let (r2_max_x, r2_max_y) = (r2.x + r2.width - 1, r2.y + r2.height - 1);
    (match dir {
        Direction::North => {
            if r2_max_y > r1.y {
                r2_max_y - r1.y
            } else {
                r1.y - r2_max_y
            }
        }
        Direction::West => {
            if r2_max_x > r1.x {
                r2_max_x - r1.x
            } else {
                r1.x - r2_max_x
            }
        }
        Direction::South => {
            if r2.y < r1_max_y {
                r1_max_y - r2.y
            } else {
                r2.y - r1_max_y
            }
        }
        Direction::East => {
            if r2.x < r1_max_x {
                r1_max_x - r2.x
            } else {
                r2.x - r1_max_x
            }
        }
    }) as i64
}

/// bspwm: `src/geometry.c` `on_dir_side()`, the `TIGHTNESS_HIGH` branch
/// only — bspwm's default (`src/settings.c`) and, for now, the only mode
/// this build implements (`docs/bsp-ipc.md`, scope: no
/// `directional_focus_tightness` setting yet).
fn on_dir_side(r1: Rect, r2: Rect, dir: Direction) -> bool {
    let (r1_max_x, r1_max_y) = (r1.x + r1.width - 1, r1.y + r1.height - 1);
    let (r2_max_x, r2_max_y) = (r2.x + r2.width - 1, r2.y + r2.height - 1);
    let eliminated = match dir {
        Direction::North => r2.y >= r1.y,
        Direction::West => r2.x >= r1.x,
        Direction::South => r2_max_y <= r1_max_y,
        Direction::East => r2_max_x <= r1_max_x,
    };
    if eliminated {
        return false;
    }
    match dir {
        Direction::North | Direction::South => {
            (r2.x >= r1.x && r2.x <= r1_max_x)
                || (r2_max_x >= r1.x && r2_max_x <= r1_max_x)
                || (r1.x > r2.x && r1.x < r2_max_x)
        }
        Direction::West | Direction::East => {
            (r2.y >= r1.y && r2.y <= r1_max_y)
                || (r2_max_y >= r1.y && r2_max_y <= r1_max_y)
                || (r1.y > r2.y && r1_max_y < r2_max_y)
        }
    }
}

/// bspwm: `src/tree.c` `find_nearest_neighbor()`, restricted to each
/// monitor's *focused* desktop, as bspwm itself does. The `history_rank`
/// tie-break is not available yet (no focus history, `docs/bsp-ipc.md`
/// scope); ties keep whichever candidate was found first in
/// monitor/leaf order, which is deterministic but not bspwm-identical.
fn find_nearest_neighbor(
    ctx: Ctx,
    reference: Coordinates,
    dir: Direction,
    modifiers: &NodeModifiers,
) -> Result<Coordinates, ResolveError> {
    let Some(ref_node) = reference.node else {
        return Err(ResolveError::NoMatch);
    };
    let ref_tree = ctx.tree(reference);
    let ref_desktop = &ctx.wm.monitors[reference.monitor].desktops[reference.desktop];
    let ref_rect = ref_tree.get_rectangle(ref_node, ref_desktop.window_gap, ref_desktop.layout);

    let mut best: Option<(Coordinates, i64)> = None;
    for (mi, m) in ctx.wm.monitors.iter().enumerate() {
        let Some(di) = m.focused else { continue };
        let d = &m.desktops[di];
        let mut f = d.tree.first_extrema(d.tree.root);
        while let Some(n) = f {
            let node = d.tree.node(n);
            let candidate = Coordinates {
                monitor: mi,
                desktop: di,
                node: Some(n),
            };
            let is_ref = mi == reference.monitor && di == reference.desktop && n == ref_node;
            let skip = is_ref
                || node.client.is_none()
                || node.hidden
                || d.tree.is_descendant(Some(n), Some(ref_node))
                || !node_matches(ctx, candidate, reference, modifiers);
            if !skip {
                let r = d.tree.get_rectangle(n, d.window_gap, d.layout);
                if on_dir_side(ref_rect, r, dir) {
                    let dist = boundary_distance(ref_rect, r, dir);
                    if best.is_none_or(|(_, bd)| dist < bd) {
                        best = Some((candidate, dist));
                    }
                }
            }
            f = d.tree.next_leaf(Some(n), d.tree.root);
        }
    }
    best.map(|(c, _)| c).ok_or(ResolveError::NoMatch)
}

pub(crate) fn node_matches(
    ctx: Ctx,
    loc: Coordinates,
    reference: Coordinates,
    m: &NodeModifiers,
) -> bool {
    let Some(node_id) = loc.node else {
        return false;
    };
    let tree = ctx.tree(loc);
    let node = tree.node(node_id);

    if let Some(want) = m.focused {
        if (ctx.focused() == Some(loc)) != want {
            return false;
        }
    }
    if let Some(want) = m.active {
        if (tree.focus == Some(node_id)) != want {
            return false;
        }
    }
    if let Some(want) = m.automatic {
        if node.presel.is_none() != want {
            return false;
        }
    }
    if let Some(want) = m.local {
        let is = loc.monitor == reference.monitor && loc.desktop == reference.desktop;
        if is != want {
            return false;
        }
    }
    if let Some(want) = m.leaf {
        if tree.is_leaf(node_id) != want {
            return false;
        }
    }
    if let Some(want) = m.window {
        if node.client.is_some() != want {
            return false;
        }
    }
    if let Some((state, want)) = m.state {
        if node.client.as_ref().is_some_and(|c| c.state == state) != want {
            return false;
        }
    }
    if let Some((layer, want)) = m.layer {
        if node.client.as_ref().is_some_and(|c| c.layer == layer) != want {
            return false;
        }
    }
    if let Some((split_type, want)) = m.split_type {
        if (node.split_type == split_type) != want {
            return false;
        }
    }
    if m.same_class.is_some() {
        // No adapter-supplied window class/instance metadata yet
        // (`docs/bsp-ipc.md`, scope): treat as never matching
        // rather than silently ignoring the constraint.
        return false;
    }
    if let Some(want) = m.descendant_of {
        let is = reference
            .node
            .is_some_and(|r| node_id != r && tree.is_descendant(Some(node_id), Some(r)));
        if is != want {
            return false;
        }
    }
    if let Some(want) = m.ancestor_of {
        let is = reference
            .node
            .is_some_and(|r| node_id != r && tree.is_descendant(Some(r), Some(node_id)));
        if is != want {
            return false;
        }
    }
    for (flag, want) in &m.flags {
        let is = match flag {
            NodeFlag::Hidden => node.hidden,
            NodeFlag::Sticky => node.sticky,
            NodeFlag::Private => node.private,
            NodeFlag::Locked => node.locked,
            NodeFlag::Marked => node.marked,
            NodeFlag::Urgent => node.client.as_ref().is_some_and(|c| c.urgent),
        };
        if is != *want {
            return false;
        }
    }
    true
}

// ---- Desktop resolution --------------------------------------------------

/// Resolves a desktop selector to a single coordinate (`node` left `None`),
/// relative to `reference`.
pub fn resolve_desktop(
    ctx: Ctx,
    reference: Coordinates,
    sel: &DesktopSelector,
) -> Result<Coordinates, ResolveError> {
    let reference = match &sel.reference {
        Some(r) => resolve_desktop(ctx, reference, r)?,
        None => reference,
    };
    let candidate = resolve_desktop_descriptor(ctx, reference, &sel.descriptor)?;
    if desktop_matches(ctx, candidate, reference, &sel.modifiers) {
        Ok(candidate)
    } else {
        Err(ResolveError::NoMatch)
    }
}

fn resolve_desktop_descriptor(
    ctx: Ctx,
    reference: Coordinates,
    d: &DesktopDescriptor,
) -> Result<Coordinates, ResolveError> {
    match d {
        DesktopDescriptor::Focused => ctx.focused().ok_or(ResolveError::NoMatch),
        DesktopDescriptor::Cycle(cyc) => {
            let m = &ctx.wm.monitors[reference.monitor];
            let len = m.desktops.len();
            if len == 0 {
                return Err(ResolveError::NoMatch);
            }
            let next = match cyc {
                CycleDir::Next => (reference.desktop + 1) % len,
                CycleDir::Prev => (reference.desktop + len - 1) % len,
            };
            Ok(Coordinates {
                desktop: next,
                node: None,
                ..reference
            })
        }
        DesktopDescriptor::Any => ctx.all_desktops().next().ok_or(ResolveError::NoMatch),
        DesktopDescriptor::Nth { monitor, n } => {
            let mi = match monitor {
                Some(sel) => resolve_monitor(ctx, reference, sel)?.monitor,
                None => reference.monitor,
            };
            let idx = (*n as usize).checked_sub(1).ok_or(ResolveError::NoMatch)?;
            if idx < ctx.wm.monitors[mi].desktops.len() {
                Ok(Coordinates {
                    monitor: mi,
                    desktop: idx,
                    node: None,
                })
            } else {
                Err(ResolveError::NoMatch)
            }
        }
        DesktopDescriptor::Id(id) => {
            let (mi, di) = ctx.locate_desktop(*id).ok_or(ResolveError::NoMatch)?;
            Ok(Coordinates {
                monitor: mi,
                desktop: di,
                node: None,
            })
        }
        DesktopDescriptor::Name(name) => {
            for (mi, m) in ctx.wm.monitors.iter().enumerate() {
                if let Some(di) = m.desktops.iter().position(|d| &d.name == name) {
                    return Ok(Coordinates {
                        monitor: mi,
                        desktop: di,
                        node: None,
                    });
                }
            }
            Err(ResolveError::NoMatch)
        }
        DesktopDescriptor::Last
        | DesktopDescriptor::Newest
        | DesktopDescriptor::Older
        | DesktopDescriptor::Newer => Err(ResolveError::Unsupported(
            "focus history is not tracked yet",
        )),
    }
}

fn desktop_is_urgent(tree: &Tree) -> bool {
    let mut f = tree.first_extrema(tree.root);
    while let Some(n) = f {
        if tree.node(n).client.as_ref().is_some_and(|c| c.urgent) {
            return true;
        }
        f = tree.next_leaf(Some(n), tree.root);
    }
    false
}

pub(crate) fn desktop_matches(
    ctx: Ctx,
    loc: Coordinates,
    reference: Coordinates,
    m: &DesktopModifiers,
) -> bool {
    let desktop = &ctx.wm.monitors[loc.monitor].desktops[loc.desktop];
    if let Some(want) = m.focused {
        let is = ctx.focused().map(|c| (c.monitor, c.desktop)) == Some((loc.monitor, loc.desktop));
        if is != want {
            return false;
        }
    }
    if let Some(want) = m.active {
        if (ctx.wm.monitors[loc.monitor].focused == Some(loc.desktop)) != want {
            return false;
        }
    }
    if let Some(want) = m.occupied {
        if desktop.tree.root.is_some() != want {
            return false;
        }
    }
    if let Some(want) = m.urgent {
        if desktop_is_urgent(&desktop.tree) != want {
            return false;
        }
    }
    if let Some(want) = m.local {
        if (loc.monitor == reference.monitor) != want {
            return false;
        }
    }
    if let Some((layout, want)) = m.layout {
        if (desktop.layout == layout) != want {
            return false;
        }
    }
    if let Some((layout, want)) = m.user_layout {
        if (desktop.user_layout == layout) != want {
            return false;
        }
    }
    true
}

// ---- Monitor resolution ---------------------------------------------------

/// Resolves a monitor selector to a single coordinate (`desktop` at the
/// monitor's focused desktop, `node` left `None`), relative to `reference`.
pub fn resolve_monitor(
    ctx: Ctx,
    reference: Coordinates,
    sel: &MonitorSelector,
) -> Result<Coordinates, ResolveError> {
    let reference = match &sel.reference {
        Some(r) => resolve_monitor(ctx, reference, r)?,
        None => reference,
    };
    let candidate = resolve_monitor_descriptor(ctx, reference, &sel.descriptor)?;
    if monitor_matches(ctx, candidate, &sel.modifiers) {
        Ok(candidate)
    } else {
        Err(ResolveError::NoMatch)
    }
}

fn at_focused_desktop(ctx: Ctx, monitor: usize) -> Coordinates {
    Coordinates {
        monitor,
        desktop: ctx.wm.monitors[monitor].focused.unwrap_or(0),
        node: None,
    }
}

fn resolve_monitor_descriptor(
    ctx: Ctx,
    reference: Coordinates,
    d: &MonitorDescriptor,
) -> Result<Coordinates, ResolveError> {
    match d {
        MonitorDescriptor::Focused => {
            let mi = ctx.wm.focused_monitor.ok_or(ResolveError::NoMatch)?;
            Ok(at_focused_desktop(ctx, mi))
        }
        MonitorDescriptor::Cycle(cyc) => {
            let len = ctx.wm.monitors.len();
            if len == 0 {
                return Err(ResolveError::NoMatch);
            }
            let next = match cyc {
                CycleDir::Next => (reference.monitor + 1) % len,
                CycleDir::Prev => (reference.monitor + len - 1) % len,
            };
            Ok(at_focused_desktop(ctx, next))
        }
        MonitorDescriptor::Any => {
            if ctx.wm.monitors.is_empty() {
                Err(ResolveError::NoMatch)
            } else {
                Ok(at_focused_desktop(ctx, 0))
            }
        }
        MonitorDescriptor::Dir(dir) => find_monitor_in_direction(ctx, reference, *dir),
        MonitorDescriptor::Nth(n) => {
            let idx = (*n as usize).checked_sub(1).ok_or(ResolveError::NoMatch)?;
            if idx < ctx.wm.monitors.len() {
                Ok(at_focused_desktop(ctx, idx))
            } else {
                Err(ResolveError::NoMatch)
            }
        }
        MonitorDescriptor::Id(id) => {
            let idx = ctx.wm.monitor_index(*id).ok_or(ResolveError::NoMatch)?;
            Ok(at_focused_desktop(ctx, idx))
        }
        MonitorDescriptor::Name(name) => {
            let idx = ctx
                .wm
                .monitors
                .iter()
                .position(|m| &m.name == name)
                .ok_or(ResolveError::NoMatch)?;
            Ok(at_focused_desktop(ctx, idx))
        }
        MonitorDescriptor::Last
        | MonitorDescriptor::Newest
        | MonitorDescriptor::Older
        | MonitorDescriptor::Newer => Err(ResolveError::Unsupported(
            "focus history is not tracked yet",
        )),
        MonitorDescriptor::Pointed => Err(ResolveError::Unsupported("no pointer state yet")),
        MonitorDescriptor::Primary => {
            Err(ResolveError::Unsupported("no primary monitor concept yet"))
        }
    }
}

fn find_monitor_in_direction(
    ctx: Ctx,
    reference: Coordinates,
    dir: Direction,
) -> Result<Coordinates, ResolveError> {
    let ref_rect = ctx.wm.monitors[reference.monitor].rectangle;
    let mut best: Option<(usize, i64)> = None;
    for (mi, m) in ctx.wm.monitors.iter().enumerate() {
        if mi == reference.monitor {
            continue;
        }
        if on_dir_side(ref_rect, m.rectangle, dir) {
            let dist = boundary_distance(ref_rect, m.rectangle, dir);
            if best.is_none_or(|(_, bd)| dist < bd) {
                best = Some((mi, dist));
            }
        }
    }
    best.map(|(mi, _)| at_focused_desktop(ctx, mi))
        .ok_or(ResolveError::NoMatch)
}

pub(crate) fn monitor_matches(ctx: Ctx, loc: Coordinates, m: &MonitorModifiers) -> bool {
    let monitor = &ctx.wm.monitors[loc.monitor];
    if let Some(want) = m.focused {
        if (ctx.wm.focused_monitor == Some(loc.monitor)) != want {
            return false;
        }
    }
    if let Some(want) = m.occupied {
        let is = monitor
            .focused
            .is_some_and(|di| monitor.desktops[di].tree.root.is_some());
        if is != want {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector::{DesktopSelector, MonitorSelector, NodeSelector};
    use bsp_core::geometry::Rect;
    use bsp_core::id::{MonitorId as CoreMonitorId, WindowId};
    use bsp_core::monitor::Monitor;
    use bsp_core::node::Client;
    use bsp_core::settings::Settings;
    use bsp_core::wm::Wm;

    /// Builds a two-monitor `Wm`: `eDP-1` (0,0 800x600) with desktop "I"
    /// holding two side-by-side tiled clients, and `HDMI-A-1` (800,0
    /// 800x600) to its east with one empty desktop "II".
    fn fixture() -> (Wm, NodeRegistry, u32, u32) {
        let settings = Settings::default();
        let mut wm = Wm::new(settings.clone());

        let mut m1 = Monitor::new(
            CoreMonitorId(1),
            Some("eDP-1"),
            Rect::new(0, 0, 800, 600),
            &settings,
        );
        let mut d1 =
            bsp_core::desktop::Desktop::new(bsp_core::id::DesktopId(1), Some("I"), &settings);
        let left = d1
            .tree
            .new_client_node(&settings, Client::new(WindowId(1), 1));
        d1.tree.insert_node(&settings, left, None);
        m1.add_desktop(d1);
        // bspwm re-arranges after every insertion; the anchor's rectangle
        // (used to pick the new split's orientation, `src/tree.c`
        // `insert_node()`) must be real before the second window lands,
        // or `LongestSide` sees a 0x0 rect and always picks Horizontal.
        m1.arrange(0, &settings);
        let right = m1.desktops[0]
            .tree
            .new_client_node(&settings, Client::new(WindowId(2), 1));
        m1.desktops[0]
            .tree
            .insert_node(&settings, right, Some(left));
        m1.desktops[0].tree.focus = Some(left);
        m1.arrange(0, &settings);

        let mut m2 = Monitor::new(
            CoreMonitorId(2),
            Some("HDMI-A-1"),
            Rect::new(800, 0, 800, 600),
            &settings,
        );
        let d2 = bsp_core::desktop::Desktop::new(bsp_core::id::DesktopId(2), Some("II"), &settings);
        m2.add_desktop(d2);

        wm.add_monitor(m1);
        wm.add_monitor(m2);
        wm.focus_monitor(0);

        let mut registry = NodeRegistry::new();
        let left_id = registry.register(bsp_core::id::DesktopId(1), left);
        let right_id = registry.register(bsp_core::id::DesktopId(1), right);
        (wm, registry, left_id, right_id)
    }

    #[test]
    fn resolves_focused_node() {
        let (wm, registry, left_id, _) = fixture();
        let ctx = Ctx {
            wm: &wm,
            registry: &registry,
        };
        let reference = ctx.focused().unwrap();
        let sel = NodeSelector::parse("focused").unwrap();
        let got = resolve_node(ctx, reference, &sel).unwrap();
        assert_eq!(
            registry.id_of(wm.monitors[0].desktops[0].id, got.node.unwrap()),
            Some(left_id)
        );
    }

    #[test]
    fn resolves_node_by_id() {
        let (wm, registry, _, right_id) = fixture();
        let ctx = Ctx {
            wm: &wm,
            registry: &registry,
        };
        let reference = ctx.focused().unwrap();
        let sel = NodeSelector::parse(&format!("0x{right_id:08x}")).unwrap();
        let got = resolve_node(ctx, reference, &sel).unwrap();
        assert_eq!(
            registry.id_of(wm.monitors[0].desktops[0].id, got.node.unwrap()),
            Some(right_id)
        );
    }

    #[test]
    fn resolves_east_direction_to_the_neighbor() {
        let (wm, registry, _, right_id) = fixture();
        let ctx = Ctx {
            wm: &wm,
            registry: &registry,
        };
        let reference = ctx.focused().unwrap(); // left leaf
        let sel = NodeSelector::parse("east").unwrap();
        let got = resolve_node(ctx, reference, &sel).unwrap();
        assert_eq!(
            registry.id_of(wm.monitors[0].desktops[0].id, got.node.unwrap()),
            Some(right_id)
        );
    }

    #[test]
    fn cycle_next_then_prev_returns_to_start() {
        let (wm, registry, left_id, _) = fixture();
        let ctx = Ctx {
            wm: &wm,
            registry: &registry,
        };
        let reference = ctx.focused().unwrap();
        let next_sel = NodeSelector::parse("next").unwrap();
        let next = resolve_node(ctx, reference, &next_sel).unwrap();
        let prev_sel = NodeSelector::parse("prev").unwrap();
        let back = resolve_node(ctx, next, &prev_sel).unwrap();
        assert_eq!(
            registry.id_of(wm.monitors[0].desktops[0].id, back.node.unwrap()),
            Some(left_id)
        );
    }

    #[test]
    fn modifier_filters_out_non_matching_candidate() {
        let (wm, registry, _, _) = fixture();
        let ctx = Ctx {
            wm: &wm,
            registry: &registry,
        };
        let reference = ctx.focused().unwrap();
        // Neither leaf is floating, so `any.floating` must find nothing.
        let sel = NodeSelector::parse("any.floating").unwrap();
        assert_eq!(
            resolve_node(ctx, reference, &sel),
            Err(ResolveError::NoMatch)
        );
    }

    #[test]
    fn resolves_monitor_by_direction() {
        let (wm, registry, _, _) = fixture();
        let ctx = Ctx {
            wm: &wm,
            registry: &registry,
        };
        let reference = at_focused_desktop(ctx, 0);
        let sel = MonitorSelector::parse("east").unwrap();
        let got = resolve_monitor(ctx, reference, &sel).unwrap();
        assert_eq!(got.monitor, 1);
    }

    #[test]
    fn resolves_monitor_by_name() {
        let (wm, registry, _, _) = fixture();
        let ctx = Ctx {
            wm: &wm,
            registry: &registry,
        };
        let reference = at_focused_desktop(ctx, 0);
        let sel = MonitorSelector::parse("HDMI-A-1").unwrap();
        let got = resolve_monitor(ctx, reference, &sel).unwrap();
        assert_eq!(got.monitor, 1);
    }

    #[test]
    fn resolves_desktop_by_name_on_other_monitor() {
        let (wm, registry, _, _) = fixture();
        let ctx = Ctx {
            wm: &wm,
            registry: &registry,
        };
        let reference = at_focused_desktop(ctx, 0);
        let sel = DesktopSelector::parse("II").unwrap();
        let got = resolve_desktop(ctx, reference, &sel).unwrap();
        assert_eq!((got.monitor, got.desktop), (1, 0));
    }

    #[test]
    fn resolves_desktop_cycle_wraps_around() {
        let (wm, registry, _, _) = fixture();
        let ctx = Ctx {
            wm: &wm,
            registry: &registry,
        };
        // Monitor 0 has a single desktop, so `next` wraps to itself.
        let reference = at_focused_desktop(ctx, 0);
        let sel = DesktopSelector::parse("next").unwrap();
        let got = resolve_desktop(ctx, reference, &sel).unwrap();
        assert_eq!((got.monitor, got.desktop), (0, 0));
    }
}
