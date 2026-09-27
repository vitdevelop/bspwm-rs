//! Focus history: which node, desktop and monitor were focused, and in what
//! order, for the `last`, `older`, `newer` and `newest` selectors and the
//! `history_rank` tie-break of directional focus.
//!
//! bspwm: `src/history.c`. The list keeps its exact semantics: entries run
//! from the oldest (head) to the newest (tail); an entry is `latest` unless
//! a newer entry names the same node (or, for a desktop-only entry, the same
//! desktop); a focus that lands on a monitor or desktop other than the
//! focused one is inserted next to its own desktop's entries instead of at
//! the tail.
//!
//! Differences from bspwm, on purpose:
//! - bspwm's C code calls `history_add`/`history_remove` from inside
//!   `focus_node()`, `activate_node()`, `unlink_node()` and friends. Here
//!   focus is written directly in several crates, so [`Wm::sync_history`]
//!   (`crate::wm`) observes the resulting state and calls [`History::add`] /
//!   [`History::remove_matching`] for what changed. It is called after every
//!   command and once per event-loop turn; two focus changes inside one call
//!   are recorded as one.
//! - Entries name nodes by their client's [`WindowId`], not a tree slot,
//!   because a slot is reused and changes when a node moves between trees.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::id::{DesktopId, MonitorId, WindowId};

/// Where an entry points: a monitor, a desktop on it, and optionally a node.
/// A `None` node is a desktop-only entry (an empty desktop that was focused).
///
/// bspwm: `src/types.h` `coordinates_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loc {
    /// The monitor.
    pub monitor: MonitorId,
    /// The desktop.
    pub desktop: DesktopId,
    /// The focused node's client, or `None` for a desktop-only entry.
    pub node: Option<WindowId>,
}

#[derive(Debug, Clone)]
struct Entry {
    loc: Loc,
    latest: bool,
    seq: u64,
}

/// The direction of `older`/`newer` (and `last`, which is `older`).
///
/// bspwm: `src/types.h` `history_dir_t`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    /// Towards the head: less recently focused.
    Older,
    /// Towards the tail: more recently focused.
    Newer,
}

/// `history_needle`: where an `older`/`newer` walk resumes while recording
/// is off. Atomic so the selector code, which only holds `&Wm`, can move it.
#[derive(Debug, Default)]
struct Needle(AtomicU64);

impl Clone for Needle {
    fn clone(&self) -> Self {
        Self(AtomicU64::new(self.0.load(Ordering::Relaxed)))
    }
}

impl Needle {
    fn get(&self) -> Option<u64> {
        match self.0.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    fn set(&self, seq: Option<u64>) {
        self.0.store(seq.unwrap_or(0), Ordering::Relaxed);
    }
}

/// What [`crate::wm::Wm::sync_history`] saw last time, to tell what changed.
#[derive(Debug, Clone, Default)]
pub(crate) struct Snapshot {
    /// Each desktop's focused window.
    pub(crate) desk_focus: HashMap<DesktopId, Option<WindowId>>,
    /// Each monitor's shown desktop at the last sync.
    pub(crate) shown: HashMap<crate::id::MonitorId, DesktopId>,
    /// Whether a sync has run: the first one only records where things start
    /// (bspwm records nothing for its initial desktop).
    pub(crate) primed: bool,
    /// The globally focused monitor, desktop and node.
    pub(crate) global: Option<Loc>,
}

/// The focus history.
///
/// bspwm: `src/bspwm.c` `history_head`/`history_tail`/`history_needle`,
/// `record_history`.
#[derive(Debug, Clone)]
pub struct History {
    entries: Vec<Entry>,
    next_seq: u64,
    needle: Needle,
    /// `bspc wm --record-history`: whether focus changes are added.
    pub record: bool,
    pub(crate) snapshot: Snapshot,
}

impl Default for History {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            next_seq: 1,
            needle: Needle::default(),
            record: true,
            snapshot: Snapshot::default(),
        }
    }
}

impl History {
    /// Every entry's location, oldest first (`wm -d`'s `focusHistory`).
    ///
    /// bspwm: `src/query.c` `query_history()`.
    pub fn locations(&self) -> impl Iterator<Item = Loc> + '_ {
        self.entries.iter().map(|e| e.loc)
    }

    /// Adds `loc`. `focused` is true when it is the input focus itself,
    /// false for a desktop's own focus changing while another has the input.
    ///
    /// bspwm: `src/history.c` `history_add()`.
    pub fn add(&mut self, loc: Loc, focused: bool) {
        if !self.record {
            return;
        }
        if focused {
            self.needle.set(None);
        }
        let entry = Entry { loc, latest: true, seq: self.next_seq };
        let Some(tail) = self.entries.last() else {
            self.next_seq += 1;
            self.entries.push(entry);
            return;
        };
        let already_newest = match loc.node {
            Some(n) => tail.loc.node == Some(n),
            None => loc.desktop == tail.loc.desktop,
        };
        if already_newest {
            return;
        }
        self.next_seq += 1;
        let mut insert_after = focused.then(|| self.entries.len() - 1);
        for i in (0..self.entries.len()).rev() {
            let hh = &mut self.entries[i];
            let same = match loc.node {
                Some(n) => hh.loc.node == Some(n),
                None => hh.loc.desktop == loc.desktop,
            };
            if same {
                hh.latest = false;
            }
            if insert_after.is_none() {
                let near = match loc.node {
                    Some(_) => hh.loc.desktop == loc.desktop,
                    None => hh.loc.monitor == loc.monitor,
                };
                if near {
                    insert_after = Some(i);
                }
            }
        }
        match insert_after {
            Some(i) => self.entries.insert(i + 1, entry),
            None => {
                let mut before = 0;
                if loc.node.is_some() {
                    if let Some(i) = self.entries.iter().position(|h| h.latest && h.loc.monitor == loc.monitor) {
                        before = i;
                    }
                }
                self.entries.insert(before, entry);
            }
        }
    }

    /// Removes every entry `pred` matches, newest first, and collapses the
    /// duplicates this leaves next to each other.
    ///
    /// bspwm: `src/history.c` `history_remove()`: the removal order keeps the
    /// `latest` attribute right, and after each removal the entries just
    /// before it that now repeat the one after it are dropped too.
    pub fn remove_matching(&mut self, pred: impl Fn(&Loc) -> bool) {
        let needle = self.needle.get();
        let mut needle_lost = false;
        let mut b = self.entries.len();
        while b > 0 {
            b -= 1;
            if !pred(&self.entries[b].loc) {
                continue;
            }
            if b + 1 < self.entries.len() {
                let a = self.entries[b + 1].loc;
                while b > 0 {
                    let c = self.entries[b - 1].loc;
                    let dup = match a.node {
                        Some(n) => c.node == Some(n),
                        None => c.node.is_none() && c.desktop == a.desktop,
                    };
                    if !dup {
                        break;
                    }
                    needle_lost |= needle == Some(self.entries[b - 1].seq);
                    self.entries.remove(b - 1);
                    b -= 1;
                }
            }
            needle_lost |= needle == Some(self.entries[b].seq);
            self.entries.remove(b);
        }
        if needle_lost {
            self.needle.set(None);
        }
    }

    /// The newest `latest` node entry on `desktop` whose window `usable`
    /// accepts (not hidden, not inside the node being excluded).
    ///
    /// bspwm: `src/history.c` `history_last_node()`.
    pub fn last_node(&self, desktop: DesktopId, usable: impl Fn(WindowId) -> bool) -> Option<WindowId> {
        self.entries
            .iter()
            .rev()
            .find(|h| h.latest && h.loc.desktop == desktop && h.loc.node.is_some_and(&usable))
            .and_then(|h| h.loc.node)
    }

    /// The newest `latest` entry on `monitor` naming a desktop other than
    /// `desktop`.
    ///
    /// bspwm: `src/history.c` `history_last_desktop()`.
    pub fn last_desktop(&self, monitor: MonitorId, desktop: DesktopId) -> Option<DesktopId> {
        self.entries
            .iter()
            .rev()
            .find(|h| h.latest && h.loc.desktop != desktop && h.loc.monitor == monitor)
            .map(|h| h.loc.desktop)
    }

    /// The newest `latest` entry naming a monitor other than `monitor`.
    ///
    /// bspwm: `src/history.c` `history_last_monitor()`.
    pub fn last_monitor(&self, monitor: MonitorId) -> Option<MonitorId> {
        self.entries.iter().rev().find(|h| h.latest && h.loc.monitor != monitor).map(|h| h.loc.monitor)
    }

    /// The newest entry `pred` accepts, `latest` or not (`newest`).
    ///
    /// bspwm: `src/history.c` `history_find_newest_*()`.
    pub fn find_newest(&self, pred: impl Fn(&Loc) -> bool) -> Option<Loc> {
        self.entries.iter().rev().find(|h| pred(&h.loc)).map(|h| h.loc)
    }

    /// Walks from the needle towards older or newer entries and returns the
    /// first `latest` one `pred` accepts (`last`, `older`, `newer`). While
    /// recording is off the needle moves to the match, so repeated calls
    /// step through the history; while it is on, every call starts at the
    /// newest entry.
    ///
    /// bspwm: `src/history.c` `history_find_node()`, `history_find_desktop()`,
    /// `history_find_monitor()` (they differ only in `pred`).
    pub fn find(&self, dir: Dir, pred: impl Fn(&Loc) -> bool) -> Option<Loc> {
        let start = match self.needle.get().and_then(|s| self.entries.iter().position(|e| e.seq == s)) {
            Some(i) if !self.record => Some(i),
            _ => self.entries.len().checked_sub(1),
        };
        if self.record || self.needle.get().is_none() {
            self.needle.set(self.entries.last().map(|e| e.seq));
        }
        let mut i = start?;
        loop {
            let h = &self.entries[i];
            if h.latest && pred(&h.loc) {
                if !self.record {
                    self.needle.set(Some(h.seq));
                }
                return Some(h.loc);
            }
            i = match dir {
                Dir::Older => i.checked_sub(1)?,
                Dir::Newer if i + 1 < self.entries.len() => i + 1,
                Dir::Newer => return None,
            };
        }
    }

    /// How many entries newer than `window`'s `latest` one there are;
    /// `u32::MAX` if it has none. Lower is more recently focused.
    ///
    /// bspwm: `src/history.c` `history_rank()`.
    #[must_use]
    pub fn rank(&self, window: WindowId) -> u32 {
        self.entries
            .iter()
            .rev()
            .position(|h| h.latest && h.loc.node == Some(window))
            .map_or(u32::MAX, |r| u32::try_from(r).unwrap_or(u32::MAX))
    }

    /// Forgets everything (`empty_history()`).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.needle.set(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(m: u32, d: u32, n: Option<u32>) -> Loc {
        Loc { monitor: MonitorId(m), desktop: DesktopId(d), node: n.map(WindowId) }
    }

    fn nodes(h: &History) -> Vec<Option<u32>> {
        h.locations().map(|l| l.node.map(|w| w.0)).collect()
    }

    #[test]
    fn a_focus_is_appended_and_the_same_node_twice_is_ignored() {
        let mut h = History::default();
        h.add(loc(1, 1, Some(10)), true);
        h.add(loc(1, 1, Some(10)), true);
        h.add(loc(1, 1, Some(11)), true);
        assert_eq!(nodes(&h), [Some(10), Some(11)]);
    }

    #[test]
    fn refocusing_an_older_node_keeps_the_old_entry_but_marks_it_not_latest() {
        let mut h = History::default();
        for n in [10, 11, 10] {
            h.add(loc(1, 1, Some(n)), true);
        }
        assert_eq!(nodes(&h), [Some(10), Some(11), Some(10)]);
        // `last` on this desktop skips the newest (10), whose earlier entry is
        // no longer latest, and finds 11.
        let found = h.find(Dir::Older, |l| l.node != Some(WindowId(10)));
        assert_eq!(found, Some(loc(1, 1, Some(11))));
        assert_eq!(h.rank(WindowId(10)), 0);
        assert_eq!(h.rank(WindowId(11)), 1);
        assert_eq!(h.rank(WindowId(99)), u32::MAX);
    }

    #[test]
    fn a_non_focused_add_lands_next_to_its_own_desktop() {
        let mut h = History::default();
        h.add(loc(1, 1, Some(10)), true);
        h.add(loc(1, 2, Some(20)), true);
        // desktop 1's focus changes while desktop 2 has the input
        h.add(loc(1, 1, Some(11)), false);
        assert_eq!(nodes(&h), [Some(10), Some(11), Some(20)]);
    }

    #[test]
    fn last_desktop_and_monitor_skip_the_current_one() {
        let mut h = History::default();
        h.add(loc(1, 1, Some(10)), true);
        h.add(loc(1, 2, Some(20)), true);
        h.add(loc(2, 3, Some(30)), true);
        assert_eq!(h.last_desktop(MonitorId(1), DesktopId(2)), Some(DesktopId(1)));
        assert_eq!(h.last_monitor(MonitorId(2)), Some(MonitorId(1)));
        assert_eq!(h.last_node(DesktopId(2), |_| true), Some(WindowId(20)));
        assert_eq!(h.last_node(DesktopId(2), |w| w != WindowId(20)), None);
    }

    #[test]
    fn removing_a_node_collapses_the_duplicates_it_leaves_behind() {
        let mut h = History::default();
        for n in [10, 11, 10] {
            h.add(loc(1, 1, Some(n)), true);
        }
        // 10, 11, 10 -> remove 11 -> the two 10s are now adjacent: one goes
        h.remove_matching(|l| l.node == Some(WindowId(11)));
        assert_eq!(nodes(&h), [Some(10)]);
    }

    #[test]
    fn older_and_newer_step_through_the_history_while_recording_is_off() {
        let mut h = History::default();
        for n in [10, 11, 12] {
            h.add(loc(1, 1, Some(n)), true);
        }
        h.record = false;
        let cur = WindowId(12);
        let older = |h: &History| h.find(Dir::Older, |l| l.node != Some(cur)).and_then(|l| l.node);
        assert_eq!(older(&h), Some(WindowId(11)));
        // the needle is now on 11; the next `older` (with 11 as reference) goes on to 10
        let step = h.find(Dir::Older, |l| l.node != Some(WindowId(11))).and_then(|l| l.node);
        assert_eq!(step, Some(WindowId(10)));
        let back = h.find(Dir::Newer, |l| l.node != Some(WindowId(10))).and_then(|l| l.node);
        assert_eq!(back, Some(WindowId(11)));
    }

    #[test]
    fn nothing_is_recorded_while_recording_is_off() {
        let mut h = History { record: false, ..History::default() };
        h.add(loc(1, 1, Some(10)), true);
        assert_eq!(nodes(&h), Vec::<Option<u32>>::new());
    }

    #[test]
    fn desktop_only_entries_dedupe_on_the_desktop() {
        let mut h = History::default();
        h.add(loc(1, 1, None), false);
        h.add(loc(1, 1, None), true);
        h.add(loc(1, 2, None), true);
        assert_eq!(nodes(&h), [None, None]);
    }
}
