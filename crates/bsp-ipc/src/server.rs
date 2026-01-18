//! Non-blocking Unix socket listener and connections, driven by the
//! compositor's event loop.
//!
//! bspwm: `src/bspwm.c` `main()`'s `select`/`accept`/`recv` loop and
//! `src/subscribe.c` (`add_subscriber()`, `put_status()`,
//! `prune_dead_subscribers()`). One `recv()` is treated as one whole
//! request (bspwm never loops reading more for a single command,
//! `src/bspwm.c`: `recv(cli_fd, msg, sizeof(msg)-1, 0)`), and every
//! command but `subscribe` gets exactly one reply before the connection
//! closes.
//!
//! This module provides the raw building blocks — [`Listener`],
//! [`Connection`], [`Subscribers`] — as plain, non-blocking, `poll`-driven
//! types with no event loop of their own: `bsp-ipc` stays free of
//! `calloop` (`docs/design.md`, Architecture), so wiring a `Listener`'s
//! and every open `Connection`'s file descriptor into the compositor's
//! actual event loop is `bsp-compositor`'s job, which does not
//! exist yet. What is here is fully usable and tested against real Unix
//! sockets today.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::command::SubscriberMask;
use crate::report::EventKind;
use crate::wire::{self, Reply};

/// The control socket, bound and listening in non-blocking mode.
pub struct Listener {
    inner: UnixListener,
    path: PathBuf,
}

impl Listener {
    /// Binds and listens on `path`, removing a stale socket file left
    /// behind by a previous run first, and setting the socket's mode to
    /// `0600` (`docs/design.md`, Session/security: "created in
    /// `XDG_RUNTIME_DIR` with mode 0600, in a directory only you can
    /// read" — the directory permission is the caller's responsibility,
    /// typically already `0700` for `XDG_RUNTIME_DIR`).
    pub fn bind(path: &Path) -> io::Result<Self> {
        if path.exists() {
            fs::remove_file(path)?;
        }
        let inner = UnixListener::bind(path)?;
        inner.set_nonblocking(true)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            inner,
            path: path.to_path_buf(),
        })
    }

    /// Accepts a pending connection, if any. Returns `Ok(None)` rather
    /// than blocking when there is nothing to accept yet.
    pub fn accept(&self) -> io::Result<Option<Connection>> {
        match self.inner.accept() {
            Ok((stream, _addr)) => {
                stream.set_nonblocking(true)?;
                Ok(Some(Connection::new(stream)))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl AsRawFd for Listener {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// One accepted client connection.
pub struct Connection {
    stream: UnixStream,
}

impl Connection {
    fn new(stream: UnixStream) -> Self {
        Self { stream }
    }

    /// Attempts one non-blocking read of a whole request. `Ok(None)`
    /// means no data is available yet (try again once the fd is
    /// readable); `Err` with `UnexpectedEof` means the peer closed the
    /// connection.
    ///
    /// bspwm: `src/bspwm.c` `main()`'s `recv(cli_fd, msg, sizeof(msg)-1, 0)`.
    pub fn try_read_request(&mut self) -> io::Result<Option<Vec<String>>> {
        let mut buf = [0u8; 8192];
        match self.stream.read(&mut buf) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client closed the connection",
            )),
            Ok(n) => Ok(Some(wire::decode_request(&buf[..n]))),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Sends a reply and, for anything but `subscribe`, the caller then
    /// drops this `Connection` to close it (bspwm: `process_message()`
    /// `fflush`/`fclose`s the response stream for every domain but
    /// `subscribe`, which `return`s early to keep it open).
    pub fn send_reply(&mut self, reply: Reply) -> io::Result<()> {
        self.stream.write_all(&reply.into_bytes())
    }

    /// Writes one already-newline-terminated line to a `subscribe`d
    /// connection (a report line or an event). Returns `false` — rather
    /// than an `io::Error` — on any write failure, since the only
    /// sensible response to a dead subscriber is to drop it (bspwm:
    /// `src/subscribe.c` `put_status()`/`prune_dead_subscribers()`).
    pub fn send_line(&mut self, line: &str) -> bool {
        self.stream.write_all(line.as_bytes()).is_ok()
    }
}

impl AsRawFd for Connection {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}

/// A `subscribe`d connection's state.
///
/// bspwm: `src/types.h` `subscriber_list_t` (`field`/`count`; `fifo_path`
/// is not implemented yet — `docs/bsp-ipc.md`, scope).
pub struct Subscriber {
    connection: Connection,
    masks: Vec<SubscriberMask>,
    /// Remaining event deliveries before this subscriber is dropped, or
    /// `None` for unlimited (`-c`/`--count`).
    remaining: Option<u32>,
}

impl Subscriber {
    /// Whether this subscriber's masks cover `kind`.
    ///
    /// bspwm: `src/subscribe.h` `subscriber_mask_t` (`SBSC_MASK_MONITOR`/
    /// `SBSC_MASK_DESKTOP`/`SBSC_MASK_NODE` are contiguous bit ranges over
    /// exactly the category's events; `SBSC_MASK_ALL` additionally covers
    /// `pointer_action`, which no single-word category does).
    pub fn wants(&self, kind: EventKind) -> bool {
        use EventKind::*;
        self.masks.iter().any(|m| match m {
            SubscriberMask::All => true,
            SubscriberMask::Report => false,
            SubscriberMask::Monitor => matches!(
                kind,
                MonitorAdd
                    | MonitorRename
                    | MonitorRemove
                    | MonitorSwap
                    | MonitorFocus
                    | MonitorGeometry
            ),
            SubscriberMask::Desktop => matches!(
                kind,
                DesktopAdd
                    | DesktopRename
                    | DesktopRemove
                    | DesktopSwap
                    | DesktopTransfer
                    | DesktopFocus
                    | DesktopActivate
                    | DesktopLayout
            ),
            SubscriberMask::Node => matches!(
                kind,
                NodeAdd
                    | NodeRemove
                    | NodeSwap
                    | NodeTransfer
                    | NodeFocus
                    | NodePresel
                    | NodeStack
                    | NodeActivate
                    | NodeGeometry
                    | NodeState
                    | NodeFlag
                    | NodeLayer
            ),
            SubscriberMask::Event(k) => *k == kind,
        })
    }

    /// Whether this subscriber wants `report` lines.
    pub fn wants_report(&self) -> bool {
        self.masks
            .iter()
            .any(|m| matches!(m, SubscriberMask::All | SubscriberMask::Report))
            || self.masks.is_empty()
    }

    /// Delivers `line` (already newline-terminated) if `wants`/`wants_report`
    /// said yes, decrementing and expiring this subscriber's `--count` as
    /// bspwm's `put_status()` does. Returns `false` if the subscriber
    /// should now be dropped (write failed, or its count ran out).
    fn deliver(&mut self, line: &str) -> bool {
        if !self.connection.send_line(line) {
            return false;
        }
        if let Some(r) = &mut self.remaining {
            *r -= 1;
            if *r == 0 {
                return false;
            }
        }
        true
    }
}

/// The set of open `subscribe` connections, and delivery to them.
///
/// bspwm: `src/bspwm.h` `subscribe_head`/`subscribe_tail`.
#[derive(Default)]
pub struct Subscribers {
    next_id: u64,
    by_id: HashMap<u64, Subscriber>,
}

impl Subscribers {
    /// An empty subscriber set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a new subscriber, immediately sending the current report
    /// line if it wants `report` (bspwm: `src/subscribe.c`
    /// `add_subscriber()` prints one report line right away). `masks`
    /// empty means "report only" (bspwm: `cmd_subscribe()`, `field == 0`
    /// defaults to `SBSC_MASK_REPORT`).
    pub fn add(
        &mut self,
        connection: Connection,
        masks: Vec<SubscriberMask>,
        count: Option<u32>,
        initial_report: &str,
    ) -> u64 {
        let mut sub = Subscriber {
            connection,
            masks,
            remaining: count,
        };
        let id = self.next_id;
        self.next_id += 1;
        if sub.wants_report() && !sub.deliver(initial_report) {
            return id; // dropped immediately; caller sees it absent from `by_id`
        }
        self.by_id.insert(id, sub);
        id
    }

    /// Delivers `event` to every subscriber whose mask covers it,
    /// dropping any that fail or run out of `--count`.
    pub fn broadcast_event(&mut self, event: &crate::report::Event) {
        let line = format!("{event}\n");
        self.by_id
            .retain(|_, sub| !sub.wants(event.kind()) || sub.deliver(&line));
    }

    /// Delivers a fresh report line to every `report`-subscribed
    /// connection, dropping any that fail.
    ///
    /// bspwm calls `put_status(SBSC_MASK_REPORT)` after most state
    /// changes; the executor instead reports back whether anything
    /// changed and leaves the call site to the caller (`docs/bsp-ipc.md`,
    /// scope: exact per-call-site triggers were not replicated
    /// one by one).
    pub fn broadcast_report(&mut self, report: &crate::report::Report) {
        let line = report.to_string();
        self.by_id
            .retain(|_, sub| !sub.wants_report() || sub.deliver(&line));
    }

    /// Number of open subscribers.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether there are no open subscribers.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Event;
    use std::os::unix::net::UnixStream as ClientStream;

    fn temp_socket_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("bsp-ipc-test-{name}-{}.sock", std::process::id()));
        p
    }

    #[test]
    fn listener_binds_with_owner_only_permissions() {
        let path = temp_socket_path("perms");
        let listener = Listener::bind(&path).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        drop(listener);
        assert!(!path.exists());
    }

    #[test]
    fn accept_returns_none_with_no_pending_connection() {
        let path = temp_socket_path("no-pending");
        let listener = Listener::bind(&path).unwrap();
        assert!(listener.accept().unwrap().is_none());
    }

    #[test]
    fn accept_then_read_request_and_send_reply_round_trips() {
        let path = temp_socket_path("roundtrip");
        let listener = Listener::bind(&path).unwrap();
        let mut client = ClientStream::connect(&path).unwrap();
        client.write_all(b"node\0-f\0west\0").unwrap();

        // Give the kernel a moment to deliver; a non-blocking accept/read
        // loop like the real event loop would use.
        let mut conn = None;
        for _ in 0..1000 {
            if let Some(c) = listener.accept().unwrap() {
                conn = Some(c);
                break;
            }
        }
        let mut conn = conn.expect("connection was not accepted");

        let mut args = None;
        for _ in 0..1000 {
            if let Some(a) = conn.try_read_request().unwrap() {
                args = Some(a);
                break;
            }
        }
        assert_eq!(args.unwrap(), vec!["node", "-f", "west"]);

        conn.send_reply(Reply::Ok("done\n".to_string())).unwrap();
        drop(conn);

        let mut buf = Vec::new();
        client.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"done\n");
    }

    fn connected_pair(name: &str) -> (Connection, ClientStream) {
        let path = temp_socket_path(name);
        let listener = Listener::bind(&path).unwrap();
        let client = ClientStream::connect(&path).unwrap();
        let mut conn = None;
        for _ in 0..1000 {
            if let Some(c) = listener.accept().unwrap() {
                conn = Some(c);
                break;
            }
        }
        (conn.unwrap(), client)
    }

    #[test]
    fn subscriber_with_empty_masks_gets_report_only() {
        let (conn, mut client) = connected_pair("report-only");
        let mut subs = Subscribers::new();
        subs.add(conn, vec![], None, "Wreport1\n");
        assert_eq!(subs.len(), 1);

        subs.broadcast_event(&Event::NodeAdd {
            monitor: 1,
            desktop: 1,
            ip_id: 0,
            node: 1,
        });
        subs.broadcast_report(&crate::report::Report {
            prefix: String::new(),
            monitors: vec![],
        });

        let mut buf = [0u8; 256];
        let n = client.read(&mut buf).unwrap();
        // Only the initial report and the broadcast_report call reached
        // the client — the node_add event did not, since an empty mask
        // list means report-only.
        assert_eq!(&buf[..n], b"Wreport1\n\n");
    }

    #[test]
    fn subscriber_with_node_mask_gets_node_events_not_desktop() {
        let (conn, mut client) = connected_pair("node-mask");
        let mut subs = Subscribers::new();
        subs.add(conn, vec![SubscriberMask::Node], None, "");

        subs.broadcast_event(&Event::DesktopFocus {
            monitor: 1,
            desktop: 1,
        });
        subs.broadcast_event(&Event::NodeFocus {
            monitor: 1,
            desktop: 1,
            node: 1,
        });

        let mut buf = [0u8; 256];
        let n = client.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"node_focus 0x00000001 0x00000001 0x00000001\n");
    }

    #[test]
    fn subscriber_count_expires_after_n_deliveries() {
        let (conn, mut client) = connected_pair("count");
        let mut subs = Subscribers::new();
        // A mask that does not include `report` (`Event(MonitorFocus)`,
        // not `All`): the initial subscribe-time report bspwm always
        // sends to a `report`-covering subscriber (`src/subscribe.c`
        // `add_subscriber()`) would otherwise consume this `--count 1`
        // budget by itself before the event under test ever fires — which
        // is exactly what `wants_report_subscriber_counts_the_initial_report`
        // below checks.
        subs.add(
            conn,
            vec![SubscriberMask::Event(EventKind::MonitorFocus)],
            Some(1),
            "",
        );

        subs.broadcast_event(&Event::MonitorFocus { id: 1 });
        assert_eq!(subs.len(), 0);

        let mut buf = [0u8; 256];
        let n = client.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"monitor_focus 0x00000001\n");
    }

    #[test]
    fn wants_report_subscriber_counts_the_initial_report() {
        // bspwm: `src/subscribe.c` `add_subscriber()` sends the initial
        // report unconditionally for a `report`-covering subscriber and
        // counts it against `--count`; with `count == 1` that alone
        // exhausts the budget, so the subscriber never sees a later event.
        let (conn, mut client) = connected_pair("count-report");
        let mut subs = Subscribers::new();
        subs.add(conn, vec![SubscriberMask::All], Some(1), "Winitial\n");
        assert_eq!(subs.len(), 0);

        let mut buf = [0u8; 256];
        let n = client.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"Winitial\n");
    }

    #[test]
    fn subscriber_dropped_when_client_disconnects() {
        let (conn, client) = connected_pair("disconnect");
        drop(client);
        let mut subs = Subscribers::new();
        subs.add(conn, vec![SubscriberMask::All], None, "");
        subs.broadcast_event(&Event::MonitorFocus { id: 1 });
        assert_eq!(subs.len(), 0);
    }
}
