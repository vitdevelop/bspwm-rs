//! A tiny X11 client for live-testing XWayland window management (run it in
//! the QEMU test VM with `DISPLAY` pointing at `bspwm-rs`'s Xwayland):
//!
//! ```text
//! x11_probe normal [--secs N]           a plain window
//! x11_probe dock TOP [--secs N]         a panel: type DOCK, TOP pixels of `_NET_WM_STRUT_PARTIAL` at the top
//! x11_probe dialog [--secs N]           type DIALOG (bspwm floats it centred)
//! x11_probe toolbar [--secs N]          type TOOLBAR (bspwm does not focus it)
//! x11_probe clip TEXT [--secs N]        owns CLIPBOARD and serves TEXT as UTF8_STRING
//! x11_probe paste                       asks for CLIPBOARD as UTF8_STRING and prints it
//! x11_probe grab [--secs N]             maps a window, grabs the keyboard (`XGrabKeyboard`) and prints every key press
//! x11_probe pgrab [--secs N]            maps a window and grabs the pointer (`XGrabPointer`), as a game does
//! x11_probe urgent [--secs N]           maps a window and sets the ICCCM urgency hint after two seconds
//!
//! Every mapped window prints `button press <window>` for each click it gets,
//! and `_NET_WM_STATE [...]` / `_NET_WM_DESKTOP n` whenever the window manager changes them.
//! ```
//!
//! Every mode prints `mapped <id> <w>x<h>`-style lines a test script can grep.

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, GrabMode, PropMode, SelectionNotifyEvent, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

fn atom(conn: &RustConnection, name: &str) -> u32 {
    conn.intern_atom(false, name.as_bytes()).expect("intern").reply().expect("reply").atom
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_default();
    let mut positional = Vec::new();
    let mut secs = 60u64;
    while let Some(arg) = args.next() {
        if arg == "--secs" {
            secs = args.next().and_then(|v| v.parse().ok()).unwrap_or(60);
        } else {
            positional.push(arg);
        }
    }
    let (conn, screen_num) = RustConnection::connect(None).expect("cannot connect to the X server ($DISPLAY)");
    let screen = &conn.setup().roots[screen_num];
    let sw = screen.width_in_pixels;
    let win = conn.generate_id().expect("id");
    let (w, h, x, y) = match mode.as_str() {
        "dock" => (sw, positional.first().and_then(|v| v.parse().ok()).unwrap_or(30u16), 0i16, 0i16),
        "dialog" => (300, 200, 10, 10),
        "toolbar" => (200, 80, 10, 10),
        _ => (400, 300, 10, 10),
    };
    conn.create_window(
        screen.root_depth,
        win,
        screen.root,
        x,
        y,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        0,
        &CreateWindowAux::new().background_pixel(screen.white_pixel).event_mask(EventMask::EXPOSURE | EventMask::PROPERTY_CHANGE | EventMask::KEY_PRESS | EventMask::BUTTON_PRESS),
    )
    .expect("create window");
    conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_CLASS, AtomEnum::STRING, b"x11probe\0X11Probe\0").expect("class");
    conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, mode.as_bytes()).expect("name");

    let type_atom = match mode.as_str() {
        "dock" => Some("_NET_WM_WINDOW_TYPE_DOCK"),
        "dialog" => Some("_NET_WM_WINDOW_TYPE_DIALOG"),
        "toolbar" => Some("_NET_WM_WINDOW_TYPE_TOOLBAR"),
        _ => None,
    };
    if let Some(name) = type_atom {
        let (kind, value) = (atom(&conn, "_NET_WM_WINDOW_TYPE"), atom(&conn, name));
        conn.change_property32(PropMode::REPLACE, win, kind, AtomEnum::ATOM, &[value]).expect("type");
    }
    if mode == "dock" {
        // left, right, top, bottom, left_start_y, left_end_y, right_start_y, right_end_y,
        // top_start_x, top_end_x, bottom_start_x, bottom_end_x
        let strut = [0, 0, h as u32, 0, 0, 0, 0, 0, 0, sw as u32 - 1, 0, 0];
        let property = atom(&conn, "_NET_WM_STRUT_PARTIAL");
        conn.change_property32(PropMode::REPLACE, win, property, AtomEnum::CARDINAL, &strut).expect("strut");
    }

    let clipboard = atom(&conn, "CLIPBOARD");
    let utf8 = atom(&conn, "UTF8_STRING");
    let targets = atom(&conn, "TARGETS");
    let text = positional.first().cloned().unwrap_or_default();
    match mode.as_str() {
        "clip" => {
            conn.set_selection_owner(win, clipboard, x11rb::CURRENT_TIME).expect("owner");
            conn.flush().expect("flush");
            println!("owning CLIPBOARD with {text:?}");
        }
        "paste" => {
            let property = atom(&conn, "PASTE_PROBE");
            conn.convert_selection(win, clipboard, utf8, property, x11rb::CURRENT_TIME).expect("convert");
            conn.flush().expect("flush");
        }
        _ => {
            conn.map_window(win).expect("map");
            conn.flush().expect("flush");
            println!("mapped {win:#x} {w}x{h}");
        }
    }
    if mode == "grab" {
        // Give the window manager a moment to focus the new window first.
        std::thread::sleep(Duration::from_millis(1500));
        let status = conn.grab_keyboard(true, win, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC).expect("grab").reply().expect("grab reply").status;
        println!("keyboard grab: {status:?}");
    }
    if mode == "urgent" {
        std::thread::sleep(Duration::from_secs(2));
        // WM_HINTS: flags (InputHint | XUrgencyHint), input = true, the rest unused.
        let hints = [1u32 | 256, 1, 0, 0, 0, 0, 0, 0, 0];
        conn.change_property32(PropMode::REPLACE, win, AtomEnum::WM_HINTS, AtomEnum::WM_HINTS, &hints).expect("hints");
        conn.flush().expect("flush");
        println!("urgency hint set");
    }
    let net_state = atom(&conn, "_NET_WM_STATE");
    let net_desktop = atom(&conn, "_NET_WM_DESKTOP");
    if mode == "pgrab" {
        std::thread::sleep(Duration::from_millis(1500));
        let status = conn
            .grab_pointer(false, win, EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION, GrabMode::ASYNC, GrabMode::ASYNC, x11rb::NONE, x11rb::NONE, x11rb::CURRENT_TIME)
            .expect("grab")
            .reply()
            .expect("grab reply")
            .status;
        println!("pointer grab: {status:?}");
    }

    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        match conn.poll_for_event() {
            Ok(Some(Event::SelectionRequest(request))) => {
                let mut reply_property = request.property;
                if request.target == targets {
                    let _ = conn.change_property32(PropMode::REPLACE, request.requestor, request.property, AtomEnum::ATOM, &[targets, utf8]);
                } else if request.target == utf8 {
                    let _ = conn.change_property8(PropMode::REPLACE, request.requestor, request.property, utf8, text.as_bytes());
                } else {
                    reply_property = x11rb::NONE;
                }
                let notify = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: request.time,
                    requestor: request.requestor,
                    selection: request.selection,
                    target: request.target,
                    property: reply_property,
                };
                let _ = conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify);
                let _ = conn.flush();
                println!("served a selection request");
            }
            Ok(Some(Event::SelectionNotify(notify))) => {
                if notify.property == x11rb::NONE {
                    println!("paste: the selection owner refused");
                } else if let Ok(reply) = conn.get_property(true, win, notify.property, AtomEnum::ANY, 0, 1024).map(|c| c.reply()) {
                    match reply {
                        Ok(reply) => println!("paste: {:?}", String::from_utf8_lossy(&reply.value)),
                        Err(err) => println!("paste: cannot read the property: {err}"),
                    }
                }
                if mode == "paste" {
                    return;
                }
            }
            Ok(Some(Event::KeyPress(key))) => println!("key press {}", key.detail),
            Ok(Some(Event::ButtonPress(b))) => println!("button press {:#x} {}", b.event, b.detail),
            Ok(Some(Event::PropertyNotify(p))) if p.window == win && (p.atom == net_state || p.atom == net_desktop) => {
                let Ok(Ok(reply)) = conn.get_property(false, win, p.atom, AtomEnum::ANY, 0, 64).map(|c| c.reply()) else { continue };
                let values: Vec<u32> = reply.value32().map(|v| v.collect()).unwrap_or_default();
                if p.atom == net_desktop {
                    println!("_NET_WM_DESKTOP {}", values.first().copied().unwrap_or(u32::MAX));
                } else {
                    let names: Vec<String> = values
                        .iter()
                        .filter_map(|a| conn.get_atom_name(*a).ok()?.reply().ok())
                        .map(|r| String::from_utf8_lossy(&r.name).trim_start_matches("_NET_WM_STATE_").to_string())
                        .collect();
                    println!("_NET_WM_STATE {names:?}");
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    if mode == "paste" {
        println!("paste: timed out");
    }
}
