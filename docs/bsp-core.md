# bsp-core

The pure-logic crate: it owns every piece of bspwm state and behavior and has no Wayland or Smithay dependency.

## Modules

| Module | Holds |
| --- | --- |
| `geometry` | `Rect`, `Padding` |
| `id` | `NodeId`, `WindowId`, `DesktopId`, `MonitorId`, `IdGen` |
| `node` | Client data: `WindowId`, state (tiled, pseudo_tiled, floating, fullscreen), layer, urgent flag |
| `tree` | Arena of nodes (`NodeId(u32)`), split type and ratio, first/second child, presel, receptacles; every tree operation; `apply_layout` |
| `desktop` | Desktop name, layout (tiled or monocle), padding, window gap, the tree |
| `monitor` | Monitor name, rectangle, padding, panel `struts` (compositor-owned reserved space, added to padding in `arrange`), desktop list, focused desktop, `arrange`, `adapt_geometry` |
| `rules` | `bspc rule` entries matched on class, instance and name; `RuleConsequence` |
| `settings` | Every `bspc config` key the tree engine reads, with bspwm's defaults, including the four `*_color` settings |
| `stack` | The stacking order of every managed window (`StackingList`, `StackMove`), a port of bspwm's `stack.c` (`stack()`, `limit_above()`, `limit_below()`); a window never leaves its level, `Client::stack_level` |
| `history` | Focus history: `History`, `Loc`, `Dir`; a port of bspwm's `history.c` list operations |
| `wm` | `Wm`: every monitor, the focused one, the global rule list and settings — bspwm's `mon_head`/`mon_tail`/`mon`/`rule_head` globals collected into one struct, for `bsp-ipc` (IPC) to resolve selectors and run commands against |
| `event` / `effect` (planned) | `Event` enum, `Effect` enum tying `bsp-core` to an adapter — not started; needs the nested compositor's compositor layer to have a shape worth committing to |

## Core scope

Every function below is pure: it takes and mutates plain data and returns plain values, with no I/O. bspwm interleaves these structural operations with side effects that need a real display — drawing borders, EWMH, the input focus, the stacking list, `subscribe` reports, history. Those are left out; each function's doc comment says so and names the bspwm function it mirrors, so nothing was dropped silently. They land once `bsp-compositor` (nested compositor) or `bsp-ipc` (IPC) has an adapter to receive them.

Two structural simplifications, not bspwm behavior differences (so not entered in `docs/design.md`'s Deliberate deviations table — nothing here is user-visible or permanent):

- **`NodeId` is per-`Tree`, not global.** bspwm's `node_t*` keeps its identity when a node moves to a different desktop's tree (`transfer_node`, `swap_nodes`). Here, a `NodeId` is only meaningful within the arena that issued it (`docs/design.md`: "`bsp-core` uses its own `WindowId(u32)`"), so moving a node to a different `Tree` — `Tree::transplant_to` — clones its subtree into the destination arena under a fresh id and frees the original. `WindowId`, which is what the adapter and `bspc` actually key on, is unaffected.
- **`swap_nodes` is single-tree only.** bspwm's cross-desktop `swap_nodes` relies on the pointer-identity behavior above; a cross-tree swap here is two `transplant_to` calls (move `n1` to `n2`'s old anchor and vice versa) rather than a dedicated method, since with per-tree ids there is no shorter path.
- **Desktop ordering.** `Monitor::desktops` is a plain `Vec` in insertion (or explicit `bspc monitor -o` reorder) order — bspwm's own desktop list has no automatic position-based sort either; only monitors do.
- **Monitor ordering** (the hardware backend, `docs/design.md` roadmap) is implemented: `Rect::compare` (`geometry.rs`) ports bspwm's `src/geometry.c` `rect_cmp()`, and `Wm::add_monitor`/`Wm::reorder_monitor` (`wm.rs`) port `src/monitor.c` `add_monitor()`/`reorder_monitor()` — a monitor lands in on-screen-position order among its neighbors when added, and walks back into position when its rectangle changes (`bsp-ipc`'s `MonitorAction::SetRectangle` calls `reorder_monitor` after applying the new rectangle, matching `update_root()`'s own call site). `add_monitor` reaches the same end state as bspwm's own list-splice insertion through a different mechanism (append, then bubble into place via `reorder_monitor`'s swaps) so it reuses `swap_monitors`' already-correct focus-index bookkeeping instead of duplicating it; the only observable difference is for two monitors sharing the *exact* same rectangle (`Rect::compare` returns `Equal`), where bspwm's insertion reverses arrival order and this does not — not expected to matter for real, distinct monitors. What still needs the hardware backend's real hardware: nothing calls `add_monitor`/`reorder_monitor` on a real hotplug event yet, since there is no udev/DRM output list to hotplug from (`docs/bsp-compositor.md`).

## Public functions

| Function | Signature | Behavior |
| --- | --- | --- |
| `Rect::new` | `fn(x: i32, y: i32, width: i32, height: i32) -> Rect` | Constructs a rectangle. |
| `Rect::area` | `fn(&self) -> i64` | `src/geometry.c` `area()`. |
| `Rect::right` | `fn(&self) -> i32` | x + width. |
| `Rect::bottom` | `fn(&self) -> i32` | y + height. |
| `Rect::compare` | `fn(&self, other: &Rect) -> std::cmp::Ordering` | `src/geometry.c` `rect_cmp()`. |
| `Rect::contains_rect` | `fn(&self, other: &Rect) -> bool` | `src/geometry.c` `contains()`. |
| `Rect::contains_point` | `fn(&self, px: i32, py: i32) -> bool` | `src/geometry.c` `is_inside()`. |
| `WindowId::fmt` (`Display`) | `fn(&self, f) -> fmt::Result` | bspwm's `0x%08X` id format. |
| `IdGen::new` | `fn() -> IdGen` | Starts a monotonic id counter at 1. |
| `IdGen::alloc` | `fn(&mut self) -> u32` | Next id. |
| `ClientState::is_tiled` | `fn(self) -> bool` | `src/helpers.h` `IS_TILED`. |
| `Client::new` | `fn(window: WindowId, border_width: i32) -> Client` | `src/tree.c` `make_client()`. |
| `Node::is_leaf` | `fn(&self) -> bool` | `src/tree.c` `is_leaf()`. |
| `Node::is_receptacle` | `fn(&self) -> bool` | `src/helpers.h` `IS_RECEPTACLE`. |
| `Node::parent`/`first_child`/`second_child` | `fn(&self) -> Option<NodeId>` | Read-only structural accessors. |
| `Tree::new` | `fn() -> Tree` | Empty tree. |
| `Tree::new_node` | `fn(&mut self, settings: &Settings) -> NodeId` | `src/tree.c` `make_node()`. |
| `Tree::new_client_node` | `fn(&mut self, settings: &Settings, client: Client) -> NodeId` | `make_node()` + attaching a client. |
| `Tree::node`/`node_mut` | `fn(&self/&mut self, NodeId) -> &/&mut Node` | Arena access; panics on a stale id. |
| `Tree::contains` | `fn(&self, NodeId) -> bool` | Whether an id still names a live node. |
| `Tree::is_leaf` | `fn(&self, NodeId) -> bool` | `src/tree.c` `is_leaf()`. |
| `Tree::is_receptacle` | `fn(&self, NodeId) -> bool` | `src/helpers.h` `IS_RECEPTACLE`. |
| `Tree::is_first_child`/`is_second_child` | `fn(&self, NodeId) -> bool` | `src/tree.c` `is_first_child()`/`is_second_child()`. |
| `Tree::brother` | `fn(&self, NodeId) -> Option<NodeId>` | `src/tree.c` `brother_tree()`. |
| `Tree::is_child` | `fn(&self, Option<NodeId>, Option<NodeId>) -> bool` | `src/tree.c` `is_child()`. |
| `Tree::is_descendant` | `fn(&self, Option<NodeId>, Option<NodeId>) -> bool` | `src/tree.c` `is_descendant()`. |
| `Tree::first_extrema`/`second_extrema` | `fn(&self, Option<NodeId>) -> Option<NodeId>` | `src/tree.c` `first_extrema()`/`second_extrema()`. |
| `Tree::next_leaf`/`prev_leaf` | `fn(&self, Option<NodeId>, Option<NodeId>) -> Option<NodeId>` | `src/tree.c` `next_leaf()`/`prev_leaf()`. |
| `Tree::next_tiled_leaf`/`prev_tiled_leaf` | `fn(&self, Option<NodeId>, Option<NodeId>) -> Option<NodeId>` | `src/tree.c` `next_tiled_leaf()`/`prev_tiled_leaf()`. |
| `Tree::is_focusable` | `fn(&self, NodeId) -> bool` | `src/tree.c` `is_focusable()`. |
| `Tree::clients_count_in` | `fn(&self, Option<NodeId>) -> u32` | `src/tree.c` `clients_count_in()`. |
| `Tree::tiled_count` | `fn(&self, Option<NodeId>, bool) -> i32` | `src/tree.c` `tiled_count()`. |
| `Tree::sticky_count`/`private_count`/`locked_count` | `fn(&self, Option<NodeId>) -> u32` | `src/tree.c` `DEF_FLAG_COUNT`. |
| `Tree::get_rectangle` | `fn(&self, NodeId, i32, Layout, bool) -> Rect` | `src/tree.c` `get_rectangle()`; the last argument is `gapless_monocle` (the window gap is dropped only when it is set and the layout is monocle). |
| `Tree::node_area` | `fn(&self, NodeId) -> i64` | `src/tree.c` `node_area()` (simplified; see doc comment). |
| `Tree::presel_dir` | `fn(&mut self, NodeId, Direction, f64)` | `src/tree.c` `presel_dir()`. |
| `Tree::presel_ratio` | `fn(&mut self, NodeId, f64, Direction)` | `src/tree.c` `presel_ratio()`. |
| `Tree::cancel_presel` | `fn(&mut self, NodeId)` | `src/tree.c` `cancel_presel()`. |
| `Tree::cancel_presel_in` | `fn(&mut self, Option<NodeId>)` | `src/tree.c` `cancel_presel_in()`. |
| `Tree::set_vacant` | `fn(&mut self, NodeId, bool)` | `src/tree.c` `set_vacant()`. |
| `Tree::set_hidden` | `fn(&mut self, NodeId, bool)` | `src/tree.c` `set_hidden()` (minus refocusing). |
| `Tree::rebuild_constraints_from_leaves` | `fn(&mut self, Option<NodeId>)` | `src/tree.c` `rebuild_constraints_from_leaves()`. |
| `Tree::rebuild_constraints_towards_root` | `fn(&mut self, Option<NodeId>)` | `src/tree.c` `rebuild_constraints_towards_root()`. |
| `Tree::set_sticky`/`set_private`/`set_locked`/`set_marked` | `fn(&mut self, NodeId, bool)` | `src/tree.c` `set_sticky()` etc. (minus monitor bookkeeping/reports). |
| `Tree::set_urgent` | `fn(&mut self, NodeId, bool) -> bool` | `src/tree.c` `set_urgent()`. |
| `Tree::set_layer` | `fn(&mut self, NodeId, Layer) -> bool` | `src/tree.c` `set_layer()` (minus stacking). |
| `Tree::set_floating` | `fn(&mut self, NodeId, bool)` | `src/tree.c` `set_floating()` (minus stacking). |
| `Tree::set_fullscreen` | `fn(&mut self, NodeId, bool)` | `src/tree.c` `set_fullscreen()` (minus stacking/EWMH). |
| `Tree::set_state` | `fn(&mut self, NodeId, ClientState) -> bool` | `src/tree.c` `set_state()` (minus `single_monocle`/reports). |
| `Tree::find_public` | `fn(&self, Option<NodeId>) -> Option<NodeId>` | `src/tree.c` `find_public()`. |
| `Tree::insert_node` (**split**) | `fn(&mut self, &Settings, NodeId, Option<NodeId>) -> Option<NodeId>` | `src/tree.c` `insert_node()` (minus reports). |
| `Tree::unlink_node` | `fn(&mut self, &Settings, NodeId)` | `src/tree.c` `unlink_node()` (minus history/presel windows). |
| `Tree::free_node` | `fn(&mut self, NodeId)` | `src/tree.c` `free_node()`. |
| `Tree::remove_node` | `fn(&mut self, &Settings, NodeId)` | `src/tree.c` `remove_node()` (minus history/stack/EWMH/refocus). |
| `Tree::rotate_tree` (**rotate**) | `fn(&mut self, Option<NodeId>, i32)` | `src/tree.c` `rotate_tree()`/`rotate_tree_rec()`. |
| `Tree::flip_tree` (**flip**) | `fn(&mut self, Option<NodeId>, FlipAxis)` | `src/tree.c` `flip_tree()`. |
| `Tree::equalize_tree` (**equalize**) | `fn(&mut self, Option<NodeId>, &Settings)` | `src/tree.c` `equalize_tree()`. |
| `Tree::balance_tree` (**balance**) | `fn(&mut self, Option<NodeId>) -> i32` | `src/tree.c` `balance_tree()`. |
| `Tree::adjust_ratios` | `fn(&mut self, Option<NodeId>, Rect)` | `src/tree.c` `adjust_ratios()`. |
| `Tree::find_fence` | `fn(&self, NodeId, Direction) -> Option<NodeId>` | `src/tree.c` `find_fence()`. |
| `Tree::get_handle` | `fn(&self, NodeId, (i32, i32), PointerAction) -> ResizeHandle` | `src/pointer.c` `get_handle()`. |
| `Tree::move_floating` (**move**) | `fn(&mut self, NodeId, i32, i32) -> bool` | `src/window.c` `move_client()` (floating-rectangle translation only; see doc comment). |
| `Tree::resize_node` (**resize**) | `fn(&mut self, NodeId, ResizeHandle, i32, i32, bool) -> bool` | `src/window.c` `resize_client()`. |
| `Tree::swap_nodes` (**swap**) | `fn(&mut self, NodeId, NodeId) -> bool` | `src/tree.c` `swap_nodes()`, single-tree (see Core scope). |
| `Tree::transplant_to`/`transplant_within` (**transplant**) | `fn(&mut self, &Settings, NodeId, ..) -> NodeId`/`bool` | `src/tree.c` `transfer_node()` (minus focus/history/EWMH/`single_monocle`). |
| `Tree::circulate_leaves` | `fn(&mut self, &Settings, Option<NodeId>, CirculateDir)` | `src/tree.c` `circulate_leaves()` (minus refocus). |
| `Tree::apply_layout` | `fn(&mut self, Option<NodeId>, Rect, i32, Layout, Rect, LayoutOptions)` | `src/tree.c` `apply_layout()` (geometry only; see doc comment). `LayoutOptions` carries `gapless_monocle`, `borderless_monocle`, `borderless_singleton` (already ANDed with "only monitor") and `center_pseudo_tiled`; the layout stores each client's drawn border in `Client::shown_border_width` (0 for fullscreen, borderless monocle, the singleton) and centres pseudo-tiled windows. A floating client's `tiled_rectangle` is left alone, as in bspwm. |
| `Desktop::new` | `fn(DesktopId, Option<&str>, &Settings) -> Desktop` | `src/desktop.c` `make_desktop()`. |
| `Desktop::rename` | `fn(&mut self, &str)` | `src/desktop.c` `rename_desktop()`. |
| `Desktop::set_layout` | `fn(&mut self, Layout, bool, Layout) -> bool` | `src/desktop.c` `set_layout()`. |
| `Monitor::new` | `fn(MonitorId, Option<&str>, Rect, &Settings) -> Monitor` | `src/monitor.c` `make_monitor()`. |
| `Monitor::rename` | `fn(&mut self, &str)` | `src/monitor.c` `rename_monitor()`. |
| `Monitor::add_desktop` | `fn(&mut self, Desktop)` | `src/desktop.c` `add_desktop()`: the desktop takes the monitor's gap and border width. |
| `Settings` (bspwm settings kept for `bspc config`) | fields | Besides the tree settings: `focus_follows_pointer`, `pointer_follows_focus`, `pointer_follows_monitor` (acted on by the compositor), `directional_focus_tightness` (`Tightness`, read by directional selectors), `ignore_ewmh_fullscreen` (`StateTransition`), `external_rules_command`, and `presel_feedback` and `honor_size_hints` (`HonorSizeHints`, both acted on by the compositor), stored-only, `mapping_events_count`, `remove_disabled_monitors`, `remove_unplugged_monitors`, `merge_overlapping_monitors` |
| `RuleConsequence::{monitor_desc, desktop_desc, node_desc, rect}` | fields | The placement a rule asks for (selector text, resolved when the window is managed) and its `rectangle=`; merged like every other field |
| `Desktop::apply_single_monocle` | `fn(&mut self, bool)` | `single_monocle`: monocle layout while at most one tiled window, the user's layout otherwise. `Monitor::arrange` calls it before every layout, which covers every place bspwm re-checks it |
| `Monitor::sole` / `Wm::refresh_sole` | field / `fn(&mut self)` | Whether this is the only monitor (`borderless_singleton` applies only then); kept current by `add_monitor`/`remove_monitor` |
| `Monitor::insert_desktop` | `fn(&mut self, Desktop)` | `src/desktop.c` `insert_desktop()`: appends a desktop keeping its own gap and border width (used when a desktop moves between monitors). |
| `Monitor::remove_desktop` | `fn(&mut self, usize) -> Desktop` | `src/desktop.c` `remove_desktop()`/`unlink_desktop()`. |
| `Monitor::activate_desktop` | `fn(&mut self, usize) -> bool` | `src/desktop.c` `activate_desktop()`. |
| `Monitor::swap_desktops` | `fn(&mut self, usize, usize)` | `src/desktop.c` `swap_desktops()`, single-monitor. |
| `Monitor::arrange` | `fn(&mut self, usize, &Settings)` | `src/tree.c` `arrange()`. |
| `adapt_geometry` | `fn(&mut Tree, Option<NodeId>, Rect, Rect)` | `src/monitor.c` `adapt_geometry()`. |
| `Rule::matches` | `fn(&self, &str, &str, &str) -> bool` | `src/rule.c` `apply_rules()`'s matching (exact string or `*`, not a glob). |
| `match_rules` | `fn(&mut Vec<Rule>, &str, &str, &str) -> RuleConsequence` | `src/rule.c` `apply_rules()`'s full loop: merges every matching rule's consequence, removing (and stopping at) the first one-shot match. |
| `RuleConsequence::merge` | `fn(&mut self, &RuleConsequence)` | Applies a further matched rule's fields on top, `Some`-over-`None`; mirrors `apply_rules()`'s loop merging every match onto one accumulator. |
| `RuleConsequence::should_center`/`should_follow` | `fn(&self) -> bool` | `center`/`follow` are `Option<bool>` like the other keys (a later rule's `center=off` clears an earlier `on`, `parse_key_value()`); unmentioned means `false`. |
| `RuleConsequence::should_manage`/`should_focus`/`should_border` | `fn(&self) -> bool` | Resolves `manage`/`focus`/`border` with bspwm's `make_rule_consequence()` default of `true` when no rule mentioned the field. |
| `Wm::new` | `fn(Settings) -> Wm` | No monitors, no rules. |
| `Wm::add_monitor` | `fn(&mut self, Monitor) -> usize` | `src/monitor.c` `add_monitor()` (minus RandR/EWMH); appends then walks it into on-screen-position order via `reorder_monitor`. |
| `Wm::remove_monitor` | `fn(&mut self, usize) -> Monitor` | `src/monitor.c` `remove_monitor()` (minus EWMH; caller empties `desktops` first). |
| `Wm::focus_monitor` | `fn(&mut self, usize) -> bool` | `src/monitor.c` `focus_node()`'s monitor-focusing half. |
| `Wm::swap_monitors` | `fn(&mut self, usize, usize)` | `src/monitor.c` `swap_monitors()`. |
| `Wm::reorder_monitor` | `fn(&mut self, usize) -> usize` | `src/monitor.c` `reorder_monitor()`; returns the monitor's index after reordering. |
| `Wm::focused_monitor`/`focused_monitor_mut` | `fn(&self/&mut self) -> Option<&/&mut Monitor>` | The focused monitor, if any. |
| `Wm::monitor_index` | `fn(&self, MonitorId) -> Option<usize>` | Finds a monitor's slot by id. |
| `Wm::apply_ewmh_struts` | `fn(&mut self, &EwmhStruts, (i32, i32)) -> bool` | `src/ewmh.c` `ewmh_handle_struts()`: grows the padding of every monitor a panel's `_NET_WM_STRUT_PARTIAL` touches (max with the existing padding, offset for a negative one); `true` if any changed. |
| `EwmhStruts::from_cardinals` | `fn(&[u32]) -> Option<EwmhStruts>` | The twelve `CARDINAL`s of `_NET_WM_STRUT_PARTIAL` in EWMH order. |

## Testing

Unit tests per operation (`crates/bsp-core/src/*.rs`, `#[cfg(test)] mod tests`), plus property tests (`crates/bsp-core/tests/property.rs`) that run random operation sequences and check: every leaf maps to one window, ratios stay in `[0, 1]`, no window is lost, and rectangles tile the desktop without overlap.

## Focus history

`history::History` is bspwm's `history.c`: a list from the oldest to the newest focus, where an entry stays `latest` until a newer entry names the same node (or, for an empty desktop, the same desktop), and a focus that lands on a monitor or desktop other than the focused one is inserted next to its own desktop's entries. Entries name a node by its client's `WindowId` (a tree slot is reused and changes when a node moves between trees). bspwm records from inside `focus_node()`, `activate_node()` and the unlink/transfer functions; focus is written directly in several crates here, so `Wm::sync_history()` observes the result instead and records what changed. Call it after every command and once per event-loop turn; two focus changes inside one call are recorded as one entry.

| Function | Signature | Notes |
| --- | --- | --- |
| `Wm::sync_history` | `fn(&mut self)` | Adds the focus changes since the last call (`history_add()`), drops entries whose window, desktop or monitor is gone or moved (`history_remove()`) |
| `History::add` | `fn(&mut self, Loc, focused: bool)` | `src/history.c` `history_add()`; does nothing while `record` is false |
| `History::remove_matching` | `fn(&mut self, impl Fn(&Loc) -> bool)` | `history_remove()`, including collapsing the duplicates a removal leaves |
| `History::find` | `fn(&self, Dir, impl Fn(&Loc) -> bool) -> Option<Loc>` | `history_find_node()`/`_desktop()`/`_monitor()`; the needle only moves while recording is off |
| `History::find_newest` | `fn(&self, impl Fn(&Loc) -> bool) -> Option<Loc>` | `history_find_newest_*()` |
| `History::last_node` / `last_desktop` / `last_monitor` | see `history.rs` | `history_last_*()` |
| `History::rank` | `fn(&self, WindowId) -> u32` | `history_rank()`, the tie-break of directional focus |
| `Wm::fallback_focus` | `fn(&self, usize, usize) -> Option<NodeId>` | The node a desktop focuses when none is chosen: the last focused (`history_last_node()`), else the first focusable leaf. Reads only |
| `Client::stack_level` | `fn(&self) -> i32` | `src/stack.c` `stack_level()`: `3 * layer + state`; used to decide which fullscreen window covers a focused one |
| `Tree::next_node` / `Tree::prev_node` | `fn(&self, Option<NodeId>) -> Option<NodeId>` | `src/tree.c` `next_node()`/`prev_node()`: in-order walk over leaves and splits |
| `Tree::transplant_to_mapped` | `fn(&mut self, &Settings, NodeId, &mut Tree, Option<NodeId>) -> (NodeId, Vec<(NodeId, NodeId)>)` | `transplant_to` plus every `(old, new)` node id pair of the moved subtree, so the registry can carry each wire id across |
| `StackingList::stack` | `fn(&mut self, WindowId, bool, impl Fn(WindowId) -> Option<i32>) -> Option<StackMove>` | `src/stack.c` `stack()` for one window: on top of its level when focused, at the bottom of it otherwise; returns where it went (the `node_stack` event) |
| `StackingList::remove` / `windows` | `fn(&mut self, WindowId)` / `fn(&self) -> &[WindowId]` | `remove_stack_node()`; the order, bottom first (`Wm::stacking`) |
| `Wm::refocus_after_removal` | `fn(&mut self, monitor: usize, desktop: usize)` | Gives a desktop whose focused node was removed the most recently focused remaining window, else its first leaf; the compositor calls it after every unmap |
| `History::locations` | `fn(&self) -> impl Iterator<Item = Loc>` | Oldest first, for `wm -d` |

Settings gained `normal_border_color` (`#30302f`), `active_border_color` (`#474645`), `focused_border_color` (`#817f7f`) `presel_feedback_color` (`#f4d775`) and `status_prefix` (`W`, the text a status report starts with), plus `settings::is_hex_color` and `parse_hex_color` (`src/helpers.c` `is_hex_color()`).

`Tree::node_ids()` lists every node reachable from the root (splits included), pre-order; the registry sweep in `bsp-ipc` uses it.

`Tree::swap_subtrees_with(n1, other, n2) -> SubtreeSwap` exchanges two subtrees of two trees in their exact slots (clone into the other arena, put in place, free the old ones), returning the `(old, new)` id pairs of both directions; a focus that left is replaced by the incoming subtree's focused or first leaf.

`Tree::children(id)` (private) returns both children of a split; the tree code stops with a `debug_assert!` instead of unwrapping when a split lacks one. `tests/property.rs` also fuzzes swaps, transplants and circulation between two trees.

`Monitor::wired` (bspwm `wired`). `node::SizeHints` and `Client::{size_hints, honor_size_hints}` with `should_honor_size_hints()` (`SHOULD_HONOR_SIZE_HINTS`), `apply_size_hints(w, h)` (`apply_size_hints()`) and `shown_rectangle()` (the layout or floating rectangle after the hints). Leaves keep `Constraints::default()`, as in bspwm. `RuleConsequence::honor_size_hints`.

`tree::presel_rect(node_rect, presel, gap)` ports `draw_presel_feedback()`. `Monitor::sticky_count()` counts the sticky nodes of the monitor. `Tree::swap_subtrees_with` gives a tree whose focus left the incoming root (bspwm).
