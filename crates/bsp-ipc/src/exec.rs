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
use crate::selector::resolve_impl::{self, Coordinates};
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
        Command::Subscribe { .. } | Command::Quit(_) => {
            unreachable!("Subscribe/Quit are handled by the server, not exec::execute")
        }
    }
}

// ---- shared helpers --------------------------------------------------

fn resolve_ctx<'a, A: Adapter>(ctx: &'a ExecCtx<A>) -> resolve_impl::Ctx<'a> {
    resolve_impl::Ctx {
        wm: ctx.wm,
        registry: ctx.registry,
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
fn resolve_err_reply(err: ResolveError) -> Reply {
    match err {
        ResolveError::NoMatch => Reply::Fail(String::new()),
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
            Err(e) => return (resolve_err_reply(e), Vec::new()),
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
                        fail = Some(resolve_err_reply(e).into_message());
                        break 'actions;
                    }
                };
                do_focus(ctx, dst, &mut events);
            }
            NodeAction::Activate(sel) => {
                let dst = match resolve_or_default(ctx, reference, sel.as_ref(), trg) {
                    Ok(c) => c,
                    Err(e) => {
                        fail = Some(resolve_err_reply(e).into_message());
                        break 'actions;
                    }
                };
                do_activate(ctx, dst, &mut events);
            }
            NodeAction::ToDesktop(sel, follow) => {
                let dst = match resolve_desktop(ctx, reference, sel) {
                    Ok(c) => c,
                    Err(e) => {
                        fail = Some(resolve_err_reply(e).into_message());
                        break 'actions;
                    }
                };
                let anchor = tree(ctx.wm, dst).focus;
                match do_transfer(ctx, trg, dst, anchor, *follow, &mut events) {
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
                        fail = Some(resolve_err_reply(e).into_message());
                        break 'actions;
                    }
                };
                let dst = Coordinates {
                    monitor: m.monitor,
                    desktop: ctx.wm.monitors[m.monitor].focused.unwrap_or(0),
                    node: None,
                };
                let anchor = tree(ctx.wm, dst).focus;
                match do_transfer(ctx, trg, dst, anchor, *follow, &mut events) {
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
                        fail = Some(resolve_err_reply(e).into_message());
                        break 'actions;
                    }
                };
                let anchor = dst.node;
                match do_transfer(ctx, trg, dst, anchor, *follow, &mut events) {
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
                        fail = Some(resolve_err_reply(e).into_message());
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
                    PreselDirArg::Cancel => {
                        tree_mut(ctx.wm, trg).cancel_presel(n);
                        events.push(Event::NodePresel {
                            monitor: monitor_wire_id(ctx.wm, trg.monitor),
                            desktop: desktop_id(ctx.wm, trg).0,
                            node: wid(ctx, trg),
                            detail: PreselDetail::Cancel,
                        });
                    }
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
                let geometry = tree(ctx.wm, trg)
                    .node(n)
                    .client
                    .as_ref()
                    .unwrap()
                    .floating_rectangle;
                events.push(Event::NodeGeometry {
                    monitor: monitor_wire_id(ctx.wm, trg.monitor),
                    desktop: desktop_id(ctx.wm, trg).0,
                    node: wid(ctx, trg),
                    geometry,
                });
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
                if tree(ctx.wm, trg).node(n).client.as_ref().unwrap().state
                    == bsp_core::node::ClientState::Floating
                {
                    let geometry = tree(ctx.wm, trg)
                        .node(n)
                        .client
                        .as_ref()
                        .unwrap()
                        .floating_rectangle;
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
                tree_mut(ctx.wm, trg).circulate_leaves(&settings, Some(n), *dir);
                changed = true;
            }
            NodeAction::InsertReceptacle => {
                let settings = ctx.wm.settings.clone();
                let t = tree_mut(ctx.wm, trg);
                let r = t.new_node(&settings);
                t.insert_node(&settings, r, trg.node);
                ctx.registry.register(desktop_id(ctx.wm, trg), r);
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
                if !tree_mut(ctx.wm, trg).set_state(n, target) {
                    fail = Some(String::new());
                    break 'actions;
                }
                events.push(Event::NodeState {
                    monitor: monitor_wire_id(ctx.wm, trg.monitor),
                    desktop: desktop_id(ctx.wm, trg).0,
                    node: wid(ctx, trg),
                    state: target,
                    on: true,
                });
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
                let flag_name = match key {
                    NodeFlagKey::Hidden => {
                        tree_mut(ctx.wm, trg).set_hidden(n, value);
                        changed = true;
                        "hidden"
                    }
                    NodeFlagKey::Sticky => {
                        tree_mut(ctx.wm, trg).set_sticky(n, value);
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
                    flag: flag_name,
                    on: value,
                });
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
            }
            NodeAction::Close => {
                let Some(n) = trg.node else {
                    fail = Some(String::new());
                    break 'actions;
                };
                let t = tree(ctx.wm, trg);
                if t.locked_count(Some(n)) > 0 {
                    fail = Some(String::new());
                    break 'actions;
                }
                let Some(client) = t.node(n).client.clone() else {
                    fail = Some(String::new());
                    break 'actions;
                };
                ctx.adapter.close_window(client.window);
                break 'actions;
            }
            NodeAction::Kill => {
                let Some(n) = trg.node else {
                    fail = Some(String::new());
                    break 'actions;
                };
                let Some(client) = tree(ctx.wm, trg).node(n).client.clone() else {
                    fail = Some(String::new());
                    break 'actions;
                };
                ctx.adapter.kill_window(client.window);
                let d = desktop_id(ctx.wm, trg);
                let settings = ctx.wm.settings.clone();
                tree_mut(ctx.wm, trg).remove_node(&settings, n);
                ctx.registry.unregister(d, n);
                events.push(Event::NodeRemove {
                    monitor: monitor_wire_id(ctx.wm, trg.monitor),
                    desktop: d.0,
                    node: 0,
                });
                changed = true;
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

fn do_focus<A: Adapter>(ctx: &mut ExecCtx<A>, dst: Coordinates, events: &mut Vec<Event>) {
    do_activate(ctx, dst, events);
    let monitor_changed = ctx.wm.focused_monitor != Some(dst.monitor);
    // The effectively-focused desktop changes either because a different
    // monitor is now focused, or because the same monitor's focused
    // desktop index changes — checking only the latter would miss a
    // focus switch that lands on a monitor whose own focused desktop
    // happens to already be `dst.desktop` (e.g. its only desktop).
    let desktop_changed =
        monitor_changed || ctx.wm.monitors[dst.monitor].focused != Some(dst.desktop);
    ctx.wm.focused_monitor = Some(dst.monitor);
    ctx.wm.monitors[dst.monitor].focused = Some(dst.desktop);
    if monitor_changed {
        events.push(Event::MonitorFocus {
            id: monitor_wire_id(ctx.wm, dst.monitor),
        });
    }
    if desktop_changed {
        events.push(Event::DesktopFocus {
            monitor: monitor_wire_id(ctx.wm, dst.monitor),
            desktop: desktop_id(ctx.wm, dst).0,
        });
    }
    events.push(Event::NodeFocus {
        monitor: monitor_wire_id(ctx.wm, dst.monitor),
        desktop: desktop_id(ctx.wm, dst).0,
        node: wid(ctx, dst),
    });
}

fn do_activate<A: Adapter>(ctx: &mut ExecCtx<A>, dst: Coordinates, events: &mut Vec<Event>) {
    tree_mut(ctx.wm, dst).focus = dst.node;
    events.push(Event::NodeActivate {
        monitor: monitor_wire_id(ctx.wm, dst.monitor),
        desktop: desktop_id(ctx.wm, dst).0,
        node: wid(ctx, dst),
    });
}

/// `-d`/`-m`/`-n`: transfers `src`'s node to `dst`, next to `anchor`.
/// Returns the new target coordinate (`src.monitor`/`.desktop` updated to
/// `dst`'s, per bspwm's `cmd_node()`).
fn do_transfer<A: Adapter>(
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
    let settings = ctx.wm.settings.clone();
    let src_desktop = desktop_id(ctx.wm, src);
    let dst_desktop = desktop_id(ctx.wm, dst);
    let was_focused = tree(ctx.wm, src).focus == Some(n);

    let new_node = if src.desktop == dst.desktop && src.monitor == dst.monitor {
        if !tree_mut(ctx.wm, src).transplant_within(&settings, n, anchor) {
            return Err(String::new());
        }
        n
    } else {
        let (src_tree, dst_tree) = two_trees_mut(
            ctx.wm,
            (src.monitor, src.desktop),
            (dst.monitor, dst.desktop),
        );
        let new_node = src_tree.transplant_to(&settings, n, dst_tree, anchor);
        ctx.registry
            .relocate((src_desktop, n), (dst_desktop, new_node));
        new_node
    };

    let new_trg = Coordinates {
        monitor: dst.monitor,
        desktop: dst.desktop,
        node: Some(new_node),
    };
    if follow || was_focused {
        do_focus(ctx, new_trg, events);
    }
    events.push(Event::NodeTransfer {
        src_monitor: monitor_wire_id(ctx.wm, src.monitor),
        src_desktop: src_desktop.0,
        src_node: ctx.registry.id_of(dst_desktop, new_node).unwrap_or(0),
        dst_monitor: monitor_wire_id(ctx.wm, dst.monitor),
        dst_desktop: dst_desktop.0,
        dst_node: ctx.registry.id_of(dst_desktop, new_node).unwrap_or(0),
    });
    Ok(new_trg)
}

/// `-s`/`--swap`. Same-tree swaps use `Tree::swap_nodes` directly;
/// cross-desktop/monitor swaps are not implemented yet (see
/// `docs/bsp-ipc.md`, scope — bspwm's cross-tree swap relies on
/// node identity surviving a tree change, which `bsp-core`'s per-tree
/// `NodeId` does not, and reproducing it via two transplants would need
/// `insert_node`'s exact-slot-replacement semantics, not its anchor-based
/// splitting).
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
        return Err(
            "node -s: swapping across desktops/monitors is not implemented yet.\n".to_string(),
        );
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
        do_focus(ctx, src, events);
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
            Err(e) => return (resolve_err_reply(e), Vec::new()),
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
                            fail = Some(resolve_err_reply(e).into_message());
                            break 'actions;
                        }
                    },
                    None => trg,
                };
                do_focus(ctx, dst, &mut events);
            }
            DesktopAction::Activate(sel) => {
                let dst = match sel {
                    Some(s) => match resolve_desktop(ctx, reference, s) {
                        Ok(c) => c,
                        Err(e) => {
                            fail = Some(resolve_err_reply(e).into_message());
                            break 'actions;
                        }
                    },
                    None => trg,
                };
                if !ctx.wm.monitors[dst.monitor].activate_desktop(dst.desktop) {
                    fail = Some(String::new());
                    break 'actions;
                }
                events.push(Event::DesktopActivate {
                    monitor: monitor_wire_id(ctx.wm, dst.monitor),
                    desktop: desktop_id(ctx.wm, dst).0,
                });
            }
            DesktopAction::ToMonitor(sel, follow) => {
                if ctx.wm.monitors[trg.monitor].desktops.len() <= 1 {
                    fail = Some(String::new());
                    break 'actions;
                }
                let dst_mon = match resolve_monitor(ctx, reference, sel) {
                    Ok(c) => c.monitor,
                    Err(e) => {
                        fail = Some(resolve_err_reply(e).into_message());
                        break 'actions;
                    }
                };
                let src_id = desktop_id(ctx.wm, trg);
                let d = ctx.wm.monitors[trg.monitor].remove_desktop(trg.desktop);
                ctx.wm.monitors[dst_mon].add_desktop(d);
                let new_index = ctx.wm.monitors[dst_mon].desktops.len() - 1;
                let new_trg = Coordinates {
                    monitor: dst_mon,
                    desktop: new_index,
                    node: None,
                };
                events.push(Event::DesktopTransfer {
                    src_monitor: monitor_wire_id(ctx.wm, trg.monitor),
                    src_desktop: src_id.0,
                    dst_monitor: monitor_wire_id(ctx.wm, dst_mon),
                });
                trg = new_trg;
                if *follow {
                    ctx.wm.focused_monitor = Some(dst_mon);
                    ctx.wm.monitors[dst_mon].focused = Some(new_index);
                }
            }
            DesktopAction::Swap(sel, follow) => {
                let dst = match resolve_desktop(ctx, reference, sel) {
                    Ok(c) => c,
                    Err(e) => {
                        fail = Some(resolve_err_reply(e).into_message());
                        break 'actions;
                    }
                };
                if trg.monitor == dst.monitor {
                    ctx.wm.monitors[trg.monitor].swap_desktops(trg.desktop, dst.desktop);
                } else {
                    let a = ctx.wm.monitors[trg.monitor].remove_desktop(trg.desktop);
                    let b = ctx.wm.monitors[dst.monitor].remove_desktop(dst.desktop);
                    ctx.wm.monitors[trg.monitor].desktops.insert(trg.desktop, b);
                    ctx.wm.monitors[dst.monitor].desktops.insert(dst.desktop, a);
                }
                events.push(Event::DesktopSwap {
                    src_monitor: monitor_wire_id(ctx.wm, trg.monitor),
                    src_desktop: desktop_id(ctx.wm, dst).0,
                    dst_monitor: monitor_wire_id(ctx.wm, dst.monitor),
                    dst_desktop: desktop_id(ctx.wm, trg).0,
                });
                let new_trg = Coordinates {
                    monitor: dst.monitor,
                    desktop: trg.desktop,
                    node: None,
                };
                trg = Coordinates {
                    monitor: dst.monitor,
                    ..trg
                };
                let _ = new_trg;
                if *follow {
                    ctx.wm.focused_monitor = Some(trg.monitor);
                }
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
            DesktopAction::Rename(name) => {
                let old_name = ctx.wm.monitors[trg.monitor].desktops[trg.desktop]
                    .name
                    .clone();
                ctx.wm.monitors[trg.monitor].desktops[trg.desktop].rename(name);
                events.push(Event::DesktopRename {
                    monitor: monitor_wire_id(ctx.wm, trg.monitor),
                    desktop: desktop_id(ctx.wm, trg).0,
                    old_name,
                    new_name: name.clone(),
                });
            }
            DesktopAction::Bubble(cyc) => {
                let len = ctx.wm.monitors[trg.monitor].desktops.len();
                if len > 1 {
                    let target = match cyc {
                        crate::value::CycleDir::Next => (trg.desktop + 1) % len,
                        crate::value::CycleDir::Prev => (trg.desktop + len - 1) % len,
                    };
                    ctx.wm.monitors[trg.monitor].swap_desktops(trg.desktop, target);
                    trg.desktop = target;
                }
            }
            DesktopAction::Remove => {
                let d = &ctx.wm.monitors[trg.monitor].desktops[trg.desktop];
                if ctx.wm.monitors[trg.monitor].desktops.len() <= 1 || d.tree.root.is_some() {
                    fail = Some(String::new());
                    break 'actions;
                }
                let removed_id = desktop_id(ctx.wm, trg);
                ctx.wm.monitors[trg.monitor].remove_desktop(trg.desktop);
                events.push(Event::DesktopRemove {
                    monitor: monitor_wire_id(ctx.wm, trg.monitor),
                    desktop: removed_id.0,
                });
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
            Err(e) => return (resolve_err_reply(e), Vec::new()),
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
                            fail = Some(resolve_err_reply(e).into_message());
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
                do_focus(ctx, dst, &mut events);
            }
            MonitorAction::Swap(sel) => {
                let dst_monitor = match resolve_monitor(ctx, reference, sel) {
                    Ok(c) => c.monitor,
                    Err(e) => {
                        fail = Some(resolve_err_reply(e).into_message());
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
                let settings = ctx.wm.settings.clone();
                for name in names {
                    let id = bsp_core::id::DesktopId(next_desktop_id(ctx.wm));
                    ctx.wm.monitors[trg_monitor].add_desktop(Desktop::new(
                        id,
                        Some(name),
                        &settings,
                    ));
                    events.push(Event::DesktopAdd {
                        monitor: monitor_wire_id(ctx.wm, trg_monitor),
                        desktop: id.0,
                        name: name.clone(),
                    });
                }
            }
            MonitorAction::ReorderDesktops(names) => {
                let mut order = Vec::new();
                for name in names {
                    if let Some(idx) = ctx.wm.monitors[trg_monitor]
                        .desktops
                        .iter()
                        .position(|d| &d.name == name)
                    {
                        order.push(idx);
                    }
                }
                let desktops = &mut ctx.wm.monitors[trg_monitor].desktops;
                let mut reordered: Vec<Desktop> =
                    order.iter().map(|&i| desktops[i].clone()).collect();
                let mut rest: Vec<Desktop> = desktops
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !order.contains(i))
                    .map(|(_, d)| d.clone())
                    .collect();
                reordered.append(&mut rest);
                *desktops = reordered;
            }
            MonitorAction::ResetDesktops(names) => {
                let settings = ctx.wm.settings.clone();
                let existing = ctx.wm.monitors[trg_monitor].desktops.len();
                for (i, name) in names.iter().enumerate() {
                    if i < existing {
                        ctx.wm.monitors[trg_monitor].desktops[i].rename(name);
                    } else {
                        let id = bsp_core::id::DesktopId(next_desktop_id(ctx.wm));
                        ctx.wm.monitors[trg_monitor].add_desktop(Desktop::new(
                            id,
                            Some(name),
                            &settings,
                        ));
                    }
                }
                while ctx.wm.monitors[trg_monitor].desktops.len() > names.len().max(1) {
                    let last = ctx.wm.monitors[trg_monitor].desktops.len() - 1;
                    if ctx.wm.monitors[trg_monitor].desktops[last]
                        .tree
                        .root
                        .is_some()
                    {
                        break;
                    }
                    ctx.wm.monitors[trg_monitor].remove_desktop(last);
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
                let id = monitor_wire_id(ctx.wm, trg_monitor);
                ctx.wm.remove_monitor(trg_monitor);
                events.push(Event::MonitorRemove { id });
                break 'actions;
            }
            MonitorAction::SetRectangle(r) => {
                trg_monitor = set_monitor_rectangle(ctx, trg_monitor, *r, &mut events);
            }
            MonitorAction::Rename(name) => {
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

fn next_desktop_id(wm: &Wm) -> u32 {
    wm.monitors
        .iter()
        .flat_map(|m| m.desktops.iter().map(|d| d.id.0))
        .max()
        .unwrap_or(0)
        + 1
}

fn next_monitor_id(wm: &Wm) -> u32 {
    wm.monitors.iter().map(|m| m.id.0).max().unwrap_or(0) + 1
}

// ======================= query =======================

fn exec_query<A: Adapter>(ctx: &mut ExecCtx<A>, q: &QueryCommand) -> Reply {
    let Some(reference) = focused_coords(ctx) else {
        return Reply::Fail("query: No focused location.\n".to_string());
    };

    let monitor_ref = match &q.monitor {
        Some(sel) => match resolve_monitor(ctx, reference, sel) {
            Ok(c) => c,
            Err(e) => return resolve_err_reply(e),
        },
        None => reference,
    };
    let desktop_ref = match &q.desktop {
        Some(sel) => match resolve_desktop(ctx, monitor_ref, sel) {
            Ok(c) => c,
            Err(e) => return resolve_err_reply(e),
        },
        None => monitor_ref,
    };
    let node_ref = match &q.node {
        Some(sel) => match resolve_node(ctx, desktop_ref, sel) {
            Ok(c) => c,
            Err(e) => return resolve_err_reply(e),
        },
        None => desktop_ref,
    };

    match q.domain {
        QueryDomain::Nodes => {
            let mut out = String::new();
            let rctx = resolve_ctx(ctx);
            for loc in rctx.all_desktops() {
                if q.monitor.is_some() && loc.monitor != monitor_ref.monitor {
                    continue;
                }
                if q.desktop.is_some()
                    && (loc.monitor, loc.desktop) != (desktop_ref.monitor, desktop_ref.desktop)
                {
                    continue;
                }
                let t = rctx.tree(loc);
                let mut n = t.first_extrema(t.root);
                while let Some(id) = n {
                    let c = Coordinates {
                        node: Some(id),
                        ..loc
                    };
                    out.push_str(&format!("0x{:08X}\n", wid(ctx, c)));
                    n = t.next_leaf(Some(id), t.root);
                }
            }
            if out.is_empty() {
                Reply::Fail(String::new())
            } else {
                Reply::Ok(out)
            }
        }
        QueryDomain::Desktops => {
            let mut out = String::new();
            for (mi, m) in ctx.wm.monitors.iter().enumerate() {
                if q.monitor.is_some() && mi != monitor_ref.monitor {
                    continue;
                }
                for d in &m.desktops {
                    if q.names {
                        out.push_str(&d.name);
                    } else {
                        out.push_str(&format!("0x{:08X}", d.id.0));
                    }
                    out.push('\n');
                }
            }
            if out.is_empty() {
                Reply::Fail(String::new())
            } else {
                Reply::Ok(out)
            }
        }
        QueryDomain::Monitors => {
            let mut out = String::new();
            for m in &ctx.wm.monitors {
                if q.names {
                    out.push_str(&m.name);
                } else {
                    out.push_str(&format!("0x{:08X}", m.id.0));
                }
                out.push('\n');
            }
            if out.is_empty() {
                Reply::Fail(String::new())
            } else {
                Reply::Ok(out)
            }
        }
        QueryDomain::Tree => {
            let node_id =
                |d: DesktopId, n: NodeId| -> u32 { ctx.registry.id_of(d, n).unwrap_or(0) };
            let adapter = &*ctx.adapter;
            let names = |w: WindowId| adapter.window_class(w);
            let json = if node_ref.node.is_some() || q.node.is_some() {
                serde_json::to_string(&JsonNode::from_tree(
                    tree(ctx.wm, node_ref),
                    desktop_id(ctx.wm, node_ref),
                    node_ref.node.unwrap(),
                    &node_id,
                    &names,
                ))
            } else if desktop_ref.desktop != usize::MAX && q.desktop.is_some() {
                serde_json::to_string(&JsonDesktop::from_desktop(
                    &ctx.wm.monitors[desktop_ref.monitor].desktops[desktop_ref.desktop],
                    &node_id,
                    &names,
                ))
            } else {
                serde_json::to_string(&JsonMonitor::from_monitor(
                    &ctx.wm.monitors[monitor_ref.monitor],
                    &node_id,
                    &names,
                ))
            };
            match json {
                Ok(s) => Reply::Ok(format!("{s}\n")),
                Err(_) => Reply::Fail(String::new()),
            }
        }
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
                            ctx.wm.rules.retain(|r| {
                                let full =
                                    format!("{}:{}:{}", r.class_name, r.instance_name, r.name);
                                &full != cause && &r.class_name != cause
                            });
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
                let state = JsonState::new(ctx.wm, clients_count, &node_id, &names);
                match serde_json::to_string(&state) {
                    Ok(s) => return (Reply::Ok(format!("{s}\n")), events),
                    Err(_) => {
                        fail = Some(String::new());
                        break 'actions;
                    }
                }
            }
            WmAction::LoadState(_) => {
                fail = Some("wm -l: not implemented yet.\n".to_string());
                break 'actions;
            }
            WmAction::AddMonitor(name, rect) => {
                let id = bsp_core::id::MonitorId(next_monitor_id(ctx.wm));
                let settings = ctx.wm.settings.clone();
                let mut m = Monitor::new(id, Some(name), *rect, &settings);
                let did = bsp_core::id::DesktopId(next_desktop_id(ctx.wm));
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
            WmAction::RecordHistory(_) => {
                // No-op: no focus history is tracked yet (`docs/bsp-ipc.md`,
                // scope). Accepted rather than rejected, since it
                // has no wrong effect to produce.
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
        prefix: String::new(),
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
                Err(e) => return resolve_err_reply(e),
            }
        }
        ConfigTarget::Desktop(sel) => {
            let Some(reference) = focused_coords(ctx) else {
                return Reply::Fail(String::new());
            };
            match resolve_desktop(ctx, reference, sel) {
                Ok(c) => Some(c),
                Err(e) => return resolve_err_reply(e),
            }
        }
        ConfigTarget::Node(sel) => {
            let Some(reference) = focused_coords(ctx) else {
                return Reply::Fail(String::new());
            };
            match resolve_node(ctx, reference, sel) {
                Ok(c) => Some(c),
                Err(e) => return resolve_err_reply(e),
            }
        }
    };

    match &c.value {
        Some(value) => set_setting(ctx, target, &c.name, value),
        None => get_setting(ctx, target, &c.name),
    }
}

// ======================= output/input =======================

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
    name: &str,
    value: &str,
) -> Reply {
    match name {
        "window_gap" => {
            let Ok(v) = value.parse::<i32>() else {
                return Reply::Fail(String::new());
            };
            match target {
                Some(c) => ctx.wm.monitors[c.monitor].desktops[c.desktop].window_gap = v,
                None => {
                    ctx.wm.settings.window_gap = v;
                    for m in &mut ctx.wm.monitors {
                        for d in &mut m.desktops {
                            d.window_gap = v;
                        }
                    }
                }
            }
        }
        "border_width" => {
            let Ok(v) = value.parse::<i32>() else {
                return Reply::Fail(String::new());
            };
            ctx.wm.settings.border_width = v;
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
            ctx.wm.settings.single_monocle = match crate::value::parse_bool(value) {
                Some(b) => b,
                None => return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n")),
            };
        }
        "center_pseudo_tiled" => {
            ctx.wm.settings.center_pseudo_tiled = match crate::value::parse_bool(value) {
                Some(b) => b,
                None => return Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n")),
            };
        }
        "top_padding" | "right_padding" | "bottom_padding" | "left_padding" => {
            let Ok(v) = value.parse::<i32>() else {
                return Reply::Fail(String::new());
            };
            let apply = |p: &mut bsp_core::geometry::Padding| match name {
                "top_padding" => p.top = v,
                "right_padding" => p.right = v,
                "bottom_padding" => p.bottom = v,
                _ => p.left = v,
            };
            match target {
                Some(c) if c.node.is_none() => {
                    apply(&mut ctx.wm.monitors[c.monitor].desktops[c.desktop].padding)
                }
                None => {
                    apply(&mut ctx.wm.settings.padding);
                    for m in &mut ctx.wm.monitors {
                        for d in &mut m.desktops {
                            apply(&mut d.padding);
                        }
                    }
                }
                _ => {}
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

fn get_setting<A: Adapter>(ctx: &ExecCtx<A>, target: Option<Coordinates>, name: &str) -> Reply {
    let s = &ctx.wm.settings;
    let out = match name {
        "split_ratio" => format!("{}", s.split_ratio),
        "window_gap" => match target {
            Some(c) => format!(
                "{}",
                ctx.wm.monitors[c.monitor].desktops[c.desktop].window_gap
            ),
            None => format!("{}", s.window_gap),
        },
        "border_width" => format!("{}", s.border_width),
        "top_padding" => padding_get(ctx, target, |p| p.top),
        "right_padding" => padding_get(ctx, target, |p| p.right),
        "bottom_padding" => padding_get(ctx, target, |p| p.bottom),
        "left_padding" => padding_get(ctx, target, |p| p.left),
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
    get: impl Fn(&bsp_core::geometry::Padding) -> i32,
) -> String {
    match target {
        Some(c) if c.node.is_none() => {
            format!(
                "{}",
                get(&ctx.wm.monitors[c.monitor].desktops[c.desktop].padding)
            )
        }
        _ => format!("{}", get(&ctx.wm.settings.padding)),
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
        // A tiled resize re-arranges instead of reporting its own
        // `node_geometry` event (bspwm: `resize_client()` only does
        // that for `STATE_FLOATING`).
        assert!(!events
            .iter()
            .any(|e| matches!(e, Event::NodeGeometry { .. })));
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
    fn node_close_calls_adapter_and_kill_removes_node() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "node -c");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(adapter.closed, vec![WindowId(1)]);

        let (reply, events) = run(&mut wm, &mut registry, &mut adapter, "node -k");
        assert_eq!(reply, Reply::Ok(String::new()));
        assert_eq!(adapter.killed, vec![WindowId(1)]);
        assert!(events.iter().any(|e| e.kind() == EventKind::NodeRemove));
        assert!(wm.monitors[0].desktops[0].tree.root.is_some()); // right window remains
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
        // backend overrides it.
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
                assert_eq!(s.lines().count(), 2);
                assert!(s.contains("0x00000001"));
                assert!(s.contains("0x00000002"));
            }
            other => panic!("expected Ok, got {other:?}"),
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
        // (`docs/bsp-compositor.md` Hotkeys progress) — this only checks
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
    fn config_unknown_setting_fails() {
        let (mut wm, mut registry, mut adapter) = fixture();
        let (reply, _) = run(&mut wm, &mut registry, &mut adapter, "config bogus_setting");
        assert!(matches!(reply, Reply::Fail(_)));
    }
}
