//! Interactive pointer bindings: `bspc config pointer_modifier`/
//! `pointer_action1..3`/`click_to_focus`/`pointer_motion_interval`/
//! `swallow_first_click`, and the drag itself (`docs/bsp-hotkeys.md`'s
//! "Pointer bindings for moving and resizing floating windows, as
//! bspwm's `pointer_action` settings do").
//!
//! bspwm intercepts pointer buttons via X11 *passive grabs*, registered
//! per (button, modifier) combination up front (`window_grab_buttons()`):
//! one grab per `click_to_focus` button held with no modifier, one per
//! button with a configured `pointer_action` held with `pointer_modifier`
//! — so `src/events.c` `button_press()` only ever runs for a press that
//! already matched one of those two registrations, and its `cleaned_mask`
//! check just disambiguates *which* of the two fired. Wayland has no
//! passive-grab equivalent: this compositor sees every press
//! unconditionally, so [`on_button_press`] re-derives that same
//! selection itself instead — a press matching neither is forwarded to
//! the client completely untouched, exactly as if bspwm had never
//! registered a grab for it either.
//!
//! `pointer_modifier` is stored as this crate's own `bsp_hotkeys::
//! binding::Modifier` (default `Super`, canonicalized to `Mod4` before
//! every comparison — `Modifier::canonical`, `bsp-hotkeys`) rather than
//! bspwm's literal `mod1`..`mod5`/`shift`/`control`/`lock`-only
//! vocabulary (`src/parse.c` `parse_modifier_mask()`): every one of
//! those raw X11 bit names is fully supported too (`cleaned_modifiers`
//! resolves `Mod1`..`Mod5` from the live keymap, full modifier coverage,
//! `docs/design.md`'s hotkeys row), this crate's vocabulary is simply a
//! deliberate superset — it also accepts `hyper`/`meta`/`mode_switch`,
//! which `parse_modifier_mask()` itself does not, the same wider
//! grammar `bsp-hotkeys` already parses for sxhkdrc chords
//! (`docs/bsp-hotkeys.md`). A deliberate, documented deviation, not a
//! guess.
//!
//! Not implemented: `focus_follows_pointer`, `pointer_follows_focus`,
//! `pointer_follows_monitor` (hover-based automatic focus/warping — a
//! separate bspwm feature from the click/drag bindings this module
//! covers); `grab_pointer()`'s "click on empty monitor background
//! switches monitor focus" case (only a window actually under the
//! pointer resolves here); cross-monitor transfer on a floating drag
//! (same gap as `bsp_core::tree::Tree::move_floating`, `docs/bsp-core.md`);
//! and ICCCM/`xdg_toplevel` size hints during a drag-resize (no such
//! hints are tracked anywhere in this build yet).

use std::collections::HashSet;

use smithay::backend::input::ButtonState;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, Focus, GestureHoldBeginEvent, GestureHoldEndEvent,
    GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
    GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData, MotionEvent, PointerGrab,
    PointerInnerHandle, RelativeMotionEvent,
};
use smithay::utils::{Logical, Point, Serial};

use bsp_core::id::{NodeId, WindowId};
use bsp_core::node::ClientState;
use bsp_core::tree::{PointerAction, ResizeHandle};
use bsp_hotkeys::binding::Modifier;
use bsp_ipc::report::{Event, PointerPhase};

use crate::state::State;

/// `click_to_focus`'s three shapes.
///
/// bspwm: `int8_t click_to_focus` (`-1`/`XCB_BUTTON_INDEX_ANY`/a real
/// button, `src/parse.c` `parse_button_index()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickToFocus {
    /// `none`: no plain click focuses by itself.
    None,
    /// `any`: any of button1/2/3 focuses.
    Any,
    /// `buttonN`.
    Button(u8),
}

/// `bspc config pointer_modifier`/`pointer_action1..3`/`click_to_focus`/
/// `pointer_motion_interval`/`swallow_first_click` — compositor-local,
/// like `State::hotkeys_inline_bspc`: `bsp-core::Settings`' own module
/// doc comment reserves settings like these for `bsp-compositor`.
///
/// bspwm: `src/settings.c`/`src/settings.h`'s matching globals.
#[derive(Debug, Clone, PartialEq)]
pub struct PointerSettings {
    /// Held together with a `BUTTONS[]` button to start a drag.
    pub modifier: Modifier,
    /// What button1/2/3 (index 0/1/2) does when held with `modifier`.
    pub actions: [PointerAction; 3],
    /// Which plain (no-modifier) click focuses the window under it.
    pub click_to_focus: ClickToFocus,
    /// Minimum milliseconds between two motion updates during a drag.
    pub motion_interval: u32,
    /// If `true`, a click-to-focus press that actually changed focus is
    /// not also forwarded to the newly focused client.
    pub swallow_first_click: bool,
}

impl Default for PointerSettings {
    fn default() -> Self {
        Self {
            modifier: Modifier::Super,
            actions: [
                PointerAction::Move,
                PointerAction::ResizeSide,
                PointerAction::ResizeCorner,
            ],
            click_to_focus: ClickToFocus::Button(1),
            motion_interval: 17,
            swallow_first_click: false,
        }
    }
}

/// Linux evdev button codes (`linux/input-event-codes.h`), the shape
/// `smithay::input::pointer::ButtonEvent::button` carries.
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

/// Maps an evdev button code to bspwm's `BUTTONS[]` index (1/2/3),
/// following the standard X11 button numbering libinput itself uses
/// when feeding X (1=left, 2=middle, 3=right) — not a bspwm-specific
/// detail, the same convention every X11/Wayland input stack shares.
fn button_index(code: u32) -> Option<u8> {
    match code {
        BTN_LEFT => Some(1),
        BTN_MIDDLE => Some(2),
        BTN_RIGHT => Some(3),
        _ => None,
    }
}

/// `bspc config pointer_modifier`/`pointer_action1..3`/`click_to_focus`/
/// `pointer_motion_interval`/`swallow_first_click` — a family of
/// compositor-only settings `bsp_ipc::exec` has never heard of
/// (`State::pointer_settings`'s doc comment), intercepted here before a
/// `Command::Config` naming one of them would otherwise reach
/// `bsp_ipc::exec::execute` and fall into its generic "Unknown setting"
/// reply. Mirrors `ipc::try_hotkeys_inline_bspc`'s own interception
/// exactly, including being reused as-is by `crate::hotkeys::run_inline`
/// for the same equivalence reason.
///
/// `Some` when `command` named one of these settings (handled, get or
/// set); `None` for anything else.
pub fn try_config(
    state: &mut State,
    command: &bsp_ipc::command::Command,
) -> Option<bsp_ipc::wire::Reply> {
    use bsp_ipc::wire::Reply;

    let bsp_ipc::command::Command::Config(c) = command else {
        return None;
    };
    let value = c.value.as_deref();
    Some(match c.name.as_str() {
        "pointer_modifier" => match value {
            None => Reply::Ok(format!(
                "{}\n",
                format_modifier(state.pointer_settings.modifier)
            )),
            Some(v) => match parse_pointer_modifier(v) {
                Some(m) => {
                    state.pointer_settings.modifier = m;
                    Reply::Ok(String::new())
                }
                None => invalid("pointer_modifier", v),
            },
        },
        "pointer_motion_interval" => match value {
            None => Reply::Ok(format!("{}\n", state.pointer_settings.motion_interval)),
            Some(v) => match v.parse::<u32>() {
                Ok(n) => {
                    state.pointer_settings.motion_interval = n;
                    Reply::Ok(String::new())
                }
                Err(_) => invalid("pointer_motion_interval", v),
            },
        },
        "pointer_action1" | "pointer_action2" | "pointer_action3" => {
            let index = c.name.as_bytes()[c.name.len() - 1] - b'1';
            match value {
                None => Reply::Ok(format!(
                    "{}\n",
                    format_pointer_action(state.pointer_settings.actions[index as usize])
                )),
                Some(v) => match parse_pointer_action(v) {
                    Some(a) => {
                        state.pointer_settings.actions[index as usize] = a;
                        Reply::Ok(String::new())
                    }
                    None => invalid(&c.name, v),
                },
            }
        }
        "click_to_focus" => match value {
            None => Reply::Ok(format!(
                "{}\n",
                format_click_to_focus(state.pointer_settings.click_to_focus)
            )),
            Some(v) => match parse_click_to_focus(v) {
                Some(c) => {
                    state.pointer_settings.click_to_focus = c;
                    Reply::Ok(String::new())
                }
                None => invalid("click_to_focus", v),
            },
        },
        "swallow_first_click" => match value {
            None => Reply::Ok(format!(
                "{}\n",
                bool_str(state.pointer_settings.swallow_first_click)
            )),
            Some(v) => match bsp_ipc::value::parse_bool(v) {
                Some(b) => {
                    state.pointer_settings.swallow_first_click = b;
                    Reply::Ok(String::new())
                }
                None => invalid("swallow_first_click", v),
            },
        },
        _ => return None,
    })
}

fn invalid(name: &str, value: &str) -> bsp_ipc::wire::Reply {
    bsp_ipc::wire::Reply::Fail(format!("config: {name}: Invalid value: '{value}'.\n"))
}

fn bool_str(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

/// bspwm's own `pointer_modifier` vocabulary is `mod1`..`mod5`/`shift`/
/// `control`/`lock` (raw X11 modifier-bit names, `src/parse.c`
/// `parse_modifier_mask()`) — this module's own doc comment explains
/// why that has no reliable Wayland equivalent. `Modifier::parse`
/// accepts `bsp-hotkeys`' own, symbolic chord-modifier vocabulary
/// instead; `any` is rejected here (meaningless as a single held
/// modifier to compare against).
fn parse_pointer_modifier(s: &str) -> Option<Modifier> {
    match Modifier::parse(s)? {
        Modifier::Any => None,
        m => Some(m),
    }
}

fn format_modifier(m: Modifier) -> &'static str {
    match m {
        Modifier::Shift => "shift",
        Modifier::Control => "control",
        Modifier::Lock => "lock",
        Modifier::Alt => "alt",
        Modifier::Super => "super",
        Modifier::Hyper => "hyper",
        Modifier::Meta => "meta",
        Modifier::ModeSwitch => "mode_switch",
        Modifier::Mod1 => "mod1",
        Modifier::Mod2 => "mod2",
        Modifier::Mod3 => "mod3",
        Modifier::Mod4 => "mod4",
        Modifier::Mod5 => "mod5",
        Modifier::Any => "any",
    }
}

/// bspwm: `src/parse.c` `parse_pointer_action()`.
fn parse_pointer_action(s: &str) -> Option<PointerAction> {
    Some(match s {
        "move" => PointerAction::Move,
        "resize_side" => PointerAction::ResizeSide,
        "resize_corner" => PointerAction::ResizeCorner,
        "focus" => PointerAction::Focus,
        "none" => PointerAction::None,
        _ => return None,
    })
}

/// bspwm: `src/parse.c` `print_pointer_action()` (`src/messages.c`).
fn format_pointer_action(a: PointerAction) -> &'static str {
    match a {
        PointerAction::Move => "move",
        PointerAction::ResizeSide => "resize_side",
        PointerAction::ResizeCorner => "resize_corner",
        PointerAction::Focus => "focus",
        PointerAction::None => "none",
    }
}

/// bspwm: `src/parse.c` `parse_button_index()`.
fn parse_click_to_focus(s: &str) -> Option<ClickToFocus> {
    Some(match s {
        "any" => ClickToFocus::Any,
        "none" => ClickToFocus::None,
        "button1" => ClickToFocus::Button(1),
        "button2" => ClickToFocus::Button(2),
        "button3" => ClickToFocus::Button(3),
        _ => return None,
    })
}

fn format_click_to_focus(c: ClickToFocus) -> String {
    match c {
        ClickToFocus::Any => "any".to_string(),
        ClickToFocus::None => "none".to_string(),
        ClickToFocus::Button(n) => format!("button{n}"),
    }
}

/// The held modifiers relevant to `pointer_modifier`/`click_to_focus`
/// comparisons, with `Lock` (Caps Lock) always excluded.
///
/// bspwm: `src/helpers.h` `cleaned_mask(m)`, `m & ~(num_lock |
/// scroll_lock | caps_lock)` — stripped before *every* comparison
/// against `pointer_modifier` or a plain (no-modifier) click, so an
/// incidental Caps Lock never breaks either match. `num_lock` is
/// likewise excluded here by simply never being queried at all (unlike
/// `crate::hotkeys::resolve_modifiers`, which needs it for `Mod2`
/// coverage).
///
/// Full modifier coverage (`docs/design.md`'s hotkeys row): resolves
/// `Mod1`/`Mod3`/`Mod4`/`Mod5` from `ModifiersState`'s own fields for
/// the same reason `crate::hotkeys::resolve_modifiers` does — see that
/// function's doc comment, including why `Hyper`/`Meta` are
/// deliberately *not* queried and inserted here the same way (a real
/// bug, live-tested and reverted: on essentially every stock keymap
/// they alias the very same bit `alt`/`mod1` already does, so this
/// function only ever emits the eight real bits and leaves resolving a
/// configured `pointer_modifier hyper`/`meta` to
/// [`canonicalize_modifier`], `on_button_press`'s equivalent of
/// `crate::hotkeys::canonicalize_virtual_modifiers`).
fn cleaned_modifiers(state: &mut State) -> HashSet<Modifier> {
    let mods = state.seat.get_keyboard().unwrap().modifier_state();
    let mut set = HashSet::new();
    if mods.shift {
        set.insert(Modifier::Shift);
    }
    if mods.ctrl {
        set.insert(Modifier::Control);
    }
    if mods.alt {
        set.insert(Modifier::Mod1);
    }
    if mods.iso_level5_shift {
        set.insert(Modifier::Mod3);
    }
    if mods.logo {
        set.insert(Modifier::Mod4);
    }
    if mods.iso_level3_shift {
        set.insert(Modifier::Mod5);
    }
    set
}

/// Resolves `pointer_modifier`'s configured [`Modifier`] to whatever a
/// live [`cleaned_modifiers`] held set could actually contain: the
/// fixed `Alt`/`Super`/`ModeSwitch` aliases through
/// [`Modifier::canonical`], and `Hyper`/`Meta` through a live keymap
/// query — `crate::hotkeys::real_bit_for`, shared with
/// `crate::hotkeys::canonicalize_virtual_modifiers`, whose doc comment
/// explains why this can't just be a fixed alias like the other three.
/// Unlike that function, this one re-resolves on every press rather
/// than once at load time: `pointer_settings.modifier` is a single
/// value, not a whole chord list to rewrite in place, and re-querying
/// it costs one more `mod_get_index` lookup on top of the
/// `with_xkb_state` call `cleaned_modifiers` already makes for every
/// press regardless — not a new cost category.
///
/// `None` if `modifier` is `Hyper`/`Meta` and the live keymap doesn't
/// define that virtual modifier at all: it then can never be held,
/// exactly like any other unmatched chord.
fn canonicalize_modifier(state: &mut State, modifier: Modifier) -> Option<Modifier> {
    match modifier {
        Modifier::Hyper | Modifier::Meta => {
            let name = if modifier == Modifier::Hyper {
                "Hyper"
            } else {
                "Meta"
            };
            let keyboard = state.seat.get_keyboard()?;
            keyboard.with_xkb_state(state, |ctx| crate::hotkeys::real_bit_for(ctx.xkb(), name))
        }
        other => Some(other.canonical()),
    }
}

/// Handles one pointer button *press*, before the caller's normal
/// forward-to-client handling.
///
/// Returns `true` if `button`'s press should still be forwarded to the
/// client afterward; `false` if it was fully consumed here (a drag
/// grab started, or `swallow_first_click` suppressed a focus-changing
/// click) — see this module's doc comment for why a press matching
/// neither `pointer_modifier` nor `click_to_focus` is always forwarded
/// (`true`) with no focus change at all, matching bspwm's own
/// behavior for a press no passive grab was ever registered for.
///
/// Does nothing (just returns `true`, deferring entirely to whatever
/// grab is already active) while a [`DragGrab`] is in progress: this
/// function is plain application logic called from `crate::input`
/// before `PointerHandle::button()`, not part of Smithay's own grab
/// dispatch, so without this check a second button pressed mid-drag
/// could start a second, overlapping grab on top of the first —
/// bspwm's own X11 grab has no such hazard (its passive-grab
/// registration means a second button press during an active grab
/// never reaches `button_press()` in the first place, `src/pointer.c`
/// `grab_pointer()`'s `XCB_EVENT_MASK_BUTTON_RELEASE|MOTION`-only mask).
pub fn on_button_press(state: &mut State, button: u32, serial: Serial, time: u32) -> bool {
    if state.pointer.is_grabbed() {
        return true;
    }
    let Some(index) = button_index(button) else {
        return true;
    };
    let held = cleaned_modifiers(state);
    let configured = canonicalize_modifier(state, state.pointer_settings.modifier);

    if held.len() == 1 && configured.is_some_and(|m| held.contains(&m)) {
        let action = state.pointer_settings.actions[(index - 1) as usize];
        if action != PointerAction::None {
            begin_action(state, action, button, serial, time);
        }
        // bspwm: `button_press()`'s `else` branch never touches
        // `replay` (initialized to `false` and left alone), so a press
        // matching this branch is *always* swallowed — even one
        // `begin_action` itself ends up doing nothing for (no window
        // under the pointer, `ACTION_NONE`, a fullscreen target).
        return false;
    }

    if held.is_empty() {
        let matches = match state.pointer_settings.click_to_focus {
            ClickToFocus::None => false,
            ClickToFocus::Any => true,
            ClickToFocus::Button(b) => b == index,
        };
        if matches {
            return !click_to_focus(state, serial);
        }
    }

    true
}

/// Focuses the window under the pointer if it isn't already focused.
/// Returns whether focus actually changed, driving
/// `swallow_first_click` (bspwm: `grab_pointer(ACTION_FOCUS)`'s own
/// return value serves exactly this role for `button_press()`'s
/// `replay = !grab_pointer(ACTION_FOCUS) || !swallow_first_click`).
fn click_to_focus(state: &mut State, serial: Serial) -> bool {
    let location = state.pointer.current_location();
    let Some((mi, di, node)) = crate::input::window_under(state, location)
        .and_then(|id| crate::input::locate_window(state, id))
    else {
        return false;
    };
    let already_focused = state.wm.monitors[mi].desktops[di].tree.focus == Some(node)
        && state.wm.focused_monitor == Some(mi);
    if already_focused {
        return false;
    }
    crate::input::set_focus(state, mi, di, node, serial);
    true
}

/// Starts `action` on whichever node is under the pointer, if any —
/// `Focus` just focuses (no drag); `Move`/`ResizeSide`/`ResizeCorner`
/// set a [`DragGrab`] so every subsequent motion/button event drives it
/// instead of the default pointer behavior, until release.
///
/// bspwm: `src/pointer.c` `grab_pointer()`.
fn begin_action(state: &mut State, action: PointerAction, button: u32, serial: Serial, time: u32) {
    let location = state.pointer.current_location();
    let Some((mi, di, node)) = crate::input::window_under(state, location)
        .and_then(|id| crate::input::locate_window(state, id))
    else {
        return;
    };

    if action == PointerAction::Focus {
        let already_focused = state.wm.monitors[mi].desktops[di].tree.focus == Some(node)
            && state.wm.focused_monitor == Some(mi);
        if !already_focused {
            crate::input::set_focus(state, mi, di, node, serial);
        }
        return;
    }

    let client_state = state.wm.monitors[mi].desktops[di]
        .tree
        .node(node)
        .client
        .as_ref()
        .unwrap()
        .state;
    // bspwm: `grab_pointer()`'s own `STATE_FULLSCREEN` check — swallows
    // the press (handled above, by never touching `replay`) without
    // starting a drag.
    if client_state == ClientState::Fullscreen {
        return;
    }

    let window_id = state.wm.monitors[mi].desktops[di]
        .tree
        .node(node)
        .client
        .as_ref()
        .unwrap()
        .window;
    let handle = state.wm.monitors[mi].desktops[di].tree.get_handle(
        node,
        (location.x as i32, location.y as i32),
        action,
    );

    broadcast_pointer_action(state, mi, di, node, action, PointerPhase::Begin);

    let grab = DragGrab {
        start_data: GrabStartData {
            focus: state.pointer.current_focus().map(|focus| (focus, location)),
            button,
            location,
        },
        monitor: mi,
        desktop: di,
        node,
        window_id,
        action,
        handle,
        last_location: location,
        last_time: time,
    };
    let pointer = state.pointer.clone();
    pointer.set_grab(state, grab, serial, Focus::Clear);
}

/// `monitor`/`desktop`/`node`'s wire ids, the shape every `Event`
/// carries (`bsp_ipc::report`) — `bsp-ipc::registry::NodeRegistry`
/// keyed by `bsp-core`'s own ids, same lookup `bsp_ipc::exec`'s
/// internal helpers do.
fn wire_ids(state: &State, mi: usize, di: usize, node: NodeId) -> (u32, u32, u32) {
    let desktop_id = state.wm.monitors[mi].desktops[di].id;
    (
        state.wm.monitors[mi].id.0,
        desktop_id.0,
        state.registry.id_of(desktop_id, node).unwrap_or(0),
    )
}

/// bspwm: `src/pointer.c` `grab_pointer()`'s `put_status(
/// SBSC_MASK_POINTER_ACTION, "pointer_action ... %s %s\n", ...)` calls
/// — only for `Move`/`ResizeSide`/`ResizeCorner` (no `Focus` branch
/// exists there either).
fn broadcast_pointer_action(
    state: &mut State,
    mi: usize,
    di: usize,
    node: NodeId,
    action: PointerAction,
    phase: PointerPhase,
) {
    let action = match action {
        PointerAction::Move => "move",
        PointerAction::ResizeSide => "resize_side",
        PointerAction::ResizeCorner => "resize_corner",
        PointerAction::Focus | PointerAction::None => return,
    };
    let (monitor, desktop, node) = wire_ids(state, mi, di, node);
    state.subscribers.broadcast_event(&Event::PointerAction {
        monitor,
        desktop,
        node,
        action,
        phase,
    });
}

/// The Wayland equivalent of `src/pointer.c` `track_pointer()`'s
/// interactive loop: a [`PointerGrab`] set on button-press
/// (`begin_action`) that every subsequent motion/button event is
/// routed through instead of the default pointer behavior, until the
/// button that started it (and every other button) is released.
struct DragGrab {
    start_data: GrabStartData<State>,
    monitor: usize,
    desktop: usize,
    node: NodeId,
    window_id: WindowId,
    action: PointerAction,
    /// Which edge/corner is being dragged; only meaningful for
    /// `ResizeSide`/`ResizeCorner`, computed once at grab start
    /// (`get_handle`) exactly as bspwm's own `track_pointer()` does —
    /// not recomputed per motion tick.
    handle: ResizeHandle,
    last_location: Point<f64, Logical>,
    last_time: u32,
}

impl DragGrab {
    fn do_move(&mut self, data: &mut State, location: Point<f64, Logical>, dx: i32, dy: i32) {
        let Some(client) = data.wm.monitors[self.monitor].desktops[self.desktop]
            .tree
            .node(self.node)
            .client
            .clone()
        else {
            return;
        };
        if client.state.is_tiled() {
            self.do_tiled_move(data, location);
        } else {
            let moved = data.wm.monitors[self.monitor].desktops[self.desktop]
                .tree
                .move_floating(self.node, dx, dy);
            if moved {
                crate::shell::sync_wayland_from_core(data);
            }
        }
    }

    /// bspwm: `src/window.c` `move_client()`'s tiled branch — swaps
    /// with whichever *other* tiled leaf is now under the pointer, on
    /// the same monitor. `bsp_core::tree::Tree::swap_nodes` is
    /// structural only (`docs/bsp-core.md`: bspwm's own C `swap_nodes`
    /// also carries the X11 side effect of moving the real windows;
    /// this port doesn't), so re-arranging and re-syncing afterward is
    /// this caller's job, unlike bspwm's own `move_client`.
    fn do_tiled_move(&mut self, data: &mut State, location: Point<f64, Logical>) {
        let Some(target_window_id) = crate::input::window_under(data, location) else {
            return;
        };
        if target_window_id == self.window_id {
            return;
        }
        let Some((tmi, tdi, target_node)) = crate::input::locate_window(data, target_window_id)
        else {
            return;
        };
        if tmi != self.monitor {
            return;
        }
        let target_tiled = data.wm.monitors[tmi].desktops[tdi]
            .tree
            .node(target_node)
            .client
            .as_ref()
            .is_some_and(|c| c.state.is_tiled());
        if !target_tiled {
            return;
        }
        let swapped = data.wm.monitors[self.monitor].desktops[self.desktop]
            .tree
            .swap_nodes(self.node, target_node);
        if swapped {
            let settings = data.wm.settings.clone();
            data.wm.monitors[self.monitor].arrange(self.desktop, &settings);
            crate::shell::sync_wayland_from_core(data);
        }
    }

    fn do_resize(&mut self, data: &mut State, dx: i32, dy: i32) {
        let resized = data.wm.monitors[self.monitor].desktops[self.desktop]
            .tree
            .resize_node(self.node, self.handle, dx, dy, true);
        if resized {
            let settings = data.wm.settings.clone();
            data.wm.monitors[self.monitor].arrange(self.desktop, &settings);
            crate::shell::sync_wayland_from_core(data);
        }
    }
}

impl PointerGrab<State> for DragGrab {
    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(
            <State as smithay::input::SeatHandler>::PointerFocus,
            Point<f64, Logical>,
        )>,
        event: &MotionEvent,
    ) {
        // Focus stays cleared for the whole drag (`Focus::Clear` at
        // grab start): the dragged/resized client sees no pointer
        // motion at all while the compositor is driving it, matching
        // bspwm's fully-swallowed X11 grab.
        handle.motion(data, None, event);

        let dt = event.time.saturating_sub(self.last_time);
        if dt < data.pointer_settings.motion_interval {
            return;
        }
        let dx = (event.location.x - self.last_location.x).round() as i32;
        let dy = (event.location.y - self.last_location.y).round() as i32;
        if dx == 0 && dy == 0 {
            return;
        }
        self.last_time = event.time;
        self.last_location = event.location;

        match self.action {
            PointerAction::Move => self.do_move(data, event.location, dx, dy),
            PointerAction::ResizeSide | PointerAction::ResizeCorner => self.do_resize(data, dx, dy),
            PointerAction::Focus | PointerAction::None => {}
        }
    }

    fn relative_motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(
            <State as smithay::input::SeatHandler>::PointerFocus,
            Point<f64, Logical>,
        )>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        // bspwm's own X11 grab only listens for `BUTTON_RELEASE`/
        // `MOTION` while a drag is active (`grab_pointer()`'s
        // `xcb_grab_pointer(...XCB_EVENT_MASK_BUTTON_RELEASE|MOTION...)`)
        // — a *press* during the drag reaches this compositor
        // unconditionally (Wayland has no equivalent selective mask),
        // so it's ignored here rather than recursively starting a
        // second grab.
        if event.state != ButtonState::Released {
            return;
        }
        if !handle.current_pressed().is_empty() {
            return;
        }
        broadcast_pointer_action(
            data,
            self.monitor,
            self.desktop,
            self.node,
            self.action,
            PointerPhase::End,
        );
        if let Some(client) = data.wm.monitors[self.monitor].desktops[self.desktop]
            .tree
            .node(self.node)
            .client
            .clone()
        {
            let geometry = match client.state {
                ClientState::Floating => client.floating_rectangle,
                _ => client.tiled_rectangle,
            };
            let (monitor, desktop, node) = wire_ids(data, self.monitor, self.desktop, self.node);
            data.subscribers.broadcast_event(&Event::NodeGeometry {
                monitor,
                desktop,
                node,
                geometry,
            });
        }
        let report = bsp_ipc::exec::build_report(&data.wm);
        data.subscribers.broadcast_report(&report);
        handle.unset_grab(self, data, event.serial, event.time, true);
    }

    fn axis(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data);
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &GrabStartData<State> {
        &self.start_data
    }

    fn unset(&mut self, _data: &mut State) {}
}
