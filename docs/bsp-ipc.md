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

What is *not* done: wiring `server::Listener`/`Connection` file descriptors into an actual `calloop` event loop, since `bsp-compositor` does not exist yet to own that loop — `server`'s types are plain, non-blocking and `poll`-ready for whenever it does. A few executor operations are also deliberately deferred rather than guessed at:

- **`node --move`/`--resize`**: need floating-client geometry helpers bspwm keeps in `src/window.c`, which have no `bsp-core` port yet (scoped to the tiling tree, not floating-window geometry math).
- **Cross-desktop/cross-monitor `node --swap`**: bspwm's cross-tree swap relies on a node keeping its identity across trees; `bsp-core`'s per-tree arena `NodeId` does not (`docs/bsp-core.md`, scope), and reproducing the swap via two transplants would need `insert_node`'s exact-slot-replacement semantics rather than its anchor-based splitting. Same-tree swap (the common case) works.
- **`wm --load-state`, `wm --adopt-orphans`**: no saved-state format or orphan-window concept exists yet.
- Every `subscribe --fifo` request is parsed but the FIFO itself is never created (`mkfifo` has no safe `std` wrapper); a subscriber always gets its report/events over the request connection itself.
- The executor reports back whether a command changed anything and leaves *when* to push a fresh `report` line to `report`-subscribed connections to the caller, rather than replicating bspwm's `put_status(SBSC_MASK_REPORT)` call site by call site throughout `src/tree.c`/`src/desktop.c`/`src/monitor.c`.

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
| `FakeAdapter` | `new`/`set_class` + `Adapter` impl | An in-memory adapter for tests: a lookup table plus a record of what was closed/killed |

### `exec`

| Function | Signature | Behavior |
| --- | --- | --- |
| `execute` | `fn(&mut ExecCtx<A>, &Command) -> (Reply, Vec<Event>)` | Runs any `Command` but `Subscribe`/`Quit` (the server handles those directly) against `bsp-core`, mirroring `src/messages.c`'s `cmd_node()` … `cmd_config()` |
| `build_report` | `fn(&ExecCtx<A>) -> Report` | Builds the current `subscribe report`/`wm -g` line from live state |

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
