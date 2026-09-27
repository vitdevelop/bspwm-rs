//! Executes a parsed [`Command`] against live [`Wm`] state: the layer that
//! finally connects `command`'s parser and `selector`'s resolver to
//! `bsp-core`'s tree/desktop/monitor operations, producing a [`Reply`] and
//! the [`Event`]s to broadcast to `subscribe`d connections.
//!
//! bspwm: `src/messages.c` (`cmd_node()` … `cmd_config()`), which this
//! module's `exec_*` functions mirror one-to-one. Call [`execute`] for
//! every [`Command`] except `Subscribe`/`Quit`, which the server handles
//! directly (registering a subscriber, or signaling shutdown) since
//! neither produces an ordinary reply.

use crate::adapter::Adapter;
use crate::command::*;
use crate::registry::NodeRegistry;
use crate::report::*;
use crate::selector::resolve_impl::{self};
pub use crate::selector::resolve_impl::Coordinates;
use crate::selector::{DesktopSelector, MonitorSelector, NodeSelector, ResolveError};
use crate::value::AlterState;
use crate::wire::Reply;
use bsp_core::desktop::Desktop;
use bsp_core::id::{DesktopId, NodeId, WindowId};
use bsp_core::monitor::Monitor;
use bsp_core::rules::Rule;
use bsp_core::tree::{Direction, Tree};
use bsp_core::wm::Wm;

/// Borrowed state one [`execute`] call needs: the window manager, the
/// node id registry, and the adapter for window-system effects.
pub struct ExecCtx<'a, A: Adapter> {
    /// The window manager to mutate.
    pub wm: &'a mut Wm,
    /// The node id registry, kept in step with every node created, freed
    /// or moved to another tree.
    pub registry: &'a mut NodeRegistry,
    /// The window-system adapter.
    pub adapter: &'a mut A,
}

/// Executes `cmd`. Panics if given `Command::Subscribe` or `Command::Quit`
/// — see this module's doc comment.
pub fn execute<A: Adapter>(ctx: &mut ExecCtx<A>, cmd: &Command) -> (Reply, Vec<Event>) {
    // Focus may have changed since the last command (a click, a new window):
    // record it first, so `last`/`older` see it, and again for what this
    // command itself changes.
    ctx.wm.sync_history();
    // Nodes the caller changed behind `execute`'s back (a window mapped) get ids too.
    ctx.registry.sync_with(ctx.wm);
    let layouts = layout_snapshot(ctx.wm);
    let mut result = execute_inner(ctx, cmd);
    push_layout_changes(ctx.wm, &layouts, &mut result.1);
    ctx.wm.sync_history();
    // Splits made or freed by the command get and lose their ids.
    ctx.registry.sync_with(ctx.wm);
    push_geometry_changes(ctx.wm, ctx.registry, &mut result.1);
    result
}

fn execute_inner<A: Adapter>(ctx: &mut ExecCtx<A>, cmd: &Command) -> (Reply, Vec<Event>) {
    match cmd {
        Command::Node { selector, actions } => exec_node(ctx, selector.as_ref(), actions),
        Command::Desktop { selector, actions } => exec_desktop(ctx, selector.as_ref(), actions),
        Command::Monitor { selector, actions } => exec_monitor(ctx, selector.as_ref(), actions),
        Command::Query(q) => (exec_query(ctx, q), Vec::new()),
        Command::Rule(actions) => (exec_rule(ctx, actions), Vec::new()),
        Command::Wm(actions) => exec_wm(ctx, actions),
        Command::Config(c) => (exec_config(ctx, c), Vec::new()),
        Command::Output { name, actions } => {
            (exec_output(ctx, name.as_deref(), actions), Vec::new())
        }
        Command::Input { device, actions } => {
            (exec_input(ctx, device.as_deref(), actions), Vec::new())
        }
        // The server answers these itself; reaching here is a caller's mistake.
        Command::Subscribe { .. } | Command::Quit(_) => (Reply::Fail("Unsupported command.\n".to_string()), Vec::new()),
    }
}

// ---- shared helpers --------------------------------------------------

fn resolve_ctx<'a, A: Adapter>(ctx: &'a ExecCtx<A>) -> resolve_impl::Ctx<'a> {
    resolve_impl::Ctx {
        wm: ctx.wm,
        registry: ctx.registry,
        adapter: Some(&*ctx.adapter),
    }
}

fn focused_coords<A: Adapter>(ctx: &ExecCtx<A>) -> Option<Coordinates> {
    resolve_ctx(ctx).focused()
}

fn resolve_node<A: Adapter>(
    ctx: &ExecCtx<A>,
    reference: Coordinates,
    sel: &NodeSelector,
) -> Result<Coordinates, ResolveError> {
    resolve_impl::resolve_node(resolve_ctx(ctx), reference, sel)
}

fn resolve_desktop<A: Adapter>(
    ctx: &ExecCtx<A>,
    reference: Coordinates,
    sel: &DesktopSelector,
) -> Result<Coordinates, ResolveError> {
    resolve_impl::resolve_desktop(resolve_ctx(ctx), reference, sel)
}

fn resolve_monitor<A: Adapter>(
    ctx: &ExecCtx<A>,
    reference: Coordinates,
    sel: &MonitorSelector,
) -> Result<Coordinates, ResolveError> {
    resolve_impl::resolve_monitor(resolve_ctx(ctx), reference, sel)
}

/// bspwm's `handle_failure()`/plain `fail(rsp, "")`: a selector that fails
/// to resolve reports either a syntax problem (unreachable here — the
/// parser already rejected those) or, for `Unsupported`, a specific
/// message; a bare `NoMatch` is bspwm's silent empty failure.
/// The reply for a selector that failed; `src` names the command and option,
/// as bspwm's messages do.
///
/// bspwm: `src/messages.c` `handle_failure()`.
fn resolve_err_reply(err: ResolveError, src: &str) -> Reply {
    match err {
        ResolveError::NoMatch => Reply::Fail(String::new()),
        ResolveError::BadDescriptor(desc) => Reply::Fail(format!("{src}: Invalid descriptor found in '{desc}'.\n")),
        ResolveError::Unsupported(why) => Reply::Fail(format!("Not yet supported: {why}.\n")),
    }
}

fn tree(wm: &Wm, c: Coordinates) -> &Tree {
    &wm.monitors[c.monitor].desktops[c.desktop].tree
}

fn tree_mut(wm: &mut Wm, c: Coordinates) -> &mut Tree {
    &mut wm.monitors[c.monitor].desktops[c.desktop].tree
}

fn desktop_id(wm: &Wm, c: Coordinates) -> DesktopId {
    wm.monitors[c.monitor].desktops[c.desktop].id
}

fn monitor_wire_id(wm: &Wm, monitor: usize) -> WireMonitorId {
    wm.monitors[monitor].id.0
}

fn wid<A: Adapter>(ctx: &ExecCtx<A>, c: Coordinates) -> WireNodeId {
    let Some(n) = c.node else { return 0 };
    ctx.registry.id_of(desktop_id(ctx.wm, c), n).unwrap_or(0)
}

/// Returns two mutable tree borrows for `a` and `b`, which must name
/// different `(monitor, desktop)` slots. Needed because
/// [`bsp_core::tree::Tree::transplant_to`] takes `&mut self` and
/// `dest: &mut Tree`, and both trees live inside the same `Wm`.
fn two_trees_mut(wm: &mut Wm, a: (usize, usize), b: (usize, usize)) -> (&mut Tree, &mut Tree) {
    assert_ne!(a, b, "two_trees_mut: same (monitor, desktop) slot");
    if a.0 == b.0 {
        let desktops = &mut wm.monitors[a.0].desktops;
        if a.1 < b.1 {
            let (left, right) = desktops.split_at_mut(b.1);
            (&mut left[a.1].tree, &mut right[0].tree)
        } else {
            let (left, right) = desktops.split_at_mut(a.1);
            (&mut right[0].tree, &mut left[b.1].tree)
        }
    } else if a.0 < b.0 {
        let (left, right) = wm.monitors.split_at_mut(b.0);
        (
            &mut left[a.0].desktops[a.1].tree,
            &mut right[0].desktops[b.1].tree,
        )
    } else {
        let (left, right) = wm.monitors.split_at_mut(a.0);
        (
            &mut right[0].desktops[a.1].tree,
            &mut left[b.0].desktops[b.1].tree,
        )
    }
}

/// Applies a new rectangle to monitor `monitor`: adapts every desktop's
/// tree geometry, re-arranges, records a `MonitorGeometry` event, and
/// re-sorts the monitor into on-screen-position order. Returns the
/// monitor's index after that reorder. Shared by `bspc monitor -g` and
/// the compositor's own output mode/scale/position changes (`bspc
/// output`, the hardware backend).
pub fn set_monitor_rectangle<A: Adapter>(
    ctx: &mut ExecCtx<A>,
    monitor: usize,
    r: bsp_core::geometry::Rect,
    events: &mut Vec<Event>,
) -> usize {
    let old = ctx.wm.monitors[monitor].rectangle;
    ctx.wm.monitors[monitor].rectangle = r;
    for i in 0..ctx.wm.monitors[monitor].desktops.len() {
        let root = ctx.wm.monitors[monitor].desktops[i].tree.root;
        bsp_core::monitor::adapt_geometry(&mut ctx.wm.monitors[monitor].desktops[i].tree, root, old, r);
    }
    events.push(Event::MonitorGeometry {
        id: monitor_wire_id(ctx.wm, monitor),
        geometry: r,
    });
    for i in 0..ctx.wm.monitors[monitor].desktops.len() {
        arrange(
            ctx,
            Coordinates {
                monitor,
                desktop: i,
                node: None,
            },
        );
    }
    // bspwm: `src/monitor.c` `update_root()` calls `reorder_monitor(m)`
    // right after applying a new rectangle, so a monitor that moved on
    // screen sorts back into on-screen-position order among its
    // neighbors.
    ctx.wm.reorder_monitor(monitor)
}

/// Moves every desktop of monitor `from` to monitor `to`, in order, keeping
/// their names, layouts and windows. `from` is left with no desktop.
///
/// bspwm: `src/monitor.c` `merge_monitors()`.
pub fn merge_monitors<A: Adapter>(ctx: &mut ExecCtx<A>, from: usize, to: usize, events: &mut Vec<Event>) {
    if from == to {
        return;
    }
    while !ctx.wm.monitors[from].desktops.is_empty() {
        let first = Coordinates { monitor: from, desktop: 0, node: None };
        if transfer_desktop(ctx, first, to, false, events).is_none() {
            return;
        }
    }
}

/// Removes the monitor at `index` (with whatever it still holds: merge its
/// desktops first to keep them), reports `monitor_remove`, and focuses the
/// monitor that takes over when it was the focused one. Returns the index the
/// monitor `keep` has afterwards.
///
/// bspwm: `src/monitor.c` `remove_monitor()`.
pub fn remove_monitor<A: Adapter>(ctx: &mut ExecCtx<A>, index: usize, keep: usize, events: &mut Vec<Event>) -> usize {
    let was_focused = ctx.wm.focused_monitor == Some(index);
    events.push(Event::MonitorRemove { id: monitor_wire_id(ctx.wm, index) });
    ctx.wm.remove_monitor(index);
    let keep = if keep > index { keep - 1 } else { keep };
    if was_focused {
        if let Some(mi) = ctx.wm.focused_monitor {
            // `focus_node` reports the monitor change only if it sees one.
            ctx.wm.focused_monitor = None;
            if let Some(desktop) = ctx.wm.monitors[mi].focused {
                focus_node(ctx, Coordinates { monitor: mi, desktop, node: None }, events);
            } else {
                ctx.wm.focused_monitor = Some(mi);
            }
        }
    }
    keep
}

/// The monitor the desktops of the unplugged monitor `gone` go to: the last
/// wired one in list order (bspwm's `last_wired`), or `None`.
pub fn merge_target(wm: &Wm, gone: usize) -> Option<usize> {
    (0..wm.monitors.len()).rev().find(|&i| i != gone && wm.monitors[i].wired)
}

/// The effective layout of every desktop, keyed by desktop id.
pub fn layout_snapshot(wm: &Wm) -> std::collections::HashMap<bsp_core::id::DesktopId, bsp_core::tree::Layout> {
    wm.monitors.iter().flat_map(|m| m.desktops.iter()).map(|d| (d.id, d.layout)).collect()
}

/// Appends a `desktop_layout` event for every desktop whose effective layout
/// differs from `before` and that no event in `events` already reports (the
/// `single_monocle` flip that `arrange` does on its own is the case this
/// catches; bspwm reports it from `arrange()` too).
pub fn push_layout_changes(
    wm: &Wm,
    before: &std::collections::HashMap<bsp_core::id::DesktopId, bsp_core::tree::Layout>,
    events: &mut Vec<Event>,
) {
    for (mi, m) in wm.monitors.iter().enumerate() {
        for d in &m.desktops {
            let Some(&old) = before.get(&d.id) else { continue };
            if old == d.layout {
                continue;
            }
            let reported = events.iter().any(|e| matches!(e, Event::DesktopLayout { desktop, layout, .. } if *desktop == d.id.0 && *layout == d.layout));
            if !reported {
                events.push(Event::DesktopLayout { monitor: monitor_wire_id(wm, mi), desktop: d.id.0, layout: d.layout });
            }
        }
    }
}

/// Reports `node_geometry` for every window whose rectangle (as its state
/// shows it) is no longer where it was last put, and records the new one.
/// Hidden windows too: bspwm lays them out like the others.
///
/// bspwm: `src/tree.c` `apply_layout()`, which compares with the window's
/// geometry and reports each window it moves or resizes.
pub fn push_geometry_changes(wm: &mut Wm, registry: &NodeRegistry, events: &mut Vec<Event>) {
    for m in &mut wm.monitors {
        let monitor = m.id.0;
        let monitor_rect = m.rectangle;
        for d in &mut m.desktops {
            let desktop = d.id;
            let t = &mut d.tree;
            let mut leaf = t.first_extrema(t.root);
            while let Some(n) = leaf {
                leaf = t.next_leaf(Some(n), t.root);
                let Some(c) = t.node_mut(n).client.as_mut() else { continue };
                let r = if c.state == bsp_core::node::ClientState::Fullscreen { monitor_rect } else { c.shown_rectangle() };
                if c.window_rectangle == Some(r) {
                    continue;
                }
                c.window_rectangle = Some(r);
                events.push(Event::NodeGeometry {
                    monitor,
                    desktop: desktop.0,
                    node: registry.id_of(desktop, n).unwrap_or(0),
                    geometry: r,
                });
            }
        }
    }
}

/// bspwm: `src/tree.c` `arrange()`, invoked via `crate::monitor::Monitor::arrange`.
fn arrange<A: Adapter>(ctx: &mut ExecCtx<A>, c: Coordinates) {
    let settings = ctx.wm.settings.clone();
    ctx.wm.monitors[c.monitor].arrange(c.desktop, &settings);
}

/// `crate::value::ResizeHandle` (the wire format) to
/// `bsp_core::tree::ResizeHandle` (the tree operation) — the two are
/// the same 8-way shape by construction, kept as separate types since
/// `bsp-core` cannot depend on `bsp-ipc` (`docs/design.md` Architecture:
/// dependencies point one way).
fn to_core_resize_handle(h: crate::value::ResizeHandle) -> bsp_core::tree::ResizeHandle {
    use crate::value::ResizeHandle as W;
    use bsp_core::tree::ResizeHandle as C;
    match h {
        W::Left => C::Left,
        W::Top => C::Top,
        W::Right => C::Right,
        W::Bottom => C::Bottom,
        W::TopLeft => C::TopLeft,
        W::TopRight => C::TopRight,
        W::BottomRight => C::BottomRight,
        W::BottomLeft => C::BottomLeft,
    }
}

// ======================= node =======================

fn exec_node<A: Adapter>(
    ctx: &mut ExecCtx<A>,
    selector: Option<&NodeSelector>,
    actions: &[NodeAction],
) -> (Reply, Vec<Event>) {
    let Some(reference) = focused_coords(ctx) else {
        return (
            Reply::Fail("node: Missing arguments.\n".to_string()),
            Vec::new(),
        );
    };
    let mut trg = match selector {
        Some(sel) => match resolve_node(ctx, reference, sel) {
            Ok(c) => c,
            Err(e) => return (resolve_err_reply(e, "node"), Vec::new()),
        },
        None => reference,
    };

    let mut events = Vec::new();
    let mut changed = false;
    let mut fail: Option<String> = None;

    'actions: for action in actions {
        match action {
            NodeAction::Focus(sel) => {
                let dst = match resolve_or_default(ctx, reference, sel.as_ref(), trg) {
                    Ok(c) => c,
                    Err(e) => {
                        fail = Some(resolve_err_reply(e, "node -f").into_message());
                        break 'actions;
                    }
                };
                if dst.node.is_none() || !focus_node(ctx, dst, &mut events) {
                    fail = Some(String::new());
                    break 'actions;
                }
            }
            NodeAction::Activate(sel) => {
                let dst = match resolve_or_default(ctx, reference, sel.as_ref(), trg) {
                    Ok(c) => c,
                    Err(e) => {
                        fail = Some(resolve_err_reply(e, "node -a").into_message());
                        break 'actions;
                    }
                };
                if dst.node.is_none() || !activate_node(ctx, dst, &mut events) {
                    fail = Some(String::new());
                    break 'actions;
                }
            }
            NodeAction::ToDesktop(sel, follow) => {
                let dst = match resolve_desktop(ctx, reference, sel) {
                    Ok(c) => c,
                    Err(e) => {
                        fail = Some(resolve_err_reply(e, "node -d").into_message());
                        break 'actions;
                    }
                };
                let anchor = tree(ctx.wm, dst).focus;
                match transfer_node(ctx, trg, dst, anchor, *follow, &mut events) {
                    Ok(new_trg) => {
                        trg = new_trg;
                        changed = true;
                    }
                    Err(msg) => {
                        fail = Some(msg);
                        break 'actions;
                    }
                }
            }
            NodeAction::ToMonitor(sel, follow) => {
                let m = match resolve_monitor(ctx, reference, sel) {
                    Ok(c) => c,
                    Err(e) => {
                        fail = Some(resolve_err_reply(e, "node -m").into_message());
                        break 'actions;
                    }
                };
                let dst = Coordinates {
                    monitor: m.monitor,
                    desktop: ctx.wm.monitors[m.monitor].focused.unwrap_or(0),
                    node: None,
                };
                let anchor = tree(ctx.wm, dst).focus;
                match transfer_node(ctx, trg, dst, anchor, *follow, &mut events) {
                    Ok(new_trg) => {
                        trg = new_trg;
                        changed = true;
                    }
                    Err(msg) => {
                        fail = Some(msg);
                        break 'actions;
                    }
                }
            }
            NodeAction::ToNode(sel, follow) => {
                let dst = match resolve_node(ctx, reference, sel) {
                    Ok(c) => c,
                    Err(e) => {
                        fail = Some(resolve_err_reply(e, "node -n").into_message());
                        break 'actions;
                    }
                };
                let anchor = dst.node;
                match transfer_node(ctx, trg, dst, anchor, *follow, &mut events) {
                    Ok(new_trg) => {
                        trg = new_trg;
                        changed = true;
                    }
                    Err(msg) => {
                        fail = Some(msg);
                        break 'actions;
                    }
                }
            }
            NodeAction::Swap(sel, follow) => {
                let dst = match resolve_node(ctx, reference, sel) {
                    Ok(c) => c,
                    Err(e) => {
                        fail = Some(resolve_err_reply(e, "node -s").into_message());
                        break 'actions;
                    }
                };
                match do_swap(ctx, trg, dst, *follow, &mut events) {
                    Ok(new_trg) => {
                        trg = new_trg;
                        changed = true;
                    }
                    Err(msg) => {
                        fail = Some(msg);
                        break 'actions;
                    }
                }
            }
            NodeAction::PreselDir(arg) => {
                let Some(n) = trg.node else {
                    fail = Some(String::new());
                    break 'actions;
                };
                let t = tree(ctx.wm, trg);
                if t.node(n).vacant {
                    fail = Some(String::new());
                    break 'actions;
                }
                let default_ratio = ctx.wm.settings.split_ratio;
                match arg {
                    // bspwm: `cancel_presel()` reports only a preselection it removed.
                    PreselDirArg::Cancel if t.node(n).presel.is_some() => {
                        tree_mut(ctx.wm, trg).cancel_presel(n);
                        events.push(Event::NodePresel {
                            monitor: monitor_wire_id(ctx.wm, trg.monitor),
                            desktop: desktop_id(ctx.wm, trg).0,
                            node: wid(ctx, trg),
                            detail: PreselDetail::Cancel,
                        });
                    }
                    PreselDirArg::Cancel => {}
                    PreselDirArg::Set(dir, alternate) => {
                        let cancel =
                            *alternate && t.node(n).presel.is_some_and(|p| p.split_dir == *dir);
                        if cancel {
                            tree_mut(ctx.wm, trg).cancel_presel(n);
                            events.push(Event::NodePresel {
                                monitor: monitor_wire_id(ctx.wm, trg.monitor),
                                desktop: desktop_id(ctx.wm, trg).0,
                                node: wid(ctx, trg),
                                detail: PreselDetail::Cancel,
                            });
                        } else {
                            tree_mut(ctx.wm, trg).presel_dir(n, *dir, default_ratio);
                            events.push(Event::NodePresel {
                                monitor: monitor_wire_id(ctx.wm, trg.monitor),
                                desktop: desktop_id(ctx.wm, trg).0,
                                node: wid(ctx, trg),
                                detail: PreselDetail::Dir(*dir),
                            });
                        }
                    }
                }
            }
            NodeAction::PreselRatio(ratio) => {
                let Some(n) = trg.node else {
                    fail = Some(String::new());
                    break 'actions;
                };
                if tree(ctx.wm, trg).node(n).vacant {
                    fail = Some(String::new());
                    break 'actions;
                }
                let default_dir = Direction::East;
                tree_mut(ctx.wm, trg).presel_ratio(n, *ratio, default_dir);
                events.push(Event::NodePresel {
                    monitor: monitor_wire_id(ctx.wm, trg.monitor),
                    desktop: desktop_id(ctx.wm, trg).0,
                    node: wid(ctx, trg),
                    detail: PreselDetail::Ratio(*ratio),
                });
            }
            NodeAction::Move(dx, dy) => {
                let Some(n) = trg.node else {
                    fail = Some(String::new());
                    break 'actions;
                };
                if !tree_mut(ctx.wm, trg).move_floating(n, *dx, *dy) {
                    fail = Some(String::new());
                    break 'actions;
                }
                // bspwm: `src/window.c` `move_client()` only reports
                // `node_geometry` from this call site (not-grabbing,
                // i.e. every `bspc` command); `move_floating` only ever
                // succeeds for a non-tiled node, so this always fires
                // together with it.
                if let Some(geometry) = tree(ctx.wm, trg).node(n).client.as_ref().map(|c| c.floating_rectangle) {
                    if let Some(c) = tree_mut(ctx.wm, trg).node_mut(n).client.as_mut() {
                        c.window_rectangle = Some(geometry);
                    }
                    events.push(Event::NodeGeometry {
                        monitor: monitor_wire_id(ctx.wm, trg.monitor),
                        desktop: desktop_id(ctx.wm, trg).0,
                        node: wid(ctx, trg),
                        geometry,
                    });
                }
                changed = true;
            }
            NodeAction::Resize(handle, dx, dy) => {
                let Some(n) = trg.node else {
                    fail = Some(String::new());
                    break 'actions;
                };
                let h = to_core_resize_handle(*handle);
                if !tree_mut(ctx.wm, trg).resize_node(n, h, *dx, *dy, true) {
                    fail = Some(String::new());
                    break 'actions;
                }
                // bspwm: `src/window.c` `resize_client()` only reports
                // `node_geometry` for a purely `STATE_FLOATING` node —
                // a tiled resize re-arranges instead (no direct
                // geometry event of its own), and a pseudo-tiled one's
                // `floating_rectangle` is only a size *preference*
                // `apply_layout` reads back, not its real on-screen
                // rectangle yet.
                if let Some(geometry) = tree(ctx.wm, trg)
                    .node(n)
                    .client
                    .as_ref()
                    .filter(|c| c.state == bsp_core::node::ClientState::Floating)
                    .map(|c| c.floating_rectangle)
                {
                    if let Some(c) = tree_mut(ctx.wm, trg).node_mut(n).client.as_mut() {
                        c.window_rectangle = Some(geometry);
                    }
                    events.push(Event::NodeGeometry {
                        monitor: monitor_wire_id(ctx.wm, trg.monitor),
                        desktop: desktop_id(ctx.wm, trg).0,
                        node: wid(ctx, trg),
                        geometry,
                    });
                }
                changed = true;
            }
            NodeAction::SetSplitType(arg) => {
                let Some(n) = trg.node else {
                    fail = Some(String::new());
                    break 'actions;
                };
                let t = tree_mut(ctx.wm, trg);
                let target_type = match arg {
                    SplitTypeArg::Cycle => match t.node(n).split_type {
                        bsp_core::tree::SplitType::Horizontal => {
                            bsp_core::tree::SplitType::Vertical
                        }
                        bsp_core::tree::SplitType::Vertical => {
                            bsp_core::tree::SplitType::Horizontal
                        }
                    },
                    SplitTypeArg::Set(t) => *t,
                };
                t.node_mut(n).split_type = target_type;
                changed = true;
            }
            NodeAction::SetRatio(arg) => {
                let Some(n) = trg.node else {
                    fail = Some(String::new());
                    break 'actions;
                };
                let t = tree(ctx.wm, trg);
                let node = t.node(n);
                let rat = match arg {
                    RatioArg::Absolute(r) => Some(*r),
                    RatioArg::Delta(delta) => {
                        let mut rat = node.split_ratio;
                        if *delta > -1.0 && *delta < 1.0 {
                            rat += *delta as f64;
                        } else {
                            let max = if node.split_type == bsp_core::tree::SplitType::Horizontal {
                                node.rect.height
                            } else {
                                node.rect.width
                            };
                            rat = ((max as f64 * rat) + *delta as f64) / max as f64;
                        }
                        if rat > 0.0 && rat < 1.0 {
                            Some(rat)
                        } else {
                            None
                        }
                    }
                };
                match rat {
                    Some(r) => {
                        tree_mut(ctx.wm, trg).node_mut(n).split_ratio = r;
                        changed = true;
                    }
                    None => {
                        fail = Some(String::new());
                        break 'actions;
                    }
                }
            }
            NodeAction::Rotate(deg) => {
                let n = trg.node;
                tree_mut(ctx.wm, trg).rotate_tree(n, *deg);
                changed = true;
            }
            NodeAction::Flip(axis) => {
                let n = trg.node;
                tree_mut(ctx.wm, trg).flip_tree(n, *axis);
                changed = true;
            }
            NodeAction::Equalize => {
                let n = trg.node;
                let settings = ctx.wm.settings.clone();
                tree_mut(ctx.wm, trg).equalize_tree(n, &settings);
                changed = true;
            }
            NodeAction::Balance => {
                let n = trg.node;
                tree_mut(ctx.wm, trg).balance_tree(n);
                changed = true;
            }
            NodeAction::Circulate(dir) => {
                let Some(n) = trg.node else {
                    fail = Some(String::new());
                    break 'actions;
                };
                let settings = ctx.wm.settings.clone();
                // bspwm keeps the focus at the same place in the tree: the child
                // slot of the focused node's parent, whichever window lands there.
                let slot = {
                    let t = tree(ctx.wm, trg);
                    t.focus.and_then(|f| t.node(f).parent()).map(|p| (p, t.focus.is_some_and(|f| t.is_first_child(f))))
                };
                tree_mut(ctx.wm, trg).circulate_leaves(&settings, Some(n), *dir);
                changed = true;
                if let Some((p, first)) = slot {
                    let t = tree(ctx.wm, trg);
                    let f = if first { t.node(p).first_child() } else { t.node(p).second_child() };
                    if let Some(f) = f.filter(|&f| t.is_leaf(f)) {
                        let target = Coordinates { node: Some(f), ..trg };
                        if is_focused_desktop(ctx.wm, trg) {
                            focus_node(ctx, target, &mut events);
                        } else {
                            activate_node(ctx, target, &mut events);
                        }
                    }
                }
            }
            NodeAction::InsertReceptacle => {
                let settings = ctx.wm.settings.clone();
                let before = presels_before_insert(ctx, trg, trg.node, &mut events);
                let t = tree_mut(ctx.wm, trg);
                let r = t.new_node(&settings);
                t.insert_node(&settings, r, trg.node);
                let d = desktop_id(ctx.wm, trg);
                ctx.registry.register_with_split(d, tree(ctx.wm, trg), r);
                push_consumed_presels(ctx, trg, &before, &mut events);
                // bspwm: `insert_receptacle()` reports `node_add`.
                events.push(Event::NodeAdd {
                    monitor: monitor_wire_id(ctx.wm, trg.monitor),
                    desktop: d.0,
                    ip_id: wid(ctx, trg),
                    node: ctx.registry.id_of(d, r).unwrap_or(0),
                });
                changed = true;
            }
            NodeAction::SetState(arg) => {
                let Some(n) = trg.node else {
                    fail = Some(String::new());
                    break 'actions;
                };
                let t = tree(ctx.wm, trg);
                let Some(client) = t.node(n).client.as_ref() else {
                    fail = Some(String::new());
                    break 'actions;
                };
                let target = match arg {
                    StateArg::ToLastState => client.last_state,
                    StateArg::Value(state, alternate) => {
                        if *alternate && client.state == *state {
                            client.last_state
                        } else {
                            *state
                        }
                    }
                };
                if !set_state_reporting(ctx, trg, target, &mut events) {
                    fail = Some(String::new());
                    break 'actions;
                }
                changed = true;
            }
            NodeAction::SetFlag(key, alter) => {
                let Some(n) = trg.node else {
                    fail = Some(String::new());
                    break 'actions;
                };
                let t = tree(ctx.wm, trg);
                let node = t.node(n);
                let current = match key {
                    NodeFlagKey::Hidden => node.hidden,
                    NodeFlagKey::Sticky => node.sticky,
                    NodeFlagKey::Private => node.private,
                    NodeFlagKey::Locked => node.locked,
                    NodeFlagKey::Marked => node.marked,
                };
                let value = match alter {
                    AlterState::Toggle => !current,
                    AlterState::Set(b) => *b,
                };
                if value != current {
                    trg = set_flag_reporting(ctx, trg, *key, value, &mut events);
                    changed = true;
                }
            }
            NodeAction::SetLayer(layer) => {
                let Some(n) = trg.node else {
                    fail = Some(String::new());
                    break 'actions;
                };
                if !tree_mut(ctx.wm, trg).set_layer(n, *layer) {
                    fail = Some(String::new());
                    break 'actions;
                }
                events.push(Event::NodeLayer {
                    monitor: monitor_wire_id(ctx.wm, trg.monitor),
                    desktop: desktop_id(ctx.wm, trg).0,
                    node: wid(ctx, trg),
                    layer: *layer,
                });
                let focused = tree(ctx.wm, trg).focus == Some(n);
                stack_node(ctx, trg, focused, &mut events);
            }
            NodeAction::Close => {
                // bspwm: `close_node()` asks every window of the subtree to close;
                // a receptacle has none, which is not an error.
                let Some(n) = trg.node else {
                    fail = Some(String::new());
                    break 'actions;
                };
                let t = tree(ctx.wm, trg);
                if t.locked_count(Some(n)) > 0 {
                    fail = Some(String::new());
                    break 'actions;
                }
                for window in subtree_windows(t, n) {
                    ctx.adapter.close_window(window);
                }
                break 'actions;
            }
            NodeAction::Kill => {
                // bspwm: `kill_node()` kills the client of every window of the
                // subtree and leaves the nodes in the tree: they are removed when
                // the windows are actually destroyed. Only a receptacle, which has
                // no window to wait for, is removed at once.
                let Some(n) = trg.node else {
                    fail = Some(String::new());
                    break 'actions;
                };
                if tree(ctx.wm, trg).is_receptacle(n) {
                    remove_node_reporting(ctx, trg, n, &mut events);
                    changed = true;
                } else {
                    for window in subtree_windows(tree(ctx.wm, trg), n) {
                        ctx.adapter.kill_window(window);
                    }
                }
                break 'actions;
            }
        }
    }

    if changed {
        arrange(ctx, trg);
    }

    let reply = match fail {
        Some(msg) => Reply::Fail(msg),
        None => Reply::Ok(String::new()),
    };
    (reply, events)
}

/// Where a new window goes when a matched rule names a place: the node
/// (`node=`), else the desktop (`desktop=`, at its focus), else the monitor
/// (`monitor=`, at its shown desktop). `None` if the rule names none or the
/// selector matches nothing, and the window goes where the focus is. The node of
/// the result is the window's insertion anchor. Selectors are resolved from the
/// focused monitor and desktop.
///
/// bspwm: `src/window.c` `manage_window()`'s `node_desc`/`desktop_desc`/`monitor_desc`
/// blocks (a sticky window ignores them).
pub fn resolve_rule_target<A: Adapter>(ctx: &ExecCtx<A>, csq: &bsp_core::rules::RuleConsequence) -> Option<Coordinates> {
    if csq.sticky == Some(true) {
        return None;
    }
    let mi = ctx.wm.focused_monitor?;
    let di = ctx.wm.monitors[mi].focused?;
    let here = Coordinates { monitor: mi, desktop: di, node: ctx.wm.monitors[mi].desktops[di].tree.focus };
    if let Some(desc) = &csq.node_desc {
        let sel = crate::command::parse_node_selector_arg(desc).ok()?;
        return resolve_node(ctx, here, &sel).ok();
    }
    if let Some(desc) = &csq.desktop_desc {
        let sel = crate::command::parse_desktop_selector_arg(desc).ok()?;
        let c = resolve_desktop(ctx, Coordinates { node: None, ..here }, &sel).ok()?;
        return Some(Coordinates { node: tree(ctx.wm, c).focus, ..c });
    }
    if let Some(desc) = &csq.monitor_desc {
        let sel = crate::command::parse_monitor_selector_arg(desc).ok()?;
        let c = resolve_monitor(ctx, Coordinates { desktop: 0, node: None, ..here }, &sel).ok()?;
        let d = ctx.wm.monitors[c.monitor].focused?;
        let c = Coordinates { monitor: c.monitor, desktop: d, node: None };
        return Some(Coordinates { node: tree(ctx.wm, c).focus, ..c });
    }
    None
}

/// A window about to be managed, as only the window system can describe it.
#[derive(Debug, Clone, Copy)]
pub struct NewWindow {
    /// Its id (already known to the adapter, with its class and instance).
    pub window: WindowId,
    /// The geometry it has of its own (an X11 window's, bspwm's
    /// `initialize_floating_rectangle()`); `None` for a Wayland window, which
    /// has none before its first configure.
    pub geometry: Option<bsp_core::geometry::Rect>,
    /// Its ICCCM `WM_NORMAL_HINTS` or xdg min/max size.
    pub size_hints: bsp_core::node::SizeHints,
}

/// Puts a new window into the tree, as bspwm's `manage_window()` does from the
/// rule target on: the node goes next to the target's focused node (after the
/// rule's `split_dir`/`split_ratio` preselect it), gets the rule's state,
/// layer and flags, is arranged, reported (`node_add`), and focused, activated
/// or stacked. The `manage=off` branch and EWMH struts stay with the caller,
/// as does showing it. Returns where it went, `None` when there is no focused
/// desktop to put it on.
///
/// bspwm: `src/window.c` `manage_window()`.
pub fn manage_window<A: Adapter>(
    ctx: &mut ExecCtx<A>,
    new: &NewWindow,
    csq: &bsp_core::rules::RuleConsequence,
    events: &mut Vec<Event>,
) -> Option<Coordinates> {
    let mi = ctx.wm.focused_monitor?;
    let di = ctx.wm.monitors[mi].focused?;
    // `monitor=`/`desktop=`/`node=` name where it goes; a sticky one stays on
    // the focused desktop.
    let trg = resolve_rule_target(ctx, csq).unwrap_or(Coordinates {
        monitor: mi,
        desktop: di,
        node: ctx.wm.monitors[mi].desktops[di].tree.focus,
    });
    let settings = ctx.wm.settings.clone();
    let desktop_border_width = ctx.wm.monitors[trg.monitor].desktops[trg.desktop].border_width;
    let monitor_rect = ctx.wm.monitors[trg.monitor].rectangle;
    let d = desktop_id(ctx.wm, trg);

    if let Some(f) = trg.node {
        let at = Coordinates { node: Some(f), ..trg };
        if let Some(dir) = csq.split_dir {
            tree_mut(ctx.wm, trg).presel_dir(f, dir, settings.split_ratio);
            events.push(Event::NodePresel { monitor: monitor_wire_id(ctx.wm, trg.monitor), desktop: d.0, node: wid(ctx, at), detail: PreselDetail::Dir(dir) });
        }
        if let Some(ratio) = csq.split_ratio {
            // `presel_ratio()` defaults to east when nothing is preselected.
            tree_mut(ctx.wm, trg).presel_ratio(f, ratio, Direction::East);
            events.push(Event::NodePresel { monitor: monitor_wire_id(ctx.wm, trg.monitor), desktop: d.0, node: wid(ctx, at), detail: PreselDetail::Ratio(ratio) });
        }
    }
    let border_width = if csq.should_border() { desktop_border_width } else { 0 };
    let mut client = bsp_core::node::Client::new(new.window, border_width);
    client.size_hints = new.size_hints;
    client.window_rectangle = new.geometry;
    let mut center = csq.should_center();
    if let Some(geometry) = new.geometry {
        client.floating_rectangle = geometry;
    }
    if let Some(rect) = csq.rect {
        client.floating_rectangle = rect;
    } else if new.geometry.is_some_and(|g| g.x == 0 && g.y == 0) {
        // A window that asked for no position is centred.
        center = true;
    }
    if new.geometry.is_some() || csq.rect.is_some() {
        // `embrace_client()` and `adapt_geometry()`: brought inside the
        // monitor it overlaps, then moved proportionally onto the target's.
        let from = monitor_from_rect(ctx.wm, client.floating_rectangle).unwrap_or(monitor_rect);
        embrace_rect(&mut client.floating_rectangle, from);
        client.floating_rectangle = adapted_rect(client.floating_rectangle, from, monitor_rect);
    }
    if center && (new.geometry.is_some() || csq.rect.is_some()) {
        center_rect(&mut client.floating_rectangle, monitor_rect, border_width);
    }

    let t = tree_mut(ctx.wm, trg);
    let n = t.new_client_node(&settings, client);
    if let (Some(honor), Some(c)) = (csq.honor_size_hints, t.node_mut(n).client.as_mut()) {
        c.honor_size_hints = honor;
    }
    // Kept out of the tiling while it is inserted when it will not tile.
    let will_not_tile = matches!(csq.state, Some(bsp_core::node::ClientState::Floating | bsp_core::node::ClientState::Fullscreen))
        || csq.hidden == Some(true);
    t.node_mut(n).vacant = will_not_tile;
    let before = presels_before_insert(ctx, trg, trg.node, events);
    let t = tree_mut(ctx.wm, trg);
    let used_anchor = t.insert_node(&settings, n, trg.node);
    t.node_mut(n).vacant = false;
    ctx.registry.register(d, n);
    // The split the insertion made needs an id too.
    ctx.registry.sync_with(ctx.wm);
    push_consumed_presels(ctx, trg, &before, events);
    events.push(Event::NodeAdd {
        monitor: monitor_wire_id(ctx.wm, trg.monitor),
        desktop: d.0,
        ip_id: used_anchor.and_then(|a| ctx.registry.id_of(d, a)).unwrap_or(0),
        node: ctx.registry.id_of(d, n).unwrap_or(0),
    });

    let at = Coordinates { node: Some(n), ..trg };
    // A Wayland window has no geometry of its own: it floats where it tiles,
    // the slot computed before its state can make it vacant (a vacant node
    // gets no tiled rectangle).
    let seed_from_slot = new.geometry.is_none() && csq.rect.is_none();
    if seed_from_slot {
        ctx.wm.monitors[trg.monitor].arrange(trg.desktop, &settings);
        if let Some(c) = tree_mut(ctx.wm, at).node_mut(n).client.as_mut() {
            c.floating_rectangle = c.tiled_rectangle;
        }
    }
    let t = tree_mut(ctx.wm, at);
    // A floating window opened over a window takes that window's layer.
    if csq.state == Some(bsp_core::node::ClientState::Floating) {
        if let Some(layer) = used_anchor.and_then(|a| t.node(a).client.as_ref()).map(|c| c.layer) {
            if let Some(c) = t.node_mut(n).client.as_mut() {
                c.layer = layer;
            }
        }
    }
    if let Some(layer) = csq.layer {
        t.set_layer(n, layer);
    }
    if let Some(state) = csq.state {
        set_state_reporting(ctx, at, state, events);
    }
    if seed_from_slot && center {
        if let Some(c) = tree_mut(ctx.wm, at).node_mut(n).client.as_mut().filter(|c| !c.state.is_tiled()) {
            center_rect(&mut c.floating_rectangle, monitor_rect, border_width);
        }
    }
    // A new node's flags are all off: only those the rule turns on change.
    let mut at = at;
    for (key, value) in [
        (NodeFlagKey::Hidden, csq.hidden),
        (NodeFlagKey::Sticky, csq.sticky),
        (NodeFlagKey::Private, csq.private),
        (NodeFlagKey::Locked, csq.locked),
        (NodeFlagKey::Marked, csq.marked),
    ] {
        if value == Some(true) {
            at = set_flag_reporting(ctx, at, key, true, events);
        }
    }
    ctx.wm.monitors[at.monitor].arrange(at.desktop, &settings);

    if csq.hidden != Some(true) && csq.should_focus() {
        if is_focused_desktop(ctx.wm, at) || csq.should_follow() {
            focus_node(ctx, at, events);
        } else {
            activate_node(ctx, at, events);
        }
    } else {
        // Not focused: the bottom of its level.
        stack_node(ctx, at, false, events);
    }
    Some(at)
}

/// Takes a closed window's node out of the tree (`node_remove`), refocuses its
/// desktop if it held the focus, and re-arranges the desktop. Returns where it
/// was, `None` when it was not in the tree.
///
/// bspwm: `src/window.c` `unmanage_window()`.
pub fn unmanage_window<A: Adapter>(ctx: &mut ExecCtx<A>, window: WindowId, events: &mut Vec<Event>) -> Option<Coordinates> {
    let at = locate_window(ctx.wm, window)?;
    let n = at.node?;
    remove_node_reporting(ctx, at, n, events);
    let settings = ctx.wm.settings.clone();
    ctx.wm.monitors[at.monitor].arrange(at.desktop, &settings);
    Some(at)
}

/// Where `window`'s node is.
fn locate_window(wm: &Wm, window: WindowId) -> Option<Coordinates> {
    wm.monitors.iter().enumerate().find_map(|(mi, m)| {
        m.desktops.iter().enumerate().find_map(|(di, d)| {
            node_of_window(&d.tree, window).map(|n| Coordinates { monitor: mi, desktop: di, node: Some(n) })
        })
    })
}

/// The rectangle of the monitor that holds `r`'s centre, or else the nearest
/// one (bspwm: `monitor_from_client()`).
fn monitor_from_rect(wm: &Wm, r: bsp_core::geometry::Rect) -> Option<bsp_core::geometry::Rect> {
    let (xc, yc) = (r.x + r.width / 2, r.y + r.height / 2);
    let rects = wm.monitors.iter().map(|m| m.rectangle);
    rects
        .clone()
        .find(|m| xc >= m.x && xc < m.x + m.width && yc >= m.y && yc < m.y + m.height)
        .or_else(|| rects.min_by_key(|m| ((m.x + m.width / 2) - xc).abs() + ((m.y + m.height / 2) - yc).abs()))
}

/// Brings a rectangle lying wholly outside `m` back to its edge (bspwm:
/// `embrace_client()`).
fn embrace_rect(r: &mut bsp_core::geometry::Rect, m: bsp_core::geometry::Rect) {
    if r.x + r.width <= m.x {
        r.x = m.x;
    } else if r.x >= m.x + m.width {
        r.x = m.x + m.width - r.width;
    }
    if r.y + r.height <= m.y {
        r.y = m.y;
    } else if r.y >= m.y + m.height {
        r.y = m.y + m.height - r.height;
    }
}

/// `r` moved from monitor rectangle `from` onto `to` as `adapt_geometry()`
/// moves a floating window.
fn adapted_rect(r: bsp_core::geometry::Rect, from: bsp_core::geometry::Rect, to: bsp_core::geometry::Rect) -> bsp_core::geometry::Rect {
    if from == to {
        return r;
    }
    let mut t = Tree::new();
    let mut client = bsp_core::node::Client::new(WindowId(0), 0);
    client.floating_rectangle = r;
    let n = t.new_client_node(&bsp_core::settings::Settings::default(), client);
    t.root = Some(n);
    bsp_core::monitor::adapt_geometry(&mut t, Some(n), from, to);
    t.node(n).client.as_ref().map_or(r, |c| c.floating_rectangle)
}

/// Centres `r` in `a`, pinned to its top left when it does not fit (bspwm:
/// `window_center()`).
pub fn center_rect(r: &mut bsp_core::geometry::Rect, a: bsp_core::geometry::Rect, border_width: i32) {
    r.x = if r.width >= a.width { a.x } else { a.x + (a.width - r.width) / 2 };
    r.y = if r.height >= a.height { a.y } else { a.y + (a.height - r.height) / 2 };
    r.x -= border_width;
    r.y -= border_width;
}

/// Swaps desktop `a` with desktop `b` (on the same monitor or on two): they trade
/// places, and each monitor keeps showing whatever is in its shown slot. On two
/// monitors the windows are moved onto the other monitor's rectangle and both
/// desktops re-arranged. Focus stays on its slot, or, with `follow` (and always
/// on one monitor), goes with the desktop it was on.
///
/// bspwm: `src/desktop.c` `swap_desktops()`.
pub fn swap_desktops<A: Adapter>(ctx: &mut ExecCtx<A>, a: Coordinates, b: Coordinates, follow: bool, events: &mut Vec<Event>) -> bool {
    if a.monitor == b.monitor && a.desktop == b.desktop {
        return false;
    }
    let (m1, i1, m2, i2) = (a.monitor, a.desktop, b.monitor, b.desktop);
    events.push(Event::DesktopSwap {
        src_monitor: monitor_wire_id(ctx.wm, m1),
        src_desktop: ctx.wm.monitors[m1].desktops[i1].id.0,
        dst_monitor: monitor_wire_id(ctx.wm, m2),
        dst_desktop: ctx.wm.monitors[m2].desktops[i2].id.0,
    });
    let d1_active = ctx.wm.monitors[m1].focused == Some(i1);
    let d2_active = ctx.wm.monitors[m2].focused == Some(i2);
    let d1_focused = d1_active && ctx.wm.focused_monitor == Some(m1);
    let d2_focused = d2_active && ctx.wm.focused_monitor == Some(m2);
    // bspwm parks the sticky nodes of a shown desktop on a scratch desktop and
    // puts them on whatever the monitor shows after the swap: the other desktop.
    // Collected first so the second move cannot take the first's back.
    let at = |monitor: usize, desktop: usize| Coordinates { monitor, desktop, node: None };
    let stickies1 = if d1_active { sticky_subtree_windows(tree(ctx.wm, at(m1, i1))) } else { Vec::new() };
    let stickies2 = if d2_active { sticky_subtree_windows(tree(ctx.wm, at(m2, i2))) } else { Vec::new() };
    move_windows_subtrees(ctx, at(m1, i1), at(m2, i2), &stickies1, events);
    move_windows_subtrees(ctx, at(m2, i2), at(m1, i1), &stickies2, events);
    if m1 == m2 {
        ctx.wm.monitors[m1].desktops.swap(i1, i2);
    } else {
        let (lo, hi, lo_i, hi_i) = if m1 < m2 { (m1, m2, i1, i2) } else { (m2, m1, i2, i1) };
        let (left, right) = ctx.wm.monitors.split_at_mut(hi);
        std::mem::swap(&mut left[lo].desktops[lo_i], &mut right[0].desktops[hi_i]);
        let (r1, r2) = (ctx.wm.monitors[m1].rectangle, ctx.wm.monitors[m2].rectangle);
        // d1 is on monitor 2 now, d2 on monitor 1.
        let root = ctx.wm.monitors[m2].desktops[i2].tree.root;
        bsp_core::monitor::adapt_geometry(&mut ctx.wm.monitors[m2].desktops[i2].tree, root, r1, r2);
        let root = ctx.wm.monitors[m1].desktops[i1].tree.root;
        bsp_core::monitor::adapt_geometry(&mut ctx.wm.monitors[m1].desktops[i1].tree, root, r2, r1);
        arrange(ctx, Coordinates { monitor: m1, desktop: i1, node: None });
        arrange(ctx, Coordinates { monitor: m2, desktop: i2, node: None });
    }
    // d1 now lives at (m2, i2) and d2 at (m1, i1).
    let (t1, t2) = if follow || m1 == m2 { ((m2, i2), (m1, i1)) } else { ((m1, i1), (m2, i2)) };
    let at = |(monitor, desktop): (usize, usize)| Coordinates { monitor, desktop, node: None };
    if d1_focused {
        focus_node(ctx, at(t1), events);
    } else if d1_active {
        activate_node(ctx, at(t1), events);
    }
    if d2_focused {
        focus_node(ctx, at(t2), events);
    } else if d2_active {
        activate_node(ctx, at(t2), events);
    }
    true
}

/// The desktop `monitor` should show once the one at `removed` is gone: the last
/// one it showed before (`history`), else the neighbour `remove_desktop` chose.
fn desktop_after_removal(wm: &Wm, monitor: usize, removed: DesktopId) -> Option<usize> {
    let m = &wm.monitors[monitor];
    wm.history
        .last_desktop(m.id, removed)
        .and_then(|id| m.desktops.iter().position(|d| d.id == id))
        .or(m.focused)
}

/// Moves the desktop at `src` to the end of monitor `dst_monitor`'s desktops,
/// laying its windows out on the new monitor. The monitor it left shows another
/// desktop. Returns the desktop's new index, or `None` for the same monitor.
///
/// bspwm: `src/desktop.c` `transfer_desktop()`.
pub fn transfer_desktop<A: Adapter>(ctx: &mut ExecCtx<A>, src: Coordinates, dst_monitor: usize, follow: bool, events: &mut Vec<Event>) -> Option<usize> {
    let ms = src.monitor;
    if ms == dst_monitor {
        return None;
    }
    let d_was_active = ctx.wm.monitors[ms].focused == Some(src.desktop);
    let ms_was_focused = ctx.wm.focused_monitor == Some(ms);
    let (ms_id, d_id) = (ctx.wm.monitors[ms].id, ctx.wm.monitors[ms].desktops[src.desktop].id);
    // bspwm: `sc`, the sticky nodes that were on screen with the desktop.
    let had_stickies = d_was_active && tree(ctx.wm, src).sticky_count(tree(ctx.wm, src).root) > 0;
    let d = ctx.wm.monitors[ms].remove_desktop(src.desktop);
    if d_was_active {
        if let Some(next) = desktop_after_removal(ctx.wm, ms, d_id) {
            ctx.wm.monitors[ms].focused = Some(next);
        }
    }
    let new_index = ctx.wm.monitors[dst_monitor].desktops.len();
    ctx.wm.monitors[dst_monitor].insert_desktop(d);
    events.push(Event::DesktopTransfer {
        src_monitor: ms_id.0,
        src_desktop: d_id.0,
        dst_monitor: monitor_wire_id(ctx.wm, dst_monitor),
    });
    let (rs, rd) = (ctx.wm.monitors[ms].rectangle, ctx.wm.monitors[dst_monitor].rectangle);
    let moved = Coordinates { monitor: dst_monitor, desktop: new_index, node: None };
    let root = tree(ctx.wm, moved).root;
    bsp_core::monitor::adapt_geometry(tree_mut(ctx.wm, moved), root, rs, rd);
    arrange(ctx, moved);
    let left = ctx.wm.monitors[ms].focused.map(|desktop| Coordinates { monitor: ms, desktop, node: None });
    if d_was_active {
        if follow {
            if let Some(left) = left {
                activate_node(ctx, left, events);
            }
            if ms_was_focused {
                focus_node(ctx, moved, events);
            }
        } else if let Some(left) = left {
            if ms_was_focused {
                focus_node(ctx, left, events);
            } else {
                activate_node(ctx, left, events);
            }
        }
    }
    // bspwm: the sticky nodes stay on the monitor they were shown on (or, if it
    // has no desktop left, on the one the desktop went to).
    if had_stickies {
        if let Some(desktop) = ctx.wm.monitors[ms].focused {
            move_sticky_subtrees(ctx, moved, Coordinates { monitor: ms, desktop, node: None }, events);
        } else if let Some(desktop) = ctx.wm.monitors[dst_monitor].focused.filter(|&d| d != new_index) {
            move_sticky_subtrees(ctx, moved, Coordinates { monitor: dst_monitor, desktop, node: None }, events);
        }
    }
    // The monitor had no desktop of its own: this one is shown there now.
    if (!follow || !d_was_active || !ms_was_focused) && ctx.wm.monitors[dst_monitor].desktops.len() == 1 {
        if ctx.wm.focused_monitor == Some(dst_monitor) {
            focus_node(ctx, moved, events);
        } else {
            activate_node(ctx, moved, events);
        }
    }
    Some(new_index)
}

/// Removes a desktop, first moving its windows to the previous desktop of the
/// monitor (the next one for the first). Fails on a monitor's only desktop.
///
/// bspwm: `src/messages.c` `cmd_desktop()`'s `-r`: `merge_desktops()` then
/// `remove_desktop()`.
fn remove_desktop_merging<A: Adapter>(ctx: &mut ExecCtx<A>, trg: Coordinates, events: &mut Vec<Event>) -> bool {
    let m = trg.monitor;
    if ctx.wm.monitors[m].desktops.len() <= 1 {
        return false;
    }
    let fallback = if trg.desktop == 0 { 1 } else { trg.desktop - 1 };
    let from = Coordinates { monitor: m, desktop: trg.desktop, node: tree(ctx.wm, trg).root };
    let to = Coordinates { monitor: m, desktop: fallback, node: None };
    if from.node.is_some() {
        let anchor = tree(ctx.wm, to).focus;
        let _ = transfer_node_unchecked(ctx, from, to, anchor, false, events);
    }
    let was_active = ctx.wm.monitors[m].focused == Some(trg.desktop);
    let id = ctx.wm.monitors[m].desktops[trg.desktop].id;
    events.push(Event::DesktopRemove { monitor: monitor_wire_id(ctx.wm, m), desktop: id.0 });
    ctx.wm.monitors[m].remove_desktop(trg.desktop);
    if was_active {
        if let Some(next) = desktop_after_removal(ctx.wm, m, id) {
            ctx.wm.monitors[m].focused = Some(next);
            let shown = Coordinates { monitor: m, desktop: next, node: None };
            if ctx.wm.focused_monitor == Some(m) {
                focus_node(ctx, shown, events);
            } else {
                activate_node(ctx, shown, events);
            }
        }
    }
    true
}

/// The level (`Client::stack_level`) of every managed window.
fn window_levels(wm: &Wm) -> std::collections::HashMap<WindowId, i32> {
    let mut levels = std::collections::HashMap::new();
    for m in &wm.monitors {
        for d in &m.desktops {
            let mut f = d.tree.first_extrema(d.tree.root);
            while let Some(n) = f {
                if let Some(c) = d.tree.node(n).client.as_ref() {
                    levels.insert(c.window, c.stack_level());
                }
                f = d.tree.next_leaf(Some(n), d.tree.root);
            }
        }
    }
    levels
}

/// The wire id of the node showing `window`, 0 if there is none.
fn wire_id_of_window<A: Adapter>(ctx: &ExecCtx<A>, window: WindowId) -> WireNodeId {
    for m in &ctx.wm.monitors {
        for d in &m.desktops {
            let mut f = d.tree.first_extrema(d.tree.root);
            while let Some(n) = f {
                if d.tree.node(n).client.as_ref().is_some_and(|c| c.window == window) {
                    return ctx.registry.id_of(d.id, n).unwrap_or(0);
                }
                f = d.tree.next_leaf(Some(n), d.tree.root);
            }
        }
    }
    0
}

/// Puts every window under `trg`'s node in its place in the stacking order: on
/// top of its level when `focused`, at the bottom of it otherwise, reporting
/// `node_stack` for each move.
///
/// bspwm: `src/stack.c` `stack()`.
pub fn stack_node<A: Adapter>(ctx: &mut ExecCtx<A>, trg: Coordinates, focused: bool, events: &mut Vec<Event>) {
    let Some(n) = trg.node else { return };
    let windows = subtree_windows(tree(ctx.wm, trg), n);
    let levels = window_levels(ctx.wm);
    for window in windows {
        if let Some(m) = ctx.wm.stacking.stack(window, focused, |w| levels.get(&w).copied()) {
            events.push(Event::NodeStack {
                node: wire_id_of_window(ctx, m.window),
                above: m.above,
                sibling: wire_id_of_window(ctx, m.sibling),
            });
        }
    }
}

/// Drops every window under `n` from the stacking order (its node is going away).
///
/// bspwm: `src/stack.c` `remove_stack_node()`.
pub fn unstack_node(wm: &mut Wm, trg: Coordinates, n: NodeId) {
    for window in subtree_windows(tree(wm, trg), n) {
        wm.stacking.remove(window);
    }
}

/// The windows of every client leaf under `n`, in tree order.
fn subtree_windows(t: &Tree, n: NodeId) -> Vec<WindowId> {
    let mut windows = Vec::new();
    let mut f = t.first_extrema(Some(n));
    while let Some(leaf) = f {
        if let Some(c) = t.node(leaf).client.as_ref() {
            windows.push(c.window);
        }
        f = t.next_leaf(Some(leaf), Some(n));
    }
    windows
}

/// Splits `s` at the colons not escaped with a backslash, dropping the escapes.
///
/// bspwm: `src/helpers.c` `tokenize_with_escape()` with `COL_TOK`.
fn split_escaped_colons(s: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let (Some(next), Some(field)) = (chars.next(), fields.last_mut()) {
                    field.push(next);
                }
            }
            ':' => fields.push(String::new()),
            c => {
                if let Some(field) = fields.last_mut() {
                    field.push(c);
                }
            }
        }
    }
    fields
}

/// Before inserting at `anchor` in `desk`: the nodes that have a
/// preselection, for [`push_consumed_presels`], including the one the
/// insertion makes itself next to a private node (reported here as bspwm's
/// `presel_dir()` reports it).
fn presels_before_insert<A: Adapter>(ctx: &ExecCtx<A>, desk: Coordinates, anchor: Option<NodeId>, events: &mut Vec<Event>) -> Vec<NodeId> {
    let t = tree(ctx.wm, desk);
    let mut before: Vec<NodeId> = t.node_ids().into_iter().filter(|&n| t.node(n).presel.is_some()).collect();
    if let Some(f) = anchor.or(t.root).filter(|&f| !t.is_receptacle(f)) {
        if let (f, Some(dir)) = t.private_insertion(f) {
            events.push(Event::NodePresel {
                monitor: monitor_wire_id(ctx.wm, desk.monitor),
                desktop: desktop_id(ctx.wm, desk).0,
                node: wid(ctx, Coordinates { node: Some(f), ..desk }),
                detail: PreselDetail::Dir(dir),
            });
            before.push(f);
        }
    }
    before
}

/// Reports `node_presel ... cancel` for each of `before` (the desktop's
/// preselected nodes before an insertion) whose preselection the insertion
/// used up.
///
/// bspwm: `src/tree.c` `insert_node()` -> `cancel_presel()`.
fn push_consumed_presels<A: Adapter>(ctx: &ExecCtx<A>, desk: Coordinates, before: &[NodeId], events: &mut Vec<Event>) {
    let t = tree(ctx.wm, desk);
    for &n in before {
        if t.contains(n) && t.node(n).presel.is_none() {
            events.push(Event::NodePresel {
                monitor: monitor_wire_id(ctx.wm, desk.monitor),
                desktop: desktop_id(ctx.wm, desk).0,
                node: wid(ctx, Coordinates { node: Some(n), ..desk }),
                detail: PreselDetail::Cancel,
            });
        }
    }
}

/// Removes a node at once (`node_remove`): a closed window's, or an empty
/// receptacle's; and refocuses the desktop if it was the focused node.
///
/// bspwm: `src/tree.c` `kill_node()`'s receptacle branch, `remove_node()`.
fn remove_node_reporting<A: Adapter>(ctx: &mut ExecCtx<A>, trg: Coordinates, n: NodeId, events: &mut Vec<Event>) {
    let d = desktop_id(ctx.wm, trg);
    events.push(Event::NodeRemove {
        monitor: monitor_wire_id(ctx.wm, trg.monitor),
        desktop: d.0,
        node: ctx.registry.id_of(d, n).unwrap_or(0),
    });
    let settings = ctx.wm.settings.clone();
    unstack_node(ctx.wm, trg, n);
    tree_mut(ctx.wm, trg).remove_node(&settings, n);
    ctx.registry.unregister(d, n);
    if tree(ctx.wm, trg).focus.is_none() {
        let desk = Coordinates { node: None, ..trg };
        if is_focused_desktop(ctx.wm, trg) {
            focus_node(ctx, desk, events);
        } else {
            activate_node(ctx, desk, events);
        }
    }
}

fn resolve_or_default<A: Adapter>(
    ctx: &ExecCtx<A>,
    reference: Coordinates,
    sel: Option<&NodeSelector>,
    default: Coordinates,
) -> Result<Coordinates, ResolveError> {
    match sel {
        Some(s) => resolve_node(ctx, reference, s),
        None => Ok(default),
    }
}

trait ReplyExt {
    fn into_message(self) -> String;
}
impl ReplyExt for Reply {
    fn into_message(self) -> String {
        match self {
            Reply::Fail(m) => m,
            Reply::Ok(_) => String::new(),
        }
    }
}

/// Whether `c` names the desktop that is on screen on the focused monitor
/// (bspwm: `d == mon->desk`).
fn is_focused_desktop(wm: &Wm, c: Coordinates) -> bool {
    wm.focused_monitor == Some(c.monitor) && wm.monitors[c.monitor].focused == Some(c.desktop)
}

/// Sets one of a node's flags to `value` (it differs from the current one) and
/// reports it (`node_flag`). Hiding the focused node moves the focus on;
/// making a node on a desktop that is not shown sticky first brings it to the
/// shown one, and the returned coordinates are where it is then.
///
/// bspwm: `src/tree.c` `set_hidden()`, `set_sticky()`, `set_private()`,
/// `set_locked()`, `set_marked()`.
fn set_flag_reporting<A: Adapter>(
    ctx: &mut ExecCtx<A>,
    mut trg: Coordinates,
    key: NodeFlagKey,
    value: bool,
    events: &mut Vec<Event>,
) -> Coordinates {
    let Some(n) = trg.node else { return trg };
    let flag = match key {
        NodeFlagKey::Hidden => {
            let held_focus = {
                let t = tree(ctx.wm, trg);
                t.is_descendant(t.focus, Some(n))
            };
            tree_mut(ctx.wm, trg).set_hidden(n, value);
            events.push(Event::NodeFlag {
                monitor: monitor_wire_id(ctx.wm, trg.monitor),
                desktop: desktop_id(ctx.wm, trg).0,
                node: wid(ctx, trg),
                flag: "hidden",
                on: value,
            });
            // The desktop's focus moves off a node that was just hidden, and
            // lands on the next focusable one.
            if held_focus || tree(ctx.wm, trg).focus.is_none() {
                tree_mut(ctx.wm, trg).focus = None;
                let desk = Coordinates { node: None, ..trg };
                if is_focused_desktop(ctx.wm, trg) {
                    focus_node(ctx, desk, events);
                } else {
                    activate_node(ctx, desk, events);
                }
            }
            return trg;
        }
        NodeFlagKey::Sticky => {
            // A node on a desktop that is not shown first goes to the one its
            // monitor shows.
            if !is_focused_desktop_of_its_monitor(ctx.wm, trg) {
                if let Some(desktop) = ctx.wm.monitors[trg.monitor].focused {
                    let dst = Coordinates { monitor: trg.monitor, desktop, node: None };
                    let anchor = tree(ctx.wm, dst).focus;
                    if let Ok(moved) = transfer_node_unchecked(ctx, trg, dst, anchor, false, events) {
                        trg = moved;
                    }
                }
            }
            if let Some(n) = trg.node {
                tree_mut(ctx.wm, trg).set_sticky(n, value);
            }
            "sticky"
        }
        NodeFlagKey::Private => {
            tree_mut(ctx.wm, trg).set_private(n, value);
            "private"
        }
        NodeFlagKey::Locked => {
            tree_mut(ctx.wm, trg).set_locked(n, value);
            "locked"
        }
        NodeFlagKey::Marked => {
            tree_mut(ctx.wm, trg).set_marked(n, value);
            "marked"
        }
    };
    events.push(Event::NodeFlag {
        monitor: monitor_wire_id(ctx.wm, trg.monitor),
        desktop: desktop_id(ctx.wm, trg).0,
        node: wid(ctx, trg),
        flag,
        on: value,
    });
    trg
}

/// Sets `node`'s client state to `target`, reporting the state left (`off`)
/// and the state entered (`on`); returns `false` if nothing changed. Leaving
/// floating or fullscreen on the desktop's focused node also lowers any
/// fullscreen window that would now cover it ([`neutralize_occluding_windows`]).
///
/// bspwm: `src/tree.c` `set_state()`, `set_floating()`, `set_fullscreen()`.
fn set_state_reporting<A: Adapter>(
    ctx: &mut ExecCtx<A>,
    trg: Coordinates,
    target: bsp_core::node::ClientState,
    events: &mut Vec<Event>,
) -> bool {
    use bsp_core::node::ClientState;
    let Some(n) = trg.node else { return false };
    let Some(previous) = tree(ctx.wm, trg).node(n).client.as_ref().map(|c| c.state) else {
        return false;
    };
    if !tree_mut(ctx.wm, trg).set_state(n, target) {
        return false;
    }
    for (state, on) in [(previous, false), (target, true)] {
        events.push(Event::NodeState {
            monitor: monitor_wire_id(ctx.wm, trg.monitor),
            desktop: desktop_id(ctx.wm, trg).0,
            node: wid(ctx, trg),
            state,
            on,
        });
    }
    if matches!(previous, ClientState::Floating | ClientState::Fullscreen) && tree(ctx.wm, trg).focus == Some(n) {
        neutralize_occluding_windows(ctx, trg, events);
    }
    // Entering or leaving floating or fullscreen changes the level: bspwm's
    // `set_floating()`/`set_fullscreen()` call `stack(d, n, d->focus == n)`
    // (tiled and pseudo-tiled share one level).
    let leveled = |s: ClientState| matches!(s, ClientState::Floating | ClientState::Fullscreen);
    if leveled(previous) || leveled(target) {
        let focused = tree(ctx.wm, trg).focus == Some(n);
        stack_node(ctx, trg, focused, events);
    }
    true
}

/// A fullscreen window that would cover a newly focused window (it sits at a
/// higher stack level) drops back to the state it had before, so the focused
/// window is not hidden behind it. Re-arranges the desktop if any did.
///
/// bspwm: `src/tree.c` `neutralize_occluding_windows()`.
fn neutralize_occluding_windows<A: Adapter>(ctx: &mut ExecCtx<A>, n: Coordinates, events: &mut Vec<Event>) {
    use bsp_core::node::ClientState;
    let mut victims = Vec::new();
    {
        let t = tree(ctx.wm, n);
        let mut f = t.first_extrema(n.node);
        while let Some(fl) = f {
            if let Some(fc) = t.node(fl).client.as_ref() {
                let mut a = t.first_extrema(t.root);
                while let Some(al) = a {
                    if al != fl {
                        if let Some(ac) = t.node(al).client.as_ref() {
                            if ac.state == ClientState::Fullscreen
                                && fc.stack_level() < ac.stack_level()
                                && !victims.iter().any(|(v, _)| *v == al)
                            {
                                victims.push((al, ac.last_state));
                            }
                        }
                    }
                    a = t.next_leaf(Some(al), t.root);
                }
            }
            f = t.next_leaf(Some(fl), n.node);
        }
    }
    if victims.is_empty() {
        return;
    }
    for (victim, last_state) in victims {
        set_state_reporting(ctx, Coordinates { node: Some(victim), ..n }, last_state, events);
    }
    arrange(ctx, n);
}

/// Focuses `dst`: makes its monitor, desktop and node the focused ones. With no
/// node, the desktop's own focused node (else the last focused one, else the
/// first focusable leaf) is used. Fails, changing nothing, if that node is
/// hidden. Clears the node's urgent flag, lowers fullscreen windows that would
/// cover it, and reports `monitor_focus`, `desktop_focus` and `node_focus` (no
/// `node_focus` for an empty desktop).
///
/// bspwm: `src/tree.c` `focus_node()`.
pub fn focus_node<A: Adapter>(ctx: &mut ExecCtx<A>, dst: Coordinates, events: &mut Vec<Event>) -> bool {
    // bspwm: `guess`, no node given: the desktop's own focus is wanted.
    let guess = dst.node.is_none();
    let mut n = dst.node;
    if n.is_none() && tree(ctx.wm, dst).root.is_some() {
        n = tree(ctx.wm, dst).focus.or_else(|| ctx.wm.fallback_focus(dst.monitor, dst.desktop));
    }
    if let Some(node) = n {
        if !tree(ctx.wm, dst).is_focusable(node) {
            return false;
        }
    }
    // bspwm: the sticky nodes come along to the desktop about to be shown, and
    // a focused sticky window keeps the focus.
    if let Some(shown) = ctx.wm.monitors[dst.monitor].focused.filter(|&s| s != dst.desktop) {
        let shown_at = Coordinates { monitor: dst.monitor, desktop: shown, node: None };
        let sticky_focus = if guess {
            let t = tree(ctx.wm, shown_at);
            t.focus.filter(|&f| t.node(f).sticky).and_then(|f| t.node(f).client.as_ref().map(|c| c.window))
        } else {
            None
        };
        transfer_sticky_nodes(ctx, dst.monitor, dst.desktop, events);
        if let Some(window) = sticky_focus {
            n = node_of_window(tree(ctx.wm, dst), window).or(n);
        } else if n.is_none() {
            n = tree(ctx.wm, dst).focus;
        }
    }
    let monitor_changed = ctx.wm.focused_monitor != Some(dst.monitor);
    let desktop_changed = monitor_changed || ctx.wm.monitors[dst.monitor].focused != Some(dst.desktop);
    let target = Coordinates { node: n, ..dst };
    let current_focus = tree(ctx.wm, dst).focus;
    if current_focus.is_some() && n != current_focus {
        neutralize_occluding_windows(ctx, target, events);
    }
    if n.is_some() {
        set_urgent(ctx, target, false, events);
    }
    ctx.wm.focused_monitor = Some(dst.monitor);
    ctx.wm.monitors[dst.monitor].focused = Some(dst.desktop);
    tree_mut(ctx.wm, dst).focus = n;
    if monitor_changed {
        events.push(Event::MonitorFocus { id: monitor_wire_id(ctx.wm, dst.monitor) });
    }
    if desktop_changed {
        events.push(Event::DesktopFocus {
            monitor: monitor_wire_id(ctx.wm, dst.monitor),
            desktop: desktop_id(ctx.wm, dst).0,
        });
    }
    if n.is_some() {
        events.push(Event::NodeFocus {
            monitor: monitor_wire_id(ctx.wm, dst.monitor),
            desktop: desktop_id(ctx.wm, dst).0,
            node: wid(ctx, target),
        });
        // A focused window comes to the top of its level.
        stack_node(ctx, target, true, events);
    }
    true
}

/// Makes `dst`'s node its desktop's focused node without moving the focus
/// there: what a desktop that is not on screen (or a monitor that is not
/// focused) remembers as its focus. Fails on the focused desktop, or if the
/// node is hidden.
///
/// bspwm: `src/tree.c` `activate_node()`.
pub fn activate_node<A: Adapter>(ctx: &mut ExecCtx<A>, dst: Coordinates, events: &mut Vec<Event>) -> bool {
    let mut n = dst.node;
    if n.is_none() && tree(ctx.wm, dst).root.is_some() {
        n = tree(ctx.wm, dst).focus.or_else(|| ctx.wm.fallback_focus(dst.monitor, dst.desktop));
    }
    if is_focused_desktop(ctx.wm, dst) || n.is_some_and(|node| !tree(ctx.wm, dst).is_focusable(node)) {
        return false;
    }
    let target = Coordinates { node: n, ..dst };
    let current_focus = tree(ctx.wm, dst).focus;
    if n.is_some() && current_focus.is_some() && n != current_focus {
        neutralize_occluding_windows(ctx, target, events);
    }
    tree_mut(ctx.wm, dst).focus = n;
    if n.is_some() {
        events.push(Event::NodeActivate {
            monitor: monitor_wire_id(ctx.wm, dst.monitor),
            desktop: desktop_id(ctx.wm, dst).0,
            node: wid(ctx, target),
        });
        stack_node(ctx, target, true, events);
    }
    true
}

/// Marks `trg`'s client as demanding attention (or clears it), reporting
/// `node_flag ... urgent on|off`. A window that is already the focused node of
/// the focused desktop cannot become urgent.
///
/// bspwm: `src/tree.c` `set_urgent()`.
pub fn set_urgent<A: Adapter>(ctx: &mut ExecCtx<A>, trg: Coordinates, value: bool, events: &mut Vec<Event>) {
    let Some(n) = trg.node else { return };
    if value && is_focused_desktop(ctx.wm, trg) && tree(ctx.wm, trg).focus == Some(n) {
        return;
    }
    let Some(client) = tree_mut(ctx.wm, trg).node_mut(n).client.as_mut() else { return };
    if client.urgent == value {
        return;
    }
    client.urgent = value;
    events.push(Event::NodeFlag {
        monitor: monitor_wire_id(ctx.wm, trg.monitor),
        desktop: desktop_id(ctx.wm, trg).0,
        node: wid(ctx, trg),
        flag: "urgent",
        on: value,
    });
}

/// The node in `t` whose client is `window`.
fn node_of_window(t: &Tree, window: WindowId) -> Option<NodeId> {
    let mut f = t.first_extrema(t.root);
    while let Some(n) = f {
        if t.node(n).client.as_ref().is_some_and(|c| c.window == window) {
            return Some(n);
        }
        f = t.next_leaf(Some(n), t.root);
    }
    None
}

/// `-d`/`-m`/`-n`: transfers `src`'s node to `dst`, next to `anchor`.
/// Returns the new target coordinate (`src.monitor`/`.desktop` updated to
/// `dst`'s, per bspwm's `cmd_node()`).
///
/// Focus follows bspwm's rules exactly: without `follow`, the source desktop
/// keeps the focus (on its next node) even when the moved node was focused;
/// with it, the moved node is focused where it landed. A floating window that
/// changes monitor is repositioned proportionally.
///
/// bspwm: `src/tree.c` `transfer_node()`.
pub fn transfer_node<A: Adapter>(
    ctx: &mut ExecCtx<A>,
    src: Coordinates,
    dst: Coordinates,
    anchor: Option<NodeId>,
    follow: bool,
    events: &mut Vec<Event>,
) -> Result<Coordinates, String> {
    let Some(n) = src.node else {
        return Err(String::new());
    };
    // A sticky window goes where it is shown: not to a desktop that isn't.
    let same_desktop = src.monitor == dst.monitor && src.desktop == dst.desktop;
    // bspwm: `sticky_still && sc > 0 && dd != md->desk`, `sc` counted only when
    // the source is the shown desktop.
    if !same_desktop
        && is_focused_desktop_of_its_monitor(ctx.wm, src)
        && tree(ctx.wm, src).sticky_count(Some(n)) > 0
        && !is_focused_desktop_of_its_monitor(ctx.wm, dst)
    {
        return Err(String::new());
    }
    transfer_node_unchecked(ctx, src, dst, anchor, follow, events)
}

/// Whether `c`'s desktop is the one its monitor shows.
fn is_focused_desktop_of_its_monitor(wm: &Wm, c: Coordinates) -> bool {
    wm.monitors[c.monitor].focused == Some(c.desktop)
}

/// The sticky subtrees of the desktop `monitor` shows move to its desktop
/// `to` before that one takes over, so they stay on screen.
///
/// bspwm: `src/tree.c` `transfer_sticky_nodes()`, called by
/// `activate_desktop()` and `focus_node()`.
fn transfer_sticky_nodes<A: Adapter>(ctx: &mut ExecCtx<A>, monitor: usize, to: usize, events: &mut Vec<Event>) {
    let Some(from) = ctx.wm.monitors[monitor].focused else { return };
    if from == to {
        return;
    }
    let src = Coordinates { monitor, desktop: from, node: None };
    let dst = Coordinates { monitor, desktop: to, node: None };
    move_sticky_subtrees(ctx, src, dst, events);
}

/// The window of the first leaf of every topmost sticky subtree of `t`
/// (a sticky node's descendants go with it), in tree order.
fn sticky_subtree_windows(t: &Tree) -> Vec<WindowId> {
    let mut out = Vec::new();
    let mut stack: Vec<NodeId> = t.root.into_iter().collect();
    while let Some(n) = stack.pop() {
        let node = t.node(n);
        if node.sticky {
            let mut f = t.first_extrema(Some(n));
            while let Some(leaf) = f {
                if let Some(c) = t.node(leaf).client.as_ref() {
                    out.push(c.window);
                    break;
                }
                f = t.next_leaf(Some(leaf), Some(n));
            }
            continue;
        }
        stack.extend(node.second_child());
        stack.extend(node.first_child());
    }
    out
}

/// Moves every sticky subtree of desktop `src` to desktop `dst` (either
/// monitor), each inserted at `dst`'s focus.
fn move_sticky_subtrees<A: Adapter>(ctx: &mut ExecCtx<A>, src: Coordinates, dst: Coordinates, events: &mut Vec<Event>) {
    let windows = sticky_subtree_windows(tree(ctx.wm, src));
    move_windows_subtrees(ctx, src, dst, &windows, events);
}

/// Moves, for each of `windows` still on desktop `src`, its topmost sticky
/// ancestor (or itself) to desktop `dst` at `dst`'s focus.
fn move_windows_subtrees<A: Adapter>(ctx: &mut ExecCtx<A>, src: Coordinates, dst: Coordinates, windows: &[WindowId], events: &mut Vec<Event>) {
    let src = Coordinates { node: None, ..src };
    let dst = Coordinates { node: None, ..dst };
    if (src.monitor, src.desktop) == (dst.monitor, dst.desktop) {
        return;
    }
    for &window in windows {
        let t = tree(ctx.wm, src);
        let Some(mut node) = node_of_window(t, window) else { continue };
        // The topmost sticky node above the window is what moves.
        let mut up = t.node(node).parent();
        while let Some(p) = up {
            if t.node(p).sticky {
                node = p;
            }
            up = t.node(p).parent();
        }
        let anchor = tree(ctx.wm, dst).focus;
        let _ = transfer_node_unchecked(ctx, Coordinates { node: Some(node), ..src }, dst, anchor, false, events);
    }
}

/// [`transfer_node`] without the sticky check.
fn transfer_node_unchecked<A: Adapter>(
    ctx: &mut ExecCtx<A>,
    src: Coordinates,
    dst: Coordinates,
    anchor: Option<NodeId>,
    follow: bool,
    events: &mut Vec<Event>,
) -> Result<Coordinates, String> {
    let Some(n) = src.node else {
        return Err(String::new());
    };
    let same_desktop = src.monitor == dst.monitor && src.desktop == dst.desktop;
    if same_desktop {
        let t = tree(ctx.wm, src);
        if anchor == Some(n) || t.is_child(Some(n), anchor) || t.is_descendant(anchor, Some(n)) {
            return Err(String::new());
        }
    }
    let settings = ctx.wm.settings.clone();
    let src_desktop = desktop_id(ctx.wm, src);
    let dst_desktop = desktop_id(ctx.wm, dst);

    let (held_focus, last_ds_focus, held_window) = {
        let t = tree(ctx.wm, src);
        let held = t.is_descendant(t.focus, Some(n));
        // A focus that is the moved node's own parent dies with the unlink.
        let last = if t.is_child(Some(n), t.focus) { None } else { t.focus };
        let window = if held { t.focus.and_then(|f| t.node(f).client.as_ref().map(|c| c.window)) } else { None };
        (held, last, window)
    };
    let ds_was_focused = is_focused_desktop(ctx.wm, src);
    let moved_id = ctx.registry.id_of(src_desktop, n).unwrap_or(0);
    let anchor_id = anchor.and_then(|a| ctx.registry.id_of(dst_desktop, a)).unwrap_or(0);
    events.push(Event::NodeTransfer {
        src_monitor: monitor_wire_id(ctx.wm, src.monitor),
        src_desktop: src_desktop.0,
        src_node: moved_id,
        dst_monitor: monitor_wire_id(ctx.wm, dst.monitor),
        dst_desktop: dst_desktop.0,
        dst_node: anchor_id,
    });

    let preselected_before = presels_before_insert(ctx, dst, anchor, events);
    let new_node = if same_desktop {
        if !tree_mut(ctx.wm, src).transplant_within(&settings, n, anchor) {
            return Err(String::new());
        }
        n
    } else {
        let (src_tree, dst_tree) = two_trees_mut(ctx.wm, (src.monitor, src.desktop), (dst.monitor, dst.desktop));
        let (new_node, moved) = src_tree.transplant_to_mapped(&settings, n, dst_tree, anchor);
        // Every node of the subtree keeps its wire id, not just its root.
        for (old, new) in moved {
            ctx.registry.relocate((src_desktop, old), (dst_desktop, new));
        }
        new_node
    };
    if src.monitor != dst.monitor {
        let (rs, rd) = (ctx.wm.monitors[src.monitor].rectangle, ctx.wm.monitors[dst.monitor].rectangle);
        let t = tree_mut(ctx.wm, dst);
        // A window already lying on the destination monitor is left where it is.
        let already_there = t.node(new_node).client.as_ref().is_some_and(|c| {
            let f = c.floating_rectangle;
            let (cx, cy) = (f.x + f.width / 2, f.y + f.height / 2);
            cx >= rd.x && cx < rd.x + rd.width && cy >= rd.y && cy < rd.y + rd.height
        });
        if !already_there {
            bsp_core::monitor::adapt_geometry(t, Some(new_node), rs, rd);
        }
    }

    // The split the insertion made gets its id now, for this command's events.
    ctx.registry.register_with_split(dst_desktop, tree(ctx.wm, dst), new_node);
    push_consumed_presels(ctx, dst, &preselected_before, events);
    let new_trg = Coordinates { monitor: dst.monitor, desktop: dst.desktop, node: Some(new_node) };
    // bspwm: `stack(dd, ns, false)`.
    stack_node(ctx, new_trg, false, events);
    // The focus the moved node held, in its new tree.
    let moved_focus = if same_desktop {
        last_ds_focus
    } else {
        held_window.and_then(|w| node_of_window(tree(ctx.wm, dst), w)).or(Some(new_node))
    };
    let src_here = Coordinates { node: None, ..src };

    if same_desktop {
        if held_focus {
            if ds_was_focused {
                focus_node(ctx, Coordinates { node: moved_focus, ..src }, events);
            } else {
                activate_node(ctx, Coordinates { node: moved_focus, ..src }, events);
            }
        }
    } else {
        if held_focus {
            if follow {
                if ds_was_focused {
                    focus_node(ctx, Coordinates { node: moved_focus, ..dst }, events);
                }
                activate_node(ctx, src_here, events);
            } else if ds_was_focused {
                focus_node(ctx, src_here, events);
            } else {
                activate_node(ctx, src_here, events);
            }
        }
        if (!held_focus || !follow || !ds_was_focused) && tree(ctx.wm, dst).focus == Some(new_node) {
            let target = Coordinates { node: if held_focus { moved_focus } else { Some(new_node) }, ..dst };
            if is_focused_desktop(ctx.wm, dst) {
                focus_node(ctx, target, events);
            } else {
                activate_node(ctx, target, events);
            }
        }
    }
    arrange(ctx, src);
    if !same_desktop {
        arrange(ctx, dst);
    }
    Ok(new_trg)
}

/// `node -s` between two desktops (of one monitor or of two): the subtrees
/// exchange the exact slots, keeping their wire ids; floating windows are
/// adapted to the other monitor's rectangle and both desktops are laid out
/// again.
///
/// bspwm: `src/tree.c` `swap_nodes()`, the branch for `d1 != d2`.
fn swap_across_desktops<A: Adapter>(
    ctx: &mut ExecCtx<A>,
    src: Coordinates,
    dst: Coordinates,
    follow: bool,
    events: &mut Vec<Event>,
) -> Result<Coordinates, String> {
    let (Some(n1), Some(n2)) = (src.node, dst.node) else {
        return Err(String::new());
    };
    // Sticky windows stay on the shown desktop of their monitor.
    if tree(ctx.wm, src).sticky_count(Some(n1)) > 0 || tree(ctx.wm, dst).sticky_count(Some(n2)) > 0 {
        return Err(String::new());
    }
    events.push(Event::NodeSwap {
        src_monitor: monitor_wire_id(ctx.wm, src.monitor),
        src_desktop: desktop_id(ctx.wm, src).0,
        src_node: wid(ctx, src),
        dst_monitor: monitor_wire_id(ctx.wm, dst.monitor),
        dst_desktop: desktop_id(ctx.wm, dst).0,
        dst_node: wid(ctx, dst),
    });
    let (d1, d2) = (desktop_id(ctx.wm, src), desktop_id(ctx.wm, dst));
    let held = |ctx: &ExecCtx<A>, c: Coordinates, n: NodeId| {
        let t = tree(ctx.wm, c);
        t.is_descendant(t.focus, Some(n))
    };
    let (n1_held, n2_held) = (held(ctx, src, n1), held(ctx, dst, n2));
    let (last_d1_focus, last_d2_focus) = (tree(ctx.wm, src).focus, tree(ctx.wm, dst).focus);
    // bspwm: `mon->desk == d`.
    let focused_desktop = |ctx: &ExecCtx<A>, c: Coordinates| is_focused_desktop(ctx.wm, c);
    let (d1_was_focused, d2_was_focused) = (focused_desktop(ctx, src), focused_desktop(ctx, dst));
    let swap = {
        let (t1, t2) = two_trees_mut(ctx.wm, (src.monitor, src.desktop), (dst.monitor, dst.desktop));
        t1.swap_subtrees_with(n1, t2, n2)
    };
    let mut moves = Vec::new();
    moves.extend(swap.into_other.1.iter().map(|&(old, new)| ((d1, old), (d2, new))));
    moves.extend(swap.into_self.1.iter().map(|&(old, new)| ((d2, old), (d1, new))));
    ctx.registry.relocate_many(&moves);
    let (new1, new2) = (swap.into_other.0, swap.into_self.0);
    if src.monitor != dst.monitor {
        let (r1, r2) = (ctx.wm.monitors[src.monitor].rectangle, ctx.wm.monitors[dst.monitor].rectangle);
        bsp_core::monitor::adapt_geometry(tree_mut(ctx.wm, dst), Some(new1), r1, r2);
        bsp_core::monitor::adapt_geometry(tree_mut(ctx.wm, src), Some(new2), r2, r1);
    }
    arrange(ctx, src);
    arrange(ctx, dst);
    // bspwm: `swap_nodes()`'s focus rules, per side that held the focus.
    let map = |pairs: &[(NodeId, NodeId)], old: Option<NodeId>| old.and_then(|o| pairs.iter().find(|(from, _)| *from == o).map(|(_, to)| *to));
    let src_desk = Coordinates { node: None, ..src };
    let dst_desk = Coordinates { node: None, ..dst };
    if n1_held {
        if d1_was_focused {
            if follow {
                focus_node(ctx, Coordinates { node: map(&swap.into_other.1, last_d1_focus), ..dst_desk }, events);
            } else {
                focus_node(ctx, Coordinates { node: tree(ctx.wm, src).focus, ..src_desk }, events);
            }
        } else {
            activate_node(ctx, Coordinates { node: tree(ctx.wm, src).focus, ..src_desk }, events);
        }
    }
    if n2_held {
        if d2_was_focused {
            if follow {
                focus_node(ctx, Coordinates { node: map(&swap.into_self.1, last_d2_focus), ..src_desk }, events);
            } else {
                focus_node(ctx, Coordinates { node: tree(ctx.wm, dst).focus, ..dst_desk }, events);
            }
        } else {
            activate_node(ctx, Coordinates { node: tree(ctx.wm, dst).focus, ..dst_desk }, events);
        }
    }
    Ok(Coordinates { node: Some(new1), ..dst })
}

/// `-s`/`--swap`. Same-tree swaps use `Tree::swap_nodes` directly; two
/// desktops go through [`swap_across_desktops`].
fn do_swap<A: Adapter>(
    ctx: &mut ExecCtx<A>,
    src: Coordinates,
    dst: Coordinates,
    follow: bool,
    events: &mut Vec<Event>,
) -> Result<Coordinates, String> {
    let (Some(n1), Some(n2)) = (src.node, dst.node) else {
        return Err(String::new());
    };
    if src.monitor != dst.monitor || src.desktop != dst.desktop {
        return swap_across_desktops(ctx, src, dst, follow, events);
    }
    if !tree_mut(ctx.wm, src).swap_nodes(n1, n2) {
        return Err(String::new());
    }
    events.push(Event::NodeSwap {
        src_monitor: monitor_wire_id(ctx.wm, src.monitor),
        src_desktop: desktop_id(ctx.wm, src).0,
        src_node: wid(ctx, src),
        dst_monitor: monitor_wire_id(ctx.wm, dst.monitor),
        dst_desktop: desktop_id(ctx.wm, dst).0,
        dst_node: wid(ctx, dst),
    });
    if follow {
        focus_node(ctx, src, events);
    }
    Ok(dst)
}

// ======================= desktop =======================

fn exec_desktop<A: Adapter>(
    ctx: &mut ExecCtx<A>,
    selector: Option<&DesktopSelector>,
    actions: &[DesktopAction],
) -> (Reply, Vec<Event>) {
    let Some(reference) = focused_coords(ctx) else {
        return (
            Reply::Fail("desktop: Missing arguments.\n".to_string()),
            Vec::new(),
        );
    };
    let mut trg = match selector {
        Some(sel) => match resolve_desktop(ctx, reference, sel) {
            Ok(c) => c,
            Err(e) => return (resolve_err_reply(e, "desktop"), Vec::new()),
        },
        None => reference,
    };

    let mut events = Vec::new();
    let mut fail: Option<String> = None;

    'actions: for action in actions {
        match action {
            DesktopAction::Focus(sel) => {
                let dst = match sel {
                    Some(s) => match resolve_desktop(ctx, reference, s) {
                        Ok(c) => c,
                        Err(e) => {
                            fail = Some(resolve_err_reply(e, "desktop -f").into_message());
                            break 'actions;
                        }
                    },
                    None => trg,
                };
                // bspwm: `focus_node(dst.monitor, dst.desktop, NULL)`; the desktop's
                // own focused node, whatever node `trg` happens to carry.
                focus_node(ctx, Coordinates { node: None, ..dst }, &mut events);
            }
            DesktopAction::Activate(sel) => {
                let dst = match sel {
                    Some(s) => match resolve_desktop(ctx, reference, s) {
                        Ok(c) => c,
                        Err(e) => {
                            fail = Some(resolve_err_reply(e, "desktop -a").into_message());
                            break 'actions;
                        }
                    },
                    None => trg,
                };
                if ctx.wm.monitors[dst.monitor].focused == Some(dst.desktop) {
                    fail = Some(String::new());
                    break 'actions;
                }
                transfer_sticky_nodes(ctx, dst.monitor, dst.desktop, &mut events);
                ctx.wm.monitors[dst.monitor].activate_desktop(dst.desktop);
                events.push(Event::DesktopActivate {
                    monitor: monitor_wire_id(ctx.wm, dst.monitor),
                    desktop: desktop_id(ctx.wm, dst).0,
                });
                // bspwm: `if (activate_desktop(..)) activate_node(m, d, NULL)`.
                activate_node(ctx, Coordinates { node: None, ..dst }, &mut events);
            }
            DesktopAction::ToMonitor(sel, follow) => {
                if ctx.wm.monitors[trg.monitor].desktops.len() <= 1 {
                    fail = Some(String::new());
                    break 'actions;
                }
                let dst_mon = match resolve_monitor(ctx, reference, sel) {
                    Ok(c) => c.monitor,
                    Err(e) => {
                        fail = Some(resolve_err_reply(e, "desktop -m").into_message());
                        break 'actions;
                    }
                };
                match transfer_desktop(ctx, trg, dst_mon, *follow, &mut events) {
                    // bspwm: `trg.monitor = dst.monitor`; the desktop is its last one.
                    Some(index) => trg = Coordinates { monitor: dst_mon, desktop: index, node: None },
                    None => {
                        fail = Some(String::new());
                        break 'actions;
                    }
                }
            }
            DesktopAction::Swap(sel, follow) => {
                let dst = match resolve_desktop(ctx, reference, sel) {
                    Ok(c) => c,
                    Err(e) => {
                        fail = Some(resolve_err_reply(e, "desktop -s").into_message());
                        break 'actions;
                    }
                };
                if !swap_desktops(ctx, trg, dst, *follow, &mut events) {
                    fail = Some(String::new());
                    break 'actions;
                }
                // The desktop we swapped now sits where `dst` was.
                trg = Coordinates { monitor: dst.monitor, desktop: dst.desktop, node: None };
            }
            DesktopAction::SetLayout(arg) => {
                let d = &mut ctx.wm.monitors[trg.monitor].desktops[trg.desktop];
                let target = match arg {
                    LayoutArg::Cycle => match d.user_layout {
                        bsp_core::tree::Layout::Tiled => bsp_core::tree::Layout::Monocle,
                        bsp_core::tree::Layout::Monocle => bsp_core::tree::Layout::Tiled,
                    },
                    LayoutArg::Set(l) => *l,
                };
                let single_monocle = ctx.wm.settings.single_monocle;
                let d = &mut ctx.wm.monitors[trg.monitor].desktops[trg.desktop];
                let tiled = d.tree.tiled_count(d.tree.root, true);
                let effective = if single_monocle && tiled <= 1 {
                    bsp_core::tree::Layout::Monocle
                } else {
                    target
                };
                if d.set_layout(target, true, effective) {
                    events.push(Event::DesktopLayout {
                        monitor: monitor_wire_id(ctx.wm, trg.monitor),
                        desktop: desktop_id(ctx.wm, trg).0,
                        layout: effective,
                    });
                    arrange(ctx, trg);
                }
            }
            DesktopAction::Rename(name) => rename_desktop(ctx, trg, name, &mut events),
            DesktopAction::Bubble(cyc) => {
                // bspwm: swap with the neighbour, one step at a time; past the
                // end it keeps swapping until the desktop is at the other end.
                let len = ctx.wm.monitors[trg.monitor].desktops.len();
                let at = |desktop: usize| Coordinates { monitor: trg.monitor, desktop, node: None };
                let mut i = trg.desktop;
                let steps: Vec<usize> = match cyc {
                    crate::value::CycleDir::Next if i + 1 < len => vec![i + 1],
                    crate::value::CycleDir::Next => (0..i).rev().collect(),
                    crate::value::CycleDir::Prev if i > 0 => vec![i - 1],
                    crate::value::CycleDir::Prev => (1..len).collect(),
                };
                for j in steps {
                    swap_desktops(ctx, at(i), at(j), false, &mut events);
                    i = j;
                }
                trg.desktop = i;
            }
            DesktopAction::Remove => {
                // bspwm: the windows move to the previous desktop (else the next)
                // and the desktop goes; the last desktop of a monitor stays.
                if !remove_desktop_merging(ctx, trg, &mut events) {
                    fail = Some(String::new());
                }
                break 'actions;
            }
        }
    }

    let reply = match fail {
        Some(msg) => Reply::Fail(msg),
        None => Reply::Ok(String::new()),
    };
    (reply, events)
}

/// Renames a desktop, reporting it even when the name is the same.
///
/// bspwm: `src/desktop.c` `rename_desktop()`.
fn rename_desktop<A: Adapter>(ctx: &mut ExecCtx<A>, trg: Coordinates, name: &str, events: &mut Vec<Event>) {
    let old_name = ctx.wm.monitors[trg.monitor].desktops[trg.desktop].name.clone();
    events.push(Event::DesktopRename {
        monitor: monitor_wire_id(ctx.wm, trg.monitor),
        desktop: desktop_id(ctx.wm, trg).0,
        old_name,
        new_name: name.to_owned(),
    });
    ctx.wm.monitors[trg.monitor].desktops[trg.desktop].rename(name);
}

/// Appends a new desktop to monitor `m` (`desktop_add`).
///
/// bspwm: `src/desktop.c` `add_desktop(m, make_desktop(name, XCB_NONE))`.
fn add_desktop<A: Adapter>(ctx: &mut ExecCtx<A>, m: usize, name: &str, events: &mut Vec<Event>) {
    let settings = ctx.wm.settings.clone();
    let id = ctx.wm.next_desktop_id();
    events.push(Event::DesktopAdd { monitor: monitor_wire_id(ctx.wm, m), desktop: id.0, name: name.to_owned() });
    ctx.wm.monitors[m].add_desktop(Desktop::new(id, Some(name), &settings));
}

/// Removes an (emptied) desktop (`desktop_remove`); a monitor left without
/// its shown desktop shows another one.
///
/// bspwm: `src/desktop.c` `remove_desktop()`.
fn remove_desktop<A: Adapter>(ctx: &mut ExecCtx<A>, trg: Coordinates, events: &mut Vec<Event>) {
    let m = trg.monitor;
    let id = ctx.wm.monitors[m].desktops[trg.desktop].id;
    let was_shown = ctx.wm.monitors[m].focused == Some(trg.desktop);
    events.push(Event::DesktopRemove { monitor: monitor_wire_id(ctx.wm, m), desktop: id.0 });
    ctx.wm.monitors[m].remove_desktop(trg.desktop);
    if was_shown {
        if let Some(next) = desktop_after_removal(ctx.wm, m, id) {
            ctx.wm.monitors[m].focused = Some(next);
            let shown = Coordinates { monitor: m, desktop: next, node: None };
            let shown = Coordinates { node: tree(ctx.wm, shown).focus, ..shown };
            if ctx.wm.focused_monitor == Some(m) {
                focus_node(ctx, shown, events);
            } else {
                activate_node(ctx, shown, events);
            }
        }
    }
}

// ======================= monitor =======================

fn exec_monitor<A: Adapter>(
    ctx: &mut ExecCtx<A>,
    selector: Option<&MonitorSelector>,
    actions: &[MonitorAction],
) -> (Reply, Vec<Event>) {
    let Some(reference) = focused_coords(ctx) else {
        return (
            Reply::Fail("monitor: Missing arguments.\n".to_string()),
            Vec::new(),
        );
    };
    let trg = match selector {
        Some(sel) => match resolve_monitor(ctx, reference, sel) {
            Ok(c) => c,
            Err(e) => return (resolve_err_reply(e, "monitor"), Vec::new()),
        },
        None => reference,
    };
    let mut trg_monitor = trg.monitor;

    let mut events = Vec::new();
    let mut fail: Option<String> = None;

    'actions: for action in actions {
        match action {
            MonitorAction::Focus(sel) => {
                let dst_monitor = match sel {
                    Some(s) => match resolve_monitor(ctx, reference, s) {
                        Ok(c) => c.monitor,
                        Err(e) => {
                            fail = Some(resolve_err_reply(e, "monitor -f").into_message());
                            break 'actions;
                        }
                    },
                    None => trg_monitor,
                };
                let dst = Coordinates {
                    monitor: dst_monitor,
                    desktop: ctx.wm.monitors[dst_monitor].focused.unwrap_or(0),
                    node: None,
                };
                focus_node(ctx, dst, &mut events);
            }
            MonitorAction::Swap(sel) => {
                let dst_monitor = match resolve_monitor(ctx, reference, sel) {
                    Ok(c) => c.monitor,
                    Err(e) => {
                        fail = Some(resolve_err_reply(e, "monitor -s").into_message());
                        break 'actions;
                    }
                };
                let src_id = monitor_wire_id(ctx.wm, trg_monitor);
                let dst_id = monitor_wire_id(ctx.wm, dst_monitor);
                ctx.wm.swap_monitors(trg_monitor, dst_monitor);
                events.push(Event::MonitorSwap {
                    src: src_id,
                    dst: dst_id,
                });
                trg_monitor = dst_monitor;
            }
            MonitorAction::AddDesktops(names) => {
                for name in names {
                    add_desktop(ctx, trg_monitor, name, &mut events);
                }
            }
            MonitorAction::ReorderDesktops(names) => {
                // Position by position, the desktop there trades places with
                // the one named (bspwm swaps them, reporting each swap).
                for (p, name) in names.iter().enumerate() {
                    if p >= ctx.wm.monitors[trg_monitor].desktops.len() {
                        break;
                    }
                    let Some(q) = ctx.wm.monitors[trg_monitor].desktops.iter().position(|d| &d.name == name) else {
                        continue;
                    };
                    if q != p {
                        let at = |desktop| Coordinates { monitor: trg_monitor, desktop, node: None };
                        swap_desktops(ctx, at(p), at(q), false, &mut events);
                    }
                }
            }
            MonitorAction::ResetDesktops(names) => {
                // The first desktops take the names, more are added, and the
                // rest go, their windows onto the focused desktop.
                let count = ctx.wm.monitors[trg_monitor].desktops.len();
                let renamed = names.len().min(count);
                for (i, name) in names.iter().take(renamed).enumerate() {
                    rename_desktop(ctx, Coordinates { monitor: trg_monitor, desktop: i, node: None }, name, &mut events);
                }
                for name in &names[renamed..] {
                    add_desktop(ctx, trg_monitor, name, &mut events);
                }
                for _ in renamed..count {
                    let gone = Coordinates { monitor: trg_monitor, desktop: renamed, node: None };
                    if is_focused_desktop(ctx.wm, gone) {
                        let prev = Coordinates { desktop: renamed - 1, ..gone };
                        focus_node(ctx, Coordinates { node: tree(ctx.wm, prev).focus, ..prev }, &mut events);
                    }
                    if let Some(root) = tree(ctx.wm, gone).root {
                        if let Some(dst) = focused_coords(ctx) {
                            let dst = Coordinates { node: None, ..dst };
                            let anchor = tree(ctx.wm, dst).focus;
                            let _ = transfer_node_unchecked(ctx, Coordinates { node: Some(root), ..gone }, dst, anchor, false, &mut events);
                        }
                    }
                    remove_desktop(ctx, gone, &mut events);
                }
            }
            MonitorAction::Remove => {
                if ctx.wm.monitors.len() <= 1 {
                    fail = Some(String::new());
                    break 'actions;
                }
                let has_windows = ctx.wm.monitors[trg_monitor]
                    .desktops
                    .iter()
                    .any(|d| d.tree.root.is_some());
                if has_windows {
                    fail = Some(
                        "monitor -r: refusing to remove a monitor with windows on it.\n"
                            .to_string(),
                    );
                    break 'actions;
                }
                remove_monitor(ctx, trg_monitor, 0, &mut events);
                break 'actions;
            }
            MonitorAction::SetRectangle(r) => {
                trg_monitor = set_monitor_rectangle(ctx, trg_monitor, *r, &mut events);
            }
            MonitorAction::Rename(name) => {
                events.push(Event::MonitorRename {
                    id: monitor_wire_id(ctx.wm, trg_monitor),
                    old_name: ctx.wm.monitors[trg_monitor].name.clone(),
                    new_name: name.clone(),
                });
                ctx.wm.monitors[trg_monitor].rename(name);
            }
        }
    }

    let reply = match fail {
        Some(msg) => Reply::Fail(msg),
        None => Reply::Ok(String::new()),
    };
    (reply, events)
}

// ======================= query =======================

fn exec_query<A: Adapter>(ctx: &mut ExecCtx<A>, q: &QueryCommand) -> Reply {
    use crate::command::QueryTarget;
    let Some(focused) = focused_coords(ctx) else {
        return Reply::Fail("query: No focused location.\n".to_string());
    };
    // bspwm: `monitor_ref`, `desktop_ref`, `node_ref` (the focused ones unless
    // `-M/-D/-N SEL` names others).
    let monitor_ref = match &q.monitor_ref {
        Some(sel) => match resolve_monitor(ctx, focused, sel) {
            Ok(c) => c,
            Err(e) => return resolve_err_reply(e, "query -M"),
        },
        None => focused,
    };
    let desktop_ref = match &q.desktop_ref {
        Some(sel) => match resolve_desktop(ctx, focused, sel) {
            Ok(c) => c,
            Err(e) => return resolve_err_reply(e, "query -D"),
        },
        None => Coordinates { node: None, ..focused },
    };
    let node_ref = match &q.node_ref {
        Some(sel) => match resolve_node(ctx, focused, sel) {
            Ok(c) => c,
            Err(e) => return resolve_err_reply(e, "query -N"),
        },
        None => focused,
    };

    // bspwm: `trg`, built up by `-m/-d/-n` in order.
    let (mut trg_monitor, mut trg_desktop, mut trg_node): (Option<usize>, Option<usize>, Option<NodeId>) = (None, None, None);
    for target in &q.targets {
        match target {
            QueryTarget::Monitor(Some(sel)) => match resolve_monitor(ctx, monitor_ref, sel) {
                Ok(c) => trg_monitor = Some(c.monitor),
                Err(e) => return resolve_err_reply(e, "query -m"),
            },
            QueryTarget::Monitor(None) => (trg_monitor, trg_desktop, trg_node) = (Some(monitor_ref.monitor), None, None),
            QueryTarget::Desktop(Some(sel)) => match resolve_desktop(ctx, desktop_ref, sel) {
                Ok(c) => (trg_monitor, trg_desktop) = (Some(c.monitor), Some(c.desktop)),
                Err(e) => return resolve_err_reply(e, "query -d"),
            },
            QueryTarget::Desktop(None) => (trg_monitor, trg_desktop, trg_node) = (Some(desktop_ref.monitor), Some(desktop_ref.desktop), None),
            QueryTarget::Node(Some(sel)) => match resolve_node(ctx, node_ref, sel) {
                Ok(c) if c.node.is_some() => (trg_monitor, trg_desktop, trg_node) = (Some(c.monitor), Some(c.desktop), c.node),
                Ok(_) => return Reply::Fail(String::new()),
                Err(e) => return resolve_err_reply(e, "query -n"),
            },
            QueryTarget::Node(None) => {
                // bspwm: `trg = node_ref`, and nothing focused fails.
                let Some(n) = node_ref.node else { return Reply::Fail(String::new()) };
                (trg_monitor, trg_desktop, trg_node) = (Some(node_ref.monitor), Some(node_ref.desktop), Some(n));
            }
        }
    }

    let rctx = resolve_ctx(ctx);
    let monitor_ok = |mi: usize| {
        trg_monitor.is_none_or(|t| t == mi)
            && q.monitor_filter.as_ref().is_none_or(|f| resolve_impl::monitor_matches(rctx, Coordinates { monitor: mi, desktop: 0, node: None }, f))
    };
    let desktop_ok = |mi: usize, di: usize| {
        trg_desktop.is_none_or(|t| (trg_monitor, t) == (Some(mi), di))
            && q.desktop_filter.as_ref().is_none_or(|f| resolve_impl::desktop_matches(rctx, Coordinates { monitor: mi, desktop: di, node: None }, desktop_ref, f))
    };

    let mut out = String::new();
    match q.domain {
        QueryDomain::Nodes => {
            // bspwm: `query_node_ids()`: every node, splits included, parents first.
            for (mi, m) in ctx.wm.monitors.iter().enumerate() {
                if !monitor_ok(mi) {
                    continue;
                }
                for di in 0..m.desktops.len() {
                    if !desktop_ok(mi, di) {
                        continue;
                    }
                    let loc = Coordinates { monitor: mi, desktop: di, node: None };
                    for id in rctx.tree(loc).node_ids() {
                        let c = Coordinates { node: Some(id), ..loc };
                        if trg_node.is_some_and(|t| t != id) {
                            continue;
                        }
                        if q.node_filter.as_ref().is_some_and(|f| !resolve_impl::node_matches(rctx, c, node_ref, f)) {
                            continue;
                        }
                        out.push_str(&format!("0x{:08X}\n", wid(ctx, c)));
                    }
                }
            }
        }
        QueryDomain::Desktops => {
            for (mi, m) in ctx.wm.monitors.iter().enumerate() {
                if !monitor_ok(mi) {
                    continue;
                }
                for (di, d) in m.desktops.iter().enumerate() {
                    if !desktop_ok(mi, di) {
                        continue;
                    }
                    if q.names {
                        out.push_str(&d.name);
                    } else {
                        out.push_str(&format!("0x{:08X}", d.id.0));
                    }
                    out.push('\n');
                }
            }
        }
        QueryDomain::Monitors => {
            for (mi, m) in ctx.wm.monitors.iter().enumerate() {
                if !monitor_ok(mi) {
                    continue;
                }
                if q.names {
                    out.push_str(&m.name);
                } else {
                    out.push_str(&format!("0x{:08X}", m.id.0));
                }
                out.push('\n');
            }
        }
        QueryDomain::Tree => {
            let Some(mi) = trg_monitor else {
                return Reply::Fail("query -T: No options given.\n".to_string());
            };
            let node_id =
                |d: DesktopId, n: NodeId| -> u32 { ctx.registry.id_of(d, n).unwrap_or(0) };
            let adapter = &*ctx.adapter;
            let names = |w: WindowId| adapter.window_class(w);
            let json = match (trg_desktop, trg_node) {
                (Some(di), Some(n)) => {
                    let d = &ctx.wm.monitors[mi].desktops[di];
                    let shown = ctx.wm.monitors[mi].focused == Some(di);
                    serde_json::to_string(&JsonNode::from_tree(&d.tree, d.id, n, shown, &node_id, &names))
                }
                (Some(di), None) => {
                    let shown = ctx.wm.monitors[mi].focused == Some(di);
                    serde_json::to_string(&JsonDesktop::from_desktop(&ctx.wm.monitors[mi].desktops[di], shown, &node_id, &names))
                }
                _ => serde_json::to_string(&JsonMonitor::from_monitor(&ctx.wm.monitors[mi], &node_id, &names)),
            };
            return match json {
                Ok(s) => Reply::Ok(format!("{s}\n")),
                Err(_) => Reply::Fail(String::new()),
            };
        }
    }
    if out.is_empty() {
        Reply::Fail(String::new())
    } else {
        Reply::Ok(out)
    }
}

// ======================= rule =======================

fn exec_rule<A: Adapter>(ctx: &mut ExecCtx<A>, actions: &[RuleAction]) -> Reply {
    for action in actions {
        match action {
            RuleAction::Add {
                class_name,
                instance_name,
                name,
                one_shot,
                consequence,
                effect_raw,
                ..
            } => {
                ctx.wm.rules.push(Rule {
                    class_name: class_name.clone(),
                    instance_name: instance_name.clone(),
                    name: name.clone(),
                    consequence: consequence.clone(),
                    one_shot: *one_shot,
                    effect_raw: effect_raw.clone(),
                });
            }
            RuleAction::Remove(removals) => {
                for r in removals {
                    match r {
                        RuleRemoval::Index(idx) => {
                            let i = *idx as usize;
                            if i >= 1 && i <= ctx.wm.rules.len() {
                                ctx.wm.rules.remove(i - 1);
                            }
                        }
                        RuleRemoval::Head => {
                            if !ctx.wm.rules.is_empty() {
                                ctx.wm.rules.remove(0);
                            }
                        }
                        RuleRemoval::Tail => {
                            ctx.wm.rules.pop();
                        }
                        RuleRemoval::Cause(cause) => {
                            // bspwm 0.9.12 needs all three fields: `CLASS` alone
                            // or `CLASS:INSTANCE` removes nothing.
                            if let [class, instance, name] = split_escaped_colons(cause).as_slice() {
                                let matches = |want: &str, have: &str| want == "*" || want == have;
                                ctx.wm.rules.retain(|r| {
                                    !(matches(class, &r.class_name) && matches(instance, &r.instance_name) && matches(name, &r.name))
                                });
                            }
                        }
                    }
                }
            }
            RuleAction::List => {}
        }
    }
    // `-l`/`--list` prints the current list regardless of position in the
    // chain (bspwm: `cmd_rule()` calls `list_rules(rsp)` inline, so a
    // `rule -a ... -l` prints the list *after* the add — matched here by
    // listing once, after every action ran, only if `List` was requested).
    if actions.iter().any(|a| matches!(a, RuleAction::List)) {
        let mut out = String::new();
        for r in &ctx.wm.rules {
            let arrow = if r.one_shot { "-" } else { "=" };
            // bspwm: `src/rule.c` `list_rules()`,
            // `"%s:%s:%s %c> %s\n"` — the effect string follows the arrow.
            out.push_str(&format!(
                "{}:{}:{} {arrow}> {}\n",
                r.class_name, r.instance_name, r.name, r.effect_raw
            ));
        }
        Reply::Ok(out)
    } else {
        Reply::Ok(String::new())
    }
}

// ======================= wm =======================

fn exec_wm<A: Adapter>(ctx: &mut ExecCtx<A>, actions: &[WmAction]) -> (Reply, Vec<Event>) {
    let mut events = Vec::new();
    let mut fail: Option<String> = None;

    'actions: for action in actions {
        match action {
            WmAction::DumpState => {
                let stacking: Vec<WireNodeId> =
                    ctx.wm.stacking.windows().iter().map(|w| wire_id_of_window(ctx, *w)).collect();
                let node_id =
                    |d: DesktopId, n: NodeId| -> u32 { ctx.registry.id_of(d, n).unwrap_or(0) };
                let adapter = &*ctx.adapter;
                let names = |w: WindowId| adapter.window_class(w);
                let clients_count = ctx
                    .wm
                    .monitors
                    .iter()
                    .flat_map(|m| &m.desktops)
                    .map(|d| d.tree.clients_count_in(d.tree.root) as i32)
                    .sum();
                let state = JsonState::new(ctx.wm, clients_count, stacking, &node_id, &names);
                match serde_json::to_string(&state) {
                    Ok(s) => return (Reply::Ok(format!("{s}\n")), events),
                    Err(_) => {
                        fail = Some(String::new());
                        break 'actions;
                    }
                }
            }
            WmAction::LoadState(_) => {
                // bspwm restores a `wm -d` dump into a new process holding the
                // same X windows (`wm -r` re-execs). A Wayland compositor that
                // restarts loses every client, so there is nothing to restore
                // into; `wm -r` reloads the configuration live instead.
                fail = Some("wm -l: not supported (clients do not survive a compositor restart; wm -r reloads live).\n".to_string());
                break 'actions;
            }
            WmAction::AddMonitor(name, rect) => {
                let id = ctx.wm.next_monitor_id();
                let settings = ctx.wm.settings.clone();
                let mut m = Monitor::new(id, Some(name), *rect, &settings);
                // No output shows it: it is kept like an unplugged monitor, and an
                // output of that name that appears later shows it (`bspc output
                // --create-headless` makes one that is shown for capture).
                m.wired = false;
                let did = ctx.wm.next_desktop_id();
                m.add_desktop(Desktop::new(did, None, &settings));
                ctx.wm.add_monitor(m);
                events.push(Event::MonitorAdd {
                    id: id.0,
                    name: name.clone(),
                    geometry: *rect,
                });
            }
            WmAction::ReorderMonitors(names) => {
                let mut order = Vec::new();
                for name in names {
                    if let Some(idx) = ctx.wm.monitors.iter().position(|m| &m.name == name) {
                        order.push(idx);
                    }
                }
                let mut reordered: Vec<Monitor> =
                    order.iter().map(|&i| ctx.wm.monitors[i].clone()).collect();
                let mut rest: Vec<Monitor> = ctx
                    .wm
                    .monitors
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !order.contains(i))
                    .map(|(_, m)| m.clone())
                    .collect();
                reordered.append(&mut rest);
                ctx.wm.monitors = reordered;
                ctx.wm.refresh_sole();
            }
            WmAction::AdoptOrphans => {
                fail =
                    Some("wm -o: not applicable yet (no orphan windows to adopt).\n".to_string());
                break 'actions;
            }
            WmAction::GetStatus => {
                let report = build_report(ctx.wm);
                return (Reply::Ok(report.to_string()), events);
            }
            WmAction::RecordHistory(on) => {
                // bspwm: `src/messages.c` `cmd_wm()`'s `-h`.
                ctx.wm.history.record = *on;
            }
            WmAction::Restart => {
                // Performing the actual restart (a compositor-only side
                // effect this crate has no way to do) is the caller's
                // job — `bsp_ipc` has no process/Wayland handle to act
                // on. `bsp-compositor`'s `ipc::execute_and_broadcast`
                // inspects the original `Command` itself for this,
                // rather than anything returned from here, since this
                // action never fails and produces no `Event`s to carry
                // it through.
                break 'actions;
            }
        }
    }

    let reply = match fail {
        Some(msg) => Reply::Fail(msg),
        None => Reply::Ok(String::new()),
    };
    (reply, events)
}

/// Builds the `subscribe report`/`wm -g` line from live state.
///
/// bspwm: `src/subscribe.c` `print_report()`.
pub fn build_report(wm: &Wm) -> Report {
    let focused_monitor = wm.focused_monitor;
    let monitors = wm
        .monitors
        .iter()
        .enumerate()
        .map(|(mi, m)| {
            let desktops = m
                .desktops
                .iter()
                .enumerate()
                .map(|(di, d)| ReportDesktop {
                    name: d.name.clone(),
                    state: if desktop_is_urgent(&d.tree) {
                        ReportDesktopState::Urgent
                    } else if d.tree.root.is_none() {
                        ReportDesktopState::Free
                    } else {
                        ReportDesktopState::Occupied
                    },
                    active: m.focused == Some(di),
                })
                .collect();
            let focused_desktop_detail = m.focused.map(|di| {
                let d = &m.desktops[di];
                let focused_node_state = d
                    .tree
                    .focus
                    .map(|n| d.tree.node(n).client.as_ref().map(|c| c.state));
                let mut flags = ReportNodeFlags::default();
                if let Some(n) = d.tree.focus {
                    let node = d.tree.node(n);
                    flags = ReportNodeFlags {
                        sticky: node.sticky,
                        private: node.private,
                        locked: node.locked,
                        marked: node.marked,
                    };
                }
                ReportFocusedDesktopDetail {
                    layout: d.layout,
                    focused_node_state,
                    focused_node_flags: flags,
                }
            });
            ReportMonitor {
                name: m.name.clone(),
                focused: focused_monitor == Some(mi),
                desktops,
                focused_desktop_detail,
            }
        })
        .collect();
    Report {
        prefix: wm.settings.status_prefix.clone(),
        monitors,
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

// ======================= config =======================

fn exec_config<A: Adapter>(ctx: &mut ExecCtx<A>, c: &ConfigCommand) -> Reply {
    let target = match &c.target {
        ConfigTarget::Global => None,
        ConfigTarget::Monitor(sel) => {
            let Some(reference) = focused_coords(ctx) else {
                return Reply::Fail(String::new());
            };
            match resolve_monitor(ctx, reference, sel) {
                Ok(c) => Some(c),
                Err(e) => return resolve_err_reply(e, "config -m"),
            }
        }
        ConfigTarget::Desktop(sel) => {
            let Some(reference) = focused_coords(ctx) else {
                return Reply::Fail(String::new());
            };
            match resolve_desktop(ctx, reference, sel) {
                // A desktop target names no node, whatever the reference had.
                Ok(c) => Some(Coordinates { node: None, ..c }),
                Err(e) => return resolve_err_reply(e, "config -d"),
            }
        }
        ConfigTarget::Node(sel) => {
            let Some(reference) = focused_coords(ctx) else {
                return Reply::Fail(String::new());
            };
            match resolve_node(ctx, reference, sel) {
                Ok(c) => Some(c),
                Err(e) => return resolve_err_reply(e, "config -n"),
            }
        }
    };

    // bspwm: `cmd_config()` leaves the desktop out of the coordinates for
    // `-m`, so paddings and gaps land on the *monitor* (`SET_DEF_MON_DESK`),
    // not on its focused desktop.
    let monitor_level = matches!(c.target, ConfigTarget::Monitor(_));
    match &c.value {
        Some(value) => set_setting(ctx, target, monitor_level, &c.name, value),
        None => get_setting(ctx, target, monitor_level, &c.name),
    }
}

// ======================= output/input (hardware backend) =======================

/// `bspc output` — see `Command::Output`. Every case defers entirely to
/// `Adapter`'s output methods, which default to "no known outputs"/
/// "not supported" until a real hardware backend overrides them
/// (`docs/design.md` roadmap, the hardware backend): this function itself has no
/// hardware knowledge, only the wire-protocol shape (list/get/set).
fn exec_output<A: Adapter>(
    ctx: &mut ExecCtx<A>,
    name: Option<&str>,
    actions: &[OutputAction],
) -> Reply {
    let Some(name) = name else {
        // Creating a virtual output is the one thing that needs no name.
        if !actions.is_empty() {
            for action in actions {
                if let Err(msg) = ctx.adapter.set_output("", action) {
                    return Reply::Fail(msg);
                }
            }
            return Reply::Ok(String::new());
        }
        let names = ctx.adapter.output_names();
        return Reply::Ok(if names.is_empty() {
            String::new()
        } else {
            names.join("\n") + "\n"
        });
    };
    if actions.is_empty() {
        return match ctx.adapter.output_settings(name) {
            Some(s) => Reply::Ok(s),
            None => Reply::Fail(format!("output: unknown output '{name}'.\n")),
        };
    }
    for action in actions {
        if let Err(msg) = ctx.adapter.set_output(name, action) {
            return Reply::Fail(msg);
        }
    }
    Reply::Ok(String::new())
}

/// `bspc input` — see `Command::Input`. Mirrors `exec_output` exactly,
/// one layer down (`Adapter`'s input methods instead of its output ones).
fn exec_input<A: Adapter>(
    ctx: &mut ExecCtx<A>,
    device: Option<&str>,
    actions: &[InputAction],
) -> Reply {
    let Some(device) = device else {
        let names = ctx.adapter.input_names();
        return Reply::Ok(if names.is_empty() {
            String::new()
        } else {
            names.join("\n") + "\n"
        });
    };
    if actions.is_empty() {
        return match ctx.adapter.input_settings(device) {
            Some(s) => Reply::Ok(s),
            None => Reply::Fail(format!("input: unknown device '{device}'.\n")),
        };
    }
    for action in actions {
        if let Err(msg) = ctx.adapter.set_input(device, action) {
            return Reply::Fail(msg);
        }
    }
    Reply::Ok(String::new())
}

fn set_setting<A: Adapter>(
    ctx: &mut ExecCtx<A>,
    target: Option<Coordinates>,
    monitor_level: bool,
    name: &str,
    value: &str,
) -> Reply {
    match name {
        "window_gap" => {
            let Ok(v) = value.parse::<i32>() else {
                return Reply::Fail(String::new());
            };
            // bspwm: `SET_DEF_DEFMON_DESK`: a desktop; or a monitor and all its
            // desktops; or the default, every monitor and every desktop.
            match target {
                Some(c) if !monitor_level => ctx.wm.monitors[c.monitor].desktops[c.desktop].window_gap = v,
                Some(c) => {
                    ctx.wm.monitors[c.monitor].window_gap = v;
                    for d in &mut ctx.wm.monitors[c.monitor].desktops {
                        d.window_gap = v;
                    }
                }
                None => {
                    ctx.wm.settings.window_gap = v;
                    for m in &mut ctx.wm.monitors {
                        m.window_gap = v;
                        for d in &mut m.desktops {
                            d.window_gap = v;
                        }
                    }
                }
            }
        }
        "border_width" => {
            let Ok(v) = value.parse::<u32>() else {
                return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n"));
            };
            let v = v as i32;
            // bspwm: `SET_DEF_DEFMON_DEFDESK_WIN`: the windows of a node, of a
            // desktop, of a monitor or of everything, and the default of each
            // container along the way.
            let set_clients = |t: &mut Tree, root: Option<NodeId>| {
                let mut f = t.first_extrema(root);
                while let Some(n) = f {
                    if let Some(c) = t.node_mut(n).client.as_mut() {
                        c.border_width = v;
                    }
                    f = t.next_leaf(Some(n), root);
                }
            };
            match target {
                Some(c) if c.node.is_some() && !monitor_level => {
                    let t = &mut ctx.wm.monitors[c.monitor].desktops[c.desktop].tree;
                    set_clients(t, c.node);
                }
                Some(c) if !monitor_level => {
                    let d = &mut ctx.wm.monitors[c.monitor].desktops[c.desktop];
                    d.border_width = v;
                    let root = d.tree.root;
                    set_clients(&mut d.tree, root);
                }
                Some(c) => {
                    let m = &mut ctx.wm.monitors[c.monitor];
                    m.border_width = v;
                    for d in &mut m.desktops {
                        d.border_width = v;
                        let root = d.tree.root;
                        set_clients(&mut d.tree, root);
                    }
                }
                None => {
                    ctx.wm.settings.border_width = v;
                    for m in &mut ctx.wm.monitors {
                        m.border_width = v;
                        for d in &mut m.desktops {
                            d.border_width = v;
                            let root = d.tree.root;
                            set_clients(&mut d.tree, root);
                        }
                    }
                }
            }
        }
        "split_ratio" => {
            let Ok(v) = value.parse::<f64>() else {
                return Reply::Fail(String::new());
            };
            if !(v > 0.0 && v < 1.0) {
                return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n"));
            }
            ctx.wm.settings.split_ratio = v;
        }
        "initial_polarity" => {
            ctx.wm.settings.initial_polarity = match value {
                "first_child" => bsp_core::tree::ChildPolarity::First,
                "second_child" => bsp_core::tree::ChildPolarity::Second,
                _ => return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n")),
            };
        }
        "automatic_scheme" => {
            ctx.wm.settings.automatic_scheme = match value {
                "longest_side" => bsp_core::settings::AutomaticScheme::LongestSide,
                "alternate" => bsp_core::settings::AutomaticScheme::Alternate,
                "spiral" => bsp_core::settings::AutomaticScheme::Spiral,
                _ => return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n")),
            };
        }
        "removal_adjustment" => {
            ctx.wm.settings.removal_adjustment = match crate::value::parse_bool(value) {
                Some(b) => b,
                None => return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n")),
            };
        }
        "gapless_monocle" => {
            ctx.wm.settings.gapless_monocle = match crate::value::parse_bool(value) {
                Some(b) => b,
                None => return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n")),
            };
        }
        "borderless_monocle" => {
            ctx.wm.settings.borderless_monocle = match crate::value::parse_bool(value) {
                Some(b) => b,
                None => return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n")),
            };
        }
        "borderless_singleton" => {
            ctx.wm.settings.borderless_singleton = match crate::value::parse_bool(value) {
                Some(b) => b,
                None => return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n")),
            };
        }
        "single_monocle" => {
            let Some(b) = crate::value::parse_bool(value) else {
                return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n"));
            };
            // bspwm refuses the value it already has, and otherwise puts every
            // desktop in monocle (at most one tiled window) or its own layout.
            if b == ctx.wm.settings.single_monocle {
                return Reply::Fail(String::new());
            }
            ctx.wm.settings.single_monocle = b;
            let settings = ctx.wm.settings.clone();
            for m in &mut ctx.wm.monitors {
                for di in 0..m.desktops.len() {
                    let d = &mut m.desktops[di];
                    let single = b && d.tree.tiled_count(d.tree.root, true) <= 1;
                    d.layout = if single { bsp_core::desktop::Layout::Monocle } else { d.user_layout };
                    m.arrange(di, &settings);
                }
            }
        }
        "normal_border_color" | "active_border_color" | "focused_border_color" | "presel_feedback_color" => {
            if !bsp_core::settings::is_hex_color(value) {
                return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n"));
            }
            let slot = match name {
                "normal_border_color" => &mut ctx.wm.settings.normal_border_color,
                "active_border_color" => &mut ctx.wm.settings.active_border_color,
                "focused_border_color" => &mut ctx.wm.settings.focused_border_color,
                _ => &mut ctx.wm.settings.presel_feedback_color,
            };
            *slot = value.to_string();
        }
        "status_prefix" => ctx.wm.settings.status_prefix = value.to_string(),
        "external_rules_command" => ctx.wm.settings.external_rules_command = value.to_string(),
        "focus_follows_pointer" | "pointer_follows_focus" | "pointer_follows_monitor" | "presel_feedback"
        | "remove_disabled_monitors" | "remove_unplugged_monitors" | "merge_overlapping_monitors" => {
            let Some(b) = crate::value::parse_bool(value) else {
                return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n"));
            };
            let s = &mut ctx.wm.settings;
            match name {
                "focus_follows_pointer" => s.focus_follows_pointer = b,
                "pointer_follows_focus" => s.pointer_follows_focus = b,
                "pointer_follows_monitor" => s.pointer_follows_monitor = b,
                "presel_feedback" => s.presel_feedback = b,
                "remove_disabled_monitors" => s.remove_disabled_monitors = b,
                "remove_unplugged_monitors" => s.remove_unplugged_monitors = b,
                _ => s.merge_overlapping_monitors = b,
            }
        }
        "directional_focus_tightness" => {
            ctx.wm.settings.directional_focus_tightness = match value {
                "high" => bsp_core::settings::Tightness::High,
                "low" => bsp_core::settings::Tightness::Low,
                _ => return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n")),
            };
        }
        "honor_size_hints" => {
            use bsp_core::settings::HonorSizeHints as H;
            let v = match value {
                "floating" => H::Floating,
                "tiled" => H::Tiled,
                _ => match crate::value::parse_bool(value) {
                    Some(true) => H::Yes,
                    Some(false) => H::No,
                    None => return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n")),
                },
            };
            // bspwm: `SET_DEF_WIN`: the windows of the node, desktop or monitor
            // given, or the default and every window.
            let set_clients = |t: &mut Tree, root: Option<NodeId>| {
                let mut f = t.first_extrema(root);
                while let Some(n) = f {
                    if let Some(c) = t.node_mut(n).client.as_mut() {
                        c.honor_size_hints = v;
                    }
                    f = t.next_leaf(Some(n), root);
                }
            };
            match target {
                Some(c) if c.node.is_some() && !monitor_level => set_clients(&mut ctx.wm.monitors[c.monitor].desktops[c.desktop].tree, c.node),
                Some(c) if !monitor_level => {
                    let t = &mut ctx.wm.monitors[c.monitor].desktops[c.desktop].tree;
                    let root = t.root;
                    set_clients(t, root);
                }
                Some(c) => {
                    for d in &mut ctx.wm.monitors[c.monitor].desktops {
                        let root = d.tree.root;
                        set_clients(&mut d.tree, root);
                    }
                }
                None => {
                    ctx.wm.settings.honor_size_hints = v;
                    for d in ctx.wm.monitors.iter_mut().flat_map(|m| m.desktops.iter_mut()) {
                        let root = d.tree.root;
                        set_clients(&mut d.tree, root);
                    }
                }
            }
        }
        "ignore_ewmh_fullscreen" => {
            // bspwm: `parse_state_transition()`: none, all, or a comma list of enter/exit.
            let mut t = bsp_core::settings::StateTransition::default();
            let ok = match value {
                "none" => true,
                "all" => {
                    t = bsp_core::settings::StateTransition { enter: true, exit: true };
                    true
                }
                list => {
                    let mut any = false;
                    let mut ok = true;
                    for key in list.split(',').filter(|k| !k.is_empty()) {
                        match key {
                            "enter" => t.enter = true,
                            "exit" => t.exit = true,
                            _ => ok = false,
                        }
                        any = true;
                    }
                    ok && any
                }
            };
            if !ok {
                return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n"));
            }
            ctx.wm.settings.ignore_ewmh_fullscreen = t;
        }
        "mapping_events_count" => {
            let Ok(v) = value.parse::<i8>() else {
                return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n"));
            };
            ctx.wm.settings.mapping_events_count = v;
        }
        "center_pseudo_tiled" => {
            ctx.wm.settings.center_pseudo_tiled = match crate::value::parse_bool(value) {
                Some(b) => b,
                None => return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n")),
            };
        }
        "top_padding" | "right_padding" | "bottom_padding" | "left_padding" => {
            let Ok(v) = value.parse::<i32>() else {
                return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n"));
            };
            let apply = |p: &mut bsp_core::geometry::Padding| match name {
                "top_padding" => p.top = v,
                "right_padding" => p.right = v,
                "bottom_padding" => p.bottom = v,
                _ => p.left = v,
            };
            // bspwm: `SET_DEF_MON_DESK`: a desktop's own padding, or a monitor's,
            // or the default and every *monitor's* (a desktop's padding is added
            // on top of its monitor's, so it is not touched).
            match target {
                Some(c) if !monitor_level => apply(&mut ctx.wm.monitors[c.monitor].desktops[c.desktop].padding),
                Some(c) => apply(&mut ctx.wm.monitors[c.monitor].padding),
                None => {
                    apply(&mut ctx.wm.settings.padding);
                    for m in &mut ctx.wm.monitors {
                        apply(&mut m.padding);
                    }
                }
            }
        }
        "top_monocle_padding"
        | "right_monocle_padding"
        | "bottom_monocle_padding"
        | "left_monocle_padding" => {
            let Ok(v) = value.parse::<i32>() else {
                return Reply::Fail(String::new());
            };
            match name {
                "top_monocle_padding" => ctx.wm.settings.monocle_padding.top = v,
                "right_monocle_padding" => ctx.wm.settings.monocle_padding.right = v,
                "bottom_monocle_padding" => ctx.wm.settings.monocle_padding.bottom = v,
                _ => ctx.wm.settings.monocle_padding.left = v,
            }
        }
        _ => return Reply::Fail(format!("config: Unknown setting: '{name}'.\n")),
    }

    for mi in 0..ctx.wm.monitors.len() {
        for di in 0..ctx.wm.monitors[mi].desktops.len() {
            arrange(
                ctx,
                Coordinates {
                    monitor: mi,
                    desktop: di,
                    node: None,
                },
            );
        }
    }
    Reply::Ok(String::new())
}

fn get_setting<A: Adapter>(ctx: &ExecCtx<A>, target: Option<Coordinates>, monitor_level: bool, name: &str) -> Reply {
    let s = &ctx.wm.settings;
    let out = match name {
        // bspwm prints it with `%lf`.
        "split_ratio" => format!("{:.6}", s.split_ratio),
        "window_gap" => match target {
            Some(c) if monitor_level => format!("{}", ctx.wm.monitors[c.monitor].window_gap),
            Some(c) => format!("{}", ctx.wm.monitors[c.monitor].desktops[c.desktop].window_gap),
            None => format!("{}", s.window_gap),
        },
        "border_width" => match target {
            // bspwm: the first window of a node, else the desktop's, monitor's or default.
            Some(c) if c.node.is_some() && !monitor_level => {
                let t = &ctx.wm.monitors[c.monitor].desktops[c.desktop].tree;
                let mut f = t.first_extrema(c.node);
                let mut found = None;
                while let Some(n) = f {
                    if let Some(cl) = t.node(n).client.as_ref() {
                        found = Some(cl.border_width);
                        break;
                    }
                    f = t.next_leaf(Some(n), c.node);
                }
                match found {
                    Some(w) => format!("{w}"),
                    None => return Reply::Ok(String::new()),
                }
            }
            Some(c) if monitor_level => format!("{}", ctx.wm.monitors[c.monitor].border_width),
            Some(c) => format!("{}", ctx.wm.monitors[c.monitor].desktops[c.desktop].border_width),
            None => format!("{}", s.border_width),
        },
        "top_padding" => padding_get(ctx, target, monitor_level, |p| p.top),
        "right_padding" => padding_get(ctx, target, monitor_level, |p| p.right),
        "bottom_padding" => padding_get(ctx, target, monitor_level, |p| p.bottom),
        "left_padding" => padding_get(ctx, target, monitor_level, |p| p.left),
        "top_monocle_padding" => format!("{}", s.monocle_padding.top),
        "right_monocle_padding" => format!("{}", s.monocle_padding.right),
        "bottom_monocle_padding" => format!("{}", s.monocle_padding.bottom),
        "left_monocle_padding" => format!("{}", s.monocle_padding.left),
        "initial_polarity" => match s.initial_polarity {
            bsp_core::tree::ChildPolarity::First => "first_child".to_string(),
            bsp_core::tree::ChildPolarity::Second => "second_child".to_string(),
        },
        "automatic_scheme" => match s.automatic_scheme {
            bsp_core::settings::AutomaticScheme::LongestSide => "longest_side".to_string(),
            bsp_core::settings::AutomaticScheme::Alternate => "alternate".to_string(),
            bsp_core::settings::AutomaticScheme::Spiral => "spiral".to_string(),
        },
        "removal_adjustment" => bool_str(s.removal_adjustment),
        "gapless_monocle" => bool_str(s.gapless_monocle),
        "borderless_monocle" => bool_str(s.borderless_monocle),
        "single_monocle" => bool_str(s.single_monocle),
        "borderless_singleton" => bool_str(s.borderless_singleton),
        "center_pseudo_tiled" => bool_str(s.center_pseudo_tiled),
        "normal_border_color" => s.normal_border_color.clone(),
        "active_border_color" => s.active_border_color.clone(),
        "focused_border_color" => s.focused_border_color.clone(),
        "presel_feedback_color" => s.presel_feedback_color.clone(),
        "status_prefix" => s.status_prefix.clone(),
        "external_rules_command" => s.external_rules_command.clone(),
        "focus_follows_pointer" => bool_str(s.focus_follows_pointer),
        "pointer_follows_focus" => bool_str(s.pointer_follows_focus),
        "pointer_follows_monitor" => bool_str(s.pointer_follows_monitor),
        "presel_feedback" => bool_str(s.presel_feedback),
        "remove_disabled_monitors" => bool_str(s.remove_disabled_monitors),
        "remove_unplugged_monitors" => bool_str(s.remove_unplugged_monitors),
        "merge_overlapping_monitors" => bool_str(s.merge_overlapping_monitors),
        "directional_focus_tightness" => match s.directional_focus_tightness {
            bsp_core::settings::Tightness::High => "high".to_string(),
            bsp_core::settings::Tightness::Low => "low".to_string(),
        },
        "honor_size_hints" => match s.honor_size_hints {
            bsp_core::settings::HonorSizeHints::No => "false",
            bsp_core::settings::HonorSizeHints::Yes => "true",
            bsp_core::settings::HonorSizeHints::Floating => "floating",
            bsp_core::settings::HonorSizeHints::Tiled => "tiled",
        }
        .to_string(),
        "ignore_ewmh_fullscreen" => match (s.ignore_ewmh_fullscreen.enter, s.ignore_ewmh_fullscreen.exit) {
            (false, false) => "none".to_string(),
            (true, false) => "enter".to_string(),
            (false, true) => "exit".to_string(),
            (true, true) => "enter,exit".to_string(),
        },
        "mapping_events_count" => format!("{}", s.mapping_events_count),
        _ => return Reply::Fail(format!("config: Unknown setting: '{name}'.\n")),
    };
    Reply::Ok(format!("{out}\n"))
}

fn bool_str(b: bool) -> String {
    (if b { "true" } else { "false" }).to_string()
}

fn padding_get<A: Adapter>(
    ctx: &ExecCtx<A>,
    target: Option<Coordinates>,
    monitor_level: bool,
    get: impl Fn(&bsp_core::geometry::Padding) -> i32,
) -> String {
    match target {
        Some(c) if monitor_level => format!("{}", get(&ctx.wm.monitors[c.monitor].padding)),
        Some(c) => format!("{}", get(&ctx.wm.monitors[c.monitor].desktops[c.desktop].padding)),
        None => format!("{}", get(&ctx.wm.settings.padding)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::FakeAdapter;
    use crate::command;
    use bsp_core::geometry::Rect;
    use bsp_core::id::MonitorId;
    use bsp_core::node::{Client, ClientState};
    use bsp_core::settings::Settings;
    use bsp_core::tree::Layout;

    /// Two monitors side by side: `eDP-1` (0,0 800x600, desktop "I" with
    /// two windows side by side, WindowId 1 focused) and `HDMI-A-1`
    /// (800,0 800x600, one empty desktop "II").
    fn fixture() -> (Wm, NodeRegistry, FakeAdapter) {
        let settings = Settings::default();
        let mut wm = Wm::new(settings.clone());
        let mut registry = NodeRegistry::new();

        let mut m1 = Monitor::new(
            MonitorId(1),
            Some("eDP-1"),
            Rect::new(0, 0, 800, 600),
            &settings,
        );
        let mut d1 = Desktop::new(bsp_core::id::DesktopId(1), Some("I"), &settings);
        let left = d1
            .tree
            .new_client_node(&settings, Client::new(WindowId(1), 1));
        d1.tree.insert_node(&settings, left, None);
        m1.add_desktop(d1);
        m1.arrange(0, &settings);
        let right = m1.desktops[0]
            .tree
            .new_client_node(&settings, Client::new(WindowId(2), 1));
        m1.desktops[0]
            .tree
            .insert_node(&settings, right, Some(left));
        m1.desktops[0].tree.focus = Some(left);
        m1.arrange(0, &settings);
        registry.register(bsp_core::id::DesktopId(1), left);
        registry.register(bsp_core::id::DesktopId(1), right);

        let mut m2 = Monitor::new(
            MonitorId(2),
            Some("HDMI-A-1"),
            Rect::new(800, 0, 800, 600),
            &settings,
        );
        m2.add_desktop(Desktop::new(
            bsp_core::id::DesktopId(2),
            Some("II"),
            &settings,
        ));

        wm.add_monitor(m1);
        wm.add_monitor(m2);
        wm.focus_monitor(0);
        wm.monitors[0].focused = Some(0);
        // The windows are where the layout put them (the compositor placed them).
        push_geometry_changes(&mut wm, &registry, &mut Vec::new());

        let adapter = FakeAdapter::new();
        (wm, registry, adapter)
    }

    fn run(
        wm: &mut Wm,
        registry: &mut NodeRegistry,
        adapter: &mut FakeAdapter,
        cmdline: &str,
    ) -> (Reply, Vec<Event>) {
        let args: Vec<String> = cmdline.split(' ').map(String::from).collect();
        let cmd = command::parse(&args).unwrap();
        let mut ctx = ExecCtx {
            wm,
            registry,
            adapter,
        };
        execute(&mut ctx, &cmd)
    }

    #[test]
    fn node_focus_switches_focused_node_and_emits_event() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let left = wm.monitors[0].desktops[0].tree.focus.unwrap();
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "node -f east");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_ne!(wm.monitors[0].desktops[0].tree.focus, Some(left));
        assert!(events.iter().any(|e| e.kind() == EventKind::NodeFocus));
    }

    #[test]
    fn node_swap_same_tree_exchanges_positions() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "node -s east");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert!(events.iter().any(|e| e.kind() == EventKind::NodeSwap));
        // `swap_nodes` exchanges tree *position*, not node identity: the
        // node that used to sit leftmost (first in leaf order) now holds
        // what was the right window's client, since the two nodes traded
        // parent slots.
        let tree = &wm.monitors[0].desktops[0].tree;
        let leftmost = tree.first_extrema(tree.root).unwrap();
        assert_eq!(
            tree.node(leftmost).client.as_ref().unwrap().window,
            WindowId(2)
        );
    }

    #[test]
    fn node_transfer_to_desktop_relocates_registry_entry() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let left = wm.monitors[0].desktops[0].tree.focus.unwrap();
        let left_desktop = wm.monitors[0].desktops[0].id;
        let left_id = registry.id_of(left_desktop, left).unwrap();

        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "node -d II");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert!(events.iter().any(|e| e.kind() == EventKind::NodeTransfer));

        // `left` is gone from the source desktop (the fixture's other
        // window, `right`, stays behind), and present on the destination.
        let src_tree = &wm.monitors[0].desktops[0].tree;
        assert_eq!(src_tree.clients_count_in(src_tree.root), 1);
        let dst_tree = &wm.monitors[1].desktops[0].tree;
        assert!(dst_tree.root.is_some());
        let dst_desktop_id = wm.monitors[1].desktops[0].id;
        assert_eq!(registry.lookup(left_id).unwrap().0, dst_desktop_id);
    }

    #[test]
    fn node_transfer_to_desktop_rearranges_the_source_desktop() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let before = {
            let t = &wm.monitors[0].desktops[0].tree;
            let leaf = t.first_extrema(t.root).unwrap();
            t.node(leaf).client.as_ref().unwrap().tiled_rectangle
        };
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "node -d II");
        assert_eq!(reply, Reply::Ok(String::new()));
        // The window left behind grows into the freed space.
        let t = &wm.monitors[0].desktops[0].tree;
        let leaf = t.first_extrema(t.root).unwrap();
        let after = t.node(leaf).client.as_ref().unwrap().tiled_rectangle;
        assert_ne!(after, before);
        assert_eq!(after.width, wm.monitors[0].rectangle.width - 2 * wm.settings.window_gap.max(0) - 2 * wm.settings.border_width);
    }

    /// The (monitor, desktop) indices of the focused desktop, and its focused window.
    fn focus_of(wm: &Wm) -> (usize, usize, Option<u32>) {
        let mi = wm.focused_monitor.unwrap();
        let di = wm.monitors[mi].focused.unwrap();
        let t = &wm.monitors[mi].desktops[di].tree;
        (mi, di, t.focus.and_then(|f| t.node(f).client.as_ref().map(|c| c.window.0)))
    }

    #[test]
    fn node_to_desktop_without_follow_keeps_the_focus_on_the_source_desktop() {
        // bspwm: `transfer_node()`: the source desktop refocuses its next node;
        // only `--follow` brings the focus along.
        let (mut wm, mut registry, mut adapter) = fixture();
        assert_eq!(focus_of(&wm), (0, 0, Some(1)));
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "node -d II");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(focus_of(&wm), (0, 0, Some(2)));
        let moved = &wm.monitors[1].desktops[0].tree;
        assert_eq!(moved.focus.and_then(|f| moved.node(f).client.as_ref().map(|c| c.window.0)), Some(1));
    }

    #[test]
    fn node_to_desktop_with_follow_focuses_the_moved_node_where_it_landed() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "node -d II --follow");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(focus_of(&wm), (1, 0, Some(1)));
        assert!(events.iter().any(|e| e.kind() == EventKind::MonitorFocus));
        assert!(events.iter().any(|e| e.kind() == EventKind::NodeFocus));
        // The source desktop keeps a focus of its own for when it is shown again.
        let src = &wm.monitors[0].desktops[0].tree;
        assert_eq!(src.focus.and_then(|f| src.node(f).client.as_ref().map(|c| c.window.0)), Some(2));
    }

    #[test]
    fn moving_an_unfocused_node_leaves_the_focus_alone() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "node east -d II");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(focus_of(&wm), (0, 0, Some(1)));
    }

    #[test]
    fn node_transfer_event_names_the_moved_node_and_the_anchor() {
        // bspwm: `node_transfer <src mon> <src desk> <ns> <dst mon> <dst desk> <nd>`,
        // `nd` being 0 for an empty destination desktop.
        let (mut wm, mut registry, mut adapter) = fixture();
        let moved = {
            let t = &wm.monitors[0].desktops[0].tree;
            registry.id_of(bsp_core::id::DesktopId(1), t.focus.unwrap()).unwrap()
        };
        let (_, events) = run(&mut wm, &mut registry, &mut adapter, "node -d II");
        let transfer = events.iter().find_map(|e| match e {
            Event::NodeTransfer { src_node, dst_node, .. } => Some((*src_node, *dst_node)),
            _ => None,
        });
        assert_eq!(transfer, Some((moved, 0)));
    }

    #[test]
    fn a_floating_window_moved_to_another_monitor_is_repositioned_onto_it() {
        // bspwm: `transfer_node()` -> `adapt_geometry()` when the monitor changes.
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "node -t floating");
        {
            let t = &mut wm.monitors[0].desktops[0].tree;
            let f = t.focus.unwrap();
            t.node_mut(f).client.as_mut().unwrap().floating_rectangle = Rect::new(100, 100, 200, 200);
        }
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "node -m HDMI-A-1");
        assert_eq!(reply, Reply::Ok(String::new()));
        let t = &wm.monitors[1].desktops[0].tree;
        let r = t.node(t.first_extrema(t.root).unwrap()).client.as_ref().unwrap().floating_rectangle;
        assert!(r.x >= 800 && r.x + r.width <= 1600, "{r:?}");
    }

    fn desktop_names(wm: &Wm, monitor: usize) -> Vec<String> {
        wm.monitors[monitor].desktops.iter().map(|d| d.name.clone()).collect()
    }

    #[test]
    fn removing_an_occupied_desktop_moves_its_windows_to_the_previous_one() {
        // bspwm: `desktop -r` is `merge_desktops()` then `remove_desktop()`.
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "monitor eDP-1 -a III");
        assert_eq!(desktop_names(&wm, 0), ["I", "III"]);
        let ids_before: Vec<u32> = (1..=2).collect();
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "desktop I -r");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(desktop_names(&wm, 0), ["III"]);
        let t = &wm.monitors[0].desktops[0].tree;
        assert_eq!(t.clients_count_in(t.root), 2, "both windows survived");
        for id in ids_before {
            let (desktop, node) = registry.lookup(id).expect("the wire id still resolves");
            assert_eq!(desktop, wm.monitors[0].desktops[0].id);
            assert!(t.contains(node));
        }
        assert!(events.iter().any(|e| e.kind() == EventKind::NodeTransfer));
        assert!(events.iter().any(|e| e.kind() == EventKind::DesktopRemove));
        // A monitor's only desktop stays.
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "desktop III -r");
        assert!(matches!(reply, Reply::Fail(_)));
    }

    #[test]
    fn a_desktop_moved_to_another_monitor_is_laid_out_there_and_the_source_shows_another() {
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "monitor eDP-1 -a III");
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "desktop I -m HDMI-A-1");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(desktop_names(&wm, 0), ["III"]);
        assert_eq!(desktop_names(&wm, 1), ["II", "I"]);
        assert_eq!(wm.monitors[0].focused, Some(0), "the source monitor shows what is left");
        let t = &wm.monitors[1].desktops[1].tree;
        let leaf = t.first_extrema(t.root).unwrap();
        assert!(t.node(leaf).client.as_ref().unwrap().tiled_rectangle.x >= 800, "arranged on the second monitor");
        assert!(events.iter().any(|e| e.kind() == EventKind::DesktopTransfer));
        // Without --follow the focus stays on the monitor it was on.
        assert_eq!(wm.focused_monitor, Some(0));
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "desktop III -m HDMI-A-1");
        assert!(matches!(reply, Reply::Fail(_)), "a monitor keeps at least one desktop");
    }

    #[test]
    fn swapping_desktops_across_monitors_trades_their_windows_and_arranges_both() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "desktop I -s II");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(desktop_names(&wm, 0), ["II"]);
        assert_eq!(desktop_names(&wm, 1), ["I"]);
        let t = &wm.monitors[1].desktops[0].tree;
        let leaf = t.first_extrema(t.root).unwrap();
        assert!(t.node(leaf).client.as_ref().unwrap().tiled_rectangle.x >= 800);
        // bspwm reports the pair in the order given: `desktop_swap <m1> <d1> <m2> <d2>`.
        assert!(events.iter().any(|e| matches!(e, Event::DesktopSwap { src_desktop: 1, dst_desktop: 2, .. })));
        // Focus stays on its monitor (now showing desktop II).
        assert_eq!(wm.focused_monitor, Some(0));
        // `desktop -s` on one monitor: focus goes with the focused desktop.
        run(&mut wm, &mut registry, &mut adapter, "monitor HDMI-A-1 -a III");
        run(&mut wm, &mut registry, &mut adapter, "desktop -f I");
        assert_eq!(wm.focused_monitor, Some(1));
        run(&mut wm, &mut registry, &mut adapter, "desktop focused -s III");
        assert_eq!(desktop_names(&wm, 1), ["III", "I"]);
        assert_eq!(wm.monitors[1].focused, Some(1), "the focused desktop keeps the focus wherever it went");
    }

    fn query_ids(wm: &mut Wm, registry: &mut NodeRegistry, adapter: &mut FakeAdapter, cmd: &str) -> Vec<u32> {
        let (reply, _) = run(wm, registry, adapter, cmd);
        match reply {
            Reply::Ok(text) => text.lines().map(|l| u32::from_str_radix(l.trim_start_matches("0x"), 16).unwrap()).collect(),
            Reply::Fail(_) => Vec::new(),
        }
    }

    #[test]
    fn biggest_and_smallest_compare_the_shown_areas_and_skip_vacant_nodes() {
        // bspwm: `find_by_area()`; the reference is the real one, so `.local` works.
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "node @/ -r 0.7");
        assert_eq!(query_ids(&mut wm, &mut registry, &mut adapter, "query -N -n biggest.local"), [1]);
        assert_eq!(query_ids(&mut wm, &mut registry, &mut adapter, "query -N -n smallest.local"), [2]);
        // `local` means the reference's desktop: the other monitor's is not in it.
        run(&mut wm, &mut registry, &mut adapter, "desktop II -f");
        assert!(query_ids(&mut wm, &mut registry, &mut adapter, "query -N -n biggest.local").is_empty());
        // A floating window is vacant and never the answer.
        run(&mut wm, &mut registry, &mut adapter, "desktop I -f");
        run(&mut wm, &mut registry, &mut adapter, "node 0x00000002 -t floating");
        assert_eq!(query_ids(&mut wm, &mut registry, &mut adapter, "query -N -n smallest"), [1]);
    }

    #[test]
    fn next_and_prev_walk_every_node_and_cross_desktops_and_monitors() {
        // bspwm: `find_closest_node()`: in-order, internal nodes too, and on to the next desktop.
        let (mut wm, mut registry, mut adapter) = fixture();
        // Desktop I holds windows 1 and 2, so in order: 1, the split above them, 2.
        assert_eq!(query_ids(&mut wm, &mut registry, &mut adapter, "query -N -n next.window"), [2]);
        // Move window 2 to the second monitor: `next` from 1 now crosses to it.
        run(&mut wm, &mut registry, &mut adapter, "node 0x00000002 -d II");
        assert_eq!(query_ids(&mut wm, &mut registry, &mut adapter, "query -N -n next.window"), [2]);
        assert_eq!(query_ids(&mut wm, &mut registry, &mut adapter, "query -N -n prev.window"), [2], "and wraps backwards");
    }

    #[test]
    fn same_class_compares_with_the_reference_window() {
        let (mut wm, mut registry, mut adapter) = fixture();
        adapter.set_class(WindowId(1), "Term", "a");
        adapter.set_class(WindowId(2), "Term", "b");
        assert_eq!(query_ids(&mut wm, &mut registry, &mut adapter, "query -N -n next.window.same_class"), [2]);
        adapter.set_class(WindowId(2), "Other", "b");
        assert!(query_ids(&mut wm, &mut registry, &mut adapter, "query -N -n next.window.same_class").is_empty());
        assert_eq!(query_ids(&mut wm, &mut registry, &mut adapter, "query -N -n next.window.!same_class"), [2]);
    }

    #[test]
    fn pointed_names_the_window_and_the_monitor_under_the_pointer() {
        let (mut wm, mut registry, mut adapter) = fixture();
        // No pointer: nothing to match.
        assert!(query_ids(&mut wm, &mut registry, &mut adapter, "query -N -n pointed").is_empty());
        adapter.pointer = (Some((900, 10)), Some(WindowId(2)));
        assert_eq!(query_ids(&mut wm, &mut registry, &mut adapter, "query -N -n pointed"), [2]);
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "monitor pointed -f");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(wm.focused_monitor, Some(1));
    }

    #[test]
    fn focusing_a_hidden_node_fails_and_changes_nothing() {
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "node east -g hidden=on");
        let before = focus_of(&wm);
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "node -f east");
        assert!(matches!(reply, Reply::Fail(_)));
        assert!(events.iter().all(|e| e.kind() != EventKind::NodeFocus));
        assert_eq!(focus_of(&wm), before);
    }

    #[test]
    fn hiding_the_focused_node_moves_the_focus_to_the_next_one() {
        // bspwm: `set_hidden()` refocuses when the focused node was hidden.
        let (mut wm, mut registry, mut adapter) = fixture();
        assert_eq!(focus_of(&wm).2, Some(1));
        run(&mut wm, &mut registry, &mut adapter, "node -g hidden=on");
        assert_eq!(focus_of(&wm).2, Some(2));
    }

    #[test]
    fn focusing_a_node_clears_its_urgent_flag_and_reports_it() {
        let (mut wm, mut registry, mut adapter) = fixture();
        {
            let t = &mut wm.monitors[0].desktops[0].tree;
            let right = t.next_leaf(t.focus, t.root).unwrap();
            t.node_mut(right).client.as_mut().unwrap().urgent = true;
        }
        let (_, events) = run(&mut wm, &mut registry, &mut adapter, "node -f east");
        assert!(events.iter().any(|e| matches!(e, Event::NodeFlag { flag: "urgent", on: false, .. })));
        let t = &wm.monitors[0].desktops[0].tree;
        assert!(!t.node(t.focus.unwrap()).client.as_ref().unwrap().urgent);
    }

    #[test]
    fn focusing_another_window_lowers_a_fullscreen_one_that_would_cover_it() {
        // bspwm: `focus_node()` -> `neutralize_occluding_windows()`.
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "node -t fullscreen");
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "node -f next.window");
        assert_eq!(reply, Reply::Ok(String::new()));
        let t = &wm.monitors[0].desktops[0].tree;
        let left = t.first_extrema(t.root).unwrap();
        assert_eq!(t.node(left).client.as_ref().unwrap().state, bsp_core::node::ClientState::Tiled);
        assert!(events.iter().any(|e| matches!(e, Event::NodeState { on: true, state: bsp_core::node::ClientState::Tiled, .. })));
    }

    #[test]
    fn focusing_an_empty_desktop_reports_no_node_focus() {
        // bspwm: `focus_node()` returns before `node_focus` when there is no node.
        let (mut wm, mut registry, mut adapter) = fixture();
        let (_, events) = run(&mut wm, &mut registry, &mut adapter, "desktop II -f");
        assert!(events.iter().any(|e| e.kind() == EventKind::DesktopFocus));
        assert!(events.iter().all(|e| e.kind() != EventKind::NodeFocus));
    }

    #[test]
    fn node_activate_fails_on_the_focused_desktop() {
        // bspwm: `activate_node()` returns false for `d == mon->desk`.
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "node -a east");
        assert!(matches!(reply, Reply::Fail(_)));
    }

    fn stacking_ids(reply: &Reply) -> Vec<u64> {
        let Reply::Ok(text) = reply else { panic!("{reply:?}") };
        let json: serde_json::Value = serde_json::from_str(text.trim()).unwrap();
        json["stackingList"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect()
    }

    #[test]
    fn focusing_a_tiled_window_does_not_lift_it_above_a_floating_one() {
        // bspwm: `stack()` keeps every window inside its level.
        let (mut wm, mut registry, mut adapter) = fixture();
        let (_, events) = run(&mut wm, &mut registry, &mut adapter, "node -t floating");
        assert!(events.iter().all(|e| e.kind() != EventKind::NodeStack), "the first window has nothing to be stacked against");
        let (_, events) = run(&mut wm, &mut registry, &mut adapter, "node -f next.window");
        assert!(events.iter().any(|e| matches!(e, Event::NodeStack { above: false, .. })), "{events:?}");
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "wm -d");
        // fixture ids: 1 = the floating window, 2 = the tiled one just focused.
        assert_eq!(stacking_ids(&reply), [2, 1]);
    }

    #[test]
    fn node_layer_above_raises_a_window_over_the_floating_ones() {
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "node -t floating");
        run(&mut wm, &mut registry, &mut adapter, "node -f next.window");
        run(&mut wm, &mut registry, &mut adapter, "node -l above");
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "wm -d");
        assert_eq!(stacking_ids(&reply), [1, 2]);
    }

    #[test]
    fn a_killed_window_keeps_its_place_in_the_stack_until_it_is_destroyed() {
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "node -f east");
        run(&mut wm, &mut registry, &mut adapter, "node -k");
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "wm -d");
        assert_eq!(stacking_ids(&reply), [2], "a killed window keeps its place until it is destroyed");
    }

    #[test]
    fn node_state_reports_the_left_state_off_then_the_new_one_on() {
        // bspwm: `set_state()` puts two `node_state` lines.
        let (mut wm, mut registry, mut adapter) = fixture();
        let (_, events) = run(&mut wm, &mut registry, &mut adapter, "node -t floating");
        let lines: Vec<String> = events
            .iter()
            .filter(|e| e.kind() == EventKind::NodeState)
            .map(|e| e.to_string().split(' ').skip(4).collect::<Vec<_>>().join(" "))
            .collect();
        assert_eq!(lines, ["tiled off", "floating on"]);
    }

    #[test]
    fn node_state_floating_round_trips_via_alternate() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "node -t floating");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert!(events.iter().any(|e| matches!(
            &e,
            Event::NodeState {
                state: ClientState::Floating,
                on: true,
                ..
            }
        )));
        let left = wm.monitors[0].desktops[0].tree.focus.unwrap();
        assert_eq!(
            wm.monitors[0].desktops[0]
                .tree
                .node(left)
                .client
                .as_ref()
                .unwrap()
                .state,
            ClientState::Floating
        );
    }

    #[test]
    fn node_move_fails_on_a_tiled_node() {
        // bspwm: `move_client()` only moves a tiled node while an
        // interactive pointer drag is being tracked, which a `bspc
        // node --move` request never is (`docs/bsp-core.md`
        // `Tree::move_floating`).
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "node -v 10 10");
        assert_eq!(reply, Reply::Fail(String::new()));
        assert!(events.is_empty());
    }

    #[test]
    fn node_move_translates_a_floating_node_and_reports_geometry() {
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "node -t floating");
        let left = wm.monitors[0].desktops[0].tree.focus.unwrap();
        let before = wm.monitors[0].desktops[0]
            .tree
            .node(left)
            .client
            .as_ref()
            .unwrap()
            .floating_rectangle;

        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "node -v 10 -5");
        assert_eq!(reply, Reply::Ok(String::new()));
        let after = wm.monitors[0].desktops[0]
            .tree
            .node(left)
            .client
            .as_ref()
            .unwrap()
            .floating_rectangle;
        assert_eq!(after.x, before.x + 10);
        assert_eq!(after.y, before.y - 5);
        assert!(events.iter().any(|e| matches!(
            e,
            Event::NodeGeometry { geometry, .. } if *geometry == after
        )));
    }

    #[test]
    fn node_resize_tiled_adjusts_the_shared_fence() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let root = wm.monitors[0].desktops[0].tree.root.unwrap();
        let ratio_before = wm.monitors[0].desktops[0].tree.node(root).split_ratio;

        // `left` is focused; its right edge is the fence shared with
        // `right` (`fixture()` splits the monitor rect side by side,
        // the wider dimension, per `insert_node`'s longest-side rule).
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "node -z right 40 0");
        assert_eq!(reply, Reply::Ok(String::new()));
        let ratio_after = wm.monitors[0].desktops[0].tree.node(root).split_ratio;
        assert!(ratio_after > ratio_before);
        // A tiled resize re-arranges, and the re-arrangement reports both
        // windows it moved (bspwm: `apply_layout()`; `resize_client()` reports
        // on its own only for `STATE_FLOATING`).
        let moved: Vec<Rect> = events
            .iter()
            .filter_map(|e| match e {
                Event::NodeGeometry { geometry, .. } => Some(*geometry),
                _ => None,
            })
            .collect();
        let t = &wm.monitors[0].desktops[0].tree;
        let shown: Vec<Rect> = [t.node(root).first_child(), t.node(root).second_child()]
            .into_iter()
            .flatten()
            .filter_map(|n| t.node(n).client.as_ref().map(|c| c.shown_rectangle()))
            .collect();
        assert_eq!(moved, shown);
    }

    #[test]
    fn node_resize_floating_grows_from_the_dragged_corner() {
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "node -t floating");
        let left = wm.monitors[0].desktops[0].tree.focus.unwrap();
        let before = wm.monitors[0].desktops[0]
            .tree
            .node(left)
            .client
            .as_ref()
            .unwrap()
            .floating_rectangle;

        let (reply, events) = run(
            &mut wm,
            &mut registry,
            &mut adapter,
            "node -z bottom_right 20 20",
        );
        assert_eq!(reply, Reply::Ok(String::new()));
        let after = wm.monitors[0].desktops[0]
            .tree
            .node(left)
            .client
            .as_ref()
            .unwrap()
            .floating_rectangle;
        assert_eq!(after.width, before.width + 20);
        assert_eq!(after.height, before.height + 20);
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::NodeGeometry { .. })));
    }

    #[test]
    fn node_flag_marked_toggles() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let left = wm.monitors[0].desktops[0].tree.focus.unwrap();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "node -g marked");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert!(wm.monitors[0].desktops[0].tree.node(left).marked);
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "node -g marked");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert!(!wm.monitors[0].desktops[0].tree.node(left).marked);
    }

    #[test]
    fn node_rotate_flip_equalize_balance_do_not_fail() {
        let (mut wm, mut registry, mut adapter) = fixture();
        for cmd in ["node -R 90", "node -F horizontal", "node -E", "node -B"] {
            let (reply, _) = run(&mut wm, &mut registry, &mut adapter, cmd);
            assert_eq!(reply, Reply::Ok(String::new()), "{cmd} failed");
        }
    }

    #[test]
    fn node_insert_receptacle_registers_a_new_node() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let before = registry.lookup(1).is_some();
        assert!(before);
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "node -i");
        assert_eq!(reply, Reply::Ok(String::new()));
        // A third node should now be registered (ids 1 and 2 already
        // existed from the fixture).
        assert!(registry.lookup(3).is_some());
    }

    #[test]
    fn node_close_and_kill_call_the_adapter_and_leave_the_node_in_the_tree() {
        // bspwm: `kill_node()` kills the client and the node goes away when the
        // window is destroyed, not before.
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "node -c");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(adapter.closed, vec![WindowId(1)]);

        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "node -k");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(adapter.killed, vec![WindowId(1)]);
        assert!(events.iter().all(|e| e.kind() != EventKind::NodeRemove));
        let t = &wm.monitors[0].desktops[0].tree;
        assert_eq!(t.clients_count_in(t.root), 2, "both windows are still in the tree");
    }

    #[test]
    fn node_close_and_kill_reach_every_window_of_a_subtree() {
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "node @/ -c");
        assert_eq!(adapter.closed, vec![WindowId(1), WindowId(2)]);
        run(&mut wm, &mut registry, &mut adapter, "node @/ -k");
        assert_eq!(adapter.killed, vec![WindowId(1), WindowId(2)]);
    }

    #[test]
    fn a_locked_window_cannot_be_closed_but_can_be_killed() {
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "node -g locked=on");
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "node -c");
        assert!(matches!(reply, Reply::Fail(_)));
        assert!(adapter.closed.is_empty());
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "node -k");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(adapter.killed, vec![WindowId(1)]);
    }

    #[test]
    fn killing_a_receptacle_removes_it_at_once_with_its_own_id() {
        // bspwm: `kill_node()`'s `IS_RECEPTACLE` branch.
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "node -i");
        let id = (3..10)
            .find(|id| {
                registry.lookup(*id).is_some_and(|(d, n)| {
                    wm.monitors[0].desktops.iter().find(|x| x.id == d).is_some_and(|x| x.tree.is_receptacle(n))
                })
            })
            .unwrap();
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, &format!("node 0x{id:08X} -k"));
        assert_eq!(reply, Reply::Ok(String::new()));
        assert!(adapter.killed.is_empty());
        assert!(events.iter().any(|e| matches!(e, Event::NodeRemove { node, .. } if *node == id)));
        assert!(registry.lookup(id).is_none());
    }

    #[test]
    fn every_node_including_splits_has_a_wire_id_after_a_command() {
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "node -f east");
        let tree = &wm.monitors[0].desktops[0].tree;
        let root = tree.root.unwrap();
        assert!(tree.node(root).client.is_none());
        let id = registry.id_of(bsp_core::id::DesktopId(1), root).unwrap();
        assert_ne!(id, 0);
        assert_eq!(registry.lookup(id), Some((bsp_core::id::DesktopId(1), root)));
    }

    #[test]
    fn desktop_bubble_steps_and_wraps_like_bspwm() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let settings = wm.settings.clone();
        for (id, name) in [(3, "III"), (4, "IV")] {
            wm.monitors[1].add_desktop(Desktop::new(bsp_core::id::DesktopId(id), Some(name), &settings));
        }
        let names = |wm: &Wm| wm.monitors[1].desktops.iter().map(|d| d.name.clone()).collect::<Vec<_>>();
        run(&mut wm, &mut registry, &mut adapter, "desktop II -b next");
        assert_eq!(names(&wm), ["III", "II", "IV"]);
        let (_, events) = run(&mut wm, &mut registry, &mut adapter, "desktop IV -b next");
        assert_eq!(names(&wm), ["IV", "III", "II"]);
        assert_eq!(events.iter().filter(|e| e.kind() == EventKind::DesktopSwap).count(), 2);
        run(&mut wm, &mut registry, &mut adapter, "desktop IV -b prev");
        assert_eq!(names(&wm), ["III", "II", "IV"]);
    }

    #[test]
    fn a_single_monocle_flip_reports_desktop_layout() {
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "config single_monocle true");
        let (_, events) = run(&mut wm, &mut registry, &mut adapter, "node -t floating");
        assert_eq!(wm.monitors[0].desktops[0].layout, bsp_core::tree::Layout::Monocle);
        assert!(events.iter().any(|e| e.kind() == EventKind::DesktopLayout));
    }

    #[test]
    fn a_sticky_window_follows_the_shown_desktop_and_cannot_be_sent_elsewhere() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let settings = wm.settings.clone();
        wm.monitors[0].add_desktop(Desktop::new(bsp_core::id::DesktopId(3), Some("X"), &settings));
        run(&mut wm, &mut registry, &mut adapter, "node -g sticky=on");
        // Not to a desktop that isn't shown.
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "node -d X");
        assert!(matches!(reply, Reply::Fail(_)));
        let count = |wm: &Wm, i: usize| wm.monitors[0].desktops[i].tree.sticky_count(wm.monitors[0].desktops[i].tree.root);
        assert_eq!(count(&wm, 0), 1);
        run(&mut wm, &mut registry, &mut adapter, "desktop -f X");
        assert_eq!(count(&wm, 0), 0);
        assert_eq!(count(&wm, 1), 1);
        run(&mut wm, &mut registry, &mut adapter, "desktop -a I");
        assert_eq!(count(&wm, 1), 0);
        assert_eq!(count(&wm, 0), 1);
    }

    #[test]
    fn node_swap_exchanges_windows_between_desktops_and_monitors_keeping_ids() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let settings = wm.settings.clone();
        let d2 = bsp_core::id::DesktopId(2);
        let far = wm.monitors[1].desktops[0].tree.new_client_node(&settings, Client::new(WindowId(9), 1));
        wm.monitors[1].desktops[0].tree.insert_node(&settings, far, None);
        registry.register(d2, far);
        wm.monitors[1].arrange(0, &settings);
        let near = wm.monitors[0].desktops[0].tree.focus.unwrap();
        let near_id = registry.id_of(bsp_core::id::DesktopId(1), near).unwrap();
        let far_id = registry.id_of(d2, far).unwrap();

        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, &format!("node 0x{near_id:08X} -s 0x{far_id:08X}"));
        assert_eq!(reply, Reply::Ok(String::new()));
        assert!(events.iter().any(|e| e.kind() == EventKind::NodeSwap));
        let (d, n) = registry.lookup(near_id).unwrap();
        assert_eq!(d, d2);
        let window = wm.monitors[1].desktops[0].tree.node(n).client.as_ref().unwrap().window;
        assert_eq!(window, WindowId(1));
        let (d, n) = registry.lookup(far_id).unwrap();
        assert_eq!(d, bsp_core::id::DesktopId(1));
        assert_eq!(wm.monitors[0].desktops[0].tree.node(n).client.as_ref().unwrap().window, WindowId(9));
        // Nothing lost, nothing duplicated.
        assert_eq!(wm.monitors[0].desktops[0].tree.node_ids().len(), 3);
        assert_eq!(wm.monitors[1].desktops[0].tree.node_ids().len(), 1);
        // The moved window fits the other monitor.
        let r = wm.monitors[1].desktops[0].tree.node(wm.monitors[1].desktops[0].tree.root.unwrap()).client.as_ref().unwrap().tiled_rectangle;
        assert!(r.x >= 800);
    }

    #[test]
    fn merging_then_removing_a_monitor_keeps_every_window_and_refocuses() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let mut events = Vec::new();
        {
            let mut ctx = ExecCtx { wm: &mut wm, registry: &mut registry, adapter: &mut adapter };
            assert_eq!(merge_target(ctx.wm, 0), Some(1));
            merge_monitors(&mut ctx, 0, 1, &mut events);
            assert!(ctx.wm.monitors[0].desktops.is_empty());
            let kept = remove_monitor(&mut ctx, 0, 1, &mut events);
            assert_eq!(kept, 0);
        }
        assert_eq!(wm.monitors.len(), 1);
        let names: Vec<_> = wm.monitors[0].desktops.iter().map(|d| d.name.clone()).collect();
        assert_eq!(names, ["II", "I"]);
        let windows = wm.monitors[0].desktops[1].tree.node_ids().into_iter().filter(|&n| wm.monitors[0].desktops[1].tree.node(n).client.is_some()).count();
        assert_eq!(windows, 2);
        assert_eq!(wm.focused_monitor, Some(0));
        for kind in [EventKind::DesktopTransfer, EventKind::MonitorRemove, EventKind::MonitorFocus] {
            assert!(events.iter().any(|e| e.kind() == kind), "{kind:?}");
        }
    }

    /// The fixture plus window 9 on desktop II of the second monitor.
    fn fixture_with_far_window() -> (Wm, NodeRegistry, FakeAdapter) {
        let (mut wm, mut registry, adapter) = fixture();
        let settings = wm.settings.clone();
        let far = wm.monitors[1].desktops[0].tree.new_client_node(&settings, Client::new(WindowId(9), 1));
        wm.monitors[1].desktops[0].tree.insert_node(&settings, far, None);
        registry.register(bsp_core::id::DesktopId(2), far);
        wm.monitors[1].arrange(0, &settings);
        (wm, registry, adapter)
    }

    fn focused_window(wm: &Wm) -> Option<WindowId> {
        let m = &wm.monitors[wm.focused_monitor?];
        let t = &m.desktops[m.focused?].tree;
        t.node(t.focus?).client.as_ref().map(|c| c.window)
    }

    #[test]
    fn a_cross_desktop_swap_of_the_focused_window_focuses_what_came_in() {
        let (mut wm, mut registry, mut adapter) = fixture_with_far_window();
        let (_, events2) = run(&mut wm, &mut registry, &mut adapter, "node focused -s @II:/");
        // Not following: the focus stays on desktop I, now on window 9.
        assert_eq!(wm.focused_monitor, Some(0));
        assert_eq!(focused_window(&wm), Some(WindowId(9)));
        assert!(events2.iter().any(|e| e.kind() == EventKind::NodeFocus));
    }

    #[test]
    fn a_cross_desktop_swap_with_follow_follows_the_focused_window() {
        let (mut wm, mut registry, mut adapter) = fixture_with_far_window();
        run(&mut wm, &mut registry, &mut adapter, "node focused -s @II:/ --follow");
        assert_eq!(wm.focused_monitor, Some(1));
        assert_eq!(focused_window(&wm), Some(WindowId(1)));
    }

    #[test]
    fn setting_sticky_on_a_hidden_desktop_brings_the_window_to_the_shown_one() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let settings = wm.settings.clone();
        wm.monitors[0].add_desktop(Desktop::new(bsp_core::id::DesktopId(3), Some("X"), &settings));
        run(&mut wm, &mut registry, &mut adapter, "node -d X");
        let hidden = registry.id_of(bsp_core::id::DesktopId(3), wm.monitors[0].desktops[1].tree.root.unwrap()).unwrap();
        run(&mut wm, &mut registry, &mut adapter, &format!("node 0x{hidden:08X} -g sticky=on"));
        assert_eq!(wm.monitors[0].desktops[1].tree.root, None);
        assert_eq!(wm.monitors[0].sticky_count(), 1);
        let (d, n) = registry.lookup(hidden).unwrap();
        assert_eq!(d, bsp_core::id::DesktopId(1));
        assert!(wm.monitors[0].desktops[0].tree.node(n).sticky);
    }

    #[test]
    fn a_focused_sticky_window_keeps_the_focus_across_desktops() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let settings = wm.settings.clone();
        wm.monitors[0].add_desktop(Desktop::new(bsp_core::id::DesktopId(3), Some("X"), &settings));
        let w3 = wm.monitors[0].desktops[1].tree.new_client_node(&settings, Client::new(WindowId(3), 1));
        wm.monitors[0].desktops[1].tree.insert_node(&settings, w3, None);
        run(&mut wm, &mut registry, &mut adapter, "node -g sticky=on");
        run(&mut wm, &mut registry, &mut adapter, "desktop -f X");
        assert_eq!(focused_window(&wm), Some(WindowId(1)));
    }

    #[test]
    fn swapping_away_the_shown_desktop_leaves_its_sticky_windows_on_screen() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let settings = wm.settings.clone();
        wm.monitors[0].add_desktop(Desktop::new(bsp_core::id::DesktopId(3), Some("X"), &settings));
        run(&mut wm, &mut registry, &mut adapter, "node -g sticky=on");
        run(&mut wm, &mut registry, &mut adapter, "desktop I -s X");
        // bspwm refocuses I (now second) and the sticky window is on it.
        let shown = &wm.monitors[0].desktops[wm.monitors[0].focused.unwrap()];
        assert_eq!(shown.name, "I");
        assert_eq!(wm.monitors[0].desktops[0].name, "X");
        assert_eq!(shown.tree.sticky_count(shown.tree.root), 1);
    }

    #[test]
    fn a_sticky_split_moves_with_its_whole_subtree() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let settings = wm.settings.clone();
        wm.monitors[0].add_desktop(Desktop::new(bsp_core::id::DesktopId(3), Some("X"), &settings));
        run(&mut wm, &mut registry, &mut adapter, "node @/ -g sticky=on");
        run(&mut wm, &mut registry, &mut adapter, "desktop -f X");
        let x = &wm.monitors[0].desktops[1].tree;
        let windows = x.node_ids().into_iter().filter(|&n| x.node(n).client.is_some()).count();
        assert_eq!(windows, 2);
        assert!(wm.monitors[0].desktops[0].tree.root.is_none());
    }

    fn lines(reply: Reply) -> Vec<String> {
        match reply {
            Reply::Ok(s) => s.lines().map(String::from).collect(),
            Reply::Fail(_) => Vec::new(),
        }
    }

    #[test]
    fn query_filters_and_targets_follow_cmd_query() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let mut q = |cmd: &str| lines(run(&mut wm, &mut registry, &mut adapter, cmd).0);
        // `-n .MODIFIERS` filters: the two windows, not their split.
        assert_eq!(q("query -N -n .window"), ["0x00000001", "0x00000002"]);
        assert_eq!(q("query -D -d .occupied --names"), ["I"]);
        assert_eq!(q("query -D -d .!occupied --names"), ["II"]);
        // A flag alone is the focused one.
        assert_eq!(q("query -M -m --names"), ["eDP-1"]);
        assert_eq!(q("query -D -d --names"), ["I"]);
        // `-n SEL` narrows a desktop listing to that node's desktop.
        assert_eq!(q("query -D -n 0x00000002 --names"), ["I"]);
        assert_eq!(q("query -D -m HDMI-A-1 --names"), ["II"]);
        // Only the descriptor-free constraints are incompatible.
        assert!(q("query -T").is_empty());
        let args: Vec<String> = "query -M -d .occupied".split(' ').map(String::from).collect();
        assert!(command::parse(&args).is_err());
    }

    #[test]
    fn desktop_index_without_a_monitor_counts_across_monitors() {
        // bspwm: `desktop_from_index()` walks every monitor's desktops.
        let (mut wm, mut registry, mut adapter) = fixture();
        let settings = wm.settings.clone();
        wm.monitors[0].add_desktop(Desktop::new(bsp_core::id::DesktopId(3), Some("III"), &settings));
        let names = |r: Reply| match r {
            Reply::Ok(s) => s.trim().to_string(),
            Reply::Fail(m) => format!("fail {m}"),
        };
        assert_eq!(names(run(&mut wm, &mut registry, &mut adapter, "query -D -d ^2 --names").0), "III");
        assert_eq!(names(run(&mut wm, &mut registry, &mut adapter, "query -D -d ^3 --names").0), "II");
        assert_eq!(names(run(&mut wm, &mut registry, &mut adapter, "query -D -d HDMI-A-1:^1 --names").0), "II");
        // Moving the focused window to a desktop by index, as a common binding does.
        assert_eq!(run(&mut wm, &mut registry, &mut adapter, "node -d ^2").0, Reply::Ok(String::new()));
        assert_eq!(wm.monitors[0].desktops[1].tree.node_ids().len(), 1);
    }

    #[test]
    fn desktop_rename_and_focus() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "desktop -n Web");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(wm.monitors[0].desktops[0].name, "Web");
        assert!(events.iter().any(|e| e.kind() == EventKind::DesktopRename));

        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "desktop -f II");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(wm.focused_monitor, Some(1));
        assert!(events.iter().any(|e| e.kind() == EventKind::DesktopFocus));
    }

    #[test]
    fn desktop_layout_monocle_arranges_full_size() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "desktop -l monocle");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(wm.monitors[0].desktops[0].layout, Layout::Monocle);
        assert!(events.iter().any(|e| e.kind() == EventKind::DesktopLayout));
    }

    #[test]
    fn monitor_add_desktops_and_rename() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, events) = run(
            &mut wm,
            &mut registry,
            &mut adapter,
            "monitor HDMI-A-1 -a III IV",
        );
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(wm.monitors[1].desktops.len(), 3);
        assert!(events.iter().any(|e| e.kind() == EventKind::DesktopAdd));

        let (reply, _) = run(
            &mut wm,
            &mut registry,
            &mut adapter,
            "monitor HDMI-A-1 -n secondary",
        );
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(wm.monitors[1].name, "secondary");
    }

    #[test]
    fn monitor_swap_exchanges_positions() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, events) = run(
            &mut wm,
            &mut registry,
            &mut adapter,
            "monitor eDP-1 -s HDMI-A-1",
        );
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(wm.monitors[0].name, "HDMI-A-1");
        assert_eq!(wm.monitors[1].name, "eDP-1");
        assert!(events.iter().any(|e| e.kind() == EventKind::MonitorSwap));
    }

    #[test]
    fn monitor_set_rectangle_reorders_a_monitor_moved_past_its_neighbor() {
        // eDP-1 starts at index 0 (0,0,800,600), left of HDMI-A-1 at
        // index 1 (800,0,800,600). Moving eDP-1 to x=1600 puts it to the
        // right of HDMI-A-1, so `bspc monitor -g` should walk it into
        // the new position — bspwm: `src/monitor.c` `update_root()`
        // calls `reorder_monitor()` right after applying the rectangle.
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, events) = run(
            &mut wm,
            &mut registry,
            &mut adapter,
            "monitor eDP-1 -g 800x600+1600+0",
        );
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(wm.monitors[0].name, "HDMI-A-1");
        assert_eq!(wm.monitors[1].name, "eDP-1");
        assert!(events
            .iter()
            .any(|e| e.kind() == EventKind::MonitorGeometry));
    }

    #[test]
    fn output_bare_lists_the_adapters_known_outputs() {
        // FakeAdapter never overrides `Adapter::output_names`, so this
        // exercises the trait's own default ("no known outputs") —
        // exactly what the nested winit backend gets until a real DRM
        // backend overrides it (hardware backend).
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "output");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert!(events.is_empty());
    }

    #[test]
    fn output_get_on_an_unknown_name_fails() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "output eDP-1");
        assert_eq!(
            reply,
            Reply::Fail("output: unknown output 'eDP-1'.\n".to_string())
        );
    }

    #[test]
    fn output_set_fails_with_no_hardware_backend() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "output eDP-1 -s 1.25");
        assert_eq!(
            reply,
            Reply::Fail("output: not supported (no hardware output backend yet).\n".to_string())
        );
    }

    #[test]
    fn input_bare_lists_the_adapters_known_devices() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "input");
        assert_eq!(reply, Reply::Ok(String::new()));
    }

    #[test]
    fn input_set_fails_with_no_hardware_backend() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(
            &mut wm,
            &mut registry,
            &mut adapter,
            "input keyboard -r 25 200",
        );
        assert_eq!(
            reply,
            Reply::Fail("input: not supported (no hardware input backend yet).\n".to_string())
        );
    }

    #[test]
    fn query_nodes_lists_hex_ids() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "query -N");
        match reply {
            Reply::Ok(s) => {
                // Two windows and the split above them, parents first.
                assert_eq!(s.lines().count(), 3);
                assert!(s.contains("0x00000001"));
                assert!(s.contains("0x00000002"));
                assert!(!s.contains("0x00000000"));
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn query_nodes_with_a_node_selector_lists_only_that_node() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (all, _) = run(&mut wm, &mut registry, &mut adapter, "query -N");
        let (one, _) = run(&mut wm, &mut registry, &mut adapter, "query -N -n focused");
        match (all, one) {
            (Reply::Ok(all), Reply::Ok(one)) => {
                assert!(all.lines().count() >= 2);
                assert_eq!(one.lines().count(), 1);
                assert!(all.contains(one.trim()));
            }
            other => panic!("expected Ok replies, got {other:?}"),
        }
    }

    #[test]
    fn query_monitors_names() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "query -M --names");
        match reply {
            Reply::Ok(s) => assert_eq!(s, "eDP-1\nHDMI-A-1\n"),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn query_tree_produces_parseable_json_with_client_class() {
        let (mut wm, mut registry, mut adapter) = fixture();
        adapter.set_class(WindowId(1), "Firefox", "Navigator");
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "query -T -n focused");
        match reply {
            Reply::Ok(s) => {
                let v: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
                assert_eq!(v["client"]["className"], "Firefox");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn rule_add_then_list_shows_the_rule() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(
            &mut wm,
            &mut registry,
            &mut adapter,
            "rule -a Firefox:*:* state=floating",
        );
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(wm.rules.len(), 1);
        assert_eq!(wm.rules[0].consequence.state, Some(ClientState::Floating));

        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "rule -l");
        // bspwm: `src/rule.c` `list_rules()`, `"%s:%s:%s %c> %s\n"` — the
        // effect string must round-trip, not just the class:instance:name.
        assert_eq!(
            reply,
            Reply::Ok("Firefox:*:* => state=floating\n".to_string())
        );
    }

    #[test]
    fn wm_get_status_matches_built_report() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "wm -g");
        let expected = build_report(&wm).to_string();
        match reply {
            Reply::Ok(s) => assert_eq!(s, expected),
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn wm_restart_succeeds_with_no_events() {
        // `bsp_ipc` itself never restarts anything — that's
        // `bsp-compositor`'s job, inspecting the original `Command`
        // (`docs/bsp-compositor.md` Hotkeys and config progress) — this only checks
        // that the action itself is accepted and produces no `Event`s to
        // broadcast.
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "wm -r");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert!(events.is_empty());
    }

    #[test]
    fn wm_dump_state_produces_parseable_json() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "wm -d");
        match reply {
            Reply::Ok(s) => {
                let v: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
                assert_eq!(v["monitors"].as_array().unwrap().len(), 2);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn config_set_and_get_window_gap_globally() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "config window_gap 12");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(wm.settings.window_gap, 12);
        assert_eq!(wm.monitors[0].desktops[0].window_gap, 12);

        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "config window_gap");
        assert_eq!(reply, Reply::Ok("12\n".to_string()));
    }

    #[test]
    fn config_padding_with_m_targets_the_monitor_and_with_d_the_desktop() {
        // bspwm: `cmd_config()` leaves the desktop out of the coordinates for
        // `-m`, so the padding lands on the monitor (`SET_DEF_MON_DESK`).
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "config -m HDMI-A-1 top_padding 30");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(wm.monitors[1].padding.top, 30);
        assert_eq!(wm.monitors[1].desktops[0].padding.top, 0);
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "config -m HDMI-A-1 top_padding");
        assert_eq!(reply, Reply::Ok("30\n".to_string()));

        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "config -d II left_padding 7");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(wm.monitors[1].desktops[0].padding.left, 7);
        assert_eq!(wm.monitors[1].padding.left, 0);
    }

    #[test]
    fn config_border_width_reaches_existing_windows_and_scopes_like_bspwm() {
        // bspwm: `SET_DEF_DEFMON_DEFDESK_WIN`.
        let (mut wm, mut registry, mut adapter) = fixture();
        let widths = |wm: &Wm| {
            let t = &wm.monitors[0].desktops[0].tree;
            let mut out = Vec::new();
            let mut f = t.first_extrema(t.root);
            while let Some(n) = f {
                out.push(t.node(n).client.as_ref().unwrap().border_width);
                f = t.next_leaf(Some(n), t.root);
            }
            out
        };
        run(&mut wm, &mut registry, &mut adapter, "config border_width 4");
        assert_eq!(widths(&wm), [4, 4], "open windows change too");
        assert_eq!((wm.settings.border_width, wm.monitors[1].border_width, wm.monitors[1].desktops[0].border_width), (4, 4, 4));
        // One node only.
        run(&mut wm, &mut registry, &mut adapter, "config -n east border_width 0");
        assert_eq!(widths(&wm), [4, 0]);
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "config -n east border_width");
        assert_eq!(reply, Reply::Ok("0\n".to_string()));
        // A desktop: its default and its windows, not the other monitor.
        run(&mut wm, &mut registry, &mut adapter, "config -d focused border_width 2");
        assert_eq!(widths(&wm), [2, 2]);
        assert_eq!(wm.monitors[1].desktops[0].border_width, 4);
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "config border_width x");
        assert!(matches!(reply, Reply::Fail(_)));
    }

    #[test]
    fn config_window_gap_on_a_monitor_reaches_its_desktops() {
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "config -m HDMI-A-1 window_gap 20");
        assert_eq!(wm.monitors[1].window_gap, 20);
        assert_eq!(wm.monitors[1].desktops[0].window_gap, 20);
        assert_eq!(wm.monitors[0].desktops[0].window_gap, wm.settings.window_gap);
    }

    #[test]
    fn global_padding_sets_the_default_and_every_monitor_but_no_desktop() {
        // bspwm: `SET_DEF_MON_DESK`. A desktop's padding adds to its monitor's, so
        // setting both would pad twice.
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "config top_padding 30");
        assert_eq!(wm.settings.padding.top, 30);
        assert!(wm.monitors.iter().all(|m| m.padding.top == 30));
        assert!(wm.monitors.iter().flat_map(|m| &m.desktops).all(|d| d.padding.top == 0));
        run(&mut wm, &mut registry, &mut adapter, "config -d focused top_padding 5");
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "config -d focused top_padding");
        assert_eq!(reply, Reply::Ok("5\n".to_string()));
        assert_eq!(wm.monitors[0].padding.top, 30);
    }

    #[test]
    fn the_remaining_bspwm_settings_are_accepted_and_read_back() {
        let (mut wm, mut registry, mut adapter) = fixture();
        for (key, good, shown, bad) in [
            ("focus_follows_pointer", "true", "true", "maybe"),
            ("pointer_follows_focus", "on", "true", "x"),
            ("pointer_follows_monitor", "off", "false", "x"),
            ("presel_feedback", "false", "false", "x"),
            ("directional_focus_tightness", "low", "low", "medium"),
            ("honor_size_hints", "floating", "floating", "sometimes"),
            ("ignore_ewmh_fullscreen", "enter", "enter", "sideways"),
            ("external_rules_command", "/bin/true", "/bin/true", ""),
            ("mapping_events_count", "3", "3", "300"),
            ("remove_disabled_monitors", "true", "true", "x"),
        ] {
            let (reply, _) = run(&mut wm, &mut registry, &mut adapter, &format!("config {key} {good}"));
            assert_eq!(reply, Reply::Ok(String::new()), "{key}");
            let (reply, _) = run(&mut wm, &mut registry, &mut adapter, &format!("config {key}"));
            assert_eq!(reply, Reply::Ok(format!("{shown}\n")), "{key}");
            if !bad.is_empty() {
                let (reply, _) = run(&mut wm, &mut registry, &mut adapter, &format!("config {key} {bad}"));
                assert!(matches!(reply, Reply::Fail(_)), "{key} {bad}");
            }
        }
        run(&mut wm, &mut registry, &mut adapter, "config ignore_ewmh_fullscreen enter,exit");
        assert!(wm.settings.ignore_ewmh_fullscreen.enter && wm.settings.ignore_ewmh_fullscreen.exit);
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "config ignore_ewmh_fullscreen none");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert!(!wm.settings.ignore_ewmh_fullscreen.enter);
    }

    #[test]
    fn a_rule_naming_a_desktop_resolves_to_that_desktop_and_its_focus() {
        // bspwm: `manage_window()`'s `desktop_desc` block.
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "rule -a Firefox desktop=II");
        let csq = bsp_core::rules::match_rules(&mut wm.rules, "Firefox", "Navigator", "");
        assert_eq!(csq.desktop_desc.as_deref(), Some("II"));
        let ctx = ExecCtx { wm: &mut wm, registry: &mut registry, adapter: &mut adapter };
        let target = resolve_rule_target(&ctx, &csq).expect("desktop II exists");
        assert_eq!((target.monitor, target.desktop), (1, 0));
        // A rule naming nothing, or something that is not there, leaves the window alone.
        assert_eq!(resolve_rule_target(&ctx, &bsp_core::rules::RuleConsequence::default()), None);
        let missing = bsp_core::rules::RuleConsequence { desktop_desc: Some("^9".to_string()), ..Default::default() };
        assert_eq!(resolve_rule_target(&ctx, &missing), None);
    }

    #[test]
    fn a_rule_naming_a_monitor_lands_on_its_shown_desktop() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let csq = bsp_core::rules::RuleConsequence { monitor_desc: Some("HDMI-A-1".to_string()), ..Default::default() };
        let ctx = ExecCtx { wm: &mut wm, registry: &mut registry, adapter: &mut adapter };
        let target = resolve_rule_target(&ctx, &csq).unwrap();
        assert_eq!((target.monitor, target.desktop), (1, 0));
        let sticky = bsp_core::rules::RuleConsequence { sticky: Some(true), ..csq };
        assert_eq!(resolve_rule_target(&ctx, &sticky), None, "a sticky window ignores placement");
    }

    #[test]
    fn config_border_colors_are_validated_and_round_trip() {
        // bspwm: `cmd_config()`'s `SET_COLOR`/`GET_COLOR`, `is_hex_color()`.
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "config focused_border_color");
        assert_eq!(reply, Reply::Ok("#817f7f\n".to_string()));
        for name in ["normal_border_color", "active_border_color", "focused_border_color", "presel_feedback_color"] {
            let (reply, _) = run(&mut wm, &mut registry, &mut adapter, &format!("config {name} #93A1a1"));
            assert_eq!(reply, Reply::Ok(String::new()), "{name}");
            let (reply, _) = run(&mut wm, &mut registry, &mut adapter, &format!("config {name}"));
            assert_eq!(reply, Reply::Ok("#93A1a1\n".to_string()), "{name}");
            let (reply, _) = run(&mut wm, &mut registry, &mut adapter, &format!("config {name} 93a1a1"));
            assert!(matches!(reply, Reply::Fail(_)), "{name}");
        }
        assert_eq!(wm.settings.normal_border_color, "#93A1a1");
    }

    #[test]
    fn status_report_starts_with_the_configurable_prefix() {
        // bspwm: `STATUS_PREFIX "W"`, `print_report()`, `cmd_config()`'s `SET_STR(status_prefix)`.
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "wm -g");
        assert!(matches!(&reply, Reply::Ok(l) if l.starts_with('W')), "{reply:?}");
        run(&mut wm, &mut registry, &mut adapter, "config status_prefix X");
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "config status_prefix");
        assert_eq!(reply, Reply::Ok("X\n".to_string()));
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "wm -g");
        assert!(matches!(&reply, Reply::Ok(l) if l.starts_with('X')), "{reply:?}");
    }

    #[test]
    fn desktop_focus_keeps_the_desktops_focused_node() {
        // bspwm: `cmd_desktop()`'s `-f` is `focus_node(m, d, d->focus)`; it used
        // to clear the node focus, so coming back to a desktop focused its first window.
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "node -f east");
        let before = wm.monitors[0].desktops[0].tree.focus;
        assert!(before.is_some());
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "desktop -f focused");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(wm.monitors[0].desktops[0].tree.focus, before);
    }

    #[test]
    fn focus_history_drives_last_older_newer_and_newest() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let focus = |wm: &Wm| wm.monitors[0].desktops[0].tree.focus;
        let left = focus(&wm);
        run(&mut wm, &mut registry, &mut adapter, "node -f east");
        let right = focus(&wm);
        assert_ne!(left, right);

        // `last` goes back to the previously focused node, and again.
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "node -f last");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(focus(&wm), left);
        run(&mut wm, &mut registry, &mut adapter, "node -f last");
        assert_eq!(focus(&wm), right);

        // `newest` is the most recently focused node, `older` the one before it.
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "query -N -n newest");
        assert!(matches!(reply, Reply::Ok(ref s) if !s.is_empty()), "{reply:?}");

        // desktops: focus the other monitor's desktop, then go `last` back
        run(&mut wm, &mut registry, &mut adapter, "monitor -f HDMI-A-1");
        assert_eq!(wm.focused_monitor, Some(1));
        run(&mut wm, &mut registry, &mut adapter, "monitor -f last");
        assert_eq!(wm.focused_monitor, Some(0));
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "wm -d");
        assert!(matches!(&reply, Reply::Ok(s) if s.contains("\"focusHistory\":[{")), "{reply:?}");
    }

    #[test]
    fn wm_record_history_off_stops_recording() {
        let (mut wm, mut registry, mut adapter) = fixture();
        run(&mut wm, &mut registry, &mut adapter, "wm -h off");
        assert!(!wm.history.record);
        run(&mut wm, &mut registry, &mut adapter, "node -f east");
        assert!(wm.history.locations().all(|l| l.node != wm.monitors[0].desktops[0].tree.focus.and_then(|f| wm.monitors[0].desktops[0].tree.node(f).client.as_ref().map(|c| c.window))));
        run(&mut wm, &mut registry, &mut adapter, "wm -h on");
        assert!(wm.history.record);
    }

    #[test]
    fn config_unknown_setting_fails() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "config bogus_setting");
        assert!(matches!(reply, Reply::Fail(_)));
    }
}
