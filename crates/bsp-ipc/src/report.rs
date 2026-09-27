//! The `subscribe report` status line, `subscribe` events, and the JSON
//! shape `query -T` and `wm -d` print.
//!
//! bspwm: `src/subscribe.c` (`print_report()`, `put_status()`),
//! `doc/bspwm.1.asciidoc` (Events, Report Format), `src/query.c`
//! (`query_node()`, `query_desktop()`, `query_monitor()`, `query_client()`,
//! `query_presel()`, `query_rectangle()`, `query_constraints()`,
//! `query_padding()`, `query_state()`), `src/helpers.h` (the `*_CHR`/`*_STR`
//! macros this module's `Display` impls mirror).

use std::fmt;

use bsp_core::geometry::{Padding, Rect};
use bsp_core::node::{Client, ClientState, Layer};
use bsp_core::tree::{Layout, SplitType};

/// One monitor's line in the `subscribe report`/`wm -g` status line.
///
/// bspwm: `src/subscribe.c` `print_report()`; format documented in
/// `doc/bspwm.1.asciidoc`, Report Format.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportMonitor {
    /// The monitor's name.
    pub name: String,
    /// `true` if this is the focused monitor (`M` vs `m`).
    pub focused: bool,
    /// This monitor's desktops.
    pub desktops: Vec<ReportDesktop>,
    /// The focused desktop's layout, and its focused node's state/flags —
    /// `None` if the monitor holds no desktops (bspwm: `m->desk != NULL`
    /// gate around the `L`/`T`/`G` items).
    pub focused_desktop_detail: Option<ReportFocusedDesktopDetail>,
}

/// A desktop's occupancy state for the report line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportDesktopState {
    /// Has an urgent window (`u`/`U`).
    Urgent,
    /// Holds no nodes (`f`/`F`).
    Free,
    /// Holds at least one node (`o`/`O`).
    Occupied,
}

/// One desktop's item in a [`ReportMonitor`] line.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportDesktop {
    /// The desktop's name.
    pub name: String,
    /// Occupancy/urgency.
    pub state: ReportDesktopState,
    /// `true` if this is the monitor's focused desktop (uppercased letter).
    pub active: bool,
}

/// The `L`/`T`/`G` items appended after a monitor's desktop list.
#[derive(Debug, Clone, PartialEq)]
pub struct ReportFocusedDesktopDetail {
    /// The focused desktop's layout.
    pub layout: Layout,
    /// The focused node's state, if the desktop has a focused node with a
    /// client (`T@` if it has a focused node with no client — a
    /// receptacle — and nothing at all if there is no focused node).
    pub focused_node_state: Option<Option<ClientState>>,
    /// The focused node's active flags, in bspwm's fixed S/P/L/M order.
    pub focused_node_flags: ReportNodeFlags,
}

/// A node's sticky/private/locked/marked flags, for the `G` report item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReportNodeFlags {
    /// `S`.
    pub sticky: bool,
    /// `P`.
    pub private: bool,
    /// `L`.
    pub locked: bool,
    /// `M`.
    pub marked: bool,
}

impl fmt::Display for ReportNodeFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.sticky {
            write!(f, "S")?;
        }
        if self.private {
            write!(f, "P")?;
        }
        if self.locked {
            write!(f, "L")?;
        }
        if self.marked {
            write!(f, "M")?;
        }
        Ok(())
    }
}

/// A full `subscribe report` line (every monitor, in order), with the
/// configured `status_prefix` already applied.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    /// `status_prefix` (`bspc config status_prefix`), printed verbatim
    /// before the first monitor's item.
    pub prefix: String,
    /// Every monitor, in display order.
    pub monitors: Vec<ReportMonitor>,
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.prefix)?;
        let last = self.monitors.len().saturating_sub(1);
        for (i, m) in self.monitors.iter().enumerate() {
            write!(f, "{}{}", if m.focused { 'M' } else { 'm' }, m.name)?;
            for d in &m.desktops {
                let mut c = match d.state {
                    ReportDesktopState::Urgent => 'u',
                    ReportDesktopState::Free => 'f',
                    ReportDesktopState::Occupied => 'o',
                };
                if d.active {
                    c = c.to_ascii_uppercase();
                }
                write!(f, ":{c}{}", d.name)?;
            }
            if let Some(detail) = &m.focused_desktop_detail {
                let l = match detail.layout {
                    Layout::Tiled => 'T',
                    Layout::Monocle => 'M',
                };
                write!(f, ":L{l}")?;
                match detail.focused_node_state {
                    Some(Some(state)) => write!(f, ":T{}", state_chr(state))?,
                    Some(None) => write!(f, ":T@")?,
                    None => {}
                }
                if detail.focused_node_state.is_some() {
                    write!(f, ":G{}", detail.focused_node_flags)?;
                }
            }
            if i != last {
                write!(f, ":")?;
            }
        }
        writeln!(f)
    }
}

/// bspwm: `src/helpers.h` `STATE_CHR`.
fn state_chr(s: ClientState) -> char {
    match s {
        ClientState::Tiled => 'T',
        ClientState::Floating => 'F',
        ClientState::Fullscreen => '=',
        ClientState::PseudoTiled => 'P',
    }
}

/// bspwm: `src/helpers.h` `STATE_STR`.
fn state_str(s: ClientState) -> &'static str {
    match s {
        ClientState::Tiled => "tiled",
        ClientState::Floating => "floating",
        ClientState::Fullscreen => "fullscreen",
        ClientState::PseudoTiled => "pseudo_tiled",
    }
}

/// bspwm: `src/helpers.h` `LAYER_STR`.
fn layer_str(l: Layer) -> &'static str {
    match l {
        Layer::Below => "below",
        Layer::Normal => "normal",
        Layer::Above => "above",
    }
}

/// bspwm: `src/helpers.h` `SPLIT_TYPE_STR`.
fn split_type_str(t: SplitType) -> &'static str {
    match t {
        SplitType::Horizontal => "horizontal",
        SplitType::Vertical => "vertical",
    }
}

/// bspwm: `src/helpers.h` `LAYOUT_STR`.
fn layout_str(l: Layout) -> &'static str {
    match l {
        Layout::Tiled => "tiled",
        Layout::Monocle => "monocle",
    }
}

/// bspwm: `src/helpers.h` `SPLIT_DIR_STR`.
fn split_dir_str(d: bsp_core::tree::Direction) -> &'static str {
    use bsp_core::tree::Direction::*;
    match d {
        North => "north",
        West => "west",
        South => "south",
        East => "east",
    }
}

// ---- subscribe events -----------------------------------------------------

/// Which `subscribe` event a line reports, matching bspwm's event names
/// (`doc/bspwm.1.asciidoc`, Events).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)] // one variant per bspwm event name; self-explanatory
pub enum EventKind {
    MonitorAdd,
    MonitorRename,
    MonitorRemove,
    MonitorSwap,
    MonitorFocus,
    MonitorGeometry,
    DesktopAdd,
    DesktopRename,
    DesktopRemove,
    DesktopSwap,
    DesktopTransfer,
    DesktopFocus,
    DesktopActivate,
    DesktopLayout,
    NodeAdd,
    NodeRemove,
    NodeSwap,
    NodeTransfer,
    NodeFocus,
    NodePresel,
    NodeStack,
    NodeActivate,
    NodeGeometry,
    NodeState,
    NodeFlag,
    NodeLayer,
    PointerAction,
}

/// bspwm ids as printed over the wire: monitors/desktops use `bsp-core`'s
/// own stable ids; nodes use `bsp-ipc`'s registry-minted stable id (see
/// `crate::registry`).
pub type WireMonitorId = u32;
/// See [`WireMonitorId`].
pub type WireDesktopId = u32;
/// See [`WireMonitorId`].
pub type WireNodeId = u32;

/// A single `subscribe` event line, one variant per bspwm event name.
///
/// bspwm: `doc/bspwm.1.asciidoc`, Events.
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)] // fields mirror each event's documented arguments
pub enum Event {
    MonitorAdd {
        id: WireMonitorId,
        name: String,
        geometry: Rect,
    },
    MonitorRename {
        id: WireMonitorId,
        old_name: String,
        new_name: String,
    },
    MonitorRemove {
        id: WireMonitorId,
    },
    MonitorSwap {
        src: WireMonitorId,
        dst: WireMonitorId,
    },
    MonitorFocus {
        id: WireMonitorId,
    },
    MonitorGeometry {
        id: WireMonitorId,
        geometry: Rect,
    },
    DesktopAdd {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
        name: String,
    },
    DesktopRename {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
        old_name: String,
        new_name: String,
    },
    DesktopRemove {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
    },
    DesktopSwap {
        src_monitor: WireMonitorId,
        src_desktop: WireDesktopId,
        dst_monitor: WireMonitorId,
        dst_desktop: WireDesktopId,
    },
    DesktopTransfer {
        src_monitor: WireMonitorId,
        src_desktop: WireDesktopId,
        dst_monitor: WireMonitorId,
    },
    DesktopFocus {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
    },
    DesktopActivate {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
    },
    DesktopLayout {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
        layout: Layout,
    },
    NodeAdd {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
        ip_id: WireNodeId,
        node: WireNodeId,
    },
    NodeRemove {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
        node: WireNodeId,
    },
    NodeSwap {
        src_monitor: WireMonitorId,
        src_desktop: WireDesktopId,
        src_node: WireNodeId,
        dst_monitor: WireMonitorId,
        dst_desktop: WireDesktopId,
        dst_node: WireNodeId,
    },
    NodeTransfer {
        src_monitor: WireMonitorId,
        src_desktop: WireDesktopId,
        src_node: WireNodeId,
        dst_monitor: WireMonitorId,
        dst_desktop: WireDesktopId,
        dst_node: WireNodeId,
    },
    NodeFocus {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
        node: WireNodeId,
    },
    NodeActivate {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
        node: WireNodeId,
    },
    NodePresel {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
        node: WireNodeId,
        detail: PreselDetail,
    },
    NodeStack {
        node: WireNodeId,
        above: bool,
        sibling: WireNodeId,
    },
    NodeGeometry {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
        node: WireNodeId,
        geometry: Rect,
    },
    NodeState {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
        node: WireNodeId,
        state: ClientState,
        on: bool,
    },
    NodeFlag {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
        node: WireNodeId,
        flag: &'static str,
        on: bool,
    },
    NodeLayer {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
        node: WireNodeId,
        layer: Layer,
    },
    PointerAction {
        monitor: WireMonitorId,
        desktop: WireDesktopId,
        node: WireNodeId,
        action: &'static str,
        phase: PointerPhase,
    },
}

/// `node_presel`'s trailing `dir DIR|ratio RATIO|cancel`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PreselDetail {
    /// `dir DIR`.
    Dir(bsp_core::tree::Direction),
    /// `ratio RATIO`.
    Ratio(f64),
    /// `cancel`.
    Cancel,
}

/// `pointer_action`'s trailing `begin|end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerPhase {
    /// `begin`.
    Begin,
    /// `end`.
    End,
}

impl Event {
    /// The [`EventKind`] this event belongs to, for `subscribe` mask
    /// matching.
    pub fn kind(&self) -> EventKind {
        match self {
            Event::MonitorAdd { .. } => EventKind::MonitorAdd,
            Event::MonitorRename { .. } => EventKind::MonitorRename,
            Event::MonitorRemove { .. } => EventKind::MonitorRemove,
            Event::MonitorSwap { .. } => EventKind::MonitorSwap,
            Event::MonitorFocus { .. } => EventKind::MonitorFocus,
            Event::MonitorGeometry { .. } => EventKind::MonitorGeometry,
            Event::DesktopAdd { .. } => EventKind::DesktopAdd,
            Event::DesktopRename { .. } => EventKind::DesktopRename,
            Event::DesktopRemove { .. } => EventKind::DesktopRemove,
            Event::DesktopSwap { .. } => EventKind::DesktopSwap,
            Event::DesktopTransfer { .. } => EventKind::DesktopTransfer,
            Event::DesktopFocus { .. } => EventKind::DesktopFocus,
            Event::DesktopActivate { .. } => EventKind::DesktopActivate,
            Event::DesktopLayout { .. } => EventKind::DesktopLayout,
            Event::NodeAdd { .. } => EventKind::NodeAdd,
            Event::NodeRemove { .. } => EventKind::NodeRemove,
            Event::NodeSwap { .. } => EventKind::NodeSwap,
            Event::NodeTransfer { .. } => EventKind::NodeTransfer,
            Event::NodeFocus { .. } => EventKind::NodeFocus,
            Event::NodeActivate { .. } => EventKind::NodeActivate,
            Event::NodePresel { .. } => EventKind::NodePresel,
            Event::NodeStack { .. } => EventKind::NodeStack,
            Event::NodeGeometry { .. } => EventKind::NodeGeometry,
            Event::NodeState { .. } => EventKind::NodeState,
            Event::NodeFlag { .. } => EventKind::NodeFlag,
            Event::NodeLayer { .. } => EventKind::NodeLayer,
            Event::PointerAction { .. } => EventKind::PointerAction,
        }
    }
}

fn fmt_geometry(f: &mut fmt::Formatter<'_>, r: Rect) -> fmt::Result {
    write!(f, "{}x{}+{}+{}", r.width, r.height, r.x, r.y)
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::MonitorAdd { id, name, geometry } => {
                write!(f, "monitor_add 0x{id:08X} {name} ")?;
                fmt_geometry(f, *geometry)
            }
            Event::MonitorRename { id, old_name, new_name } => {
                write!(f, "monitor_rename 0x{id:08X} {old_name} {new_name}")
            }
            Event::MonitorRemove { id } => write!(f, "monitor_remove 0x{id:08X}"),
            Event::MonitorSwap { src, dst } => {
                write!(f, "monitor_swap 0x{src:08X} 0x{dst:08X}")
            }
            Event::MonitorFocus { id } => write!(f, "monitor_focus 0x{id:08X}"),
            Event::MonitorGeometry { id, geometry } => {
                write!(f, "monitor_geometry 0x{id:08X} ")?;
                fmt_geometry(f, *geometry)
            }
            Event::DesktopAdd { monitor, desktop, name } => {
                write!(f, "desktop_add 0x{monitor:08X} 0x{desktop:08X} {name}")
            }
            Event::DesktopRename { monitor, desktop, old_name, new_name } => {
                write!(f, "desktop_rename 0x{monitor:08X} 0x{desktop:08X} {old_name} {new_name}")
            }
            Event::DesktopRemove { monitor, desktop } => {
                write!(f, "desktop_remove 0x{monitor:08X} 0x{desktop:08X}")
            }
            Event::DesktopSwap { src_monitor, src_desktop, dst_monitor, dst_desktop } => write!(
                f,
                "desktop_swap 0x{src_monitor:08X} 0x{src_desktop:08X} 0x{dst_monitor:08X} 0x{dst_desktop:08X}"
            ),
            Event::DesktopTransfer { src_monitor, src_desktop, dst_monitor } => write!(
                f,
                "desktop_transfer 0x{src_monitor:08X} 0x{src_desktop:08X} 0x{dst_monitor:08X}"
            ),
            Event::DesktopFocus { monitor, desktop } => {
                write!(f, "desktop_focus 0x{monitor:08X} 0x{desktop:08X}")
            }
            Event::DesktopActivate { monitor, desktop } => {
                write!(f, "desktop_activate 0x{monitor:08X} 0x{desktop:08X}")
            }
            Event::DesktopLayout { monitor, desktop, layout } => {
                write!(f, "desktop_layout 0x{monitor:08X} 0x{desktop:08X} {}", layout_str(*layout))
            }
            Event::NodeAdd { monitor, desktop, ip_id, node } => write!(
                f,
                "node_add 0x{monitor:08X} 0x{desktop:08X} 0x{ip_id:08X} 0x{node:08X}"
            ),
            Event::NodeRemove { monitor, desktop, node } => {
                write!(f, "node_remove 0x{monitor:08X} 0x{desktop:08X} 0x{node:08X}")
            }
            Event::NodeSwap { src_monitor, src_desktop, src_node, dst_monitor, dst_desktop, dst_node } => write!(
                f,
                "node_swap 0x{src_monitor:08X} 0x{src_desktop:08X} 0x{src_node:08X} 0x{dst_monitor:08X} 0x{dst_desktop:08X} 0x{dst_node:08X}"
            ),
            Event::NodeTransfer { src_monitor, src_desktop, src_node, dst_monitor, dst_desktop, dst_node } => write!(
                f,
                "node_transfer 0x{src_monitor:08X} 0x{src_desktop:08X} 0x{src_node:08X} 0x{dst_monitor:08X} 0x{dst_desktop:08X} 0x{dst_node:08X}"
            ),
            Event::NodeFocus { monitor, desktop, node } => {
                write!(f, "node_focus 0x{monitor:08X} 0x{desktop:08X} 0x{node:08X}")
            }
            Event::NodeActivate { monitor, desktop, node } => {
                write!(f, "node_activate 0x{monitor:08X} 0x{desktop:08X} 0x{node:08X}")
            }
            Event::NodePresel { monitor, desktop, node, detail } => {
                write!(f, "node_presel 0x{monitor:08X} 0x{desktop:08X} 0x{node:08X} ")?;
                match detail {
                    PreselDetail::Dir(d) => write!(f, "dir {}", split_dir_str(*d)),
                    PreselDetail::Ratio(r) => write!(f, "ratio {r:.6}"),
                    PreselDetail::Cancel => write!(f, "cancel"),
                }
            }
            Event::NodeStack { node, above, sibling } => {
                write!(f, "node_stack 0x{node:08X} {} 0x{sibling:08X}", if *above { "above" } else { "below" })
            }
            Event::NodeGeometry { monitor, desktop, node, geometry } => {
                write!(f, "node_geometry 0x{monitor:08X} 0x{desktop:08X} 0x{node:08X} ")?;
                fmt_geometry(f, *geometry)
            }
            Event::NodeState { monitor, desktop, node, state, on } => write!(
                f,
                "node_state 0x{monitor:08X} 0x{desktop:08X} 0x{node:08X} {} {}",
                state_str(*state),
                if *on { "on" } else { "off" }
            ),
            Event::NodeFlag { monitor, desktop, node, flag, on } => write!(
                f,
                "node_flag 0x{monitor:08X} 0x{desktop:08X} 0x{node:08X} {flag} {}",
                if *on { "on" } else { "off" }
            ),
            Event::NodeLayer { monitor, desktop, node, layer } => {
                write!(f, "node_layer 0x{monitor:08X} 0x{desktop:08X} 0x{node:08X} {}", layer_str(*layer))
            }
            Event::PointerAction { monitor, desktop, node, action, phase } => write!(
                f,
                "pointer_action 0x{monitor:08X} 0x{desktop:08X} 0x{node:08X} {action} {}",
                match phase {
                    PointerPhase::Begin => "begin",
                    PointerPhase::End => "end",
                }
            ),
        }
    }
}

// ---- JSON ------------------------------------------------------------------

/// `query -T`/`wm -d`'s JSON rectangle shape.
///
/// bspwm: `src/query.c` `query_rectangle()`.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct JsonRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl From<Rect> for JsonRect {
    fn from(r: Rect) -> Self {
        Self {
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        }
    }
}

/// bspwm: `src/query.c` `query_padding()`.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct JsonPadding {
    top: i32,
    right: i32,
    bottom: i32,
    left: i32,
}

impl From<Padding> for JsonPadding {
    fn from(p: Padding) -> Self {
        Self {
            top: p.top,
            right: p.right,
            bottom: p.bottom,
            left: p.left,
        }
    }
}

/// bspwm: `src/query.c` `query_constraints()`.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct JsonConstraints {
    min_width: i32,
    min_height: i32,
}

impl From<bsp_core::tree::Constraints> for JsonConstraints {
    fn from(c: bsp_core::tree::Constraints) -> Self {
        Self {
            min_width: c.min_width,
            min_height: c.min_height,
        }
    }
}

/// bspwm: `src/query.c` `query_presel()`.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct JsonPresel {
    #[serde(rename = "splitDir")]
    split_dir: &'static str,
    #[serde(rename = "splitRatio")]
    split_ratio: f64,
}

impl From<bsp_core::tree::Presel> for JsonPresel {
    fn from(p: bsp_core::tree::Presel) -> Self {
        Self {
            split_dir: split_dir_str(p.split_dir),
            split_ratio: p.split_ratio,
        }
    }
}

/// bspwm: `src/query.c` `query_client()`. `className`/`instanceName` are
/// supplied by the caller (the adapter's window metadata, `bsp-core` does
/// not store them — see `docs/bsp-ipc.md`, IPC scope).
#[derive(Debug, Clone, serde::Serialize)]
pub struct JsonClient {
    #[serde(rename = "className")]
    class_name: String,
    #[serde(rename = "instanceName")]
    instance_name: String,
    #[serde(rename = "borderWidth")]
    border_width: i32,
    state: &'static str,
    #[serde(rename = "lastState")]
    last_state: &'static str,
    layer: &'static str,
    #[serde(rename = "lastLayer")]
    last_layer: &'static str,
    urgent: bool,
    shown: bool,
    #[serde(rename = "tiledRectangle")]
    tiled_rectangle: JsonRect,
    #[serde(rename = "floatingRectangle")]
    floating_rectangle: JsonRect,
}

impl JsonClient {
    /// Builds the JSON shape for `c`, given the class/instance name the
    /// adapter reports for its window (`bsp-compositor`'s job once it
    /// exists; the fake adapter in tests supplies its own).
    /// `shown` is whether the window's desktop is the one its monitor shows
    /// (bspwm's `client_t.shown`, set by `show_node()`/`hide_node()`; a hidden
    /// window on a shown desktop counts as shown).
    pub fn new(c: &Client, class_name: String, instance_name: String, shown: bool) -> Self {
        Self {
            class_name,
            instance_name,
            border_width: c.border_width,
            state: state_str(c.state),
            last_state: state_str(c.last_state),
            layer: layer_str(c.layer),
            last_layer: layer_str(c.last_layer),
            urgent: c.urgent,
            shown,
            tiled_rectangle: c.tiled_rectangle.into(),
            floating_rectangle: c.floating_rectangle.into(),
        }
    }
}

/// bspwm: `src/query.c` `query_node()`. `id` is `bsp-ipc`'s registry-minted
/// stable id (see `crate::registry`), not `bsp-core`'s arena `NodeId`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct JsonNode {
    id: WireNodeId,
    #[serde(rename = "splitType")]
    split_type: &'static str,
    #[serde(rename = "splitRatio")]
    split_ratio: f64,
    vacant: bool,
    hidden: bool,
    sticky: bool,
    private: bool,
    locked: bool,
    marked: bool,
    presel: Option<JsonPresel>,
    rectangle: JsonRect,
    constraints: JsonConstraints,
    #[serde(rename = "firstChild")]
    first_child: Option<Box<JsonNode>>,
    #[serde(rename = "secondChild")]
    second_child: Option<Box<JsonNode>>,
    client: Option<JsonClient>,
}

impl JsonNode {
    /// Builds the JSON shape for the subtree rooted at `id`, recursively.
    ///
    /// `node_id` maps an arena [`bsp_core::id::NodeId`] within `desktop` to
    /// its wire-stable id (the executor looks this up in
    /// `crate::registry::NodeRegistry`); `client_names` supplies a leaf's
    /// class/instance name, which `bsp-core` does not store (the
    /// adapter's window metadata, `docs/bsp-ipc.md` IPC scope).
    pub fn from_tree(
        tree: &bsp_core::tree::Tree,
        desktop: bsp_core::id::DesktopId,
        id: bsp_core::id::NodeId,
        shown: bool,
        node_id: &impl Fn(bsp_core::id::DesktopId, bsp_core::id::NodeId) -> WireNodeId,
        client_names: &impl Fn(bsp_core::id::WindowId) -> (String, String),
    ) -> JsonNode {
        let node = tree.node(id);
        let client = node.client.as_ref().map(|c| {
            let (class_name, instance_name) = client_names(c.window);
            JsonClient::new(c, class_name, instance_name, shown)
        });
        JsonNode {
            id: node_id(desktop, id),
            split_type: split_type_str(node.split_type),
            split_ratio: node.split_ratio,
            vacant: node.vacant,
            hidden: node.hidden,
            sticky: node.sticky,
            private: node.private,
            locked: node.locked,
            marked: node.marked,
            presel: node.presel.map(JsonPresel::from),
            rectangle: node.rect.into(),
            constraints: node.constraints.into(),
            first_child: node
                .first_child()
                .map(|c| Box::new(JsonNode::from_tree(tree, desktop, c, shown, node_id, client_names))),
            second_child: node
                .second_child()
                .map(|c| Box::new(JsonNode::from_tree(tree, desktop, c, shown, node_id, client_names))),
            client,
        }
    }
}

/// bspwm: `src/query.c` `query_desktop()`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct JsonDesktop {
    name: String,
    id: WireDesktopId,
    layout: &'static str,
    #[serde(rename = "userLayout")]
    user_layout: &'static str,
    #[serde(rename = "windowGap")]
    window_gap: i32,
    #[serde(rename = "borderWidth")]
    border_width: i32,
    #[serde(rename = "focusedNodeId")]
    focused_node_id: WireNodeId,
    padding: JsonPadding,
    root: Option<JsonNode>,
}

impl JsonDesktop {
    /// Builds the JSON shape for `d`. See [`JsonNode::from_tree`] for
    /// `shown`/`node_id`/`client_names`.
    pub fn from_desktop(
        d: &bsp_core::desktop::Desktop,
        shown: bool,
        node_id: &impl Fn(bsp_core::id::DesktopId, bsp_core::id::NodeId) -> WireNodeId,
        client_names: &impl Fn(bsp_core::id::WindowId) -> (String, String),
    ) -> JsonDesktop {
        JsonDesktop {
            name: d.name.clone(),
            id: d.id.0,
            layout: layout_str(d.layout),
            user_layout: layout_str(d.user_layout),
            window_gap: d.window_gap,
            border_width: d.border_width,
            focused_node_id: d.tree.focus.map_or(0, |n| node_id(d.id, n)),
            padding: d.padding.into(),
            root: d
                .tree
                .root
                .map(|r| JsonNode::from_tree(&d.tree, d.id, r, shown, node_id, client_names)),
        }
    }
}

/// bspwm: `src/query.c` `query_monitor()`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct JsonMonitor {
    name: String,
    id: WireMonitorId,
    #[serde(rename = "randrId")]
    randr_id: u32,
    wired: bool,
    #[serde(rename = "stickyCount")]
    sticky_count: u32,
    #[serde(rename = "windowGap")]
    window_gap: i32,
    #[serde(rename = "borderWidth")]
    border_width: i32,
    #[serde(rename = "focusedDesktopId")]
    focused_desktop_id: WireDesktopId,
    padding: JsonPadding,
    rectangle: JsonRect,
    desktops: Vec<JsonDesktop>,
}

impl JsonMonitor {
    /// Builds the JSON shape for `m`. `randrId` is always `0` (there is no
    /// RandR); `wired` is whether an output shows the monitor. See [`JsonNode::from_tree`] for
    /// `node_id`/`client_names`.
    pub fn from_monitor(
        m: &bsp_core::monitor::Monitor,
        node_id: &impl Fn(bsp_core::id::DesktopId, bsp_core::id::NodeId) -> WireNodeId,
        client_names: &impl Fn(bsp_core::id::WindowId) -> (String, String),
    ) -> JsonMonitor {
        JsonMonitor {
            name: m.name.clone(),
            id: m.id.0,
            randr_id: 0,
            wired: m.wired,
            sticky_count: m.sticky_count(),
            window_gap: m.window_gap,
            border_width: m.border_width,
            focused_desktop_id: m.focused.map_or(0, |i| m.desktops[i].id.0),
            padding: m.padding.into(),
            rectangle: m.rectangle.into(),
            desktops: m
                .desktops
                .iter()
                .enumerate()
                .map(|(i, d)| JsonDesktop::from_desktop(d, m.focused == Some(i), node_id, client_names))
                .collect(),
        }
    }
}

/// bspwm: `src/query.c` `query_state()` (`wm -d`). `primaryMonitorId`
/// (present only when a primary monitor is set) and `eventSubscribers`
/// (present only with active `subscribe` connections) are left out: this
/// build tracks neither a primary monitor nor a queryable subscriber list
/// yet (`docs/bsp-ipc.md`, IPC scope).
#[derive(Debug, Clone, serde::Serialize)]
pub struct JsonState {
    #[serde(rename = "focusedMonitorId")]
    focused_monitor_id: WireMonitorId,
    #[serde(rename = "clientsCount")]
    clients_count: i32,
    monitors: Vec<JsonMonitor>,
    #[serde(rename = "focusHistory")]
    focus_history: Vec<serde_json::Value>,
    #[serde(rename = "stackingList")]
    stacking_list: Vec<serde_json::Value>,
}

impl JsonState {
    /// Builds the JSON shape for `wm -d`. `focused_monitor_id` is `0` if
    /// no monitor is focused (bspwm never reaches this state — `mon` is
    /// always valid once a monitor exists — but an empty `Wm` can appear
    /// in tests). `focusHistory` lists `wm.history` oldest first;
    /// `stackingList` is the wire ids in stacking order, bottom first (`stacking`).
    pub fn new(
        wm: &bsp_core::wm::Wm,
        clients_count: i32,
        stacking: Vec<WireNodeId>,
        node_id: &impl Fn(bsp_core::id::DesktopId, bsp_core::id::NodeId) -> WireNodeId,
        client_names: &impl Fn(bsp_core::id::WindowId) -> (String, String),
    ) -> JsonState {
        JsonState {
            focused_monitor_id: wm.focused_monitor().map_or(0, |m| m.id.0),
            clients_count,
            monitors: wm
                .monitors
                .iter()
                .map(|m| JsonMonitor::from_monitor(m, node_id, client_names))
                .collect(),
            focus_history: wm
                .history
                .locations()
                .map(|l| {
                    let node = wm
                        .monitors
                        .iter()
                        .flat_map(|m| m.desktops.iter())
                        .find(|d| d.id == l.desktop)
                        .and_then(|d| {
                            let mut f = d.tree.first_extrema(d.tree.root);
                            while let Some(n) = f {
                                if d.tree.node(n).client.as_ref().is_some_and(|c| Some(c.window) == l.node) {
                                    return Some(node_id(d.id, n));
                                }
                                f = d.tree.next_leaf(Some(n), d.tree.root);
                            }
                            None
                        })
                        .unwrap_or(0);
                    serde_json::json!({"monitorId": l.monitor.0, "desktopId": l.desktop.0, "nodeId": node})
                })
                .collect(),
            stacking_list: stacking.into_iter().map(serde_json::Value::from).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_formats_monitors_desktops_and_focused_detail() {
        let report = Report {
            prefix: String::new(),
            monitors: vec![ReportMonitor {
                name: "eDP-1".to_string(),
                focused: true,
                desktops: vec![
                    ReportDesktop {
                        name: "I".to_string(),
                        state: ReportDesktopState::Occupied,
                        active: true,
                    },
                    ReportDesktop {
                        name: "II".to_string(),
                        state: ReportDesktopState::Free,
                        active: false,
                    },
                ],
                focused_desktop_detail: Some(ReportFocusedDesktopDetail {
                    layout: Layout::Tiled,
                    focused_node_state: Some(Some(ClientState::Tiled)),
                    focused_node_flags: ReportNodeFlags {
                        sticky: true,
                        marked: true,
                        ..Default::default()
                    },
                }),
            }],
        };
        assert_eq!(report.to_string(), "MeDP-1:OI:fII:LT:TT:GSM\n");
    }

    #[test]
    fn report_status_prefix_is_prepended_verbatim() {
        let report = Report {
            prefix: "W".to_string(),
            monitors: vec![],
        };
        assert_eq!(report.to_string(), "W\n");
    }

    #[test]
    fn node_add_event_formats_like_bspwm() {
        let e = Event::NodeAdd {
            monitor: 1,
            desktop: 2,
            ip_id: 3,
            node: 4,
        };
        assert_eq!(
            e.to_string(),
            "node_add 0x00000001 0x00000002 0x00000003 0x00000004"
        );
        assert_eq!(e.kind(), EventKind::NodeAdd);
    }

    #[test]
    fn monitor_geometry_event_formats_wxh_plus_x_plus_y() {
        let e = Event::MonitorGeometry {
            id: 1,
            geometry: Rect::new(0, 0, 1920, 1080),
        };
        assert_eq!(e.to_string(), "monitor_geometry 0x00000001 1920x1080+0+0");
    }

    #[test]
    fn node_state_event_formats_state_and_on_off() {
        let e = Event::NodeState {
            monitor: 1,
            desktop: 1,
            node: 1,
            state: ClientState::Floating,
            on: true,
        };
        assert_eq!(
            e.to_string(),
            "node_state 0x00000001 0x00000001 0x00000001 floating on"
        );
    }

    #[test]
    fn json_rect_serializes_with_bspwm_field_order() {
        let r: JsonRect = Rect::new(1, 2, 3, 4).into();
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"x":1,"y":2,"width":3,"height":4}"#
        );
    }

    #[test]
    fn json_node_serializes_a_leaf_client() {
        use bsp_core::id::{DesktopId, WindowId};
        use bsp_core::node::Client;
        use bsp_core::settings::Settings;
        use bsp_core::tree::Tree;

        let settings = Settings::default();
        let mut tree = Tree::new();
        let n = tree.new_client_node(&settings, Client::new(WindowId(1), 1));
        tree.insert_node(&settings, n, None);

        let desktop = DesktopId(1);
        let node_id = |_: DesktopId, _: bsp_core::id::NodeId| 42u32;
        let client_names = |_: WindowId| ("Foo".to_string(), "bar".to_string());
        let json = JsonNode::from_tree(&tree, desktop, n, true, &node_id, &client_names);
        let s = serde_json::to_string(&json).unwrap();
        assert!(s.starts_with(r#"{"id":42,"splitType":"vertical""#));
        assert!(s.contains(r#""className":"Foo""#));
        assert!(s.contains(r#""firstChild":null"#));
    }
}
