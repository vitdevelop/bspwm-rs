# bsp-ipc

The control socket: it speaks bspwm's wire protocol so the stock `bspc` binary, `bspwmrc` and existing scripts work unchanged. `bspc-rs` (its own crate) is a thin client over this crate's `wire` module.

## Protocol

- Socket path from `BSPWM_SOCKET`; when unset, `$XDG_RUNTIME_DIR/bspwm-rs-socket` (bspwm's X11 display-based name does not apply on Wayland — a Wayland session has at most one compositor instance per `XDG_RUNTIME_DIR`, so a fixed name is enough).
- A request is the `bspc` arguments joined by NUL bytes, every argument (including the last) NUL-terminated; the reply is text, and a failure reply starts with `0x07` (bspwm's `FAILURE_MESSAGE`) so `bspc` exits non-zero and prints the rest to standard error.
- `subscribe` keeps the connection open and streams report lines and events; every other command gets exactly one reply and the connection closes.
- Byte format checked against bspwm 0.9.12's source (`src/common.h`, `src/bspc.c`, `src/messages.c`, `src/subscribe.c`, `src/query.c`, `src/tree.c`, `src/geometry.c`) and `doc/bspwm.1.asciidoc`, not guessed.

## Modules

| Module | Holds |
| --- | --- |
| `wire` | NUL-framed request/reply encoding, `Reply`, and socket path resolution |
| `value` | Small `bspc` value grammars with no home in `bsp-core` (`CycleDir`, `ResizeHandle`, `AlterState`, `parse_bool`/`parse_degree`/`parse_id`/`parse_index`) |
| `selector` | Node/desktop/monitor selector grammar: descriptors, modifiers, parsing (`NodeSelector::parse` etc.) |
| `selector::resolve_impl` | Resolving a parsed selector against live `bsp_core::wm::Wm` state (`resolve_node`/`resolve_desktop`/`resolve_monitor`) |
| `command` | `bspc` argument grammar into a `Command` for every domain: node, desktop, monitor, query, rule, wm, config, subscribe, quit |
| `registry` | `NodeRegistry`: stable, never-reused wire node ids over `bsp-core`'s reused arena `NodeId`s |
| `report` | `subscribe report` line, `subscribe` events, and the `query -T`/`wm -d` JSON shape |
| `adapter` | `Adapter` trait (window class/instance lookup, close/kill) and `FakeAdapter`, the IPC roadmap's "fake adapter" |
| `exec` | Executes a parsed `Command` against a `Wm` + `NodeRegistry` + `Adapter`, producing a `Reply` and the `Event`s to broadcast |
| `server` | `Listener`/`Connection` (non-blocking Unix socket) and `Subscribers` (mask-matched event/report delivery) |

## IPC progress

Everything is implemented and tested end to end: wire framing, the full `bspc` argument grammar for every domain, selector parsing and structural resolution, report/event/JSON formatting matching bspwm's exact byte output, an executor connecting all of it to `bsp-core`, a `FakeAdapter`, a real (if compositor-less) socket server, and `bspc-rs` as a working client. `crates/bsp-ipc/tests/integration.rs` exercises the whole path — wire decode → `command::parse` → `exec::execute` → wire encode — over a real Unix socket.

`server::Listener`/`Connection` are also now wired into `bsp-compositor`'s real `calloop` event loop (`crates/bsp-compositor/src/ipc.rs`) and confirmed against a live, running compositor and a real client — see `docs/bsp-compositor.md`, Nested compositor progress. A few executor operations are still deliberately deferred rather than guessed at:

- **Cross-desktop/cross-monitor `node --swap`** is implemented (`exec::swap_across_desktops`, `Tree::swap_subtrees_with`): the subtrees exchange exact slots and keep their wire ids (`NodeRegistry::relocate_many`); floating rectangles are adapted to the other monitor.
- **`wm --load-state`, `wm --adopt-orphans`**: no saved-state format or orphan-window concept exists yet.
- **`rule --add`'s `monitor=`/`desktop=`/`node=`/`rectangle=`** are parsed, validated, kept as selector text in `RuleConsequence` (`monitor_desc`/`desktop_desc`/`node_desc`/`rect`) and applied by the compositor (`resolve_rule_target`); `honor_size_hints=` is parsed and listed but not applied.
- Every `subscribe --fifo` request is parsed but the FIFO itself is never created (`mkfifo` has no safe `std` wrapper); a subscriber always gets its report/events over the request connection itself.
- The executor reports back whether a command changed anything and leaves *when* to push a fresh `report` line to `report`-subscribed connections to the caller, rather than replicating bspwm's `put_status(SBSC_MASK_REPORT)` call site by call site throughout `src/tree.c`/`src/desktop.c`/`src/monitor.c`.

**`node --move`/`--resize` are implemented and live-verified**, closing the gap above: `bsp_core::tree::Tree::find_fence`/`resize_node`/`move_floating` (`docs/bsp-core.md`) port bspwm's `src/tree.c` `find_fence()` and `src/window.c` `move_client()`/`resize_client()`, verified against bspwm 0.9.12's actual source (fetched and read directly, not recalled) rather than guessed at, since several details are easy to get wrong from memory alone — notably that `--move` on a tiled node always fails (bspwm's own `move_client()` only takes that path while a live pointer drag is being tracked, which a `bspc` request never is) and that `--resize` on a tiled node adjusts an ancestor "fence" node's `split_ratio` rather than the node's own rectangle. `exec_node`'s `Move`/`Resize` arms call these and push `Event::NodeGeometry` exactly where bspwm's own `put_status(SBSC_MASK_NODE_GEOMETRY, …)` calls do: after a floating move, and after a floating (but not pseudo-tiled or tiled) resize. Confirmed live against a running compositor and two real `alacritty` clients: a tiled resize moved the shared fence (and both windows' on-screen rectangles) by exactly the given delta, a tiled move failed as bspwm's own does, and a floating move/resize (including from each of the 8 handles) produced the exact expected rectangle math. Not implemented: automatic transfer to a different monitor when a floating move's new rectangle would land under one (bspwm: `move_client()`'s `monitor_from_client`/`transfer_node` tail) — deferred alongside the cross-monitor `node --swap` gap above, a `Wm`-level cross-tree operation this pass didn't need to touch; and honoring ICCCM/`xdg_toplevel` size hints during a resize (bspwm: `apply_size_hints()`) — no such hints are tracked anywhere in this build yet (`bsp-core::node`'s own module doc comment).

**`bspc output --create-headless [WxH@HZ]` and `bspc output NAME --remove`** add and remove a virtual (headless) output (`OutputAction::CreateHeadless`/`Remove`; creating takes no name, the output is called `HEADLESS-N`; a real output cannot be removed). The compositor does the work (`docs/bsp-compositor.md`, `headless`).

**`bspc output`/`bspc input` grammar and wire plumbing are implemented** (`command::parse_output`/`parse_input`, `exec::exec_output`/`exec_input`) — not bspwm commands at all, but this project's own extensions replacing `xrandr` and `setxkbmap`/`xset r rate`/`xinput` (`docs/design.md`'s "Configuration beyond bspwm" and its the hardware backend roadmap row). `output [<name> [-m WxH@Hz] [-s SCALE] [-p X Y]]` and `input [<device> [-r HZ DELAY] [-a FACTOR]]` parse and route through `Adapter` (new default-implemented methods: `output_names`/`output_settings`/`set_output`, `input_names`/`input_settings`/`set_input`) exactly the way every other domain does — but every default returns "no known outputs/devices" or a `Reply::Fail("... not supported (no hardware ... backend yet).\n")`, since there is no real DRM output list or `libinput` device list for any backend to report yet (the hardware backend; wired for the DRM backend in Stage E, `docs/bsp-compositor.md`). Deliberately staged this way per `docs/design.md`'s roadmap: settle the wire protocol and grammar first (unit-tested against `FakeAdapter`'s defaults), wire it to real hardware once the DRM/udev backend exists to answer it (`docs/bsp-compositor.md`). Flag letters (`-m`/`-s`/`-p`/`-r`/`-a`) are this project's own choice, matching this crate's existing dash-flag style — `docs/design.md`'s own `bspc output`/`bspc input` examples are illustrative prose, not a literal CLI spec.

**`bspc config -m MON` targets the monitor** (found live, testing X11 struts): `top_padding`/`right_padding`/`bottom_padding`/`left_padding` and `window_gap` with `-m` now read and write the monitor's own values, as bspwm's `cmd_config()` does (it leaves the desktop out of the coordinates, so `SET_DEF_MON_DESK` lands on `loc.monitor`); `-d` still targets the desktop and no selector still sets the default. Covered by `config_padding_with_m_targets_the_monitor_and_with_d_the_desktop`. `border_width` per target and the other per-monitor/-desktop settings are still global-only.

**Focus history is implemented** (`bsp_core::history`, `docs/bsp-core.md`): `last`, `older`, `newer` and `newest` resolve on nodes, desktops and monitors as in bspwm's `history_find_*()`, directional selection breaks distance ties with `history_rank`, `bspc wm -h on|off` toggles recording, and `wm -d`'s `focusHistory` lists the entries oldest first. `exec::execute` calls `Wm::sync_history()` before and after every command. `bspc config` also handles `normal_border_color`, `active_border_color`, `focused_border_color` and `presel_feedback_color` (`#rrggbb` only, `is_hex_color()`).

Selector *resolution* (not parsing, which is complete) has its own known gaps, each returning `ResolveError::Unsupported` rather than a wrong answer:

- **The stacking list is implemented** (`bsp_core::stack`, `docs/bsp-core.md`): `node_stack <node> above|below <sibling>` is sent whenever a window is placed (focus, activation, state or layer change, a moved node, a new window that took no focus), and `wm -d`'s `stackingList` holds the node wire ids bottom first. Windows of every desktop share one list, as in bspwm.
- **`pointed`** (node and monitor) resolves against `Adapter::pointer_state` (the pointer position and the managed window under it, refreshed by the compositor before each command); no pointer state means no match. **`primary`** (monitor) is not supported: there is no primary-monitor concept.
- **`.same_class`** compares the class name (`Adapter::window_class`) of a node's window with the reference window's: a node with no window is never of the same class, a reference with no window has none. Resolution reaches the adapter through `selector::resolve_impl::Ctx::adapter`.
- **`next`/`prev`** walk every node in order, internal nodes included (`Tree::next_node`/`prev_node`), on to the next (previous) desktop and monitor and around, as bspwm's `find_closest_node()` does; use `next.window` to skip splits. **`biggest`/`smallest`** compare the shown area (`get_rectangle`) of non-vacant leaves against the real reference, so `.local` works.
- **Directional selection** (`node`/`monitor` `north`/`west`/`south`/`east`) always uses bspwm's `TIGHTNESS_HIGH` default (`src/geometry.c` `on_dir_side()`); `directional_focus_tightness` is not yet a `bsp-core`/`config` setting.

None of these silently produce a wrong answer: every gap above is either a `ResolveError::Unsupported` (distinct from `ResolveError::NoMatch`) or an explicit `Reply::Fail` naming what is missing, so a caller can tell "no such node"/"command failed" from "not implemented yet" apart.

`bsp-ipc`'s own `Wm`-shaped state lives in `bsp-core` (`wm::Wm`, a small aggregate of `Vec<Monitor>` plus the focused index, rules and settings — bspwm's `mon_head`/`mon_tail`/`mon`/`rule_head` globals collected into one struct), since it is plain state with no Wayland dependency and every command needs a single root to resolve against.

## Public functions

### `wire`

| Function | Signature | Behavior |
| --- | --- | --- |
| `FAILURE_MARKER` | `const u8` | `0x07`, bspwm's `FAILURE_MESSAGE` first byte |
| `SOCKET_ENV_VAR` | `const &str` | `"BSPWM_SOCKET"` |
| `DEFAULT_SOCKET_NAME` | `const &str` | `"bspwm-rs-socket"` |
| `socket_path` | `fn() -> Option<PathBuf>` | Resolves `BSPWM_SOCKET`, falling back to `$XDG_RUNTIME_DIR/bspwm-rs-socket` |
| `encode_request` | `fn<'a>(impl IntoIterator<Item = &'a str>) -> Vec<u8>` | NUL-terminates every argument (bspwm: `bspc.c` `main()`) |
| `decode_request` | `fn(&[u8]) -> Vec<String>` | Splits a raw request on NUL bytes (bspwm: `messages.c` `handle_message()`) |
| `Reply::into_bytes` / `Reply::parse` | `fn(self) -> Vec<u8>` / `fn(&[u8]) -> Reply` | Encodes/decodes the `Ok`/`Fail` reply framing |

### `value`

| Function | Signature | Behavior |
| --- | --- | --- |
| `CycleDir::parse` | `fn(&str) -> Option<CycleDir>` | `next`/`prev` |
| `ResizeHandle::parse` | `fn(&str) -> Option<ResizeHandle>` | The 8 resize handle names |
| `parse_bool` | `fn(&str) -> Option<bool>` | `true`/`on`/`false`/`off` |
| `parse_degree` | `fn(&str) -> Option<i32>` | Any integer, normalized to a 90°-multiple in `0..360` |
| `parse_id` | `fn(&str) -> Option<u32>` | `0x`-prefixed hex or decimal, whole string consumed |
| `parse_index` | `fn(&str) -> Option<u16>` | `^<n>` |

### `selector`

| Function | Signature | Behavior |
| --- | --- | --- |
| `NodeSelector::parse` / `DesktopSelector::parse` / `MonitorSelector::parse` | `fn(&str) -> Option<Self>` | Full `[REFERENCE#]DESCRIPTOR(.MODIFIER)*` grammar, including recursive references, `@`-paths, and `%name` |
| `resolve_node` / `resolve_desktop` / `resolve_monitor` | `fn(Ctx, Coordinates, &Selector) -> Result<Coordinates, ResolveError>` | Resolves a parsed selector against a `bsp_core::wm::Wm`; see IPC progress for the unsupported subset |

### `command`

| Function | Signature | Behavior |
| --- | --- | --- |
| `parse` | `fn(&[String]) -> Result<Command, ParseError>` | Full request parse: domain word dispatch (bspwm: `messages.c` `process_message()`) |
| `parse_node` / `parse_desktop` / `parse_monitor` / `parse_query` / `parse_rule` / `parse_wm` / `parse_subscribe` / `parse_quit` / `parse_config` | `fn(&[String]) -> Result<Command, ParseError>` | One per domain, mirroring `cmd_node()` … `cmd_config()`; every flag accepts both its short and long spelling |
| `Command::is_read_only` | `fn(&self) -> bool` | `query`, `config KEY`, `wm -d`/`-g`: with no event, nothing changed and the compositor sends no report |
| `parse_output` / `parse_input` | `fn(&[String]) -> Result<Command, ParseError>` | Not bspwm domains — this project's own `xrandr`/`setxkbmap`/`xset r rate`/`xinput` replacements (`docs/design.md`) |

### `registry`

| Function | Signature | Behavior |
| --- | --- | --- |
| `NodeRegistry::new` | `fn() -> Self` | Empty registry |
| `NodeRegistry::register` | `fn(&mut self, DesktopId, NodeId) -> u32` | Mints a fresh stable id for a newly created node |
| `NodeRegistry::unregister` | `fn(&mut self, DesktopId, NodeId)` | Forgets a freed node's mapping |
| `NodeRegistry::relocate` | `fn(&mut self, (DesktopId, NodeId), (DesktopId, NodeId))` | Re-points a moved node's mapping, keeping its stable id (for `Tree::transplant_to`, which mints a new arena `NodeId`) |
| `NodeRegistry::id_of` / `NodeRegistry::lookup` | `fn(&self, ...) -> Option<...>` | Stable id ↔ current location |

### `report`

| Function | Signature | Behavior |
| --- | --- | --- |
| `Report`'s `Display` impl | — | The `subscribe report`/`wm -g` status line, byte-for-byte (bspwm: `subscribe.c` `print_report()`) |
| `Event`'s `Display` impl | — | One line per `subscribe` event, matching every format in `doc/bspwm.1.asciidoc`'s Events section |
| `Event::kind` | `fn(&self) -> EventKind` | The event's category, for `subscribe` mask matching |
| `JsonNode::from_tree` / `JsonDesktop::from_desktop` / `JsonMonitor::from_monitor` / `JsonState::new` | `fn(...) -> Self` | Builds bspwm's exact `query -T`/`wm -d` JSON shape (field names and order) from `bsp-core` state, given adapter-supplied node ids and client class/instance names. `shown` (a client's) is whether its desktop is the one its monitor shows (`show_node()`/`hide_node()`) |

### `adapter`

| Function | Signature | Behavior |
| --- | --- | --- |
| `Adapter` (trait) | `window_class`/`close_window`/`kill_window` | What `exec` needs from the real window system: class/instance lookup, close, kill. `node -c` closes and `node -k` kills every window of the target's subtree (bspwm's `close_node()`/`kill_node()`); a kill leaves the nodes in the tree until the windows are destroyed, and only an empty receptacle is removed at once (with its own `node_remove` id) |
| `Adapter`'s output/input methods | `output_names`/`output_settings`/`set_output`, `input_names`/`input_settings`/`set_input` | Default-implemented as "no known outputs/devices"/"not supported"; a real hardware backend overrides them (hardware backend) |
| `FakeAdapter` | `new`/`set_class` + `Adapter` impl | An in-memory adapter for tests: a lookup table plus a record of what was closed/killed; uses every output/input default as-is |

### `exec`

| Function | Signature | Behavior |
| --- | --- | --- |
| `execute` | `fn(&mut ExecCtx<A>, &Command) -> (Reply, Vec<Event>)` | Runs any `Command` but `Subscribe`/`Quit` (the server handles those directly) against `bsp-core`, mirroring `src/messages.c`'s `cmd_node()` … `cmd_config()` |
| `build_report` | `fn(&Wm) -> Report` | Builds the current `subscribe report`/`wm -g` line from live state; it starts with `bspc config status_prefix` (default `W`, `print_report()`). `node -t` reports the left state `off` and then the new one `on` as two `node_state` events. |
| `exec::focus_node` | `fn(&mut ExecCtx<A>, Coordinates, &mut Vec<Event>) -> bool` | `src/tree.c` `focus_node()`: focuses a node (an empty node means the desktop's own focused node, else the last focused, else the first focusable leaf); fails on a hidden node; clears the urgent flag; lowers fullscreen windows that would cover it; reports `monitor_focus`, `desktop_focus`, `node_focus` (none for an empty desktop). The compositor calls it for clicks, new windows and activation, so those report like `bspc node -f` |
| `exec::activate_node` | `fn(&mut ExecCtx<A>, Coordinates, &mut Vec<Event>) -> bool` | `src/tree.c` `activate_node()`: sets a desktop's remembered focus without moving the focus; fails on the focused desktop; reports `node_activate` |
| `exec::transfer_node` | `fn(&mut ExecCtx<A>, Coordinates, Coordinates, Option<NodeId>, bool, &mut Vec<Event>) -> Result<Coordinates, String>` | `src/tree.c` `transfer_node()`: `node -d/-m/-n`. Without `--follow` the source desktop keeps the focus (on its next node); with it the moved node is focused where it landed. Floating windows are repositioned proportionally when the monitor changes. `node_transfer` carries the moved node's id and the anchor's id (0 for an empty desktop) |
| `exec::swap_desktops` | `fn(&mut ExecCtx<A>, Coordinates, Coordinates, bool, &mut Vec<Event>) -> bool` | `src/desktop.c` `swap_desktops()`: the desktops trade places on one monitor or two (windows moved onto the other monitor's rectangle, both arranged); each monitor keeps showing its slot; focus stays on its slot, or with `--follow` (always on one monitor) goes with the desktop; `desktop_swap` names the pair as given |
| `exec::transfer_desktop` | `fn(&mut ExecCtx<A>, Coordinates, usize, bool, &mut Vec<Event>) -> Option<usize>` | `src/desktop.c` `transfer_desktop()`: `desktop -m`; appended to the other monitor and laid out there, the source monitor shows another desktop (the one it showed before, else a neighbour) |
| `desktop -r` (`exec::remove_desktop_merging`) | — | `messages.c` `-r`: the desktop's windows move to the previous desktop (the next for the first) with `transfer_node()`, then it is removed; the last desktop of a monitor stays. It used to refuse an occupied desktop |
| `exec::resolve_rule_target` | `fn(&ExecCtx<A>, &RuleConsequence) -> Option<Coordinates>` | Where a matched rule's `node=`/`desktop=`/`monitor=` puts a new window (the insertion anchor is the result's node); `None` for no target, a selector that matches nothing, or a sticky window |
| `Adapter::pointer_state` | `fn(&self) -> (Option<(i32, i32)>, Option<WindowId>)` | Pointer position and the window under it, for `pointed` (default: none) |
| `command::parse_rule_effects` | `fn(&str) -> RuleConsequence` | Parses the `key=value` words `external_rules_command` prints; unknown words are skipped |
| `exec::stack_node` / `exec::unstack_node` | `fn(&mut ExecCtx<A>, Coordinates, bool, &mut Vec<Event>)` / `fn(&mut Wm, Coordinates, NodeId)` | `src/stack.c` `stack()` for a node's windows (`node_stack` events) and `remove_stack_node()`; `focus_node`, `activate_node`, `transfer_node`, `node -t` and `node -l` call them |
| `exec::set_urgent` | `fn(&mut ExecCtx<A>, Coordinates, bool, &mut Vec<Event>)` | `src/tree.c` `set_urgent()`: `node_flag ... urgent on/off`; a window already focused on the focused desktop cannot become urgent |
| `exec::Coordinates` | struct | Monitor index, desktop index and optional node of a target; re-exported for the compositor's calls above |
| `exec::set_monitor_rectangle` | `fn(&mut ExecCtx<A>, usize, Rect, &mut Vec<Event>) -> usize` | Applies a new monitor rectangle (adapt tree geometry, re-arrange, `MonitorGeometry` event, `reorder_monitor`); extracted from `bspc monitor -g`, shared with the compositor's output changes. Returns the monitor's index after reordering |
| `exec::manage_window` / `exec::NewWindow` | `fn(&mut ExecCtx<A>, &NewWindow, &RuleConsequence, &mut Vec<Event>) -> Option<Coordinates>` | `src/window.c` `manage_window()` from the rule target on: a sticky window stays on the focused desktop; the rule's `split_dir`/`split_ratio` preselect the anchor (`node_presel`); the floating rectangle is the window's own geometry (`NewWindow::geometry`, an X11 window's; centred when it asked for no position, brought onto the target monitor with `embrace_client()`/`adapt_geometry()`), a rule's `rectangle=`, or for a Wayland window the tiled slot; the node is kept vacant while inserted when it will not tile; `node_add`, the state (`node_state`), the flags the rule turns on (`node_flag`), a floating window takes the anchor's layer; then focused, activated or stacked at the bottom. `None` without a focused desktop |
| `exec::unmanage_window` | `fn(&mut ExecCtx<A>, WindowId, &mut Vec<Event>) -> Option<Coordinates>` | `src/window.c` `unmanage_window()`: `node_remove`, the node out of the tree (the focus moves on if it held it), the desktop re-arranged |
| `exec::push_geometry_changes` | `fn(&mut Wm, &NodeRegistry, &mut Vec<Event>)` | `src/tree.c` `apply_layout()`'s report: `node_geometry` for every window (hidden ones too) whose rectangle as its state shows it (the monitor for fullscreen) differs from `Client::window_rectangle`, which it then updates. `execute` and the compositor's `with_ops` run it; `node -v`/`-z` and pointer drags record what they report |
| `exec::center_rect` | `fn(&mut Rect, Rect, i32)` | `src/window.c` `window_center()` |
| `exec_output` / `exec_input` | `fn(&mut ExecCtx<A>, Option<&str>, &[OutputAction]/&[InputAction]) -> Reply` | List/get/set over `Adapter`'s output/input methods; no hardware knowledge of its own |

### `server`

| Function | Signature | Behavior |
| --- | --- | --- |
| `Listener::bind` | `fn(&Path) -> io::Result<Self>` | Binds a non-blocking Unix socket at mode `0600`, removing a stale one first; removes it again on drop |
| `Listener::accept` | `fn(&self) -> io::Result<Option<Connection>>` | Non-blocking accept |
| `Connection::try_read_request` | `fn(&mut self) -> io::Result<Option<Vec<String>>>` | One non-blocking `recv()`-equivalent, decoded (bspwm treats one `recv()` as one whole request) |
| `Connection::send_reply` / `send_line` | `fn(&mut self, ...) -> ...` | Writes a `Reply`, or one pre-formatted `subscribe` line |
| `server::REPLY_WRITE_TIMEOUT` / `MAX_QUEUED_BYTES` | consts | A reply is written whole in blocking mode for at most 2 s; a subscriber may have up to 1 MiB of unread lines queued before it is dropped |
| `Subscribers::add` | `fn(&mut self, Connection, Vec<SubscriberMask>, Option<u32>, &str) -> u64` | Registers a `subscribe`d connection, sending the initial report if its mask covers `report` (bspwm: `add_subscriber()`, which counts that initial send against `--count` too) |
| `Subscribers::broadcast_event` / `broadcast_report` | `fn(&mut self, ...)` | Delivers to every subscriber whose mask matches, dropping any that fail or exhaust `--count` |

### `bspc-rs` (separate crate)

A thin client over `wire`: connects, sends the encoded request, and streams replies back to stdout/stderr as they arrive (one iteration per chunk works for both an ordinary one-reply command and `subscribe`'s open-ended stream), exiting non-zero if any chunk was a failure. `--print-socket-path` matches `bspc`. Not implemented: polling standard output for `POLLHUP` the way `bspc.c` does, so a `bspc-rs subscribe | head -1` pipeline keeps writing until its own write fails, rather than exiting the moment the reader goes away.

## Deliberate simplifications

These affect the wire format's *edge cases*, not its everyday behavior; each is small enough to not warrant a `docs/design.md` deviation row, but is recorded here per the "read the source, don't guess" rule:

- `parse_id` accepts hex (`0x…`) and decimal, not bspwm's octal (`strtol(s, &end, 0)`'s leading-zero form) — no id bspwm ever prints is octal, so no real selector needs it.
- `rule --add`'s cause escaping (`\:` inside a class/instance/name) is not implemented (bspwm: `tokenize_with_escape()`); no rule cause in practice needs a literal colon.

## Fuzzing

`tests/fuzz.rs` (proptest) feeds random bytes to `wire::decode_request` and `Reply::parse`, and random argument vectors (a vocabulary of real tokens plus arbitrary strings, always headed by a domain word) to `command::parse` and `exec::execute` over a `FakeAdapter` fixture; it also checks the encode/decode round trip. `Subscribe`/`Quit` are skipped since `execute` panics on them by contract (handled by the server). Raise coverage with `PROPTEST_CASES=200000 cargo test --release -p bsp-ipc --test fuzz`.

## Node ids, `desktop -b`, layout events (REVIEW2 N5, N9, N14)

- `NodeRegistry::sync_with(&Wm)` gives every node in every tree (splits included) a wire id and forgets the ids of freed nodes. `exec::execute` calls it after each command; the compositor calls it after `with_ops` and after a window maps or unmaps.
- `desktop -b next|prev` swaps with the neighbour through `exec::swap_desktops`, one step at a time (one `desktop_swap` event per step); at the end of the list it keeps swapping until the desktop is at the other end.
- `exec::layout_snapshot(wm)` and `exec::push_layout_changes(wm, before, events)` add a `desktop_layout` event for every desktop whose effective layout changed and is not already reported. `execute` uses them; the compositor uses them around `with_ops`, map and unmap.

## Sticky nodes, cross-desktop swap (REVIEW2 N7, N8)

- `exec::transfer_sticky_nodes(ctx, monitor, to, events)` moves the sticky windows of the desktop the monitor shows to `to`; `focus_node`, `desktop -a` and `transfer_desktop` (of the shown desktop) call it before the shown desktop changes.
- `exec::transfer_node` refuses a sticky subtree bound for a desktop that is not shown; `transfer_node_unchecked` is the same without the check (used for the sticky move itself and by `desktop -r`).
- `exec::swap_across_desktops` implements `node -s` between desktops: two `Tree::swap_subtrees_with` slot exchanges, ids kept with `NodeRegistry::relocate_many`, `adapt_geometry` across monitors, both desktops arranged, `follow` focuses the moved node. A sticky node is refused.

`query -N` lists every node of the trees, splits included, parents first, as bspwm does; `exec::execute` runs `NodeRegistry::sync_with` first as well as last.

`exec::merge_monitors(ctx, from, to, events)` (bspwm `merge_monitors()`), `exec::remove_monitor(ctx, index, keep, events)` (`remove_monitor()`: `monitor_remove`, refocus), `exec::merge_target(wm, gone)` (the last wired monitor). `config honor_size_hints` sets the clients like bspwm's `SET_DEF_WIN`. `wm -d` reports `wired`.

REVIEW3 batches 4–5 and query:
- Sticky: `transfer_sticky_nodes` moves topmost sticky subtrees (`sticky_subtree_windows`, `move_windows_subtrees`); `focus_node` keeps a focused sticky window focused (bspwm `guess`); `swap_desktops` keeps a shown desktop's stickies on screen; `transfer_desktop` ports lines 175–211 of `desktop.c`; `node -g sticky` on a hidden desktop transfers first. `Monitor::sticky_count()` is computed.
- `swap_across_desktops` ports `swap_nodes()`'s cross-desktop focus rules.
- `NodeRegistry::register_with_split` registers a new node's fresh split at once; `node -i` reports `node_add`.
- `bspc query` (`QueryCommand { monitor_ref, desktop_ref, node_ref, targets: Vec<QueryTarget>, monitor_filter, desktop_filter, node_filter, names }`) is a port of `cmd_query()`/`query_*_ids()`.

The control socket: the first instance binds `$XDG_RUNTIME_DIR/bspwm-rs-socket`; `Listener::bind` refuses (`AddrInUse`) a path another server answers on, and the compositor then binds `bspwm-rs-$WAYLAND_DISPLAY-socket` and exports `BSPWM_SOCKET`. A `Listener` removes only its own socket file when dropped. `^n` without a monitor indexes the desktops of all monitors in order.

## Golden tests (REVIEW4 R2)

`tests/golden.rs` compares bspwm-rs with the real bspwm 0.9.12. Each `tests/golden/NAME.scn` is a scenario (`bspc ...`, `window CLASS INSTANCE [WxH+X+Y]`, `close N`); `NAME.out` is bspwm's transcript, recorded under Xvfb by `bsp-compositor`'s `examples/golden_record.rs` through `contrib/golden/record.sh` (run in the QEMU test VM: `pacman -S bspwm xorg-server-xvfb xorg-xprop` in the live system). The test replays each scenario through `exec::execute`, `exec::manage_window`/`unmanage_window` and the compositor's report rule, names every id by its first appearance, and compares each step's output, errors and exit status in order, its events as a sorted list, and the latest report line. `KNOWN` lists the steps where bspwm does something bspwm-rs cannot, with the reason (the X server's `FocusIn` makes bspwm focus again; focus history within one command). After changing a scenario, record it again and commit the new `.out`.

What the first recording found, now as bspwm does it:
- `monitor -d` renames (`desktop_rename`, even to the same name), adds (`desktop_add`) and removes the rest (`desktop_remove`), their windows moved to the focused desktop, the focus first moving off a desktop that goes; `monitor -o` swaps position by position (`desktop_swap`); `monitor -n` reports `monitor_rename`.
- `node_geometry` for windows the layout moves (`push_geometry_changes`), not only for `node -v`/`-z` and drags.
- `node_presel ... cancel` when an insertion uses up a preselection, and `dir` then `cancel` for the one `insert_node()` makes next to a private node (`Tree::private_insertion`); `node -p cancel` reports only a preselection it removed; a rule's `split_dir`/`split_ratio` report `node_presel`; ratios print with `%lf` (`0.300000`), as does `config split_ratio`.
- `node -t` between tiled and pseudo-tiled does not restack (`set_floating()`/`set_fullscreen()` do).
- `node -g FLAG` reports only a change (`set_flag_reporting`, shared with `manage_window`).
- A monitor or desktop name that names nothing fails with `CMD: Invalid descriptor found in 'NAME'.` (`ResolveError::BadDescriptor`, bspwm's `handle_failure()`).
- `rule -r CAUSE` needs `CLASS:INSTANCE:NAME` (with `*`), as bspwm 0.9.12's `remove_rule_by_cause()`; `rule -r CLASS` removes nothing.
- `config single_monocle` refuses the value it has and otherwise puts every desktop in monocle or its own layout (`desktop_layout`).
- Monitor, desktop and node ids never collide; a monitor's first desktop is `Desktop`.
- The focus history records a desktop when it is shown, not when it is made, and not the desktop a session starts on.
