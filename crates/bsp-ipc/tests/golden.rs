//! Golden tests: bspwm-rs against transcripts recorded from the real bspwm.
//!
//! Each `tests/golden/NAME.scn` is a scenario (`bspc` commands, windows
//! mapped and closed); `tests/golden/NAME.out` is what bspwm 0.9.12 printed
//! for it under Xvfb, recorded by `bsp-compositor`'s `examples/golden_record.rs`
//! (`contrib/golden/record.sh` in the QEMU test VM). This test replays the
//! scenario through the same code the compositor runs (`exec::execute`,
//! `exec::manage_window`, `exec::unmanage_window`, and the compositor's rule for
//! when a report line follows) and compares both transcripts: every step's
//! output, errors, exit status, and the `subscribe all` lines it produced.
//!
//! Node, desktop and monitor ids differ (bspwm's are X ids), so both
//! transcripts name every id by its first appearance (`#1`, `#2`, ...) before
//! they are compared, in hex output and in `query -T`/`wm -d` JSON alike.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bsp_core::desktop::Desktop;
use bsp_core::geometry::Rect;
use bsp_core::id::{DesktopId, MonitorId, WindowId};
use bsp_core::monitor::Monitor;
use bsp_core::rules::RuleConsequence;
use bsp_core::settings::Settings;
use bsp_core::wm::Wm;
use bsp_ipc::adapter::FakeAdapter;
use bsp_ipc::command::{self, Command};
use bsp_ipc::exec::{self, ExecCtx, NewWindow};
use bsp_ipc::registry::NodeRegistry;
use bsp_ipc::report::Event;
use bsp_ipc::wire::Reply;

/// Splits a scenario line into words, keeping `'...'`/`"..."` together (the
/// same rules as `golden_record`'s).
fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut quote = None;
    for ch in line.chars() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => word.push(ch),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                in_word = true;
            }
            None if ch.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            None => {
                word.push(ch);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// `WxH+X+Y`.
fn parse_geometry(s: &str) -> Option<Rect> {
    let (size, pos) = s.split_once('+')?;
    let (w, h) = size.split_once('x')?;
    let (x, y) = pos.split_once('+')?;
    Some(Rect::new(x.parse().ok()?, y.parse().ok()?, w.parse().ok()?, h.parse().ok()?))
}

/// bspwm-rs's state for one scenario, and the transcript it produced.
struct Harness {
    wm: Wm,
    registry: NodeRegistry,
    adapter: FakeAdapter,
    /// The scenario's windows, in the order they were mapped.
    windows: Vec<WindowId>,
    out: Vec<String>,
}

impl Harness {
    /// The state the `@monitor NAME WxH+X+Y DESKTOP...` header describes.
    fn new(header: &str) -> Self {
        let words: Vec<&str> = header.split_whitespace().collect();
        assert_eq!(words.first(), Some(&"@monitor"), "the transcript starts with its `@monitor` header");
        let settings = Settings::default();
        let mut wm = Wm::new(settings.clone());
        let rect = parse_geometry(words[2]).expect("the header's monitor geometry");
        let mut monitor = Monitor::new(MonitorId(bsp_core::id::FIRST_MONITOR_ID), Some(words[1]), rect, &settings);
        for (i, name) in words[3..].iter().enumerate() {
            monitor.add_desktop(Desktop::new(DesktopId(bsp_core::id::FIRST_DESKTOP_ID + i as u32), Some(name), &settings));
        }
        wm.add_monitor(monitor);
        wm.focus_monitor(0);
        wm.monitors[0].focused = Some(0);
        let mut harness = Self { wm, registry: NodeRegistry::new(), adapter: FakeAdapter::new(), windows: Vec::new(), out: vec![header.to_owned()] };
        // `subscribe` first prints the current report.
        harness.report();
        harness
    }

    fn report(&mut self) {
        let line = exec::build_report(&self.wm).to_string();
        self.out.push(format!("> {}", line.trim_end()));
    }

    /// What the compositor's `ipc::broadcast_events` sends: the events, then
    /// a report when there was any.
    fn broadcast(&mut self, events: &[Event]) {
        if events.is_empty() {
            return;
        }
        for event in events {
            self.out.push(format!("> {event}"));
        }
        self.report();
    }

    /// An operation the compositor wraps in `ipc::with_ops`.
    fn with_ops<R>(&mut self, f: impl FnOnce(&mut ExecCtx<FakeAdapter>, &mut Vec<Event>) -> R) -> R {
        self.wm.sync_history();
        let layouts = exec::layout_snapshot(&self.wm);
        let mut events = Vec::new();
        let result = {
            let mut ctx = ExecCtx { wm: &mut self.wm, registry: &mut self.registry, adapter: &mut self.adapter };
            f(&mut ctx, &mut events)
        };
        self.wm.sync_history();
        self.registry.sync_with(&self.wm);
        exec::push_layout_changes(&self.wm, &layouts, &mut events);
        exec::push_geometry_changes(&mut self.wm, &self.registry, &mut events);
        self.broadcast(&events);
        result
    }

    /// `bspc ARGS`: as the compositor's `ipc::execute_and_broadcast`, then the
    /// windows `node -c`/`-k` closed going away.
    fn bspc(&mut self, args: &[String]) {
        let command = match command::parse(args) {
            Ok(c) => c,
            Err(e) => {
                self.fail(&e.message);
                return;
            }
        };
        if matches!(command, Command::Subscribe { .. } | Command::Quit(_)) {
            self.out.push("! not replayed".to_owned());
            return;
        }
        let (reply, events) = {
            let mut ctx = ExecCtx { wm: &mut self.wm, registry: &mut self.registry, adapter: &mut self.adapter };
            exec::execute(&mut ctx, &command)
        };
        match reply {
            Reply::Ok(text) => self.out.extend(text.lines().map(str::to_owned)),
            Reply::Fail(message) => self.fail(&message),
        }
        if !(command.is_read_only() && events.is_empty()) {
            for event in &events {
                self.out.push(format!("> {event}"));
            }
            self.report();
        }
        self.wm.sync_history();
        let gone: Vec<WindowId> = self.adapter.closed.drain(..).chain(self.adapter.killed.drain(..)).collect();
        for window in gone {
            self.close_window(window);
        }
    }

    fn fail(&mut self, message: &str) {
        self.out.extend(message.lines().map(|l| format!("! {l}")));
        self.out.push("? exit 1".to_owned());
    }

    /// `window CLASS INSTANCE [WxH+X+Y]`: an X11 window of no particular type
    /// (as the compositor's `xwayland::finish_map` maps it).
    fn window(&mut self, words: &[String]) {
        let class = words.get(1).cloned().unwrap_or_default();
        let instance = words.get(2).cloned().unwrap_or_else(|| class.to_lowercase());
        let geometry = words.get(3).and_then(|g| parse_geometry(g)).unwrap_or(Rect::new(10, 10, 400, 300));
        let window = WindowId(self.windows.len() as u32 + 1);
        self.windows.push(window);
        self.adapter.set_class(window, &class, &instance);
        let mut consequence = RuleConsequence::default();
        consequence.merge(&bsp_core::rules::match_rules(&mut self.wm.rules, &class, &instance, &class));
        let new = NewWindow { window, geometry: Some(geometry), size_hints: Default::default() };
        self.with_ops(|ctx, events| exec::manage_window(ctx, &new, &consequence, events));
    }

    /// `close N`: the window is destroyed (the compositor's
    /// `shell::unmap_window`).
    fn close(&mut self, words: &[String]) {
        let index = words.get(1).and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
        match self.windows.get(index.wrapping_sub(1)).copied() {
            Some(window) => self.close_window(window),
            None => self.out.push(format!("! no window {index}")),
        }
    }

    fn close_window(&mut self, window: WindowId) {
        self.wm.stacking.remove(window);
        self.with_ops(|ctx, events| exec::unmanage_window(ctx, window, events));
    }

    fn step(&mut self, line: &str) {
        self.out.push(format!("$ {line}"));
        let words = split_words(line);
        match words.first().map(String::as_str) {
            Some("bspc") => self.bspc(&words[1..]),
            Some("window") => self.window(&words),
            Some("close") => self.close(&words),
            _ => self.out.push("! unknown step".to_owned()),
        }
    }
}

/// Names every id by its first appearance: `0xXXXXXXXX` words, and the id
/// fields of JSON lines.
struct Canon(HashMap<u64, usize>);

impl Canon {
    fn name(&mut self, id: u64) -> String {
        if id == 0 {
            return "0".to_owned();
        }
        let next = self.0.len() + 1;
        format!("#{}", self.0.entry(id).or_insert(next))
    }

    fn line(&mut self, line: &str) -> String {
        let (prefix, rest) = line.split_at(if line.starts_with("> ") { 2 } else { 0 });
        if rest.starts_with('{') {
            if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(rest) {
                self.json(None, &mut value);
                return format!("{prefix}{value}");
            }
        }
        let mut out = String::with_capacity(line.len());
        let mut i = 0;
        let bytes = line.as_bytes();
        while i < bytes.len() {
            if line[i..].starts_with("0x") && i + 10 <= line.len() && line[i + 2..i + 10].chars().all(|c| c.is_ascii_hexdigit()) {
                if let Ok(id) = u64::from_str_radix(&line[i + 2..i + 10], 16) {
                    out.push_str(&self.name(id));
                    i += 10;
                    continue;
                }
            }
            let ch = line[i..].chars().next().unwrap_or(' ');
            out.push(ch);
            i += ch.len_utf8();
        }
        out
    }

    fn json(&mut self, key: Option<&str>, value: &mut serde_json::Value) {
        use serde_json::Value;
        let is_id = |k: &str| k == "id" || k.ends_with("Id") || k == "stackingList";
        // A RandR output id: nothing of the kind on Wayland.
        if key == Some("randrId") {
            *value = Value::String("?".to_owned());
            return;
        }
        match value {
            Value::Object(map) => {
                for (k, v) in map.iter_mut() {
                    self.json(Some(k), v);
                }
            }
            Value::Array(items) => {
                for v in items {
                    self.json(key.filter(|k| *k == "stackingList"), v);
                }
            }
            Value::Number(n) if key.is_some_and(is_id) => {
                if let Some(id) = n.as_u64() {
                    *value = Value::String(self.name(id));
                }
            }
            _ => {}
        }
    }

    fn transcript(lines: &[String]) -> Vec<String> {
        let mut canon = Canon(HashMap::new());
        lines.iter().map(|l| canon.line(l)).collect()
    }
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// Runs one scenario and returns bspwm's and bspwm-rs's transcripts,
/// canonicalized.
fn run(name: &str) -> (Vec<String>, Vec<String>) {
    let dir = golden_dir();
    let scenario = std::fs::read_to_string(dir.join(format!("{name}.scn"))).expect("scenario");
    let recorded = std::fs::read_to_string(dir.join(format!("{name}.out"))).expect("recorded transcript");
    let expected: Vec<String> = recorded.lines().map(str::to_owned).collect();
    let mut harness = Harness::new(&expected[0]);
    for line in scenario.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        harness.step(line);
    }
    (Canon::transcript(&expected), Canon::transcript(&harness.out))
}

/// One step as it is compared: its title and `bspc` output in order, then its
/// events sorted (bspwm interleaves events and reports in its own order), then
/// the latest report line as of this step (bspwm prints reports at its own
/// moments, sometimes several per change and sometimes none; what a bar shows
/// is the latest one). `report` carries it from step to step.
fn normalize(step: &[String], report: &mut Option<String>) -> Vec<String> {
    let mut lines: Vec<String> = step.iter().filter(|l| !l.starts_with("> ")).cloned().collect();
    let mut events: Vec<String> = step.iter().filter(|l| l.starts_with("> ") && !l.starts_with("> W")).cloned().collect();
    events.sort();
    lines.extend(events);
    if let Some(last) = step.iter().rev().find(|l| l.starts_with("> W")) {
        *report = Some(last.clone());
    }
    lines.extend(report.clone());
    lines
}

/// How a step known to differ is compared instead.
enum Known {
    /// Only that every event bspwm-rs sends is one bspwm sent too (bspwm sent
    /// more); output and report as usual.
    Subset,
    /// The `wm -d` JSON without this key.
    WithoutKey(&'static str),
}

/// Steps where bspwm does something bspwm-rs cannot, and why.
const KNOWN: &[(&str, &str, Known, &str)] = &[
    // After a desktop switch or a closed window the X server sends `FocusIn`,
    // and bspwm focuses (and stacks, and sometimes re-reports the geometry of)
    // the window again or first: repeated or extra events. The compositor has
    // no such X round trip.
    ("desktop", "$ bspc desktop next.occupied -f", Known::Subset, "X FocusIn"),
    ("config", "$ bspc node -c", Known::Subset, "X FocusIn"),
    ("tree", "$ bspc node -d two", Known::Subset, "X FocusIn"),
    ("tree", "$ bspc desktop -f two", Known::Subset, "X FocusIn"),
    ("tree", "$ bspc node -d one --follow", Known::Subset, "X FocusIn"),
    ("tree", "$ bspc node -c", Known::Subset, "X FocusIn"),
    ("tree", "$ bspc node -k", Known::Subset, "X FocusIn"),
    // bspwm records every focus in its history, also the empty desktop
    // `node -d one --follow` focuses on the way; bspwm-rs records the focus at
    // the end of each command.
    ("tree", "$ bspc wm -d", Known::WithoutKey("focusHistory"), "focus within a command"),
];

/// A step's lines with `key` taken out of its JSON lines.
fn without_key(step: &[String], key: &str) -> Vec<String> {
    step.iter()
        .map(|l| match serde_json::from_str::<serde_json::Value>(l) {
            Ok(serde_json::Value::Object(mut map)) => {
                map.remove(key);
                serde_json::Value::Object(map).to_string()
            }
            _ => l.clone(),
        })
        .collect()
}

/// Whether bspwm's step `e` and bspwm-rs's `a` agree, given what is known to
/// differ in `scenario`.
fn agree(scenario: &str, e: &[String], a: &[String]) -> bool {
    let title = e.first().map(String::as_str).unwrap_or_default();
    match KNOWN.iter().find(|(s, t, _, _)| *s == scenario && *t == title).map(|k| &k.2) {
        None => e == a,
        Some(Known::WithoutKey(key)) => without_key(e, key) == without_key(a, key),
        Some(Known::Subset) => {
            let is_event = |l: &&String| l.starts_with("> ") && !l.starts_with("> W");
            let others = |s: &[String]| s.iter().filter(|l| !is_event(l)).cloned().collect::<Vec<_>>();
            let mut remaining: Vec<&String> = e.iter().filter(is_event).collect();
            let all_sent = a.iter().filter(is_event).all(|l| match remaining.iter().position(|r| *r == l) {
                Some(i) => {
                    remaining.remove(i);
                    true
                }
                None => false,
            });
            all_sent && others(e) == others(a)
        }
    }
}

/// The first steps that differ, with both sides' lines, for the failure message.
fn describe(scenario: &str, expected: &[String], actual: &[String]) -> Option<String> {
    let steps = |lines: &[String]| {
        let mut steps: Vec<Vec<String>> = vec![Vec::new()];
        for line in lines {
            if line.starts_with("$ ") {
                steps.push(Vec::new());
            }
            if let Some(step) = steps.last_mut() {
                step.push(line.clone());
            }
        }
        let mut report = None;
        steps.iter().map(|s| normalize(s, &mut report)).collect::<Vec<_>>()
    };
    let (e, a) = (steps(expected), steps(actual));
    let mut report = String::new();
    let mut shown = 0;
    for i in 0..e.len().max(a.len()) {
        let (es, as_) = (e.get(i), a.get(i));
        let same = match (es, as_) {
            (Some(x), Some(y)) => agree(scenario, x, y),
            (x, y) => x == y,
        };
        if !same {
            let title = es.or(as_).and_then(|s| s.first()).cloned().unwrap_or_default();
            report.push_str(&format!("\n--- step {i}: {title}\n  bspwm:\n"));
            for l in es.into_iter().flatten().skip(1) {
                report.push_str(&format!("    {l}\n"));
            }
            report.push_str("  bspwm-rs:\n");
            for l in as_.into_iter().flatten().skip(1) {
                report.push_str(&format!("    {l}\n"));
            }
            shown += 1;
            if shown == std::env::var("GOLDEN_SHOW").ok().and_then(|v| v.parse().ok()).unwrap_or(8) {
                report.push_str("\n(more differences follow)\n");
                break;
            }
        }
    }
    (shown > 0).then_some(report)
}

fn check(name: &str) {
    let (expected, actual) = run(name);
    if let Some(diff) = describe(name, &expected, &actual) {
        panic!("bspwm-rs differs from bspwm in scenario `{name}`:{diff}");
    }
}

#[test]
fn split_words_keeps_quoted_words_together() {
    assert_eq!(split_words(r#"bspc rule -a 'A B' "x y" z"#), ["bspc", "rule", "-a", "A B", "x y", "z"]);
}

#[test]
fn canon_names_ids_by_first_appearance_in_hex_and_json() {
    let lines = vec![
        "> node_add 0x00000005 0x00000007 0x00000000 0x00400001".to_owned(),
        r#"{"id":4194305,"name":"x","focusedDesktopId":7}"#.to_owned(),
    ];
    assert_eq!(
        Canon::transcript(&lines),
        vec!["> node_add #1 #2 0 #3".to_owned(), r##"{"focusedDesktopId":"#2","id":"#3","name":"x"}"##.to_owned()]
    );
}

#[test]
fn query() {
    check("query");
}

#[test]
fn tree() {
    check("tree");
}

#[test]
fn desktop() {
    check("desktop");
}

#[test]
fn config() {
    check("config");
}

#[test]
fn rules() {
    check("rules");
}
