//! Parsing `bspc` arguments (after the domain word) into a [`Command`].
//!
//! bspwm: `src/messages.c` (`process_message()`, `cmd_node()`,
//! `cmd_desktop()`, `cmd_monitor()`, `cmd_query()`, `cmd_rule()`,
//! `cmd_wm()`, `cmd_subscribe()`, `cmd_quit()`, `cmd_config()`), and
//! `doc/bspwm.1.asciidoc`'s Domains section. Every flag below is checked
//! against both spellings (`-f`/`--focus`) exactly as bspwm's `streq()`
//! chains do.

use crate::selector::{DesktopModifiers, DesktopSelector, MonitorModifiers, MonitorSelector, NodeModifiers, NodeSelector};
use crate::value::{parse_bool, parse_degree, parse_index, AlterState, CycleDir, ResizeHandle};
use bsp_core::node::{ClientState, Layer};
use bsp_core::rules::RuleConsequence;
use bsp_core::tree::{CirculateDir, Direction, FlipAxis, Layout, SplitType};

/// Why a `bspc` argument list could not be parsed. `command` is the domain
/// word and, for a flag error, the flag it was found on — matching
/// bspwm's own `fail(rsp, "<command>: ...")` prefix so a `bsp-ipc` error
/// reads the same as bspwm's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// e.g. `"node -f"`, mirroring bspwm's error-message prefix.
    pub command: String,
    /// The problem, already formatted as bspwm would print it (ending
    /// in `.\n`) or empty (bspwm's `fail(rsp, "")` for a runtime-only
    /// failure has no message at parse time — never produced here, since
    /// every case this type is used for is a parse-time syntax problem).
    pub message: String,
}

impl ParseError {
    fn new(command: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            message: message.into(),
        }
    }
    fn missing_arguments(command: &str) -> Self {
        Self::new(command, format!("{command}: Missing arguments.\n"))
    }
    fn missing_commands(command: &str) -> Self {
        Self::new(command, format!("{command}: Missing commands.\n"))
    }
    fn not_enough_arguments(command: &str, flag: &str) -> Self {
        Self::new(
            command,
            format!("{command} {flag}: Not enough arguments.\n"),
        )
    }
    fn invalid_argument(command: &str, flag: &str, value: &str) -> Self {
        Self::new(
            command,
            format!("{command} {flag}: Invalid argument: '{value}'.\n"),
        )
    }
    fn unknown_command(command: &str, value: &str) -> Self {
        Self::new(command, format!("{command}: Unknown command: '{value}'.\n"))
    }
    fn unknown_domain(value: &str) -> Self {
        Self::new("", format!("Unknown domain or command: '{value}'.\n"))
    }
}

/// A fully parsed `bspc` request, one variant per domain.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `node [NODE_SEL] COMMANDS`.
    Node {
        /// The selected node (defaults to `focused` at execution time).
        selector: Option<NodeSelector>,
        /// Actions applied in order.
        actions: Vec<NodeAction>,
    },
    /// `desktop [DESKTOP_SEL] COMMANDS`.
    Desktop {
        /// The selected desktop (defaults to `focused`).
        selector: Option<DesktopSelector>,
        /// Actions applied in order.
        actions: Vec<DesktopAction>,
    },
    /// `monitor [MONITOR_SEL] COMMANDS`.
    Monitor {
        /// The selected monitor (defaults to `focused`).
        selector: Option<MonitorSelector>,
        /// Actions applied in order.
        actions: Vec<MonitorAction>,
    },
    /// `query COMMANDS [OPTIONS]`.
    Query(QueryCommand),
    /// `rule COMMANDS`.
    Rule(Vec<RuleAction>),
    /// `wm COMMANDS`.
    Wm(Vec<WmAction>),
    /// `subscribe [OPTIONS] (all|report|monitor|desktop|node|...)*`.
    Subscribe {
        /// `-f`/`--fifo`.
        fifo: bool,
        /// `-c`/`--count`.
        count: Option<u32>,
        /// Event categories to stream; empty means `report` only (bspwm:
        /// `cmd_subscribe()`, `field == 0` defaults to `SBSC_MASK_REPORT`).
        masks: Vec<SubscriberMask>,
    },
    /// `quit [<status>]`.
    Quit(Option<i32>),
    /// `config [-m SEL|-d SEL|-n SEL] <setting> [<value>]`.
    Config(ConfigCommand),
    /// `output [<name> [COMMANDS]]` — bspwm-rs's own extension (not
    /// bspwm), replacing `xrandr` (`docs/design.md`'s "Configuration
    /// beyond bspwm"). The hardware backend (real hardware): a no-op `Adapter`
    /// default answers this grammar today; `docs/bsp-compositor.md`
    /// tracks when the DRM backend implements it for real.
    Output {
        /// The output name (a DRM connector name on real hardware), or
        /// `None` to list every known output.
        name: Option<String>,
        /// Actions applied in order; empty with `name: Some(_)` means
        /// "report that output's current settings".
        actions: Vec<OutputAction>,
    },
    /// `input [<device> [COMMANDS]]` — bspwm-rs's own extension (not
    /// bspwm), replacing `setxkbmap`/`xset r rate`/`xinput`
    /// (`docs/design.md`'s "Configuration beyond bspwm"). Same the hardware backend
    /// status as `Output` above.
    Input {
        /// `"keyboard"` or a specific pointer/device name, or `None` to
        /// list every known input device.
        device: Option<String>,
        /// Actions applied in order; empty with `device: Some(_)` means
        /// "report that device's current settings".
        actions: Vec<InputAction>,
    },
}

impl Command {
    /// Whether the command only reads (`query`, `config KEY`, `wm -d`/`-g`):
    /// when it also reported no event, nothing changed, and the compositor
    /// neither reconciles nor sends a report line.
    pub fn is_read_only(&self) -> bool {
        match self {
            Command::Query(_) => true,
            Command::Config(c) => c.value.is_none(),
            Command::Wm(actions) => actions.iter().all(|a| matches!(a, WmAction::DumpState | WmAction::GetStatus)),
            _ => false,
        }
    }
}

// ---- node ----------------------------------------------------------------

/// `node --to-desktop`/`--to-monitor`/`--to-node`/`--swap`'s `--follow`.
pub type Follow = bool;

/// One `bspc node` command.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeAction {
    /// `-f`, `--focus`.
    Focus(Option<NodeSelector>),
    /// `-a`, `--activate`.
    Activate(Option<NodeSelector>),
    /// `-d`, `--to-desktop`.
    ToDesktop(DesktopSelector, Follow),
    /// `-m`, `--to-monitor`.
    ToMonitor(MonitorSelector, Follow),
    /// `-n`, `--to-node`.
    ToNode(NodeSelector, Follow),
    /// `-s`, `--swap`.
    Swap(NodeSelector, Follow),
    /// `-p`, `--presel-dir`.
    PreselDir(PreselDirArg),
    /// `-o`, `--presel-ratio`.
    PreselRatio(f64),
    /// `-v`, `--move`.
    Move(i32, i32),
    /// `-z`, `--resize`.
    Resize(ResizeHandle, i32, i32),
    /// `-y`, `--type`.
    SetSplitType(SplitTypeArg),
    /// `-r`, `--ratio`.
    SetRatio(RatioArg),
    /// `-R`, `--rotate`.
    Rotate(i32),
    /// `-F`, `--flip`.
    Flip(FlipAxis),
    /// `-E`, `--equalize`.
    Equalize,
    /// `-B`, `--balance`.
    Balance,
    /// `-C`, `--circulate`.
    Circulate(CirculateDir),
    /// `-i`, `--insert-receptacle`.
    InsertReceptacle,
    /// `-t`, `--state`.
    SetState(StateArg),
    /// `-g`, `--flag`.
    SetFlag(NodeFlagKey, AlterState),
    /// `-l`, `--layer`.
    SetLayer(Layer),
    /// `-c`, `--close`. Must be the last action.
    Close,
    /// `-k`, `--kill`. Must be the last action.
    Kill,
}

/// `-p`/`--presel-dir`'s argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreselDirArg {
    /// `cancel`.
    Cancel,
    /// A direction, and whether it was `~`-prefixed (cancel if the current
    /// presel direction already matches).
    Set(Direction, bool),
}

/// `-y`/`--type`'s argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitTypeArg {
    /// `next`/`prev`: toggle between horizontal and vertical.
    Cycle,
    /// An explicit split type.
    Set(SplitType),
}

/// `-r`/`--ratio`'s argument: an absolute ratio, or a signed delta (bspwm
/// resolves whether the delta is a plain fraction or a pixel count against
/// the node's current rectangle at *execution* time — `src/messages.c`
/// `cmd_node()`'s `-r` branch — so it is kept unresolved here).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RatioArg {
    /// An absolute ratio in `(0, 1)`.
    Absolute(f64),
    /// A signed delta: a value in `(-1, 1)` is a plain ratio delta; outside
    /// that range it is a pixel delta relative to the node's current split
    /// axis length.
    Delta(f32),
}

/// `-t`/`--state`'s argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateArg {
    /// Bare `~`: use the node's last state.
    ToLastState,
    /// `STATE`, optionally `~`-prefixed (use the last state instead if the
    /// node's current state already matches `STATE`).
    Value(ClientState, bool),
}

/// `-g`/`--flag`'s key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeFlagKey {
    /// `hidden`.
    Hidden,
    /// `sticky`.
    Sticky,
    /// `private`.
    Private,
    /// `locked`.
    Locked,
    /// `marked`.
    Marked,
}

pub(crate) fn parse_node_selector_arg(s: &str) -> Result<NodeSelector, ParseError> {
    NodeSelector::parse(s)
        .ok_or_else(|| ParseError::new("", format!("Invalid descriptor found in '{s}'.\n")))
}

pub(crate) fn parse_desktop_selector_arg(s: &str) -> Result<DesktopSelector, ParseError> {
    DesktopSelector::parse(s)
        .ok_or_else(|| ParseError::new("", format!("Invalid descriptor found in '{s}'.\n")))
}

pub(crate) fn parse_monitor_selector_arg(s: &str) -> Result<MonitorSelector, ParseError> {
    MonitorSelector::parse(s)
        .ok_or_else(|| ParseError::new("", format!("Invalid descriptor found in '{s}'.\n")))
}

/// Parses `node [NODE_SEL] COMMANDS`.
///
/// bspwm: `src/messages.c` `cmd_node()`.
pub fn parse_node(args: &[String]) -> Result<Command, ParseError> {
    if args.is_empty() {
        return Err(ParseError::missing_arguments("node"));
    }
    let mut i = 0;
    let selector = if !args[0].starts_with('-') {
        let sel = parse_node_selector_arg(&args[0])?;
        i = 1;
        Some(sel)
    } else {
        None
    };
    if i >= args.len() {
        return Err(ParseError::missing_commands("node"));
    }

    let mut actions = Vec::new();
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        let action = match flag {
            "-f" | "--focus" => {
                let sel = take_optional_selector(args, &mut i, parse_node_selector_arg)?;
                NodeAction::Focus(sel)
            }
            "-a" | "--activate" => {
                let sel = take_optional_selector(args, &mut i, parse_node_selector_arg)?;
                NodeAction::Activate(sel)
            }
            "-d" | "--to-desktop" => {
                let sel = take_required(args, &mut i, "node", flag, parse_desktop_selector_arg)?;
                let follow = take_follow(args, &mut i);
                NodeAction::ToDesktop(sel, follow)
            }
            "-m" | "--to-monitor" => {
                let sel = take_required(args, &mut i, "node", flag, parse_monitor_selector_arg)?;
                let follow = take_follow(args, &mut i);
                NodeAction::ToMonitor(sel, follow)
            }
            "-n" | "--to-node" => {
                let sel = take_required(args, &mut i, "node", flag, parse_node_selector_arg)?;
                let follow = take_follow(args, &mut i);
                NodeAction::ToNode(sel, follow)
            }
            "-s" | "--swap" => {
                let sel = take_required(args, &mut i, "node", flag, parse_node_selector_arg)?;
                let follow = take_follow(args, &mut i);
                NodeAction::Swap(sel, follow)
            }
            "-p" | "--presel-dir" => {
                let raw = take_str(args, &mut i, "node", flag)?;
                NodeAction::PreselDir(
                    parse_presel_dir(&raw)
                        .ok_or_else(|| ParseError::invalid_argument("node", flag, &raw))?,
                )
            }
            "-o" | "--presel-ratio" => {
                let raw = take_str(args, &mut i, "node", flag)?;
                let rat: f64 = raw
                    .parse()
                    .ok()
                    .filter(|r| *r > 0.0 && *r < 1.0)
                    .ok_or_else(|| ParseError::invalid_argument("node", flag, &raw))?;
                NodeAction::PreselRatio(rat)
            }
            "-v" | "--move" => {
                if i + 1 >= args.len() {
                    return Err(ParseError::not_enough_arguments("node", flag));
                }
                let dx: i32 = args[i]
                    .parse()
                    .map_err(|_| ParseError::invalid_argument("node", flag, &args[i]))?;
                let dy: i32 = args[i + 1]
                    .parse()
                    .map_err(|_| ParseError::invalid_argument("node", flag, &args[i + 1]))?;
                i += 2;
                NodeAction::Move(dx, dy)
            }
            "-z" | "--resize" => {
                if i + 2 >= args.len() {
                    return Err(ParseError::not_enough_arguments("node", flag));
                }
                let handle = ResizeHandle::parse(&args[i])
                    .ok_or_else(|| ParseError::invalid_argument("node", flag, &args[i]))?;
                let dx: i32 = args[i + 1]
                    .parse()
                    .map_err(|_| ParseError::invalid_argument("node", flag, &args[i + 1]))?;
                let dy: i32 = args[i + 2]
                    .parse()
                    .map_err(|_| ParseError::invalid_argument("node", flag, &args[i + 2]))?;
                i += 3;
                NodeAction::Resize(handle, dx, dy)
            }
            "-y" | "--type" => {
                let raw = take_str(args, &mut i, "node", flag)?;
                let arg = if let Some(_c) = CycleDir::parse(&raw) {
                    SplitTypeArg::Cycle
                } else if let Some(t) = parse_split_type(&raw) {
                    SplitTypeArg::Set(t)
                } else {
                    return Err(ParseError::new("node", "".to_string()));
                };
                NodeAction::SetSplitType(arg)
            }
            "-r" | "--ratio" => {
                let raw = take_str(args, &mut i, "node", flag)?;
                let arg = if raw.starts_with('+') || raw.starts_with('-') {
                    let delta: f32 = raw
                        .parse()
                        .map_err(|_| ParseError::invalid_argument("node", flag, &raw))?;
                    RatioArg::Delta(delta)
                } else {
                    let rat: f64 = raw
                        .parse()
                        .ok()
                        .filter(|r| *r > 0.0 && *r < 1.0)
                        .ok_or_else(|| ParseError::invalid_argument("node", flag, &raw))?;
                    RatioArg::Absolute(rat)
                };
                NodeAction::SetRatio(arg)
            }
            "-F" | "--flip" => {
                let raw = take_str(args, &mut i, "node", flag)?;
                let axis = match raw.as_str() {
                    "horizontal" => FlipAxis::Horizontal,
                    "vertical" => FlipAxis::Vertical,
                    _ => return Err(ParseError::new("node", "".to_string())),
                };
                NodeAction::Flip(axis)
            }
            "-R" | "--rotate" => {
                let raw = take_str(args, &mut i, "node", flag)?;
                let deg = parse_degree(&raw)
                    .ok_or_else(|| ParseError::invalid_argument("node", flag, &raw))?;
                NodeAction::Rotate(deg)
            }
            "-E" | "--equalize" => NodeAction::Equalize,
            "-B" | "--balance" => NodeAction::Balance,
            "-C" | "--circulate" => {
                let raw = take_str(args, &mut i, "node", flag)?;
                let dir = match raw.as_str() {
                    "forward" => CirculateDir::Forward,
                    "backward" => CirculateDir::Backward,
                    _ => return Err(ParseError::invalid_argument("node", flag, &raw)),
                };
                NodeAction::Circulate(dir)
            }
            "-i" | "--insert-receptacle" => NodeAction::InsertReceptacle,
            "-t" | "--state" => {
                let raw = take_str(args, &mut i, "node", flag)?;
                let (alternate, rest) = match raw.strip_prefix('~') {
                    Some(rest) => (true, rest),
                    None => (false, raw.as_str()),
                };
                let arg = if alternate && rest.is_empty() {
                    StateArg::ToLastState
                } else if let Some(state) = parse_client_state(rest) {
                    StateArg::Value(state, alternate)
                } else {
                    return Err(ParseError::invalid_argument("node", flag, &raw));
                };
                NodeAction::SetState(arg)
            }
            "-g" | "--flag" => {
                let raw = take_str(args, &mut i, "node", flag)?;
                let (key_str, val_str) = match raw.split_once('=') {
                    Some((k, v)) => (k, Some(v)),
                    None => (raw.as_str(), None),
                };
                let key = match key_str {
                    "hidden" => NodeFlagKey::Hidden,
                    "sticky" => NodeFlagKey::Sticky,
                    "private" => NodeFlagKey::Private,
                    "locked" => NodeFlagKey::Locked,
                    "marked" => NodeFlagKey::Marked,
                    _ => {
                        return Err(ParseError::new(
                            "node",
                            format!("node {flag}: Invalid key: '{key_str}'.\n"),
                        ))
                    }
                };
                let state = match val_str {
                    None => AlterState::Toggle,
                    Some(v) => AlterState::Set(parse_bool(v).ok_or_else(|| {
                        ParseError::new(
                            "node",
                            format!("node {flag}: Invalid value for {key_str}: '{v}'.\n"),
                        )
                    })?),
                };
                NodeAction::SetFlag(key, state)
            }
            "-l" | "--layer" => {
                let raw = take_str(args, &mut i, "node", flag)?;
                let layer = parse_layer(&raw)
                    .ok_or_else(|| ParseError::invalid_argument("node", flag, &raw))?;
                NodeAction::SetLayer(layer)
            }
            "-c" | "--close" => {
                if i < args.len() {
                    return Err(ParseError::new(
                        "node",
                        format!("node {}: Trailing commands.\n", args[i]),
                    ));
                }
                NodeAction::Close
            }
            "-k" | "--kill" => {
                if i < args.len() {
                    return Err(ParseError::new(
                        "node",
                        format!("node {}: Trailing commands.\n", args[i]),
                    ));
                }
                NodeAction::Kill
            }
            other => return Err(ParseError::unknown_command("node", other)),
        };
        actions.push(action);
    }

    Ok(Command::Node { selector, actions })
}

fn parse_presel_dir(raw: &str) -> Option<PreselDirArg> {
    if raw == "cancel" {
        return Some(PreselDirArg::Cancel);
    }
    let (alternate, rest) = match raw.strip_prefix('~') {
        Some(rest) => (true, rest),
        None => (false, raw),
    };
    let dir = match rest {
        "north" => Direction::North,
        "west" => Direction::West,
        "south" => Direction::South,
        "east" => Direction::East,
        _ => return None,
    };
    Some(PreselDirArg::Set(dir, alternate))
}

fn parse_split_type(s: &str) -> Option<SplitType> {
    match s {
        "horizontal" => Some(SplitType::Horizontal),
        "vertical" => Some(SplitType::Vertical),
        _ => None,
    }
}

fn parse_client_state(s: &str) -> Option<ClientState> {
    match s {
        "tiled" => Some(ClientState::Tiled),
        "pseudo_tiled" => Some(ClientState::PseudoTiled),
        "floating" => Some(ClientState::Floating),
        "fullscreen" => Some(ClientState::Fullscreen),
        _ => None,
    }
}

fn parse_layer(s: &str) -> Option<Layer> {
    match s {
        "below" => Some(Layer::Below),
        "normal" => Some(Layer::Normal),
        "above" => Some(Layer::Above),
        _ => None,
    }
}

fn parse_layout(s: &str) -> Option<Layout> {
    match s {
        "monocle" => Some(Layout::Monocle),
        "tiled" => Some(Layout::Tiled),
        _ => None,
    }
}

/// Consumes the next argument (if present and not itself a flag) as an
/// optional selector, matching `-f`/`-a`'s `num > 1 && *(args+1)[0] !=
/// OPT_CHR` check.
fn take_optional_selector<T>(
    args: &[String],
    i: &mut usize,
    parse: impl Fn(&str) -> Result<T, ParseError>,
) -> Result<Option<T>, ParseError> {
    if *i < args.len() && !args[*i].starts_with('-') {
        let sel = parse(&args[*i])?;
        *i += 1;
        Ok(Some(sel))
    } else {
        Ok(None)
    }
}

fn take_required<T>(
    args: &[String],
    i: &mut usize,
    command: &str,
    flag: &str,
    parse: impl Fn(&str) -> Result<T, ParseError>,
) -> Result<T, ParseError> {
    if *i >= args.len() {
        return Err(ParseError::not_enough_arguments(command, flag));
    }
    let v = parse(&args[*i])?;
    *i += 1;
    Ok(v)
}

fn take_str(
    args: &[String],
    i: &mut usize,
    command: &str,
    flag: &str,
) -> Result<String, ParseError> {
    if *i >= args.len() {
        return Err(ParseError::not_enough_arguments(command, flag));
    }
    let v = args[*i].clone();
    *i += 1;
    Ok(v)
}

fn take_follow(args: &[String], i: &mut usize) -> bool {
    if *i < args.len() && args[*i] == "--follow" {
        *i += 1;
        true
    } else {
        false
    }
}

// ---- desktop ---------------------------------------------------------------

/// One `bspc desktop` command.
#[derive(Debug, Clone, PartialEq)]
pub enum DesktopAction {
    /// `-f`, `--focus`.
    Focus(Option<DesktopSelector>),
    /// `-a`, `--activate`.
    Activate(Option<DesktopSelector>),
    /// `-m`, `--to-monitor`.
    ToMonitor(MonitorSelector, Follow),
    /// `-s`, `--swap`.
    Swap(DesktopSelector, Follow),
    /// `-l`, `--layout`.
    SetLayout(LayoutArg),
    /// `-n`, `--rename`.
    Rename(String),
    /// `-b`, `--bubble`.
    Bubble(CycleDir),
    /// `-r`, `--remove`. Must be the last action.
    Remove,
}

/// `-l`/`--layout`'s argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutArg {
    /// `next`/`prev`: toggle between tiled and monocle.
    Cycle,
    /// An explicit layout.
    Set(Layout),
}

/// Parses `desktop [DESKTOP_SEL] COMMANDS`.
///
/// bspwm: `src/messages.c` `cmd_desktop()`.
pub fn parse_desktop(args: &[String]) -> Result<Command, ParseError> {
    if args.is_empty() {
        return Err(ParseError::missing_arguments("desktop"));
    }
    let mut i = 0;
    let selector = if !args[0].starts_with('-') {
        let sel = parse_desktop_selector_arg(&args[0])?;
        i = 1;
        Some(sel)
    } else {
        None
    };
    if i >= args.len() {
        return Err(ParseError::missing_commands("desktop"));
    }

    let mut actions = Vec::new();
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        let action = match flag {
            "-f" | "--focus" => {
                let sel = take_optional_selector(args, &mut i, parse_desktop_selector_arg)?;
                DesktopAction::Focus(sel)
            }
            "-a" | "--activate" => {
                let sel = take_optional_selector(args, &mut i, parse_desktop_selector_arg)?;
                DesktopAction::Activate(sel)
            }
            "-m" | "--to-monitor" => {
                let sel = take_required(args, &mut i, "desktop", flag, parse_monitor_selector_arg)?;
                let follow = take_follow(args, &mut i);
                DesktopAction::ToMonitor(sel, follow)
            }
            "-s" | "--swap" => {
                let sel = take_required(args, &mut i, "desktop", flag, parse_desktop_selector_arg)?;
                let follow = take_follow(args, &mut i);
                DesktopAction::Swap(sel, follow)
            }
            "-l" | "--layout" => {
                let raw = take_str(args, &mut i, "desktop", flag)?;
                let arg = if CycleDir::parse(&raw).is_some() {
                    LayoutArg::Cycle
                } else if let Some(l) = parse_layout(&raw) {
                    LayoutArg::Set(l)
                } else {
                    return Err(ParseError::invalid_argument("desktop", flag, &raw));
                };
                DesktopAction::SetLayout(arg)
            }
            "-n" | "--rename" => {
                let name = take_str(args, &mut i, "desktop", flag)?;
                DesktopAction::Rename(name)
            }
            "-b" | "--bubble" => {
                let raw = take_str(args, &mut i, "desktop", flag)?;
                let cyc = CycleDir::parse(&raw)
                    .ok_or_else(|| ParseError::invalid_argument("desktop", flag, &raw))?;
                DesktopAction::Bubble(cyc)
            }
            "-r" | "--remove" => {
                if i < args.len() {
                    return Err(ParseError::new(
                        "desktop",
                        format!("desktop {}: Trailing commands.\n", args[i]),
                    ));
                }
                DesktopAction::Remove
            }
            other => return Err(ParseError::unknown_command("desktop", other)),
        };
        actions.push(action);
    }

    Ok(Command::Desktop { selector, actions })
}

// ---- monitor -----------------------------------------------------------

/// One `bspc monitor` command.
#[derive(Debug, Clone, PartialEq)]
pub enum MonitorAction {
    /// `-f`, `--focus`.
    Focus(Option<MonitorSelector>),
    /// `-s`, `--swap`.
    Swap(MonitorSelector),
    /// `-a`, `--add-desktops`.
    AddDesktops(Vec<String>),
    /// `-o`, `--reorder-desktops`.
    ReorderDesktops(Vec<String>),
    /// `-d`, `--reset-desktops`.
    ResetDesktops(Vec<String>),
    /// `-r`, `--remove`. Must be the last action.
    Remove,
    /// `-g`, `--rectangle`.
    SetRectangle(bsp_core::geometry::Rect),
    /// `-n`, `--rename`.
    Rename(String),
}

fn parse_rectangle(s: &str) -> Option<bsp_core::geometry::Rect> {
    // WxH+X+Y
    let (wh, xy) = s.split_once('+')?;
    let (w, h) = wh.split_once('x')?;
    let (x, rest_y) = xy.split_once('+')?;
    let w: i32 = w.parse().ok()?;
    let h: i32 = h.parse().ok()?;
    let x: i32 = x.parse().ok()?;
    let y: i32 = rest_y.parse().ok()?;
    Some(bsp_core::geometry::Rect::new(x, y, w, h))
}

/// Parses `monitor [MONITOR_SEL] COMMANDS`.
///
/// bspwm: `src/messages.c` `cmd_monitor()`.
pub fn parse_monitor(args: &[String]) -> Result<Command, ParseError> {
    if args.is_empty() {
        return Err(ParseError::missing_arguments("monitor"));
    }
    let mut i = 0;
    let selector = if !args[0].starts_with('-') {
        let sel = parse_monitor_selector_arg(&args[0])?;
        i = 1;
        Some(sel)
    } else {
        None
    };
    if i >= args.len() {
        return Err(ParseError::missing_commands("monitor"));
    }

    let mut actions = Vec::new();
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        let action = match flag {
            "-f" | "--focus" => {
                let sel = take_optional_selector(args, &mut i, parse_monitor_selector_arg)?;
                MonitorAction::Focus(sel)
            }
            "-s" | "--swap" => {
                let sel = take_required(args, &mut i, "monitor", flag, parse_monitor_selector_arg)?;
                MonitorAction::Swap(sel)
            }
            "-a" | "--add-desktops" => {
                let names = take_rest(args, &mut i, "monitor", flag)?;
                MonitorAction::AddDesktops(names)
            }
            "-o" | "--reorder-desktops" => {
                let names = take_rest(args, &mut i, "monitor", flag)?;
                MonitorAction::ReorderDesktops(names)
            }
            "-d" | "--reset-desktops" => {
                let names = take_rest(args, &mut i, "monitor", flag)?;
                MonitorAction::ResetDesktops(names)
            }
            "-r" | "--remove" => {
                if i < args.len() {
                    return Err(ParseError::new(
                        "monitor",
                        format!("monitor {}: Trailing commands.\n", args[i]),
                    ));
                }
                MonitorAction::Remove
            }
            "-g" | "--rectangle" => {
                let raw = take_str(args, &mut i, "monitor", flag)?;
                let r = parse_rectangle(&raw)
                    .ok_or_else(|| ParseError::invalid_argument("monitor", flag, &raw))?;
                MonitorAction::SetRectangle(r)
            }
            "-n" | "--rename" => {
                let name = take_str(args, &mut i, "monitor", flag)?;
                MonitorAction::Rename(name)
            }
            other => return Err(ParseError::unknown_command("monitor", other)),
        };
        actions.push(action);
    }

    Ok(Command::Monitor { selector, actions })
}

// ---- output --------------------------------------------------------------

/// One `bspc output` command. Not a bspwm command — see `Command::Output`.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputAction {
    /// `-m`, `--mode WIDTHxHEIGHT@HZ` (a whole or decimal refresh rate,
    /// e.g. `1920x1080@60` or `1920x1080@59.94`).
    SetMode(OutputMode),
    /// `-s`, `--scale FACTOR`.
    SetScale(f64),
    /// `-p`, `--position X Y`.
    SetPosition(i32, i32),
    /// `-t`, `--transform NAME`: rotation and/or flip.
    SetTransform(OutputTransform),
    /// `-c`, `--create-headless [WIDTHxHEIGHT@HZ]`: add a virtual output with
    /// no display behind it (default `1920x1080@60`), named `HEADLESS-N`. Takes
    /// no output name. Something to capture or stream (`wayvnc`, a screen
    /// recorder) and to put windows on.
    CreateHeadless(Option<OutputMode>),
    /// `-r`, `--remove`: remove a virtual output; its windows move to another
    /// monitor. A real output cannot be removed.
    Remove,
}

/// An output's rotation (counter-clockwise, as in Wayland's `wl_output`) and
/// optional flip. Names on the command line: `normal`, `90`, `180`, `270`,
/// `flipped`, `flipped-90`, `flipped-180`, `flipped-270`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputTransform {
    /// No rotation.
    #[default]
    Normal,
    /// Rotated 90 degrees.
    Rotate90,
    /// Rotated 180 degrees.
    Rotate180,
    /// Rotated 270 degrees.
    Rotate270,
    /// Flipped horizontally.
    Flipped,
    /// Flipped, then rotated 90 degrees.
    Flipped90,
    /// Flipped, then rotated 180 degrees.
    Flipped180,
    /// Flipped, then rotated 270 degrees.
    Flipped270,
}

impl OutputTransform {
    /// The command-line name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Rotate90 => "90",
            Self::Rotate180 => "180",
            Self::Rotate270 => "270",
            Self::Flipped => "flipped",
            Self::Flipped90 => "flipped-90",
            Self::Flipped180 => "flipped-180",
            Self::Flipped270 => "flipped-270",
        }
    }

    /// The transform named `s`, if it is one.
    pub fn parse(s: &str) -> Option<Self> {
        [
            Self::Normal,
            Self::Rotate90,
            Self::Rotate180,
            Self::Rotate270,
            Self::Flipped,
            Self::Flipped90,
            Self::Flipped180,
            Self::Flipped270,
        ]
        .into_iter()
        .find(|t| t.name() == s)
    }
}

/// A display mode: pixel size and refresh rate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputMode {
    /// Width in pixels.
    pub width: i32,
    /// Height in pixels.
    pub height: i32,
    /// Refresh rate in millihertz — matches `wl_output.mode`'s own unit,
    /// since a real DRM output's mode ultimately becomes one of those.
    pub refresh_mhz: i32,
}

fn parse_output_mode(s: &str) -> Option<OutputMode> {
    let (wh, hz) = s.split_once('@')?;
    let (w, h) = wh.split_once('x')?;
    let width: i32 = w.parse().ok()?;
    let height: i32 = h.parse().ok()?;
    let hz: f64 = hz.parse().ok()?;
    if width <= 0 || height <= 0 || hz <= 0.0 {
        return None;
    }
    Some(OutputMode {
        width,
        height,
        refresh_mhz: (hz * 1000.0).round() as i32,
    })
}

/// Parses `output [<name> [COMMANDS]]`: `<name>` alone with no further
/// arguments reports that output's current settings; no arguments at
/// all lists every known output. Flag letters (`-m`/`-s`/`-p`) are this
/// project's own choice, matching the rest of this crate's dash-flag
/// style — `docs/design.md`'s own `bspc output` example is illustrative
/// prose, not a literal CLI spec.
pub fn parse_output(args: &[String]) -> Result<Command, ParseError> {
    let mut i = 0;
    let name = if !args.is_empty() && !args[0].starts_with('-') {
        i = 1;
        Some(args[0].clone())
    } else {
        None
    };

    let mut actions = Vec::new();
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        let action = match flag {
            "-m" | "--mode" => {
                let raw = take_str(args, &mut i, "output", flag)?;
                let mode = parse_output_mode(&raw)
                    .ok_or_else(|| ParseError::invalid_argument("output", flag, &raw))?;
                OutputAction::SetMode(mode)
            }
            "-s" | "--scale" => {
                let raw = take_str(args, &mut i, "output", flag)?;
                let scale: f64 = raw
                    .parse()
                    .ok()
                    .filter(|s| *s > 0.0)
                    .ok_or_else(|| ParseError::invalid_argument("output", flag, &raw))?;
                OutputAction::SetScale(scale)
            }
            "-p" | "--position" => {
                if i + 1 >= args.len() {
                    return Err(ParseError::not_enough_arguments("output", flag));
                }
                let x: i32 = args[i]
                    .parse()
                    .map_err(|_| ParseError::invalid_argument("output", flag, &args[i]))?;
                let y: i32 = args[i + 1]
                    .parse()
                    .map_err(|_| ParseError::invalid_argument("output", flag, &args[i + 1]))?;
                i += 2;
                OutputAction::SetPosition(x, y)
            }
            "-t" | "--transform" => {
                let raw = take_str(args, &mut i, "output", flag)?;
                let transform = OutputTransform::parse(&raw)
                    .ok_or_else(|| ParseError::invalid_argument("output", flag, &raw))?;
                OutputAction::SetTransform(transform)
            }
            "-c" | "--create-headless" => {
                // The mode is optional: it is there if the next word is not a flag.
                let mode = match args.get(i) {
                    Some(raw) if !raw.starts_with('-') => {
                        i += 1;
                        Some(parse_output_mode(raw).ok_or_else(|| ParseError::invalid_argument("output", flag, raw))?)
                    }
                    _ => None,
                };
                OutputAction::CreateHeadless(mode)
            }
            "-r" | "--remove" => OutputAction::Remove,
            other => return Err(ParseError::unknown_command("output", other)),
        };
        actions.push(action);
    }

    // No output named: `-m`/`-s`/`-p` would apply to nothing. Only creating a
    // virtual output needs no name (it is given one).
    let nameless_ok = actions.iter().all(|a| matches!(a, OutputAction::CreateHeadless(_)));
    if name.is_none() && !actions.is_empty() && !nameless_ok {
        return Err(ParseError::missing_arguments("output"));
    }

    Ok(Command::Output { name, actions })
}

// ---- input ---------------------------------------------------------------

/// One `bspc input` command. Not a bspwm command — see `Command::Input`.
#[derive(Debug, Clone, PartialEq)]
pub enum InputAction {
    /// `-r`, `--rate HZ DELAY_MS` (`xset r rate` replacement).
    SetRepeat {
        /// Repeats per second.
        rate: i32,
        /// Milliseconds before the first repeat.
        delay: i32,
    },
    /// `-a`, `--accel FACTOR` (`xinput` pointer-acceleration replacement).
    SetAccel(f64),
}

/// Parses `input [<device> [COMMANDS]]`: `<device>` (`"keyboard"` or a
/// specific device name) alone reports its current settings; no
/// arguments at all lists every known input device.
pub fn parse_input(args: &[String]) -> Result<Command, ParseError> {
    let mut i = 0;
    let device = if !args.is_empty() && !args[0].starts_with('-') {
        i = 1;
        Some(args[0].clone())
    } else {
        None
    };

    let mut actions = Vec::new();
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        let action = match flag {
            "-r" | "--rate" => {
                if i + 1 >= args.len() {
                    return Err(ParseError::not_enough_arguments("input", flag));
                }
                let rate: i32 = args[i]
                    .parse()
                    .map_err(|_| ParseError::invalid_argument("input", flag, &args[i]))?;
                let delay: i32 = args[i + 1]
                    .parse()
                    .map_err(|_| ParseError::invalid_argument("input", flag, &args[i + 1]))?;
                i += 2;
                InputAction::SetRepeat { rate, delay }
            }
            "-a" | "--accel" => {
                let raw = take_str(args, &mut i, "input", flag)?;
                let factor: f64 = raw
                    .parse()
                    .map_err(|_| ParseError::invalid_argument("input", flag, &raw))?;
                InputAction::SetAccel(factor)
            }
            other => return Err(ParseError::unknown_command("input", other)),
        };
        actions.push(action);
    }

    if device.is_none() && !actions.is_empty() {
        return Err(ParseError::missing_arguments("input"));
    }

    Ok(Command::Input { device, actions })
}

fn take_rest(
    args: &[String],
    i: &mut usize,
    command: &str,
    flag: &str,
) -> Result<Vec<String>, ParseError> {
    if *i >= args.len() {
        return Err(ParseError::not_enough_arguments(command, flag));
    }
    let rest = args[*i..].to_vec();
    *i = args.len();
    Ok(rest)
}

// ---- query -----------------------------------------------------------------

/// What `bspc query` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryDomain {
    /// `-N`, `--nodes`.
    Nodes,
    /// `-D`, `--desktops`.
    Desktops,
    /// `-M`, `--monitors`.
    Monitors,
    /// `-T`, `--tree`.
    Tree,
}

/// A parsed `bspc query` request.
///
/// bspwm: `src/messages.c` `cmd_query()`. `-M/-D/-N SEL` set the reference
/// the `-m/-d/-n` modifiers are relative to; `-m/-d/-n SEL` narrow the listing
/// to that monitor, desktop or node (`trg`), and `-m/-d/-n .MODIFIERS` filter
/// it (descriptor-free constraints).
#[derive(Debug, Clone, PartialEq)]
pub struct QueryCommand {
    /// Which domain to list/dump.
    pub domain: QueryDomain,
    /// `-M SEL`: the monitor reference (bspwm `monitor_ref`).
    pub monitor_ref: Option<MonitorSelector>,
    /// `-D SEL`: the desktop reference (`desktop_ref`).
    pub desktop_ref: Option<DesktopSelector>,
    /// `-N SEL`: the node reference (`node_ref`).
    pub node_ref: Option<NodeSelector>,
    /// `-m`/`-d`/`-n` with a selector or none (the reference), in order.
    pub targets: Vec<QueryTarget>,
    /// `-m .MODIFIERS`.
    pub monitor_filter: Option<MonitorModifiers>,
    /// `-d .MODIFIERS`.
    pub desktop_filter: Option<DesktopModifiers>,
    /// `-n .MODIFIERS`.
    pub node_filter: Option<NodeModifiers>,
    /// `--names`: print names instead of ids (`-M`/`-D` only).
    pub names: bool,
}

/// One `-m`/`-d`/`-n` target of `bspc query`; `None` is the flag given alone,
/// which means the reference (the focused one unless `-M/-D/-N SEL` said).
#[derive(Debug, Clone, PartialEq)]
pub enum QueryTarget {
    /// `-m [MONITOR_SEL]`.
    Monitor(Option<MonitorSelector>),
    /// `-d [DESKTOP_SEL]`.
    Desktop(Option<DesktopSelector>),
    /// `-n [NODE_SEL]`.
    Node(Option<NodeSelector>),
}

/// Parses `query COMMANDS [OPTIONS]`.
///
/// bspwm: `src/messages.c` `cmd_query()`.
pub fn parse_query(args: &[String]) -> Result<Command, ParseError> {
    if args.is_empty() {
        return Err(ParseError::missing_arguments("query"));
    }
    let mut domain = None;
    let mut domain_count = 0;
    let (mut monitor_ref, mut desktop_ref, mut node_ref) = (None, None, None);
    let mut targets = Vec::new();
    let (mut monitor_filter, mut desktop_filter, mut node_filter) = (None, None, None);
    let mut names = false;
    // A modifier list alone (`.window`): parsed as `any.window`, keeping the modifiers.
    let bad_modifiers = |flag: &str, arg: &str| ParseError::new("query", format!("query {flag}: Invalid modifiers found in '{arg}'.\n"));

    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        let next_is_filter = args.get(i).is_some_and(|a| a.starts_with('.'));
        match flag {
            "-T" | "--tree" => {
                domain = Some(QueryDomain::Tree);
                domain_count += 1;
            }
            "-M" | "--monitors" => {
                domain = Some(QueryDomain::Monitors);
                domain_count += 1;
                monitor_ref = take_optional_selector(args, &mut i, parse_monitor_selector_arg)?.or(monitor_ref);
            }
            "-D" | "--desktops" => {
                domain = Some(QueryDomain::Desktops);
                domain_count += 1;
                desktop_ref = take_optional_selector(args, &mut i, parse_desktop_selector_arg)?.or(desktop_ref);
            }
            "-N" | "--nodes" => {
                domain = Some(QueryDomain::Nodes);
                domain_count += 1;
                node_ref = take_optional_selector(args, &mut i, parse_node_selector_arg)?.or(node_ref);
            }
            "-m" | "--monitor" if next_is_filter => {
                let arg = &args[i];
                i += 1;
                monitor_filter = Some(parse_monitor_selector_arg(&format!("any{arg}")).map_err(|_| bad_modifiers("-m", arg))?.modifiers);
            }
            "-d" | "--desktop" if next_is_filter => {
                let arg = &args[i];
                i += 1;
                desktop_filter = Some(parse_desktop_selector_arg(&format!("any{arg}")).map_err(|_| bad_modifiers("-d", arg))?.modifiers);
            }
            "-n" | "--node" if next_is_filter => {
                let arg = &args[i];
                i += 1;
                node_filter = Some(parse_node_selector_arg(&format!("any{arg}")).map_err(|_| bad_modifiers("-n", arg))?.modifiers);
            }
            "-m" | "--monitor" => targets.push(QueryTarget::Monitor(take_optional_selector(args, &mut i, parse_monitor_selector_arg)?)),
            "-d" | "--desktop" => targets.push(QueryTarget::Desktop(take_optional_selector(args, &mut i, parse_desktop_selector_arg)?)),
            "-n" | "--node" => targets.push(QueryTarget::Node(take_optional_selector(args, &mut i, parse_node_selector_arg)?)),
            "--names" => names = true,
            other => {
                return Err(ParseError::new(
                    "query",
                    format!("query: Unknown option: '{other}'.\n"),
                ))
            }
        }
    }

    let domain = match (domain_count, domain) {
        (0, _) => return Err(ParseError::new("query", "query: No commands given.\n")),
        (1, Some(domain)) => domain,
        _ => {
            return Err(ParseError::new(
                "query",
                "query: Multiple commands given.\n",
            ))
        }
    };
    if names && matches!(domain, QueryDomain::Nodes | QueryDomain::Tree) {
        let c = if domain == QueryDomain::Nodes {
            'N'
        } else {
            'T'
        };
        return Err(ParseError::new(
            "query",
            format!("query -{c}: --names only applies to -M and -D.\n"),
        ));
    }
    // bspwm: only the descriptor-free constraints (`-d .occupied`) are refused here.
    if (domain == QueryDomain::Monitors && (desktop_filter.is_some() || node_filter.is_some()))
        || (domain == QueryDomain::Desktops && node_filter.is_some())
    {
        let c = if domain == QueryDomain::Monitors {
            'M'
        } else {
            'D'
        };
        return Err(ParseError::new(
            "query",
            format!("query -{c}: Incompatible descriptor-free constraints.\n"),
        ));
    }

    Ok(Command::Query(QueryCommand {
        domain,
        monitor_ref,
        desktop_ref,
        node_ref,
        targets,
        monitor_filter,
        desktop_filter,
        node_filter,
        names,
    }))
}

// ---- rule --------------------------------------------------------------

/// One `bspc rule` command.
// `Add` carries a whole rule; the other variants are a few words. Rules are parsed
// once per `bspc rule` command, so the size difference costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum RuleAction {
    /// `-a`, `--add`.
    Add {
        /// Class name pattern (`*` or a literal string).
        class_name: String,
        /// Instance name pattern.
        instance_name: String,
        /// Title pattern.
        name: String,
        /// `-o`/`--one-shot`.
        one_shot: bool,
        /// Parsed `key=value` effect tokens.
        consequence: RuleConsequence,
        /// `monitor=`/`desktop=`/`node=` targets from the effect string.
        /// Boxed: `RuleTarget` is large relative to `RuleAction`'s other
        /// variants, and only `-a` ever needs it.
        target: Box<RuleTarget>,
        /// The effect tokens re-joined with single spaces, kept verbatim
        /// for `rule --list` (bspwm keeps this same raw string in
        /// `rule_t.effect` rather than the parsed form — `src/rule.h`).
        effect_raw: String,
    },
    /// `-r`, `--remove`.
    Remove(Vec<RuleRemoval>),
    /// `-l`, `--list`.
    List,
}

/// One `rule --remove` argument.
#[derive(Debug, Clone, PartialEq)]
pub enum RuleRemoval {
    /// `^<n>`: 1-based index.
    Index(u16),
    /// `head`.
    Head,
    /// `tail`.
    Tail,
    /// A `class[:instance[:name]]` cause.
    Cause(String),
}

/// `rule --add`'s `monitor=`/`desktop=`/`node=`/`rectangle=` fields, which
/// have no home in [`RuleConsequence`] (it owns only fields that need no
/// parsing beyond a plain bool — see `bsp-core`'s doc comment on
/// `RuleConsequence`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RuleTarget {
    /// `monitor=MONITOR_SEL`.
    pub monitor: Option<MonitorSelector>,
    /// `desktop=DESKTOP_SEL`.
    pub desktop: Option<DesktopSelector>,
    /// `node=NODE_SEL`.
    pub node: Option<NodeSelector>,
    /// `rectangle=WxH+X+Y`.
    pub rectangle: Option<bsp_core::geometry::Rect>,
    /// `honor_size_hints=true|false|tiled|floating`.
    pub honor_size_hints: Option<HonorSizeHints>,
}

/// `honor_size_hints=`'s value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HonorSizeHints {
    /// `true`/`on`.
    Yes,
    /// `false`/`off`.
    No,
    /// `tiled`.
    Tiled,
    /// `floating`.
    Floating,
}

/// Parses `rule COMMANDS`.
///
/// bspwm: `src/messages.c` `cmd_rule()`.
pub fn parse_rule(args: &[String]) -> Result<Command, ParseError> {
    if args.is_empty() {
        return Err(ParseError::missing_commands("rule"));
    }
    let mut actions = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        let action = match flag {
            "-a" | "--add" => {
                if args.len() - i < 2 {
                    return Err(ParseError::not_enough_arguments("rule", flag));
                }
                let (class_name, instance_name, name) = split_rule_cause(&args[i]);
                i += 1;
                let mut one_shot = false;
                let mut consequence = RuleConsequence::default();
                let mut target = RuleTarget::default();
                let mut effect_tokens = Vec::new();
                while i < args.len() {
                    if args[i] == "-o" || args[i] == "--one-shot" {
                        one_shot = true;
                        i += 1;
                        continue;
                    }
                    for tok in args[i].split_whitespace() {
                        apply_rule_effect_token(tok, &mut consequence, &mut target)?;
                        effect_tokens.push(tok.to_string());
                    }
                    i += 1;
                }
                RuleAction::Add {
                    class_name,
                    instance_name,
                    name,
                    one_shot,
                    consequence,
                    target: Box::new(target),
                    effect_raw: effect_tokens.join(" "),
                }
            }
            "-r" | "--remove" => {
                if i >= args.len() {
                    return Err(ParseError::not_enough_arguments("rule", flag));
                }
                let mut removals = Vec::new();
                while i < args.len() {
                    let a = &args[i];
                    removals.push(if let Some(idx) = parse_index(a) {
                        RuleRemoval::Index(idx)
                    } else if a == "tail" {
                        RuleRemoval::Tail
                    } else if a == "head" {
                        RuleRemoval::Head
                    } else {
                        RuleRemoval::Cause(a.clone())
                    });
                    i += 1;
                }
                RuleAction::Remove(removals)
            }
            "-l" | "--list" => RuleAction::List,
            other => return Err(ParseError::unknown_command("rule", other)),
        };
        actions.push(action);
    }
    Ok(Command::Rule(actions))
}

/// Splits `(class|*)[:(instance|*)[:(name|*)]]` on unescaped colons.
/// bspwm: `src/messages.c` `cmd_rule()`'s `tokenize_with_escape()` calls,
/// simplified: `\:` is not treated as an escape here (no rule cause in
/// practice needs a literal colon), matching every other simplification in
/// this build that is safe because it is never hit by real usage.
fn split_rule_cause(s: &str) -> (String, String, String) {
    let mut parts = s.splitn(3, ':');
    let class = parts.next().unwrap_or("").to_string();
    let instance = parts
        .next()
        .unwrap_or(bsp_core::rules::MATCH_ANY)
        .to_string();
    let name = parts
        .next()
        .unwrap_or(bsp_core::rules::MATCH_ANY)
        .to_string();
    let instance = if instance.is_empty() {
        bsp_core::rules::MATCH_ANY.to_string()
    } else {
        instance
    };
    let name = if name.is_empty() {
        bsp_core::rules::MATCH_ANY.to_string()
    } else {
        name
    };
    (class, instance, name)
}

/// Parses the `key=value` effects a rule command prints (what
/// `external_rules_command` writes to its standard output), separated by any
/// whitespace. A token that is not understood is skipped, as bspwm's
/// `parse_key_value()` does.
///
/// bspwm: `src/rule.c` `parse_rule_consequence()`.
pub fn parse_rule_effects(text: &str) -> RuleConsequence {
    let mut consequence = RuleConsequence::default();
    let mut target = RuleTarget::default();
    for token in text.split_whitespace() {
        let _ = apply_rule_effect_token(token, &mut consequence, &mut target);
    }
    consequence
}

fn apply_rule_effect_token(
    tok: &str,
    consequence: &mut RuleConsequence,
    target: &mut RuleTarget,
) -> Result<(), ParseError> {
    let Some((key, value)) = tok.split_once('=') else {
        return Err(ParseError::new(
            "rule",
            format!("rule -a: Invalid effect token: '{tok}'.\n"),
        ));
    };
    let bad = || {
        ParseError::new(
            "rule",
            format!("rule -a: Invalid value for {key}: '{value}'.\n"),
        )
    };
    match key {
        "monitor" => {
            target.monitor = Some(parse_monitor_selector_arg(value).map_err(|_| bad())?);
            consequence.monitor_desc = Some(value.to_string());
        }
        "desktop" => {
            target.desktop = Some(parse_desktop_selector_arg(value).map_err(|_| bad())?);
            consequence.desktop_desc = Some(value.to_string());
        }
        "node" => {
            target.node = Some(parse_node_selector_arg(value).map_err(|_| bad())?);
            consequence.node_desc = Some(value.to_string());
        }
        "rectangle" => {
            let rect = parse_rectangle(value).ok_or_else(bad)?;
            target.rectangle = Some(rect);
            consequence.rect = Some(rect);
        }
        "state" => consequence.state = Some(parse_client_state(value).ok_or_else(bad)?),
        "layer" => consequence.layer = Some(parse_layer(value).ok_or_else(bad)?),
        "split_dir" => {
            consequence.split_dir = Some(match value {
                "north" => Direction::North,
                "west" => Direction::West,
                "south" => Direction::South,
                "east" => Direction::East,
                _ => return Err(bad()),
            })
        }
        "split_ratio" => {
            consequence.split_ratio = Some(value.parse().map_err(|_| bad())?);
        }
        "honor_size_hints" => {
            target.honor_size_hints = Some(match value {
                "tiled" => HonorSizeHints::Tiled,
                "floating" => HonorSizeHints::Floating,
                _ => match parse_bool(value) {
                    Some(true) => HonorSizeHints::Yes,
                    Some(false) => HonorSizeHints::No,
                    None => return Err(bad()),
                },
            });
            consequence.honor_size_hints = target.honor_size_hints.map(|h| match h {
                HonorSizeHints::Yes => bsp_core::settings::HonorSizeHints::Yes,
                HonorSizeHints::No => bsp_core::settings::HonorSizeHints::No,
                HonorSizeHints::Tiled => bsp_core::settings::HonorSizeHints::Tiled,
                HonorSizeHints::Floating => bsp_core::settings::HonorSizeHints::Floating,
            });
        }
        "hidden" => consequence.hidden = Some(parse_bool(value).ok_or_else(bad)?),
        "sticky" => consequence.sticky = Some(parse_bool(value).ok_or_else(bad)?),
        "private" => consequence.private = Some(parse_bool(value).ok_or_else(bad)?),
        "locked" => consequence.locked = Some(parse_bool(value).ok_or_else(bad)?),
        "marked" => consequence.marked = Some(parse_bool(value).ok_or_else(bad)?),
        "center" => consequence.center = Some(parse_bool(value).ok_or_else(bad)?),
        "follow" => consequence.follow = Some(parse_bool(value).ok_or_else(bad)?),
        "manage" => consequence.manage = Some(parse_bool(value).ok_or_else(bad)?),
        "focus" => consequence.focus = Some(parse_bool(value).ok_or_else(bad)?),
        "border" => consequence.border = Some(parse_bool(value).ok_or_else(bad)?),
        _ => {
            return Err(ParseError::new(
                "rule",
                format!("rule -a: Unknown key: '{key}'.\n"),
            ))
        }
    }
    Ok(())
}

// ---- wm ------------------------------------------------------------------

/// One `bspc wm` command.
#[derive(Debug, Clone, PartialEq)]
pub enum WmAction {
    /// `-d`, `--dump-state`.
    DumpState,
    /// `-l`, `--load-state`.
    LoadState(String),
    /// `-a`, `--add-monitor`.
    AddMonitor(String, bsp_core::geometry::Rect),
    /// `-O`, `--reorder-monitors`.
    ReorderMonitors(Vec<String>),
    /// `-o`, `--adopt-orphans`.
    AdoptOrphans,
    /// `-g`, `--get-status`.
    GetStatus,
    /// `-h`, `--record-history`.
    RecordHistory(bool),
    /// `-r`, `--restart`.
    Restart,
}

/// Parses `wm COMMANDS`.
///
/// bspwm: `src/messages.c` `cmd_wm()`.
pub fn parse_wm(args: &[String]) -> Result<Command, ParseError> {
    if args.is_empty() {
        return Err(ParseError::missing_commands("wm"));
    }
    let mut actions = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        let action = match flag {
            "-d" | "--dump-state" => WmAction::DumpState,
            "-l" | "--load-state" => {
                let path = take_str(args, &mut i, "wm", flag)?;
                WmAction::LoadState(path)
            }
            "-a" | "--add-monitor" => {
                if args.len() - i < 2 {
                    return Err(ParseError::not_enough_arguments("wm", flag));
                }
                let name = args[i].clone();
                let raw = &args[i + 1];
                let r = parse_rectangle(raw)
                    .ok_or_else(|| ParseError::invalid_argument("wm", flag, raw))?;
                i += 2;
                WmAction::AddMonitor(name, r)
            }
            "-O" | "--reorder-monitors" => {
                let names = take_rest(args, &mut i, "wm", flag)?;
                WmAction::ReorderMonitors(names)
            }
            "-o" | "--adopt-orphans" => WmAction::AdoptOrphans,
            "-g" | "--get-status" => WmAction::GetStatus,
            "-h" | "--record-history" => {
                let raw = take_str(args, &mut i, "wm", flag)?;
                let b = parse_bool(&raw)
                    .ok_or_else(|| ParseError::invalid_argument("wm", flag, &raw))?;
                WmAction::RecordHistory(b)
            }
            "-r" | "--restart" => WmAction::Restart,
            other => return Err(ParseError::unknown_command("wm", other)),
        };
        actions.push(action);
    }
    Ok(Command::Wm(actions))
}

// ---- subscribe -------------------------------------------------------------

/// One `subscribe` event category, or `report` (the default).
///
/// bspwm: `src/subscribe.h` `subscriber_mask_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriberMask {
    /// `all`.
    All,
    /// `report`.
    Report,
    /// `monitor`: every `monitor_*` event.
    Monitor,
    /// `desktop`: every `desktop_*` event.
    Desktop,
    /// `node`: every `node_*` event.
    Node,
    /// One named event.
    Event(crate::report::EventKind),
}

fn parse_subscriber_mask(s: &str) -> Option<SubscriberMask> {
    use crate::report::EventKind::*;
    Some(match s {
        "all" => SubscriberMask::All,
        "report" => SubscriberMask::Report,
        "monitor" => SubscriberMask::Monitor,
        "desktop" => SubscriberMask::Desktop,
        "node" => SubscriberMask::Node,
        "monitor_add" => SubscriberMask::Event(MonitorAdd),
        "monitor_rename" => SubscriberMask::Event(MonitorRename),
        "monitor_remove" => SubscriberMask::Event(MonitorRemove),
        "monitor_swap" => SubscriberMask::Event(MonitorSwap),
        "monitor_focus" => SubscriberMask::Event(MonitorFocus),
        "monitor_geometry" => SubscriberMask::Event(MonitorGeometry),
        "desktop_add" => SubscriberMask::Event(DesktopAdd),
        "desktop_rename" => SubscriberMask::Event(DesktopRename),
        "desktop_remove" => SubscriberMask::Event(DesktopRemove),
        "desktop_swap" => SubscriberMask::Event(DesktopSwap),
        "desktop_transfer" => SubscriberMask::Event(DesktopTransfer),
        "desktop_focus" => SubscriberMask::Event(DesktopFocus),
        "desktop_activate" => SubscriberMask::Event(DesktopActivate),
        "desktop_layout" => SubscriberMask::Event(DesktopLayout),
        "node_add" => SubscriberMask::Event(NodeAdd),
        "node_remove" => SubscriberMask::Event(NodeRemove),
        "node_swap" => SubscriberMask::Event(NodeSwap),
        "node_transfer" => SubscriberMask::Event(NodeTransfer),
        "node_focus" => SubscriberMask::Event(NodeFocus),
        "node_presel" => SubscriberMask::Event(NodePresel),
        "node_stack" => SubscriberMask::Event(NodeStack),
        "node_activate" => SubscriberMask::Event(NodeActivate),
        "node_geometry" => SubscriberMask::Event(NodeGeometry),
        "node_state" => SubscriberMask::Event(NodeState),
        "node_flag" => SubscriberMask::Event(NodeFlag),
        "node_layer" => SubscriberMask::Event(NodeLayer),
        "pointer_action" => SubscriberMask::Event(PointerAction),
        _ => return None,
    })
}

/// Parses `subscribe [OPTIONS] (all|report|monitor|desktop|node|...)*`.
///
/// bspwm: `src/messages.c` `cmd_subscribe()`.
pub fn parse_subscribe(args: &[String]) -> Result<Command, ParseError> {
    let mut fifo = false;
    let mut count = None;
    let mut masks = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        i += 1;
        match flag {
            "-c" | "--count" => {
                let raw = take_str(args, &mut i, "subscribe", flag)?;
                let c: u32 = raw
                    .parse()
                    .ok()
                    .filter(|c| *c >= 1)
                    .ok_or_else(|| ParseError::invalid_argument("subscribe", flag, &raw))?;
                count = Some(c);
            }
            "-f" | "--fifo" => fifo = true,
            other => {
                let mask = parse_subscriber_mask(other).ok_or_else(|| {
                    ParseError::new(
                        "subscribe",
                        format!("subscribe: Invalid argument: '{other}'.\n"),
                    )
                })?;
                masks.push(mask);
            }
        }
    }
    Ok(Command::Subscribe { fifo, count, masks })
}

// ---- quit ------------------------------------------------------------------

/// Parses `quit [<status>]`.
///
/// bspwm: `src/messages.c` `cmd_quit()`.
pub fn parse_quit(args: &[String]) -> Result<Command, ParseError> {
    match args.first() {
        None => Ok(Command::Quit(None)),
        Some(raw) => {
            let status: i32 = raw
                .parse()
                .map_err(|_| ParseError::new("", format!("quit: Invalid argument: '{raw}'.\n")))?;
            Ok(Command::Quit(Some(status)))
        }
    }
}

// ---- config ----------------------------------------------------------------

/// A parsed `bspc config` request.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigCommand {
    /// `-m`/`-d`/`-n`'s constraint, if given (bspwm allows at most one:
    /// the last one given wins, `src/messages.c` `cmd_config()`'s `while`
    /// loop just keeps overwriting `trg`).
    pub target: ConfigTarget,
    /// The setting name.
    pub name: String,
    /// The value to set, or `None` for a `config <setting>` read.
    pub value: Option<String>,
}

/// `config`'s `-m`/`-d`/`-n` target selector, if any.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigTarget {
    /// No `-m`/`-d`/`-n` given: the global default.
    Global,
    /// `-m MONITOR_SEL`.
    Monitor(MonitorSelector),
    /// `-d DESKTOP_SEL`.
    Desktop(DesktopSelector),
    /// `-n NODE_SEL`.
    Node(NodeSelector),
}

/// Parses `config [-m SEL|-d SEL|-n SEL] <setting> [<value>]`.
///
/// bspwm: `src/messages.c` `cmd_config()`.
pub fn parse_config(args: &[String]) -> Result<Command, ParseError> {
    if args.is_empty() {
        return Err(ParseError::missing_arguments("config"));
    }
    let mut target = ConfigTarget::Global;
    let mut i = 0;
    while i < args.len() && args[i].starts_with('-') {
        let flag = args[i].as_str();
        i += 1;
        target = match flag {
            "-m" | "--monitor" => ConfigTarget::Monitor(take_required(
                args,
                &mut i,
                "config",
                flag,
                parse_monitor_selector_arg,
            )?),
            "-d" | "--desktop" => ConfigTarget::Desktop(take_required(
                args,
                &mut i,
                "config",
                flag,
                parse_desktop_selector_arg,
            )?),
            "-n" | "--node" => ConfigTarget::Node(take_required(
                args,
                &mut i,
                "config",
                flag,
                parse_node_selector_arg,
            )?),
            other => {
                return Err(ParseError::new(
                    "config",
                    format!("config: Unknown option: '{other}'.\n"),
                ))
            }
        };
    }
    let remaining = args.len() - i;
    if remaining == 2 {
        Ok(Command::Config(ConfigCommand {
            target,
            name: args[i].clone(),
            value: Some(args[i + 1].clone()),
        }))
    } else if remaining == 1 {
        Ok(Command::Config(ConfigCommand {
            target,
            name: args[i].clone(),
            value: None,
        }))
    } else {
        Err(ParseError::new(
            "config",
            format!("config: Was expecting 1 or 2 arguments, received {remaining}.\n"),
        ))
    }
}

/// Parses a full `bspc` request: the domain word plus its arguments.
///
/// bspwm: `src/messages.c` `process_message()`.
pub fn parse(args: &[String]) -> Result<Command, ParseError> {
    let Some((domain, rest)) = args.split_first() else {
        return Err(ParseError::new("", "No arguments given.\n".to_string()));
    };
    match domain.as_str() {
        "node" => parse_node(rest),
        "desktop" => parse_desktop(rest),
        "monitor" => parse_monitor(rest),
        "query" => parse_query(rest),
        "rule" => parse_rule(rest),
        "wm" => parse_wm(rest),
        "subscribe" => parse_subscribe(rest),
        "quit" => parse_quit(rest),
        "config" => parse_config(rest),
        "output" => parse_output(rest),
        "input" => parse_input(rest),
        other => Err(ParseError::unknown_domain(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    #[test]
    fn node_focus_with_no_selector_defaults_to_none() {
        let cmd = parse(&args("node -f")).unwrap();
        assert_eq!(
            cmd,
            Command::Node {
                selector: None,
                actions: vec![NodeAction::Focus(None)],
            }
        );
    }

    #[test]
    fn node_selector_then_multiple_chained_actions() {
        let cmd = parse(&args("node west -t floating -g marked")).unwrap();
        match cmd {
            Command::Node { selector, actions } => {
                assert!(selector.is_some());
                assert_eq!(
                    actions,
                    vec![
                        NodeAction::SetState(StateArg::Value(ClientState::Floating, false)),
                        NodeAction::SetFlag(NodeFlagKey::Marked, AlterState::Toggle),
                    ]
                );
            }
            other => panic!("expected Node, got {other:?}"),
        }
    }

    #[test]
    fn node_to_desktop_with_follow() {
        let cmd = parse(&args("node -d ^2 --follow")).unwrap();
        match cmd {
            Command::Node { actions, .. } => {
                assert_eq!(actions.len(), 1);
                assert!(matches!(&actions[0], NodeAction::ToDesktop(_, true)));
            }
            other => panic!("expected Node, got {other:?}"),
        }
    }

    #[test]
    fn node_ratio_relative_delta() {
        let cmd = parse(&args("node -r +0.1")).unwrap();
        match cmd {
            Command::Node { actions, .. } => {
                assert_eq!(actions, vec![NodeAction::SetRatio(RatioArg::Delta(0.1))]);
            }
            other => panic!("expected Node, got {other:?}"),
        }
    }

    #[test]
    fn node_close_rejects_trailing_commands() {
        let err = parse(&args("node -c -k")).unwrap_err();
        assert!(err.message.contains("Trailing commands"));
    }

    #[test]
    fn node_presel_dir_alternate() {
        let cmd = parse(&args("node -p ~north")).unwrap();
        match cmd {
            Command::Node { actions, .. } => {
                assert_eq!(
                    actions,
                    vec![NodeAction::PreselDir(PreselDirArg::Set(
                        Direction::North,
                        true
                    ))]
                );
            }
            other => panic!("expected Node, got {other:?}"),
        }
    }

    #[test]
    fn desktop_layout_cycle() {
        let cmd = parse(&args("desktop -l next")).unwrap();
        match cmd {
            Command::Desktop { actions, .. } => {
                assert_eq!(actions, vec![DesktopAction::SetLayout(LayoutArg::Cycle)]);
            }
            other => panic!("expected Desktop, got {other:?}"),
        }
    }

    #[test]
    fn output_transform_parses_every_name_and_rejects_others() {
        for name in ["normal", "90", "180", "270", "flipped", "flipped-90", "flipped-180", "flipped-270"] {
            let cmd = parse(&args(&format!("output HDMI-A-1 -t {name}"))).unwrap();
            match cmd {
                Command::Output { actions, .. } => {
                    assert_eq!(actions, vec![OutputAction::SetTransform(OutputTransform::parse(name).unwrap())]);
                }
                other => panic!("expected Output, got {other:?}"),
            }
        }
        assert!(parse(&args("output HDMI-A-1 -t sideways")).is_err());
    }

    #[test]
    fn monitor_add_desktops_consumes_rest_of_args() {
        let cmd = parse(&args("monitor -a I II III")).unwrap();
        match cmd {
            Command::Monitor { actions, .. } => {
                assert_eq!(
                    actions,
                    vec![MonitorAction::AddDesktops(vec![
                        "I".to_string(),
                        "II".to_string(),
                        "III".to_string()
                    ])]
                );
            }
            other => panic!("expected Monitor, got {other:?}"),
        }
    }

    #[test]
    fn monitor_rectangle_parses_wxh_plus_x_plus_y() {
        let cmd = parse(&args("monitor -g 1920x1080+0+0")).unwrap();
        match cmd {
            Command::Monitor { actions, .. } => {
                assert_eq!(
                    actions,
                    vec![MonitorAction::SetRectangle(bsp_core::geometry::Rect::new(
                        0, 0, 1920, 1080
                    ))]
                );
            }
            other => panic!("expected Monitor, got {other:?}"),
        }
    }

    #[test]
    fn output_bare_lists_with_no_name_and_no_actions() {
        let cmd = parse(&args("output")).unwrap();
        assert_eq!(
            cmd,
            Command::Output {
                name: None,
                actions: Vec::new(),
            }
        );
    }

    #[test]
    fn output_name_alone_reports_that_outputs_settings() {
        let cmd = parse(&args("output eDP-1")).unwrap();
        assert_eq!(
            cmd,
            Command::Output {
                name: Some("eDP-1".to_string()),
                actions: Vec::new(),
            }
        );
    }

    #[test]
    fn output_mode_scale_position_chain() {
        let cmd = parse(&args("output eDP-1 -m 1920x1080@60 -s 1.25 -p 0 0")).unwrap();
        assert_eq!(
            cmd,
            Command::Output {
                name: Some("eDP-1".to_string()),
                actions: vec![
                    OutputAction::SetMode(OutputMode {
                        width: 1920,
                        height: 1080,
                        refresh_mhz: 60_000,
                    }),
                    OutputAction::SetScale(1.25),
                    OutputAction::SetPosition(0, 0),
                ],
            }
        );
    }

    #[test]
    fn output_mode_accepts_a_decimal_refresh_rate() {
        let cmd = parse(&args("output eDP-1 -m 1920x1080@59.94")).unwrap();
        match cmd {
            Command::Output { actions, .. } => {
                assert_eq!(
                    actions,
                    vec![OutputAction::SetMode(OutputMode {
                        width: 1920,
                        height: 1080,
                        refresh_mhz: 59_940,
                    })]
                );
            }
            other => panic!("expected Output, got {other:?}"),
        }
    }

    #[test]
    fn output_flags_with_no_name_is_an_error() {
        assert!(parse(&args("output -s 1.25")).is_err());
    }

    #[test]
    fn output_create_headless_takes_no_name_and_an_optional_mode() {
        assert_eq!(
            parse(&args("output --create-headless")).unwrap(),
            Command::Output { name: None, actions: vec![OutputAction::CreateHeadless(None)] }
        );
        match parse(&args("output -c 1280x720@30")).unwrap() {
            Command::Output { name: None, actions } => match actions.as_slice() {
                [OutputAction::CreateHeadless(Some(m))] => assert_eq!((m.width, m.height, m.refresh_mhz), (1280, 720, 30_000)),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
        assert!(parse(&args("output -c 1280x720")).is_err(), "a mode needs its refresh rate");
        // `-r` names what it removes.
        assert!(parse(&args("output -r")).is_err());
        assert_eq!(
            parse(&args("output HEADLESS-1 --remove")).unwrap(),
            Command::Output { name: Some("HEADLESS-1".to_string()), actions: vec![OutputAction::Remove] }
        );
    }

    #[test]
    fn output_invalid_mode_is_rejected() {
        assert!(parse(&args("output eDP-1 -m garbage")).is_err());
    }

    #[test]
    fn input_bare_lists_with_no_device_and_no_actions() {
        let cmd = parse(&args("input")).unwrap();
        assert_eq!(
            cmd,
            Command::Input {
                device: None,
                actions: Vec::new(),
            }
        );
    }

    #[test]
    fn input_rate_and_accel_chain() {
        let cmd = parse(&args("input keyboard -r 25 200")).unwrap();
        assert_eq!(
            cmd,
            Command::Input {
                device: Some("keyboard".to_string()),
                actions: vec![InputAction::SetRepeat {
                    rate: 25,
                    delay: 200
                }],
            }
        );

        let cmd = parse(&args("input mouse -a 1.5")).unwrap();
        assert_eq!(
            cmd,
            Command::Input {
                device: Some("mouse".to_string()),
                actions: vec![InputAction::SetAccel(1.5)],
            }
        );
    }

    #[test]
    fn input_flags_with_no_device_is_an_error() {
        assert!(parse(&args("input -a 1.5")).is_err());
    }

    #[test]
    fn query_tree_with_node_constraint() {
        let cmd = parse(&args("query -T -n focused")).unwrap();
        match cmd {
            Command::Query(q) => {
                assert_eq!(q.domain, QueryDomain::Tree);
                assert!(matches!(q.targets.as_slice(), [QueryTarget::Node(Some(_))]));
            }
            other => panic!("expected Query, got {other:?}"),
        }
    }

    #[test]
    fn query_rejects_multiple_domains() {
        let err = parse(&args("query -N -D")).unwrap_err();
        assert!(err.message.contains("Multiple commands"));
    }

    #[test]
    fn query_names_rejected_for_nodes() {
        let err = parse(&args("query -N --names")).unwrap_err();
        assert!(err.message.contains("--names only applies"));
    }

    #[test]
    fn rule_add_parses_cause_and_effect() {
        let cmd = parse(&args(
            "rule -a Firefox:Navigator:* state=floating follow=on",
        ))
        .unwrap();
        match cmd {
            Command::Rule(actions) => match &actions[0] {
                RuleAction::Add {
                    class_name,
                    instance_name,
                    name,
                    consequence,
                    ..
                } => {
                    assert_eq!(class_name, "Firefox");
                    assert_eq!(instance_name, "Navigator");
                    assert_eq!(name, "*");
                    assert_eq!(consequence.state, Some(ClientState::Floating));
                    assert!(consequence.should_follow());
                }
                other => panic!("expected Add, got {other:?}"),
            },
            other => panic!("expected Rule, got {other:?}"),
        }
    }

    #[test]
    fn rule_add_wildcard_only_class() {
        let cmd = parse(&args("rule -a *:*:* -o center=on")).unwrap();
        match cmd {
            Command::Rule(actions) => match &actions[0] {
                RuleAction::Add {
                    class_name,
                    instance_name,
                    name,
                    one_shot,
                    consequence,
                    ..
                } => {
                    assert_eq!(class_name, "*");
                    assert_eq!(instance_name, "*");
                    assert_eq!(name, "*");
                    assert!(one_shot);
                    assert!(consequence.should_center());
                }
                other => panic!("expected Add, got {other:?}"),
            },
            other => panic!("expected Rule, got {other:?}"),
        }
    }

    #[test]
    fn rule_remove_parses_index_head_tail_and_cause() {
        let cmd = parse(&args("rule -r ^2 head tail Firefox")).unwrap();
        assert_eq!(
            cmd,
            Command::Rule(vec![RuleAction::Remove(vec![
                RuleRemoval::Index(2),
                RuleRemoval::Head,
                RuleRemoval::Tail,
                RuleRemoval::Cause("Firefox".to_string()),
            ])])
        );
    }

    #[test]
    fn wm_dump_state() {
        assert_eq!(
            parse(&args("wm -d")).unwrap(),
            Command::Wm(vec![WmAction::DumpState])
        );
    }

    #[test]
    fn wm_add_monitor() {
        let cmd = parse(&args("wm -a eDP-1 1920x1080+0+0")).unwrap();
        assert_eq!(
            cmd,
            Command::Wm(vec![WmAction::AddMonitor(
                "eDP-1".to_string(),
                bsp_core::geometry::Rect::new(0, 0, 1920, 1080)
            )])
        );
    }

    #[test]
    fn subscribe_defaults_to_no_masks() {
        let cmd = parse(&args("subscribe")).unwrap();
        assert_eq!(
            cmd,
            Command::Subscribe {
                fifo: false,
                count: None,
                masks: vec![],
            }
        );
    }

    #[test]
    fn subscribe_parses_count_and_masks() {
        let cmd = parse(&args("subscribe -c 3 node_add desktop_focus")).unwrap();
        match cmd {
            Command::Subscribe { count, masks, .. } => {
                assert_eq!(count, Some(3));
                assert_eq!(masks.len(), 2);
            }
            other => panic!("expected Subscribe, got {other:?}"),
        }
    }

    #[test]
    fn quit_with_no_status() {
        assert_eq!(parse(&args("quit")).unwrap(), Command::Quit(None));
    }

    #[test]
    fn quit_with_status() {
        assert_eq!(
            parse(&["quit".to_string(), "1".to_string()]).unwrap(),
            Command::Quit(Some(1))
        );
    }

    #[test]
    fn config_get_global() {
        let cmd = parse(&args("config window_gap")).unwrap();
        assert_eq!(
            cmd,
            Command::Config(ConfigCommand {
                target: ConfigTarget::Global,
                name: "window_gap".to_string(),
                value: None,
            })
        );
    }

    #[test]
    fn config_set_with_monitor_target() {
        let cmd = parse(&args("config -m focused window_gap 10")).unwrap();
        match cmd {
            Command::Config(c) => {
                assert!(matches!(c.target, ConfigTarget::Monitor(_)));
                assert_eq!(c.name, "window_gap");
                assert_eq!(c.value, Some("10".to_string()));
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn unknown_domain_is_rejected() {
        let err = parse(&args("bogus")).unwrap_err();
        assert!(err.message.contains("Unknown domain"));
    }
}
