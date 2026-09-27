//! Records what the real bspwm does for a golden-test scenario
//! (`crates/bsp-ipc/tests/golden/*.scn`), for `crates/bsp-ipc/tests/golden.rs`
//! to compare bspwm-rs with. Run it against bspwm on an X server (Xvfb in the
//! QEMU test VM, `contrib/golden/record.sh`):
//!
//! ```text
//! DISPLAY=:1 golden_record SCENARIO.scn > SCENARIO.out
//! ```
//!
//! A scenario is one step per line (`#` starts a comment):
//!
//! ```text
//! bspc ARGS...                      runs the real `bspc` ($BSPC, default `bspc`)
//! window CLASS INSTANCE [WxH+X+Y]   maps a window (its own X client) and waits until bspwm manages it
//! close N                           the N-th window (1-based) destroys itself
//! ```
//!
//! The transcript starts with `@monitor NAME WxH+X+Y DESKTOP...` (the initial
//! state), then each step as `$ STEP` followed by `bspc`'s output lines, its
//! error lines as `! LINE`, a non-zero exit as `? exit N`, and what
//! `bspc subscribe all` printed meanwhile as `> LINE`.

use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, WindowClass};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// How long the subscriber must stay quiet before a step counts as finished.
const QUIET: Duration = Duration::from_millis(150);
/// How long to wait for bspwm to manage or unmanage a window.
const SETTLE: Duration = Duration::from_secs(3);

/// Splits a scenario line into words, keeping `'...'`/`"..."` together (the
/// same rules as `golden.rs`'s `split_words`).
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

/// One scenario window: its own X connection, so `bspc node -k`
/// (`XKillClient`) kills only it.
struct Probe {
    conn: RustConnection,
    window: u32,
    wm_protocols: u32,
    wm_delete: u32,
    alive: bool,
}

impl Probe {
    fn map(class: &str, instance: &str, (w, h, x, y): (u16, u16, i16, i16)) -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen_num) = RustConnection::connect(None)?;
        let screen = conn.setup().roots[screen_num].clone();
        let window = conn.generate_id()?;
        conn.create_window(
            screen.root_depth,
            window,
            screen.root,
            x,
            y,
            w,
            h,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &CreateWindowAux::new().background_pixel(screen.white_pixel).event_mask(EventMask::STRUCTURE_NOTIFY),
        )?;
        let wm_protocols = conn.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
        let wm_delete = conn.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;
        conn.change_property32(PropMode::REPLACE, window, wm_protocols, AtomEnum::ATOM, &[wm_delete])?;
        let wm_class = format!("{instance}\0{class}\0");
        conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_CLASS, AtomEnum::STRING, wm_class.as_bytes())?;
        conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_NAME, AtomEnum::STRING, class.as_bytes())?;
        conn.map_window(window)?;
        conn.flush()?;
        Ok(Self { conn, window, wm_protocols, wm_delete, alive: true })
    }

    /// Answers `WM_DELETE_WINDOW` (`bspc node -c`) by destroying the window.
    fn poll(&mut self) {
        if !self.alive {
            return;
        }
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(Event::ClientMessage(e))) if e.type_ == self.wm_protocols && e.data.as_data32()[0] == self.wm_delete => {
                    self.destroy();
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                // Killed (`XKillClient`).
                Err(_) => {
                    self.alive = false;
                    break;
                }
            }
        }
    }

    fn destroy(&mut self) {
        if self.alive {
            let _ = self.conn.destroy_window(self.window);
            let _ = self.conn.flush();
            self.alive = false;
        }
    }
}

/// `WxH+X+Y`.
fn parse_geometry(s: &str) -> Option<(u16, u16, i16, i16)> {
    let (size, pos) = s.split_once('+')?;
    let (w, h) = size.split_once('x')?;
    let (x, y) = pos.split_once('+')?;
    Some((w.parse().ok()?, h.parse().ok()?, x.parse().ok()?, y.parse().ok()?))
}

struct Recorder {
    bspc: String,
    events: Receiver<String>,
    probes: Vec<Probe>,
}

impl Recorder {
    /// Prints subscriber lines until none came for [`QUIET`] (and, with
    /// `until`, one containing it came, or [`SETTLE`] passed), answering the
    /// windows' client messages meanwhile.
    fn drain(&mut self, until: Option<&str>) {
        let start = Instant::now();
        let mut waiting = until.map(str::to_owned);
        let mut last = Instant::now();
        loop {
            for probe in &mut self.probes {
                probe.poll();
            }
            match self.events.recv_timeout(Duration::from_millis(10)) {
                Ok(line) => {
                    if waiting.as_deref().is_some_and(|w| line.contains(w)) {
                        waiting = None;
                    }
                    println!("> {line}");
                    last = Instant::now();
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            let settled = waiting.is_none() || start.elapsed() > SETTLE;
            if settled && last.elapsed() > QUIET {
                break;
            }
        }
    }

    fn bspc(&mut self, args: &[String]) {
        let output = Command::new(&self.bspc).args(args).stdin(Stdio::null()).output();
        match output {
            Ok(output) => {
                for line in String::from_utf8_lossy(&output.stdout).lines() {
                    println!("{line}");
                }
                for line in String::from_utf8_lossy(&output.stderr).lines() {
                    println!("! {line}");
                }
                if let Some(code) = output.status.code().filter(|c| *c != 0) {
                    println!("? exit {code}");
                }
            }
            Err(err) => println!("! cannot run {}: {err}", self.bspc),
        }
        self.drain(None);
    }

    fn step(&mut self, words: &[String]) {
        match words.first().map(String::as_str) {
            Some("bspc") => self.bspc(&words[1..]),
            Some("window") => {
                let class = words.get(1).cloned().unwrap_or_default();
                let instance = words.get(2).cloned().unwrap_or_else(|| class.to_lowercase());
                let geometry = words.get(3).and_then(|g| parse_geometry(g)).unwrap_or((400, 300, 10, 10));
                match Probe::map(&class, &instance, geometry) {
                    Ok(probe) => {
                        let id = format!("0x{:08X}", probe.window);
                        self.probes.push(probe);
                        self.drain(Some(&id));
                    }
                    Err(err) => println!("! cannot map a window: {err}"),
                }
            }
            Some("close") => {
                let index = words.get(1).and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
                match self.probes.get_mut(index.wrapping_sub(1)) {
                    Some(probe) => {
                        let id = format!("0x{:08X}", probe.window);
                        probe.destroy();
                        self.drain(Some(&id));
                    }
                    None => println!("! no window {index}"),
                }
            }
            _ => println!("! unknown step"),
        }
    }
}

/// `@monitor NAME WxH+X+Y DESKTOP...` from `bspc query -T -m`.
fn header(bspc: &str) -> String {
    let out = Command::new(bspc).args(["query", "-T", "-m"]).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap_or_default();
    let r = &value["rectangle"];
    let desktops: Vec<&str> =
        value["desktops"].as_array().map(|a| a.iter().filter_map(|d: &serde_json::Value| d["name"].as_str()).collect()).unwrap_or_default();
    format!(
        "@monitor {} {}x{}+{}+{} {}",
        value["name"].as_str().unwrap_or("?"),
        r["width"],
        r["height"],
        r["x"],
        r["y"],
        desktops.join(" ")
    )
}

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: golden_record SCENARIO.scn");
        std::process::exit(2);
    };
    let scenario = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("cannot read {path}: {err}");
            std::process::exit(2);
        }
    };
    let bspc = std::env::var("BSPC").unwrap_or_else(|_| "bspc".to_owned());
    println!("{}", header(&bspc));

    let mut subscriber = match Command::new(&bspc).args(["subscribe", "all"]).stdout(Stdio::piped()).spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("cannot run {bspc} subscribe: {err}");
            std::process::exit(2);
        }
    };
    let (tx, events) = channel();
    let stdout = subscriber.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>);
    if let Some(stdout) = stdout {
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
    }
    let mut recorder = Recorder { bspc, events, probes: Vec::new() };
    // The first report line `subscribe` prints is the state before the scenario.
    recorder.drain(None);

    for line in scenario.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        println!("$ {line}");
        recorder.step(&split_words(line));
    }
    let _ = subscriber.kill();
    let _ = subscriber.wait();
}
