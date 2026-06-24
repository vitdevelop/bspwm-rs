# Changelog

Every function added, changed or removed, newest first. The matching `docs/<crate>.md` file is updated in the same commit.

| Date | Crate | Change | Function | Summary |
| --- | --- | --- | --- | --- |
| 2026-06-25 | bsp-compositor | Added | `hotkeys::reload`, `hotkeys::build_matcher` | `SIGUSR1` re-reads sxhkdrc from scratch and replaces the matcher outright, matching sxhkd's `reload_cmd()`; live-verified (swapped sxhkdrc mid-run, confirmed the old binding stopped firing and the new one worked) |
| 2026-06-09 | bsp-compositor | Added | `bspwmrc::run` | Runs `bspwmrc` at startup, `execl`'d directly with run level `"0"`, same path and calling convention as bspwm itself; live-verified (test script receiving its `$1`, and the missing-file case logging cleanly with no error) |
| 2026-05-16 | bsp-compositor | Added | `input::is_emergency_quit` | `Ctrl+Alt+Shift+Escape` quits unconditionally, checked before `bsp-hotkeys`' matcher ever sees the event so a broken/missing sxhkdrc can't disable it; live-verified (process exits cleanly with no sxhkdrc loaded) |
| 2026-04-16 | bsp-compositor | Added | `hotkeys::init`, `filter`, `resolve_modifiers`, `run_inline`, `run_shell` | Reads sxhkdrc at startup and drives `bsp-hotkeys`' matcher from real keyboard events; live-verified end to end (both dispatch paths, a `shift`-modified chord) |
| 2026-04-16 | bsp-compositor | Changed | `ipc::execute_and_broadcast` (extracted from `ipc::on_readable`) | Factored the control socket's execute-reconcile-broadcast sequence into a function `hotkeys::run_inline` also calls, so both paths behave identically |
| 2026-04-16 | bsp-hotkeys | Added | `config::load`, `config::resolve_path` | Ties `lexer`/`expand`/`binding`/`dispatch` into one call over a whole sxhkdrc file; resolves its path matching sxhkd's own `$XDG_CONFIG_HOME`/`$HOME` lookup |
| 2026-04-08 | bsp-hotkeys | Added | `dispatch::classify` | Classifies a command as an in-process `bspc` call (tokenized, quote-aware) or a shell command; this project's own "Binding execution" extension, not a ported bspwm behavior |
| 2026-04-05 | bsp-hotkeys | Added | `matcher::Matcher::new`/`feed`/`abort_chain` | Chord-chain state machine, X11-free; full port of sxhkd's `find_hotkey()`/`match_chord()`/`abort_chain()` |
| 2026-03-22 | bsp-hotkeys | Added | `binding::parse_chain` | Parses an expanded chain string into `Chord`s (modifiers, keysym via `xkbcommon`, `buttonN`, release/replay/lock-chain); full port of sxhkd's `parse_chain()`/`parse_modifier()`/`parse_button()` |
| 2026-03-22 | bsp-hotkeys | Changed | `expand`'s internal `get_token` | Moved to a new private `token` module, now shared with `binding` (`token::get_token`), gaining the `ign`-consumed-separators output `binding`'s chain-locking needs |
| 2026-03-22 | bsp-hotkeys | Added | `expand::expand` | Expands every `{}`/range sequence in a raw (chain, command) pair, zipping both sides index for index; full port of sxhkd's `process_hotkey()`/`extract_chunks()`/`render_next()` |
| 2026-03-22 | bsp-hotkeys | Added | `lexer::parse` | Groups an sxhkdrc file's lines into raw (chain, command) pairs; full port of sxhkd's `load_config()` |
| 2026-02-25 | bsp-compositor | Changed | `shell::map_new_toplevel`, `new_toplevel`, `on_commit` | Deferred from `new_toplevel` to a toplevel's first `commit`, so `app_id`/title are reliably known; now matches `bspc rule` entries (`bsp_core::rules::match_rules`) and applies the resolved consequence; fixes a second bug found via live two-window testing — insertion anchored at the tree root instead of the desktop's actual focus once 2+ windows existed |
| 2026-02-25 | bsp-compositor | Changed | `state::State`, `State::new` | Adds `pending_toplevels: Vec<ToplevelSurface>`, holding toplevels awaiting their first commit before rule matching and mapping |
| 2026-02-25 | bsp-core | Added | `rules::match_rules` | Full `apply_rules()`-equivalent matching-and-merging loop over every rule, including its one-shot-stops-the-loop quirk |
| 2026-02-25 | bsp-core | Changed | `rules::RuleConsequence` | `manage`/`focus`/`border` are now `Option<bool>` (were plain `bool`, wrongly defaulting to `false`); adds `merge`/`should_manage`/`should_focus`/`should_border` |
| 2026-02-25 | bsp-ipc | Changed | `exec::exec_rule` (`rule --list`) | Fixes a formatting bug: the listed effect text was always empty (`"class:instance:name => \n"`); now prints the rule's actual effect string, matching |
| 2026-02-25 | bsp-core | Changed | `rules::Rule` | Adds `effect_raw: String`, carrying the verbatim `key=value` effect text so `rule --list` can print it back |
| 2026-02-14 | bsp-compositor | Added | `ipc::init` | Binds `bsp-ipc`'s control socket and drives its `Listener`/`Connection`/`Subscribers` from the calloop event loop; every request runs through `bsp_ipc::exec::execute` — live-verified end to end with `bspc-rs` against a running `alacritty` client |
| 2026-02-14 | bsp-compositor | Added | `shell::sync_wayland_from_core` | Reconciles every mapped client's `Space` position and `xdg_toplevel` size with `bsp-core`'s tree; called after every executed IPC command and after a nested-window resize |
| 2026-02-14 | bsp-compositor | Changed | `shell::map_new_toplevel` | Seeds `floating_rectangle` from the tiled slot at map time; fixes a live-tested bug where `bspc node -t floating` collapsed a window to 0×0 (bspwm has no Wayland-equivalent geometry source at map time, `src/window.c` `initialize_floating_rectangle()`) |
| 2026-02-14 | bsp-ipc | Changed | `exec::build_report` | Takes `&Wm` instead of `&ExecCtx<A>`, since it only ever read the `Wm` field |
| 2026-01-20 | bsp-compositor | Added | `render::render_frame`, `border_elements` | Damage-tracked per-frame rendering: client surfaces plus four-strip solid-color borders per node |
| 2026-01-20 | bsp-compositor | Added | `input::process_input_event`, `focus_node`, `focus_under_pointer` | Forwards winit keyboard/pointer events to the seat; click-to-focus and focus-follows-map wired to `bsp-core::tree::Tree::focus` |
| 2026-01-20 | bsp-compositor | Added | `shell::map_new_toplevel`, `unmap_toplevel`, `on_commit` | `XdgShellHandler`/`CompositorHandler` wiring: a new `xdg_toplevel` becomes a `bsp-core` client node (inserted, arranged, sized back via `configure`); a destroyed one is removed and re-arranged |
| 2026-01-20 | bsp-compositor | Added | `adapter::WindowAdapter` | `WindowId` ↔ Smithay `Window` map and `bsp_ipc::adapter::Adapter` impl (class/instance lookup, close via `xdg_toplevel::send_close`) |
| 2026-01-20 | bsp-compositor | Added | `state::State`, `insert_client` | The compositor's Smithay state: compositor/shm/output/seat/xdg-shell globals, `bsp-core::wm::Wm`, `bsp-ipc::registry::NodeRegistry` |
| 2026-01-20 | bsp-compositor | Added | `winit_backend::run` | The nested (winit) backend: opens a window, creates the Wayland socket, runs the main loop — run and confirmed against a real client (`alacritty`) inside a live Wayland session |
| 2026-01-18 | bspc-rs | Added | `main` | A working `bspc`-compatible client over `bsp-ipc::wire`: connects, sends the request, streams replies to stdout/stderr, exits non-zero on failure |
| 2026-01-18 | bsp-ipc | Added | `server::Listener` (`bind`/`accept`) | Non-blocking Unix socket bound at mode 0600, cleaned up on drop |
| 2026-01-18 | bsp-ipc | Added | `server::Connection` (`try_read_request`/`send_reply`/`send_line`) | One-`recv()`-per-request reading and reply/event writing |
| 2026-01-18 | bsp-ipc | Added | `server::Subscribers` (`add`/`broadcast_event`/`broadcast_report`) | Mask-matched delivery to `subscribe`d connections, with `--count` expiry and dead-connection pruning |
| 2026-01-18 | bsp-ipc | Added | `adapter::Adapter` trait, `adapter::FakeAdapter` | The window-system interface `exec` needs (class/instance lookup, close, kill), and an in-memory implementation for tests |
| 2026-01-18 | bsp-ipc | Added | `exec::execute` | Runs a parsed `Command` against `bsp-core` state for every domain but `subscribe`/`quit`, producing a `Reply` and `Event`s |
| 2026-01-18 | bsp-ipc | Added | `exec::build_report` | Builds the current `subscribe report`/`wm -g` line from live state |
| 2026-01-18 | bsp-ipc | Added | `report::JsonNode::from_tree` / `JsonDesktop::from_desktop` / `JsonMonitor::from_monitor` / `JsonState::new` | Builds bspwm's exact `query -T`/`wm -d` JSON shape from `bsp-core` state |
| 2026-01-18 | bsp-ipc | Added | `report::Event`, `Event::kind`, `Display` impl | One `subscribe` event type per bspwm event name, formatted byte-for-byte |
| 2026-01-18 | bsp-ipc | Added | `report::Report`, `Display` impl | The `subscribe report`/`wm -g` status line |
| 2026-01-18 | bsp-ipc | Added | `registry::NodeRegistry` (`new`/`register`/`unregister`/`relocate`/`id_of`/`lookup`) | Stable, never-reused wire node ids over `bsp-core`'s reused arena `NodeId`s |
| 2026-01-18 | bsp-ipc | Added | `command::parse` and one `parse_<domain>` per domain | Parses `bspc` arguments into a `Command` for node, desktop, monitor, query, rule, wm, config, subscribe, quit |
| 2026-01-18 | bsp-ipc | Added | `selector::resolve_node`/`resolve_desktop`/`resolve_monitor` | Resolves a parsed selector against `bsp_core::wm::Wm` (structural subset; history/pointer/primary deferred) |
| 2026-01-18 | bsp-ipc | Added | `selector::NodeSelector::parse`/`DesktopSelector::parse`/`MonitorSelector::parse` | Full `[REFERENCE#]DESCRIPTOR(.MODIFIER)*` selector grammar |
| 2026-01-18 | bsp-ipc | Added | `value` module (`parse_bool`/`parse_degree`/`parse_id`/`parse_index`, `CycleDir`, `ResizeHandle`, `AlterState`) | Small `bspc` value grammars with no `bsp-core` home |
| 2026-01-18 | bsp-ipc | Added | `wire::socket_path`/`encode_request`/`decode_request`/`Reply` | NUL-framed request/reply encoding and socket path resolution |
| 2026-01-18 | bsp-core | Changed | `rules::RuleConsequence` | Derives `PartialEq`, needed by `bsp-ipc`'s parsed `Command` types |
| 2026-01-18 | bsp-core | Added | `wm::Wm` (`new`/`add_monitor`/`remove_monitor`/`focus_monitor`/`swap_monitors`/`focused_monitor`/`monitor_index`) | Aggregates every monitor, the focused one, rules and settings into one root for `bsp-ipc` to operate on |
| 2026-01-07 | bsp-core | Added | `Rule::matches` | Matches a window's class/instance/title against a rule's patterns (`*` or exact string, not a glob) |
| 2026-01-07 | bsp-core | Added | `adapt_geometry` | Proportionally repositions floating clients when their bounding rectangle changes |
| 2026-01-07 | bsp-core | Added | `Monitor::arrange` | Computes a desktop's starting rectangle from monitor/desktop padding and gap, then lays it out |
| 2026-01-07 | bsp-core | Added | `Monitor::swap_desktops` | Swaps two desktops' positions on one monitor |
| 2026-01-07 | bsp-core | Added | `Monitor::activate_desktop` | Focuses a desktop by index |
| 2026-01-07 | bsp-core | Added | `Monitor::remove_desktop` | Removes a desktop, refocusing a neighbor |
| 2026-01-07 | bsp-core | Added | `Monitor::add_desktop` | Appends a desktop, inheriting the monitor's gap/border width |
| 2026-01-07 | bsp-core | Added | `Monitor::rename` | Renames a monitor |
| 2026-01-07 | bsp-core | Added | `Monitor::new` | Creates a monitor with no desktops |
| 2026-01-07 | bsp-core | Added | `Desktop::set_layout` | Sets tiled/monocle layout, tracking the user's choice separately from `single_monocle` overrides |
| 2026-01-07 | bsp-core | Added | `Desktop::rename` | Renames a desktop |
| 2026-01-07 | bsp-core | Added | `Desktop::new` | Creates an empty desktop with settings-derived defaults |
| 2026-01-07 | bsp-core | Added | `Tree::apply_layout` | Computes every node's rectangle for tiled/monocle layout, honoring split ratios and size constraints |
| 2026-01-07 | bsp-core | Added | `Tree::circulate_leaves` | Rotates tiled leaves forward/backward through their tree positions |
| 2026-01-07 | bsp-core | Added | `Tree::transplant_to` / `Tree::transplant_within` | Moves a node (and its subtree) to a new anchor, across trees or within one (**transplant**) |
| 2026-01-07 | bsp-core | Added | `Tree::swap_nodes` | Exchanges two nodes' tree positions (**swap**) |
| 2026-01-07 | bsp-core | Added | `Tree::adjust_ratios` | Recomputes split ratios so fences keep their pixel position under a new rectangle |
| 2026-01-07 | bsp-core | Added | `Tree::balance_tree` | Sets split ratios proportional to each side's leaf count (**balance**) |
| 2026-01-07 | bsp-core | Added | `Tree::equalize_tree` | Resets every split ratio in a subtree to the default (**equalize**) |
| 2026-01-07 | bsp-core | Added | `Tree::flip_tree` | Mirrors a subtree across an axis (**flip**) |
| 2026-01-07 | bsp-core | Added | `Tree::rotate_tree` | Rotates a subtree by 90/180/270 degrees (**rotate**) |
| 2026-01-07 | bsp-core | Added | `Tree::remove_node` / `Tree::unlink_node` / `Tree::free_node` | Removes a node from the tree and frees its subtree |
| 2026-01-07 | bsp-core | Added | `Tree::insert_node` | Inserts a node next to an anchor, splitting it (automatic scheme or preselection) (**split**) |
| 2026-01-07 | bsp-core | Added | `Tree::find_public` | Finds the best non-private leaf for redirected automatic insertion |
| 2026-01-07 | bsp-core | Added | `Tree::set_state` / `set_floating` / `set_fullscreen` | Client tiling-state transitions and their vacancy side effects |
| 2026-01-07 | bsp-core | Added | `Tree::set_layer` | Sets a client's stacking layer |
| 2026-01-07 | bsp-core | Added | `Tree::set_urgent` / `set_sticky` / `set_private` / `set_locked` / `set_marked` | Node/client flag setters |
| 2026-01-07 | bsp-core | Added | `Tree::set_hidden` | Hides/shows a node, propagating vacancy for tiled clients |
| 2026-01-07 | bsp-core | Added | `Tree::set_vacant` | Marks a node's slot vacant/occupied, propagating up and down the tree |
| 2026-01-07 | bsp-core | Added | `Tree::rebuild_constraints_from_leaves` / `rebuild_constraints_towards_root` | Recomputes minimum-size constraints after a structural change |
| 2026-01-07 | bsp-core | Added | `Tree::presel_dir` / `presel_ratio` / `cancel_presel` / `cancel_presel_in` | Preselection (pending split) management |
| 2026-01-07 | bsp-core | Added | `Tree::get_rectangle` / `node_area` | Reads a node's effective/area rectangle |
| 2026-01-07 | bsp-core | Added | `Tree::tiled_count` / `clients_count_in` / `sticky_count` / `private_count` / `locked_count` | Subtree counting helpers |
| 2026-01-07 | bsp-core | Added | `Tree::is_focusable` | Whether a subtree has a visible client |
| 2026-01-07 | bsp-core | Added | `Tree::next_leaf` / `prev_leaf` / `next_tiled_leaf` / `prev_tiled_leaf` | Leaf-order traversal |
| 2026-01-07 | bsp-core | Added | `Tree::first_extrema` / `second_extrema` | Deepest first/second-child descendant |
| 2026-01-07 | bsp-core | Added | `Tree::is_child` / `is_descendant` | Ancestry queries |
| 2026-01-07 | bsp-core | Added | `Tree::brother` / `is_first_child` / `is_second_child` | Sibling/position queries |
| 2026-01-07 | bsp-core | Added | `Tree::is_leaf` / `is_receptacle` | Leaf/receptacle queries |
| 2026-01-07 | bsp-core | Added | `Tree::new` / `new_node` / `new_client_node` / `node` / `node_mut` / `contains` | Arena construction and access |
| 2026-01-07 | bsp-core | Added | `Node::is_leaf` / `is_receptacle` / `parent` / `first_child` / `second_child` | Read-only node accessors |
| 2026-01-07 | bsp-core | Added | `Client::new` | Creates a client in its default (tiled, normal layer) state |
| 2026-01-07 | bsp-core | Added | `ClientState::is_tiled` | Whether a state counts as tiled (tiled or pseudo-tiled) |
| 2026-01-07 | bsp-core | Added | `IdGen::new` / `alloc` | Monotonic id generator for desktops and monitors |
| 2026-01-07 | bsp-core | Added | `WindowId`'s `Display` impl | Formats a window id |
| 2026-01-07 | bsp-core | Added | `Rect::new` / `area` / `right` / `bottom` / `contains_rect` / `contains_point` | Rectangle geometry |
