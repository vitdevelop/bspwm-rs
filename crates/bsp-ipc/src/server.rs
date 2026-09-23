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
//! actual event loop is `bsp-compositor`'s job (nested compositor), which does not
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
    /// The socket file's `(device, inode)` once bound: only that file is
    /// removed on drop, never one another instance bound at the same path.
    file_id: Option<(u64, u64)>,
}

impl Listener {
    /// Binds and listens on `path`, removing a stale socket file left
    /// behind by a previous run first, and setting the socket's mode to
    /// `0600` (`docs/design.md`, Session/security: "created in
    /// `XDG_RUNTIME_DIR` with mode 0600, in a directory only you can
    /// read" — the directory permission is the caller's responsibility,
    /// typically already `0700` for `XDG_RUNTIME_DIR`).
    ///
    /// Fails with [`io::ErrorKind::AddrInUse`] when a running server (another
    /// instance on another VT) answers at `path`: its socket is left alone.
    pub fn bind(path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        if path.exists() {
            if UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(io::ErrorKind::AddrInUse, "another instance is listening there"));
            }
            fs::remove_file(path)?;
        }
        let inner = UnixListener::bind(path)?;
        inner.set_nonblocking(true)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        let file_id = fs::metadata(path).ok().map(|m| (m.dev(), m.ino()));
        Ok(Self {
            inner,
            path: path.to_path_buf(),
            file_id,
        })
    }

    /// The path this listener is bound at.
    pub fn path(&self) -> &Path {
        &self.path
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
        use std::os::unix::fs::MetadataExt;
        // Only our own socket file: a second instance may have bound the path
        // since (it does not while we answer, but a stale file may be replaced).
        let ours = fs::metadata(&self.path).ok().map(|m| (m.dev(), m.ino()));
        if ours.is_some() && ours == self.file_id {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// One accepted client connection.
pub struct Connection {
    stream: UnixStream,
    /// A reply the socket did not take yet (see [`Connection::send_reply`]).
    queued: Vec<u8>,
}

impl Connection {
    fn new(stream: UnixStream) -> Self {
        Self { stream, queued: Vec::new() }
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
    ///
    /// The reply is queued and as much as the socket takes is written at once;
    /// the rest stays queued and [`Connection::flush`] writes it later, so a
    /// reply larger than the socket buffer (a `wm -d` dump, a big `query -T`)
    /// never makes the compositor wait for a slow reader. The connection is
    /// closed after the last byte, which is the caller's job (bspwm writes
    /// through a blocking `FILE *`; waiting is not an option here).
    pub fn send_reply(&mut self, reply: Reply) -> io::Result<()> {
        self.queued.extend_from_slice(&reply.into_bytes());
        self.flush().map(|_| ())
    }

    /// Writes as much of the queued reply as the socket takes now. `Ok(true)` when
    /// nothing is left to write.
    pub fn flush(&mut self) -> io::Result<bool> {
        while !self.queued.is_empty() {
            match self.stream.write(&self.queued) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "peer stopped reading")),
                Ok(n) => {
                    self.queued.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    /// Whether part of a reply is still waiting for the socket.
    pub fn has_queued(&self) -> bool {
        !self.queued.is_empty()
    }

    /// One non-blocking write of as much of `buf` as the socket takes now.
    fn write_some(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }
}

/// How long a reply may wait for a slow reader before the connection is dropped.
pub const REPLY_WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// How much unread output a subscriber may have queued before it is dropped
/// (a bar script that stopped reading must not grow the compositor forever).
pub const MAX_QUEUED_BYTES: usize = 1 << 20;

impl AsRawFd for Connection {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}

/// A `subscribe`d connection's state.
///
/// bspwm: `src/types.h` `subscriber_list_t` (`field`/`count`; `fifo_path`
/// is not implemented yet — `docs/bsp-ipc.md`, IPC scope).
pub struct Subscriber {
    connection: Connection,
    masks: Vec<SubscriberMask>,
    /// Remaining event deliveries before this subscriber is dropped, or
    /// `None` for unlimited (`-c`/`--count`).
    remaining: Option<u32>,
    /// Lines the socket did not take yet, oldest first. A slow reader falls
    /// behind here instead of being dropped; the queue is written out
    /// whenever the next line is delivered.
    queued: Vec<u8>,
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

    /// Writes as much of the queue as the socket takes without blocking.
    /// `false` if the connection is dead.
    fn flush(&mut self) -> bool {
        while !self.queued.is_empty() {
            match self.connection.write_some(&self.queued) {
                Ok(0) => return false,
                Ok(n) => {
                    self.queued.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        true
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
        self.queued.extend_from_slice(line.as_bytes());
        if !self.flush() || self.queued.len() > MAX_QUEUED_BYTES {
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
            queued: Vec::new(),
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
    /// IPC scope: exact per-call-site triggers were not replicated
    /// one by one).
    pub fn broadcast_report(&mut self, report: &crate::report::Report) {
        let line = report.to_string();
        self.by_id
            .retain(|_, sub| !sub.wants_report() || sub.deliver(&line));
    }

    /// Writes every subscriber's queued lines as far as its socket takes them
    /// and drops the ones whose connection is dead. Call it now and then (the
    /// compositor does each event-loop turn while [`Subscribers::has_queued`]),
    /// so lines held back by a full socket buffer are delivered once the reader
    /// catches up, not only when the next event happens to arrive.
    pub fn flush_all(&mut self) {
        self.by_id.retain(|_, sub| sub.flush());
    }

    /// Whether any subscriber still has lines waiting for its socket.
    pub fn has_queued(&self) -> bool {
        self.by_id.values().any(|sub| !sub.queued.is_empty())
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
    fn a_second_listener_does_not_take_or_delete_a_live_socket() {
        let path = temp_socket_path("second");
        let first = Listener::bind(&path).unwrap();
        let err = Listener::bind(&path).err().unwrap();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert!(UnixStream::connect(&path).is_ok(), "the first socket still answers");
        drop(first);
        assert!(!path.exists());
        // A stale file (nobody listening) is replaced.
        std::fs::write(&path, b"").unwrap();
        let again = Listener::bind(&path).unwrap();
        assert!(UnixStream::connect(&path).is_ok());
        drop(again);
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

    #[test]
    fn a_slow_subscriber_is_kept_and_gets_every_line_in_order() {
        // The socket buffer fills long before 20000 lines are out; the old
        // `write_all` on a non-blocking socket failed there and dropped the
        // subscriber.
        let (server, mut client) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        let mut subs = Subscribers::new();
        subs.add(Connection::new(server), vec![SubscriberMask::All], None, "W\n");
        let lines: Vec<String> = (0..20_000).map(|i| format!("node_focus 0x{i:08X} x\n")).collect();
        for (sent, line) in lines.iter().enumerate() {
            subs.by_id.values_mut().for_each(|s| {
                assert!(s.deliver(line), "dropped after {sent} lines");
            });
        }
        assert_eq!(subs.len(), 1);
        // Now the reader catches up; a few more deliveries flush the queue.
        client.set_nonblocking(true).unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 65536];
        for _ in 0..10_000 {
            match client.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    subs.by_id.values_mut().for_each(|s| {
                        s.flush();
                    });
                    if subs.by_id.values().all(|s| s.queued.is_empty()) {
                        break;
                    }
                }
                Err(e) => panic!("{e}"),
            }
        }
        while let Ok(n) = client.read(&mut buf) {
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        let expected: String = std::iter::once("W\n".to_string()).chain(lines).collect();
        assert_eq!(String::from_utf8(got).unwrap(), expected);
    }

    #[test]
    fn a_subscriber_that_never_reads_is_dropped_past_the_queue_limit() {
        let (server, _client) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        let mut subs = Subscribers::new();
        subs.add(Connection::new(server), vec![SubscriberMask::All], None, "W\n");
        let line = "x".repeat(1000) + "\n";
        let mut alive = true;
        for _ in 0..3000 {
            alive = subs.by_id.values_mut().all(|s| s.deliver(&line));
            if !alive {
                break;
            }
        }
        assert!(!alive, "more than {MAX_QUEUED_BYTES} unread bytes must end the subscription");
    }

    #[test]
    fn queued_lines_are_delivered_by_flush_all_without_another_event() {
        let (server, mut client) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        let mut subs = Subscribers::new();
        subs.add(Connection::new(server), vec![SubscriberMask::All], None, "W\n");
        let line = "y".repeat(999) + "\n";
        // Fill the socket buffer and beyond; the reader has not read a byte.
        for _ in 0..600 {
            subs.by_id.values_mut().for_each(|s| {
                s.deliver(&line);
            });
        }
        assert!(subs.has_queued(), "the socket buffer cannot hold 600 kB");
        client.set_nonblocking(true).unwrap();
        let mut got = 0usize;
        let mut buf = [0u8; 65536];
        for _ in 0..1000 {
            match client.read(&mut buf) {
                Ok(n) => got += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("{e}"),
            }
            subs.flush_all();
            if !subs.has_queued() {
                break;
            }
        }
        while let Ok(n) = client.read(&mut buf) {
            if n == 0 {
                break;
            }
            got += n;
        }
        assert!(!subs.has_queued());
        assert_eq!(subs.len(), 1);
        assert_eq!(got, 2 + 600 * 1000);
    }

    #[test]
    fn a_reply_larger_than_the_socket_buffer_is_sent_in_pieces_and_never_blocks() {
        let (server, mut client) = UnixStream::pair().unwrap();
        server.set_nonblocking(true).unwrap();
        let mut conn = Connection::new(server);
        let big = "z".repeat(1 << 20);
        conn.send_reply(Reply::Ok(big.clone())).unwrap();
        assert!(conn.has_queued(), "1 MiB does not fit the socket buffer");
        client.set_nonblocking(true).unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 65536];
        for _ in 0..10_000 {
            match client.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("{e}"),
            }
            if conn.flush().unwrap() && got.len() >= big.len() {
                break;
            }
        }
        while let Ok(n) = client.read(&mut buf) {
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got.len(), big.len());
        assert!(!conn.has_queued());
    }
}
