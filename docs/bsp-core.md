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
| `monitor` | Monitor name, rectangle, padding, desktop list, focused desktop, `arrange`, `adapt_geometry` |
| `rules` | `bspc rule` entries matched on class, instance and name; `RuleConsequence` |
| `settings` | Every `bspc config` key the tree engine reads, with bspwm's defaults |
| `wm` | `Wm`: every monitor, the focused one, the global rule list and settings — bspwm's `mon_head`/`mon_tail`/`mon`/`rule_head` globals collected into one struct, for `bsp-ipc` to resolve selectors and run commands against |
| `event` / `effect` (planned) | `Event` enum, `Effect` enum tying `bsp-core` to an adapter — not started; needs the nested compositor's compositor layer to have a shape worth committing to |

## scope

Every function below is pure: it takes and mutates plain data and returns plain values, with no I/O. bspwm interleaves these structural operations with side effects that need a real display — drawing borders, EWMH, the input focus, the stacking list, `subscribe` reports, history. Those are left out; each function's doc comment says so and names the bspwm function it mirrors, so nothing was dropped silently. They land once `bsp-compositor` or `bsp-ipc` has an adapter to receive them.

Two structural simplifications, not bspwm behavior differences (so not entered in `docs/design.md`'s Deliberate deviations table — nothing here is user-visible or permanent):

- **`NodeId` is per-`Tree`, not global.** bspwm's `node_t*` keeps its identity when a node moves to a different desktop's tree (`transfer_node`, `swap_nodes`). Here, a `NodeId` is only meaningful within the arena that issued it (`docs/design.md`: "`bsp-core` uses its own `WindowId(u32)`"), so moving a node to a different `Tree` — `Tree::transplant_to` — clones its subtree into the destination arena under a fresh id and frees the original. `WindowId`, which is what the adapter and `bspc` actually key on, is unaffected.
- **`swap_nodes` is single-tree only.** bspwm's cross-desktop `swap_nodes` relies on the pointer-identity behavior above; a cross-tree swap here is two `transplant_to` calls (move `n1` to `n2`'s old anchor and vice versa) rather than a dedicated method, since with per-tree ids there is no shorter path.
- **Monitor/desktop ordering.** bspwm keeps monitors and desktops sorted by on-screen position (`reorder_monitor`, `rect_cmp`) and desktops in a doubly linked list with position-preserving swaps. `Monitor::desktops` is a plain `Vec` in insertion order; the position-based ordering that matters for hotplug and `bspc monitor -f next` is the hardware backend (real hardware) territory, where there is an actual output layout to sort by.

## Public functions

| Function | Signature | Behavior |
| --- | --- | --- |
| `Rect::new` | `fn(x: i32, y: i32, width: i32, height: i32) -> Rect` | Constructs a rectangle. |
| `Rect::area` | `fn(&self) -> i64` | `src/geometry.c` `area()`. |
| `Rect::right` | `fn(&self) -> i32` | x + width. |
| `Rect::bottom` | `fn(&self) -> i32` | y + height. |
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
| `Tree::get_rectangle` | `fn(&self, NodeId, i32, Layout) -> Rect` | `src/tree.c` `get_rectangle()`. |
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
| `Tree::move_floating` (**move**) | `fn(&mut self, NodeId, i32, i32) -> bool` | `src/window.c` `move_client()` (floating-rectangle translation only; see doc comment). |
| `Tree::resize_node` (**resize**) | `fn(&mut self, NodeId, ResizeHandle, i32, i32, bool) -> bool` | `src/window.c` `resize_client()`. |
| `Tree::swap_nodes` (**swap**) | `fn(&mut self, NodeId, NodeId) -> bool` | `src/tree.c` `swap_nodes()`, single-tree (see scope). |
| `Tree::transplant_to`/`transplant_within` (**transplant**) | `fn(&mut self, &Settings, NodeId, ..) -> NodeId`/`bool` | `src/tree.c` `transfer_node()` (minus focus/history/EWMH/`single_monocle`). |
| `Tree::circulate_leaves` | `fn(&mut self, &Settings, Option<NodeId>, CirculateDir)` | `src/tree.c` `circulate_leaves()` (minus refocus). |
| `Tree::apply_layout` | `fn(&mut self, Option<NodeId>, Rect, i32, Layout, Rect)` | `src/tree.c` `apply_layout()` (geometry only; see doc comment). |
| `Desktop::new` | `fn(DesktopId, Option<&str>, &Settings) -> Desktop` | `src/desktop.c` `make_desktop()`. |
| `Desktop::rename` | `fn(&mut self, &str)` | `src/desktop.c` `rename_desktop()`. |
| `Desktop::set_layout` | `fn(&mut self, Layout, bool, Layout) -> bool` | `src/desktop.c` `set_layout()`. |
| `Monitor::new` | `fn(MonitorId, Option<&str>, Rect, &Settings) -> Monitor` | `src/monitor.c` `make_monitor()`. |
| `Monitor::rename` | `fn(&mut self, &str)` | `src/monitor.c` `rename_monitor()`. |
| `Monitor::add_desktop` | `fn(&mut self, Desktop)` | `src/desktop.c` `add_desktop()`. |
| `Monitor::remove_desktop` | `fn(&mut self, usize) -> Desktop` | `src/desktop.c` `remove_desktop()`/`unlink_desktop()`. |
| `Monitor::activate_desktop` | `fn(&mut self, usize) -> bool` | `src/desktop.c` `activate_desktop()`. |
| `Monitor::swap_desktops` | `fn(&mut self, usize, usize)` | `src/desktop.c` `swap_desktops()`, single-monitor. |
| `Monitor::arrange` | `fn(&mut self, usize, &Settings)` | `src/tree.c` `arrange()`. |
| `adapt_geometry` | `fn(&mut Tree, Option<NodeId>, Rect, Rect)` | `src/monitor.c` `adapt_geometry()`. |
| `Rule::matches` | `fn(&self, &str, &str, &str) -> bool` | `src/rule.c` `apply_rules()`'s matching (exact string or `*`, not a glob). |
| `match_rules` | `fn(&mut Vec<Rule>, &str, &str, &str) -> RuleConsequence` | `src/rule.c` `apply_rules()`'s full loop: merges every matching rule's consequence, removing (and stopping at) the first one-shot match. |
| `RuleConsequence::merge` | `fn(&mut self, &RuleConsequence)` | Applies a further matched rule's fields on top, `Some`-over-`None`; mirrors `apply_rules()`'s loop merging every match onto one accumulator. |
| `RuleConsequence::should_manage`/`should_focus`/`should_border` | `fn(&self) -> bool` | Resolves `manage`/`focus`/`border` with bspwm's `make_rule_consequence()` default of `true` when no rule mentioned the field. |
| `Wm::new` | `fn(Settings) -> Wm` | No monitors, no rules. |
| `Wm::add_monitor` | `fn(&mut self, Monitor) -> usize` | `src/monitor.c` `add_monitor()` (minus RandR/EWMH). |
| `Wm::remove_monitor` | `fn(&mut self, usize) -> Monitor` | `src/monitor.c` `remove_monitor()` (minus EWMH; caller empties `desktops` first). |
| `Wm::focus_monitor` | `fn(&mut self, usize) -> bool` | `src/monitor.c` `focus_node()`'s monitor-focusing half. |
| `Wm::swap_monitors` | `fn(&mut self, usize, usize)` | `src/monitor.c` `swap_monitors()`. |
| `Wm::focused_monitor`/`focused_monitor_mut` | `fn(&self/&mut self) -> Option<&/&mut Monitor>` | The focused monitor, if any. |
| `Wm::monitor_index` | `fn(&self, MonitorId) -> Option<usize>` | Finds a monitor's slot by id. |

## Testing

Unit tests per operation (`crates/bsp-core/src/*.rs`, `#[cfg(test)] mod tests`), plus property tests (`crates/bsp-core/tests/property.rs`) that run random operation sequences and check: every leaf maps to one window, ratios stay in `[0, 1]`, no window is lost, and rectangles tile the desktop without overlap.
