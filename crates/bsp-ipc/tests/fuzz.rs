//! Property-based "fuzzing" of the untrusted IPC surface (design.md,
//! Hardening): whatever bytes or argument vectors a local client sends,
//! decoding, parsing and executing must never panic and must always
//! produce a reply.

use bsp_core::desktop::Desktop;
use bsp_core::geometry::Rect;
use bsp_core::id::{DesktopId, MonitorId, WindowId};
use bsp_core::monitor::Monitor;
use bsp_core::node::Client as CoreClient;
use bsp_core::settings::Settings;
use bsp_core::wm::Wm;
use bsp_ipc::adapter::FakeAdapter;
use bsp_ipc::command::{self, Command};
use bsp_ipc::exec::{self, ExecCtx};
use bsp_ipc::registry::NodeRegistry;
use bsp_ipc::wire::{self, Reply};
use proptest::prelude::*;

fn fixture() -> (Wm, NodeRegistry, FakeAdapter) {
    let settings = Settings::default();
    let mut wm = Wm::new(settings.clone());
    let mut registry = NodeRegistry::new();
    let mut m = Monitor::new(
        MonitorId(1),
        Some("eDP-1"),
        Rect::new(0, 0, 800, 600),
        &settings,
    );
    let mut d = Desktop::new(DesktopId(1), Some("I"), &settings);
    let a = d
        .tree
        .new_client_node(&settings, CoreClient::new(WindowId(1), 1));
    d.tree.insert_node(&settings, a, None);
    let b = d
        .tree
        .new_client_node(&settings, CoreClient::new(WindowId(2), 1));
    d.tree.insert_node(&settings, b, Some(a));
    m.add_desktop(d);
    m.add_desktop(Desktop::new(DesktopId(2), Some("II"), &settings));
    m.arrange(0, &settings);
    registry.register(DesktopId(1), a);
    registry.register(DesktopId(1), b);
    wm.add_monitor(m);
    wm.focus_monitor(0);
    wm.monitors[0].focused = Some(0);
    (wm, registry, FakeAdapter::new())
}

const VOCAB: &[&str] = &[
    "node", "desktop", "monitor", "query", "wm", "rule", "config", "state", "subscribe", "quit",
    "output", "input", "-f", "-s", "-d", "-m", "-n", "-N", "-D", "-M", "-l", "-t", "-g", "-r",
    "-c", "-p", "-z", "-a", "-k", "-o", "-i", "-b", "-v", "-H", "-P", "--focus", "--to-desktop",
    "--to-monitor", "--swap", "--close", "--kill", "--flag", "--layer", "--state", "--ratio",
    "--rotate", "--flip", "--equalize", "--balance", "--presel-dir", "--presel-ratio",
    "--activate", "--remove", "--rename", "--reorder-monitors", "--add-desktops",
    "--add-monitor", "--move", "--resize", "focused", "last", "newest", "older", "newer",
    "west", "east", "north", "south", "next", "prev", "any", "!", ".local", ".!hidden",
    ".window", ".tiled", ".floating", "#", "@", "/", "~", ":", "0", "1", "-1", "0.5", "+10",
    "-10", "99999999999999999999", "0x10", "800x600+0+0", "hidden=on", "sticky=off", "tiled",
    "pseudo_tiled", "floating", "fullscreen", "above", "below", "normal", "horizontal",
    "vertical", "90", "180", "270", "border_width", "window_gap", "split_ratio", "focused_border_color",
    "pointer_modifier", "pointer_action1", "external_rules_command", "automatic_scheme",
    "monitor_rectangle", "\u{0}", "\u{feff}", "é", "", " ", "%", "*", "\n",
];

/// Mostly-valid shape: a domain word first so parsing gets past the
/// dispatcher and the executor is actually exercised.
fn arb_args() -> impl Strategy<Value = Vec<String>> {
    (
        prop::sample::select(&["node", "desktop", "monitor", "query", "wm", "rule", "config"][..]),
        arb_tail(),
    )
        .prop_map(|(head, mut tail)| {
            tail.insert(0, head.to_string());
            tail
        })
}

fn arb_tail() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(
        prop_oneof![
            8 => prop::sample::select(VOCAB).prop_map(String::from),
            1 => ".{0,12}",
        ],
        0..7,
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    #[test]
    fn decode_request_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = wire::decode_request(&bytes);
    }

    #[test]
    fn reply_parse_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = Reply::parse(&bytes);
    }

    #[test]
    fn encode_decode_roundtrips_without_nul(args in prop::collection::vec("[^\\x00]{0,16}", 0..8)) {
        let enc = wire::encode_request(args.iter().map(String::as_str));
        prop_assert_eq!(wire::decode_request(&enc), args);
    }

    #[test]
    fn parse_never_panics(args in arb_args(), raw in arb_tail()) {
        let _ = command::parse(&args);
        let _ = command::parse(&raw);
    }

    #[test]
    fn execute_never_panics(args in arb_args()) {
        if let Ok(cmd) = command::parse(&args) {
            if matches!(cmd, Command::Subscribe { .. } | Command::Quit { .. }) {
                return Ok(());
            }
            let (mut wm, mut registry, mut adapter) = fixture();
            let mut ctx = ExecCtx { wm: &mut wm, registry: &mut registry, adapter: &mut adapter };
            let _ = exec::execute(&mut ctx, &cmd);
        }
    }
}
