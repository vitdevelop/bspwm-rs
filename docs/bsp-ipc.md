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
| `adapter` | `Adapter` trait (window class/instance lookup, close/kill) and `FakeAdapter`, the the IPC roadmap's "fake adapter" |
| `exec` | Executes a parsed `Command` against a `Wm` + `NodeRegistry` + `Adapter`, producing a `Reply` and the `Event`s to broadcast |
| `server` | `Listener`/`Connection` (non-blocking Unix socket) and `Subscribers` (mask-matched event/report delivery) |

## IPC progress

Everything is implemented and tested end to end: wire framing, the full `bspc` argument grammar for every domain, selector parsing and structural resolution, report/event/JSON formatting matching bspwm's exact byte output, an executor connecting all of it to `bsp-core`, a `FakeAdapter`, a real (if compositor-less) socket server, and `bspc-rs` as a working client. `crates/bsp-ipc/tests/integration.rs` exercises the whole path — wire decode → `command::parse` → `exec::execute` → wire encode — over a real Unix socket.

`server::Listener`/`Connection` are also now wired into `bsp-compositor`'s real `calloop` event loop (`crates/bsp-compositor/src/ipc.rs`) and confirmed against a live, running compositor and a real client — see `docs/bsp-compositor.md`, Nested compositor progress. A few executor operations are still deliberately deferred rather than guessed at:

- **Cross-desktop/cross-monitor `node --swap`**: bspwm's cross-tree swap relies on a node keeping its identity across trees; `bsp-core`'s per-tree arena `NodeId` does not (`docs/bsp-core.md`, scope), and reproducing the swap via two transplants would need `insert_node`'s exact-slot-replacement semantics rather than its anchor-based splitting. Same-tree swap (the common case) works.
- **`wm --load-state`, `wm --adopt-orphans`**: no saved-state format or orphan-window concept exists yet.
- **`rule --add`'s `monitor=`/`desktop=`/`node=`/`rectangle=`/`honor_size_hints=` targets** (`command::RuleTarget`): parsed for validation and folded into `effect_raw` for a correct `rule --list` (bspwm: `src/rule.c` `list_rules()`, `"%s:%s:%s %c> %s\n"`), but not retained in structured form — only `bsp_core::rules::Rule`'s `consequence`/`one_shot` are stored. No rule is matched against a real window at all yet (`docs/bsp-compositor.md`, scope: every mapped window lands tiled, unconditionally), so there is nothing yet to apply a resolved target to; storing it structurally is deferred to when that wiring happens, since where it should live depends on how that matching step is designed (see `docs/bsp-compositor.md`'s Nested compositor progress).
- Every `subscribe --fifo` request is parsed but the FIFO itself is never created (`mkfifo` has no safe `std` wrapper); a subscriber always gets its report/events over the request connection itself.
- The executor reports back whether a command changed anything and leaves *when* to push a fresh `report` line to `report`-subscribed connections to the caller, rather than replicating bspwm's `put_status(SBSC_MASK_REPORT)` call site by call site throughout `src/tree.c`/`src/desktop.c`/`src/monitor.c`.

**`node --move`/`--resize` are implemented and live-verified**, closing the gap above: `bsp_core::tree::Tree::find_fence`/`resize_node`/`move_floating` (`docs/bsp-core.md`) port bspwm's `src/tree.c` `find_fence()` and `src/window.c` `move_client()`/`resize_client()`, verified against bspwm 0.9.12's actual source (fetched and read directly, not recalled) rather than guessed at, since several details are easy to get wrong from memory alone — notably that `--move` on a tiled node always fails (bspwm's own `move_client()` only takes that path while a live pointer drag is being tracked, which a `bspc` request never is) and that `--resize` on a tiled node adjusts an ancestor "fence" node's `split_ratio` rather than the node's own rectangle. `exec_node`'s `Move`/`Resize` arms call these and push `Event::NodeGeometry` exactly where bspwm's own `put_status(SBSC_MASK_NODE_GEOMETRY, …)` calls do: after a floating move, and after a floating (but not pseudo-tiled or tiled) resize. Confirmed live against a running compositor and two real `alacritty` clients: a tiled resize moved the shared fence (and both windows' on-screen rectangles) by exactly the given delta, a tiled move failed as bspwm's own does, and a floating move/resize (including from each of the 8 handles) produced the exact expected rectangle math. Not implemented: automatic transfer to a different monitor when a floating move's new rectangle would land under one (bspwm: `move_client()`'s `monitor_from_client`/`transfer_node` tail) — deferred alongside the cross-monitor `node --swap` gap above, a `Wm`-level cross-tree operation this pass didn't need to touch; and honoring ICCCM/`xdg_toplevel` size hints during a resize (bspwm: `apply_size_hints()`) — no such hints are tracked anywhere in this build yet (`bsp-core::node`'s own module doc comment).

**`bspc output`/`bspc input` grammar and wire plumbing are implemented** (`command::parse_output`/`parse_input`, `exec::exec_output`/`exec_input`) — not bspwm commands at all, but this project's own extensions replacing `xrandr` and `setxkbmap`/`xset r rate`/`xinput` (`docs/design.md`'s "Configuration beyond bspwm" and its the hardware backend roadmap row). `output [<name> [-m WxH@Hz] [-s SCALE] [-p X Y]]` and `input [<device> [-r HZ DELAY] [-a FACTOR]]` parse and route through `Adapter` (new default-implemented methods: `output_names`/`output_settings`/`set_output`, `input_names`/`input_settings`/`set_input`) exactly the way every other domain does — but every default returns "no known outputs/devices" or a `Reply::Fail("... not supported (no hardware ... backend yet).\n")`, since there is no real DRM output list or `libinput` device list for any backend to report yet (the hardware backend; wired for the DRM backend in Stage E, `docs/bsp-compositor.md`). Deliberately staged this way per `docs/design.md`'s roadmap: settle the wire protocol and grammar first (unit-tested against `FakeAdapter`'s defaults), wire it to real hardware once the DRM/udev backend exists to answer it (`docs/bsp-compositor.md`). Flag letters (`-m`/`-s`/`-p`/`-r`/`-a`) are this project's own choice, matching this crate's existing dash-flag style — `docs/design.md`'s own `bspc output`/`bspc input` examples are illustrative prose, not a literal CLI spec.

Selector *resolution* (not parsing, which is complete) has its own known gaps, each returning `ResolveError::Unsupported` rather than a wrong answer:

- **Focus history** (`last`, `newest`, `older`, `newer` on any selector, and `wm -d`'s `focusHistory` JSON array, always empty here): bspwm's `history.c` has no `bsp-core` counterpart yet.
- **The stacking list** (`node_stack` event, `wm -d`'s `stackingList` JSON array, always empty here): bspwm's `stack.c` has no `bsp-core` counterpart yet.
- **`pointed`** (node/monitor) and **`primary`** (monitor): need live pointer/EWMH state from `bsp-compositor`, which does not exist before the nested compositor/5.
- **`same_class`**: needs a window's class/instance name, which `bsp-core::node::Client` does not store (that belongs to the adapter's window map, `docs/design.md` Architecture) — always fails to match rather than ignoring the constraint.
- **`next`/`prev` node cycling** stays within the reference node's own desktop tree; bspwm's cross-desktop scope for these two descriptors was not confirmed from source in this step.
- **Directional selection** (`node`/`monitor` `north`/`west`/`south`/`east`) always uses bspwm's `TIGHTNESS_HIGH` default (`src/geometry.c` `on_dir_side()`); `directional_focus_tightness` is not yet a `bsp-core`/`config` setting.
- Tie-breaks in directional selection use iteration order rather than bspwm's `history_rank` (which needs focus history, above).

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
| `JsonNode::from_tree` / `JsonDesktop::from_desktop` / `JsonMonitor::from_monitor` / `JsonState::new` | `fn(...) -> Self` | Builds bspwm's exact `query -T`/`wm -d` JSON shape (field names and order) from `bsp-core` state, given adapter-supplied node ids and client class/instance names |

### `adapter`

| Function | Signature | Behavior |
| --- | --- | --- |
| `Adapter` (trait) | `window_class`/`close_window`/`kill_window` | What `exec` needs from the real window system: class/instance lookup, close, kill |
| `Adapter`'s output/input methods | `output_names`/`output_settings`/`set_output`, `input_names`/`input_settings`/`set_input` | Default-implemented as "no known outputs/devices"/"not supported"; a real hardware backend overrides them |
| `FakeAdapter` | `new`/`set_class` + `Adapter` impl | An in-memory adapter for tests: a lookup table plus a record of what was closed/killed; uses every output/input default as-is |

### `exec`

| Function | Signature | Behavior |
| --- | --- | --- |
| `execute` | `fn(&mut ExecCtx<A>, &Command) -> (Reply, Vec<Event>)` | Runs any `Command` but `Subscribe`/`Quit` (the server handles those directly) against `bsp-core`, mirroring `src/messages.c`'s `cmd_node()` … `cmd_config()` |
| `build_report` | `fn(&Wm) -> Report` | Builds the current `subscribe report`/`wm -g` line from live state |
| `exec::set_monitor_rectangle` | `fn(&mut ExecCtx<A>, usize, Rect, &mut Vec<Event>) -> usize` | Applies a new monitor rectangle (adapt tree geometry, re-arrange, `MonitorGeometry` event, `reorder_monitor`); extracted from `bspc monitor -g`, shared with the compositor's output changes. Returns the monitor's index after reordering |
| `exec_output` / `exec_input` | `fn(&mut ExecCtx<A>, Option<&str>, &[OutputAction]/&[InputAction]) -> Reply` | List/get/set over `Adapter`'s output/input methods; no hardware knowledge of its own |

### `server`

| Function | Signature | Behavior |
| --- | --- | --- |
| `Listener::bind` | `fn(&Path) -> io::Result<Self>` | Binds a non-blocking Unix socket at mode `0600`, removing a stale one first; removes it again on drop |
| `Listener::accept` | `fn(&self) -> io::Result<Option<Connection>>` | Non-blocking accept |
| `Connection::try_read_request` | `fn(&mut self) -> io::Result<Option<Vec<String>>>` | One non-blocking `recv()`-equivalent, decoded (bspwm treats one `recv()` as one whole request) |
| `Connection::send_reply` / `send_line` | `fn(&mut self, ...) -> ...` | Writes a `Reply`, or one pre-formatted `subscribe` line |
| `Subscribers::add` | `fn(&mut self, Connection, Vec<SubscriberMask>, Option<u32>, &str) -> u64` | Registers a `subscribe`d connection, sending the initial report if its mask covers `report` (bspwm: `add_subscriber()`, which counts that initial send against `--count` too) |
| `Subscribers::broadcast_event` / `broadcast_report` | `fn(&mut self, ...)` | Delivers to every subscriber whose mask matches, dropping any that fail or exhaust `--count` |

### `bspc-rs` (separate crate)

A thin client over `wire`: connects, sends the encoded request, and streams replies back to stdout/stderr as they arrive (one iteration per chunk works for both an ordinary one-reply command and `subscribe`'s open-ended stream), exiting non-zero if any chunk was a failure. `--print-socket-path` matches `bspc`. Not implemented: polling standard output for `POLLHUP` the way `bspc.c` does, so a `bspc-rs subscribe | head -1` pipeline keeps writing until its own write fails, rather than exiting the moment the reader goes away.

## Deliberate simplifications

These affect the wire format's *edge cases*, not its everyday behavior; each is small enough to not warrant a `docs/design.md` deviation row, but is recorded here per the "read the source, don't guess" rule:

- `parse_id` accepts hex (`0x…`) and decimal, not bspwm's octal (`strtol(s, &end, 0)`'s leading-zero form) — no id bspwm ever prints is octal, so no real selector needs it.
- `rule --add`'s cause escaping (`\:` inside a class/instance/name) is not implemented (bspwm: `tokenize_with_escape()`); no rule cause in practice needs a literal colon.
