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
//! [`super::ResolveError::Unsupported`] (`docs/bsp-ipc.md`, IPC scope).
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
use bsp_core::history::Dir;
use bsp_core::id::{DesktopId, NodeId};
use bsp_core::tree::{Direction, Tree};
use bsp_core::wm::Wm;

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
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    /// The window manager to resolve against.
    pub wm: &'a Wm,
    /// The node id registry, for resolving a selector's literal node ids.
    pub registry: &'a NodeRegistry,
    /// The window system, for window classes (`.same_class`) and the pointer
    /// (`pointed`); `None` resolves as if there were neither.
    pub adapter: Option<&'a dyn crate::adapter::Adapter>,
}

impl<'a> Ctx<'a> {
    /// A context with no window classes and no pointer (what tests, and any
    /// caller without a window system, resolve with).
    pub fn new(wm: &'a Wm, registry: &'a NodeRegistry) -> Self {
        Self { wm, registry, adapter: None }
    }

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

    /// The coordinates a history entry names, if its monitor, desktop and
    /// node all still exist.
    pub(crate) fn coordinates_of(&self, loc: &bsp_core::history::Loc) -> Option<Coordinates> {
        let monitor = self.wm.monitor_index(loc.monitor)?;
        let desktop = self.wm.monitors[monitor].desktops.iter().position(|d| d.id == loc.desktop)?;
        let node = match loc.node {
            None => None,
            Some(w) => {
                let tree = &self.wm.monitors[monitor].desktops[desktop].tree;
                let mut f = tree.first_extrema(tree.root);
                let mut found = None;
                while let Some(n) = f {
                    if tree.node(n).client.as_ref().is_some_and(|c| c.window == w) {
                        found = Some(n);
                        break;
                    }
                    f = tree.next_leaf(Some(n), tree.root);
                }
                Some(found?)
            }
        };
        Some(Coordinates { monitor, desktop, node })
    }

    /// The window of the client at `loc`'s node, if it has one.
    pub(crate) fn window_at(&self, loc: Coordinates) -> Option<bsp_core::id::WindowId> {
        let n = loc.node?;
        self.tree(loc).node(n).client.as_ref().map(|c| c.window)
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
        NodeDescriptor::Biggest => find_extremal(ctx, reference, modifiers, true),
        NodeDescriptor::Smallest => find_extremal(ctx, reference, modifiers, false),
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
        NodeDescriptor::Last => history_node(ctx, reference, modifiers, Some(Dir::Older)),
        NodeDescriptor::Older => history_node(ctx, reference, modifiers, Some(Dir::Older)),
        NodeDescriptor::Newer => history_node(ctx, reference, modifiers, Some(Dir::Newer)),
        NodeDescriptor::Newest => history_node(ctx, reference, modifiers, None),
        NodeDescriptor::Pointed => {
            // bspwm: the window under the pointer, if it is a managed one.
            let window = ctx.adapter.and_then(|a| a.pointer_state().1).ok_or(ResolveError::NoMatch)?;
            ctx.all_desktops()
                .find_map(|loc| {
                    let tree = ctx.tree(loc);
                    let mut f = tree.first_extrema(tree.root);
                    while let Some(n) = f {
                        if tree.node(n).client.as_ref().is_some_and(|c| c.window == window) {
                            return Some(Coordinates { node: Some(n), ..loc });
                        }
                        f = tree.next_leaf(Some(n), tree.root);
                    }
                    None
                })
                .ok_or(ResolveError::NoMatch)
        }
    }
}

/// `last`/`older`/`newer` (`dir` is `Some`) and `newest` (`None`) for nodes.
///
/// bspwm: `src/history.c` `history_find_node()` and
/// `history_find_newest_node()`. A hidden node and the reference node itself
/// are skipped by `older`/`newer`; `newest` skips only hidden ones.
fn history_node(
    ctx: Ctx,
    reference: Coordinates,
    modifiers: &NodeModifiers,
    dir: Option<Dir>,
) -> Result<Coordinates, ResolveError> {
    let ref_window = ctx.window_at(reference);
    let accepts = |l: &bsp_core::history::Loc| {
        let Some(w) = l.node else { return false };
        if dir.is_some() && Some(w) == ref_window {
            return false;
        }
        let Some(c) = ctx.coordinates_of(l) else { return false };
        let hidden = c.node.is_some_and(|n| ctx.tree(c).node(n).hidden);
        !hidden && node_matches(ctx, c, reference, modifiers)
    };
    let found = match dir {
        Some(d) => ctx.wm.history.find(d, accepts),
        None => ctx.wm.history.find_newest(accepts),
    };
    found.and_then(|l| ctx.coordinates_of(&l)).ok_or(ResolveError::NoMatch)
}

/// `last`/`older`/`newer` and `newest` for desktops.
///
/// bspwm: `src/history.c` `history_find_desktop()` and
/// `history_find_newest_desktop()`.
fn history_desktop(
    ctx: Ctx,
    reference: Coordinates,
    modifiers: &DesktopModifiers,
    dir: Option<Dir>,
) -> Result<Coordinates, ResolveError> {
    let ref_desktop = ctx.wm.monitors[reference.monitor].desktops[reference.desktop].id;
    let accepts = |l: &bsp_core::history::Loc| {
        if dir.is_some() && l.desktop == ref_desktop {
            return false;
        }
        ctx.coordinates_of(l)
            .is_some_and(|c| desktop_matches(ctx, Coordinates { node: None, ..c }, reference, modifiers))
    };
    let found = match dir {
        Some(d) => ctx.wm.history.find(d, accepts),
        None => ctx.wm.history.find_newest(accepts),
    };
    found
        .and_then(|l| ctx.coordinates_of(&l))
        .map(|c| Coordinates { node: None, ..c })
        .ok_or(ResolveError::NoMatch)
}

/// `last`/`older`/`newer` and `newest` for monitors.
///
/// bspwm: `src/history.c` `history_find_monitor()` and
/// `history_find_newest_monitor()`.
fn history_monitor(
    ctx: Ctx,
    reference: Coordinates,
    modifiers: &MonitorModifiers,
    dir: Option<Dir>,
) -> Result<Coordinates, ResolveError> {
    let ref_monitor = ctx.wm.monitors[reference.monitor].id;
    let accepts = |l: &bsp_core::history::Loc| {
        if dir.is_some() && l.monitor == ref_monitor {
            return false;
        }
        ctx.coordinates_of(l).is_some_and(|c| monitor_matches(ctx, Coordinates { node: None, ..c }, modifiers))
    };
    let found = match dir {
        Some(d) => ctx.wm.history.find(d, accepts),
        None => ctx.wm.history.find_newest(accepts),
    };
    found
        .and_then(|l| ctx.coordinates_of(&l))
        .map(|c| at_focused_desktop(ctx, c.monitor))
        .ok_or(ResolveError::NoMatch)
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

/// `biggest`/`smallest`: the leaf with the largest (smallest) area on any monitor
/// and desktop that matches `modifiers` against `reference`, skipping vacant
/// leaves (a floating, fullscreen or hidden window). The first of equals wins.
///
/// bspwm: `src/tree.c` `find_by_area()`, with `node_area()` measuring
/// `get_rectangle()`.
fn find_extremal(
    ctx: Ctx,
    reference: Coordinates,
    modifiers: &NodeModifiers,
    biggest: bool,
) -> Result<Coordinates, ResolveError> {
    let mut best: Option<(Coordinates, i64)> = None;
    for loc in ctx.all_desktops() {
        let tree = ctx.tree(loc);
        let d = &ctx.wm.monitors[loc.monitor].desktops[loc.desktop];
        let mut f = tree.first_extrema(tree.root);
        while let Some(n) = f {
            let candidate = Coordinates { node: Some(n), ..loc };
            if !tree.node(n).vacant && node_matches(ctx, candidate, reference, modifiers) {
                let area = tree.get_rectangle(n, d.window_gap, d.layout, ctx.wm.settings.gapless_monocle).area();
                let better = match best {
                    None => true,
                    Some((_, a)) => (biggest && area > a) || (!biggest && area < a),
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

/// `next`/`prev`: the following (preceding) node in in-order, internal nodes
/// included, continuing onto the next (previous) desktop and monitor and
/// wrapping around, until one matches `modifiers` or the walk is back at the
/// reference.
///
/// bspwm: `src/tree.c` `find_closest_node()` with its `HANDLE_BOUNDARIES`.
fn cycle_node(
    ctx: Ctx,
    reference: Coordinates,
    dir: CycleDir,
    modifiers: &NodeModifiers,
) -> Result<Coordinates, ResolveError> {
    let monitors = &ctx.wm.monitors;
    let (mut mi, mut di) = (reference.monitor, reference.desktop);
    let step = |mi: usize, di: usize, n: Option<NodeId>| {
        let tree = &monitors[mi].desktops[di].tree;
        match dir {
            CycleDir::Next => tree.next_node(n),
            CycleDir::Prev => tree.prev_node(n),
        }
    };
    // The next desktop in the direction, wrapping to the other end of the monitor
    // list; and its first (last) node.
    let advance = |mi: &mut usize, di: &mut usize| -> Option<NodeId> {
        let next_desktop = match dir {
            CycleDir::Next => (*di + 1 < monitors[*mi].desktops.len()).then_some(*di + 1),
            CycleDir::Prev => di.checked_sub(1),
        };
        match next_desktop {
            Some(d) => *di = d,
            None => {
                *mi = match dir {
                    CycleDir::Next => (*mi + 1) % monitors.len(),
                    CycleDir::Prev => (*mi + monitors.len() - 1) % monitors.len(),
                };
                *di = match dir {
                    CycleDir::Next => 0,
                    CycleDir::Prev => monitors[*mi].desktops.len().saturating_sub(1),
                };
            }
        }
        let tree = &monitors[*mi].desktops.get(*di)?.tree;
        match dir {
            CycleDir::Next => tree.first_extrema(tree.root),
            CycleDir::Prev => tree.second_extrema(tree.root),
        }
    };
    let ref_desktop_only = reference.node.is_none();
    let mut n = step(mi, di, reference.node);
    let mut guard = 0usize;
    let limit = monitors.iter().map(|m| m.desktops.len()).sum::<usize>() + 1;
    loop {
        // HANDLE_BOUNDARIES
        while n.is_none() {
            n = advance(&mut mi, &mut di);
            guard += 1;
            if (ref_desktop_only && mi == reference.monitor && di == reference.desktop) || guard > limit {
                break;
            }
        }
        // Node ids belong to their tree: back at the reference means the same desktop too.
        if n == reference.node && mi == reference.monitor && di == reference.desktop {
            return Err(ResolveError::NoMatch);
        }
        let Some(node) = n else {
            return Err(ResolveError::NoMatch);
        };
        let candidate = Coordinates { monitor: mi, desktop: di, node: Some(node) };
        if node_matches(ctx, candidate, reference, modifiers) {
            return Ok(candidate);
        }
        n = step(mi, di, Some(node));
        if ref_desktop_only && mi == reference.monitor && di == reference.desktop && n.is_none() {
            return Err(ResolveError::NoMatch);
        }
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

/// bspwm: `src/geometry.c` `on_dir_side()`; `tightness` is the
/// `directional_focus_tightness` setting (`TIGHTNESS_HIGH` by default).
fn on_dir_side(r1: Rect, r2: Rect, dir: Direction, tightness: bsp_core::settings::Tightness) -> bool {
    let (r1_max_x, r1_max_y) = (r1.x + r1.width - 1, r1.y + r1.height - 1);
    let (r2_max_x, r2_max_y) = (r2.x + r2.width - 1, r2.y + r2.height - 1);
    let eliminated = match tightness {
        // `TIGHTNESS_LOW`: only what lies entirely on the wrong side is out.
        bsp_core::settings::Tightness::Low => match dir {
            Direction::North => r2.y > r1_max_y,
            Direction::West => r2.x > r1_max_x,
            Direction::South => r2_max_y < r1.y,
            Direction::East => r2_max_x < r1.x,
        },
        bsp_core::settings::Tightness::High => match dir {
            Direction::North => r2.y >= r1.y,
            Direction::West => r2.x >= r1.x,
            Direction::South => r2_max_y <= r1_max_y,
            Direction::East => r2_max_x <= r1_max_x,
        },
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
/// monitor's *focused* desktop, as bspwm itself does. Candidates at the
/// same distance are ordered by `history_rank` (more recently focused wins),
/// then by whichever was found first.
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
    let ref_rect = ref_tree.get_rectangle(ref_node, ref_desktop.window_gap, ref_desktop.layout, ctx.wm.settings.gapless_monocle);

    let mut best: Option<(Coordinates, i64, u32)> = None;
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
                let r = d.tree.get_rectangle(n, d.window_gap, d.layout, ctx.wm.settings.gapless_monocle);
                if on_dir_side(ref_rect, r, dir, ctx.wm.settings.directional_focus_tightness) {
                    let dist = boundary_distance(ref_rect, r, dir);
                    let rank = ctx.window_at(candidate).map_or(u32::MAX, |w| ctx.wm.history.rank(w));
                    if best.is_none_or(|(_, bd, br)| dist < bd || (dist == bd && rank < br)) {
                        best = Some((candidate, dist, rank));
                    }
                }
            }
            f = d.tree.next_leaf(Some(n), d.tree.root);
        }
    }
    best.map(|(c, _, _)| c).ok_or(ResolveError::NoMatch)
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
    if let Some(want) = m.same_class {
        // bspwm: a node with no window is never of the same class; otherwise the
        // class names are compared with the reference window's (a reference with
        // no window is of no class).
        let class_of = |w| ctx.adapter.map(|a| a.window_class(w).0).unwrap_or_default();
        let excluded = match node.client.as_ref() {
            None => want,
            Some(c) => {
                let same = ctx.window_at(reference).is_some_and(|rw| class_of(c.window) == class_of(rw));
                same != want
            }
        };
        if excluded {
            return false;
        }
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
    let candidate = resolve_desktop_descriptor(ctx, reference, &sel.descriptor, &sel.modifiers)?;
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
    modifiers: &DesktopModifiers,
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
            let idx = (*n as usize).checked_sub(1).ok_or(ResolveError::NoMatch)?;
            let Some(sel) = monitor else {
                // bspwm: `desktop_from_index()` counts across every monitor.
                return ctx
                    .wm
                    .monitors
                    .iter()
                    .enumerate()
                    .flat_map(|(mi, m)| (0..m.desktops.len()).map(move |di| (mi, di)))
                    .nth(idx)
                    .map(|(monitor, desktop)| Coordinates { monitor, desktop, node: None })
                    .ok_or(ResolveError::NoMatch);
            };
            let mi = resolve_monitor(ctx, reference, sel)?.monitor;
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
        DesktopDescriptor::Last | DesktopDescriptor::Older => history_desktop(ctx, reference, modifiers, Some(Dir::Older)),
        DesktopDescriptor::Newer => history_desktop(ctx, reference, modifiers, Some(Dir::Newer)),
        DesktopDescriptor::Newest => history_desktop(ctx, reference, modifiers, None),
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
    let candidate = resolve_monitor_descriptor(ctx, reference, &sel.descriptor, &sel.modifiers)?;
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
    modifiers: &MonitorModifiers,
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
        MonitorDescriptor::Last | MonitorDescriptor::Older => history_monitor(ctx, reference, modifiers, Some(Dir::Older)),
        MonitorDescriptor::Newer => history_monitor(ctx, reference, modifiers, Some(Dir::Newer)),
        MonitorDescriptor::Newest => history_monitor(ctx, reference, modifiers, None),
        MonitorDescriptor::Pointed => {
            // bspwm: `monitor_from_point()` of the pointer's position.
            let (x, y) = ctx.adapter.and_then(|a| a.pointer_state().0).ok_or(ResolveError::NoMatch)?;
            let mi = ctx
                .wm
                .monitors
                .iter()
                .position(|m| x >= m.rectangle.x && x < m.rectangle.x + m.rectangle.width && y >= m.rectangle.y && y < m.rectangle.y + m.rectangle.height)
                .ok_or(ResolveError::NoMatch)?;
            Ok(at_focused_desktop(ctx, mi))
        }
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
        if on_dir_side(ref_rect, m.rectangle, dir, ctx.wm.settings.directional_focus_tightness) {
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
        let ctx = Ctx::new(&wm, &registry);
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
        let ctx = Ctx::new(&wm, &registry);
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
        let ctx = Ctx::new(&wm, &registry);
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
        let ctx = Ctx::new(&wm, &registry);
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
        let ctx = Ctx::new(&wm, &registry);
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
        let ctx = Ctx::new(&wm, &registry);
        let reference = at_focused_desktop(ctx, 0);
        let sel = MonitorSelector::parse("east").unwrap();
        let got = resolve_monitor(ctx, reference, &sel).unwrap();
        assert_eq!(got.monitor, 1);
    }

    #[test]
    fn resolves_monitor_by_name() {
        let (wm, registry, _, _) = fixture();
        let ctx = Ctx::new(&wm, &registry);
        let reference = at_focused_desktop(ctx, 0);
        let sel = MonitorSelector::parse("HDMI-A-1").unwrap();
        let got = resolve_monitor(ctx, reference, &sel).unwrap();
        assert_eq!(got.monitor, 1);
    }

    #[test]
    fn resolves_desktop_by_name_on_other_monitor() {
        let (wm, registry, _, _) = fixture();
        let ctx = Ctx::new(&wm, &registry);
        let reference = at_focused_desktop(ctx, 0);
        let sel = DesktopSelector::parse("II").unwrap();
        let got = resolve_desktop(ctx, reference, &sel).unwrap();
        assert_eq!((got.monitor, got.desktop), (1, 0));
    }

    #[test]
    fn resolves_desktop_cycle_wraps_around() {
        let (wm, registry, _, _) = fixture();
        let ctx = Ctx::new(&wm, &registry);
        // Monitor 0 has a single desktop, so `next` wraps to itself.
        let reference = at_focused_desktop(ctx, 0);
        let sel = DesktopSelector::parse("next").unwrap();
        let got = resolve_desktop(ctx, reference, &sel).unwrap();
        assert_eq!((got.monitor, got.desktop), (0, 0));
    }
}
