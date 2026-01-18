//! End-to-end wire → parse → execute → reply, over a real Unix socket:
//! the same path a real `bspc-rs` process and `bsp-compositor`'s future
//! server loop will use, minus the compositor's actual event loop
//! (`docs/bsp-ipc.md`, IPC progress).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream as ClientStream;
use std::path::PathBuf;

use bsp_core::desktop::Desktop;
use bsp_core::geometry::Rect;
use bsp_core::id::{DesktopId, MonitorId, WindowId};
use bsp_core::monitor::Monitor;
use bsp_core::node::Client as CoreClient;
use bsp_core::settings::Settings;
use bsp_core::wm::Wm;
use bsp_ipc::adapter::FakeAdapter;
use bsp_ipc::exec::{self, ExecCtx};
use bsp_ipc::registry::NodeRegistry;
use bsp_ipc::server::Listener;
use bsp_ipc::wire::Reply;
use bsp_ipc::{command, wire};

fn temp_socket_path(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "bsp-ipc-integration-{name}-{}.sock",
        std::process::id()
    ));
    p
}

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
    let n = d
        .tree
        .new_client_node(&settings, CoreClient::new(WindowId(1), 1));
    d.tree.insert_node(&settings, n, None);
    m.add_desktop(d);
    m.arrange(0, &settings);
    registry.register(DesktopId(1), n);

    wm.add_monitor(m);
    wm.focus_monitor(0);
    wm.monitors[0].focused = Some(0);

    (wm, registry, FakeAdapter::new())
}

/// Accepts one connection (busy-polling the non-blocking listener, as a
/// real event loop's readiness notification would trigger this same
/// call), reads one request, and returns it decoded.
fn accept_and_read(listener: &Listener) -> (bsp_ipc::server::Connection, Vec<String>) {
    let mut conn = None;
    for _ in 0..10_000 {
        if let Some(c) = listener.accept().unwrap() {
            conn = Some(c);
            break;
        }
    }
    let mut conn = conn.expect("connection was not accepted in time");
    let mut args = None;
    for _ in 0..10_000 {
        if let Some(a) = conn.try_read_request().unwrap() {
            args = Some(a);
            break;
        }
    }
    (conn, args.expect("request was not readable in time"))
}

#[test]
fn client_request_reaches_the_executor_and_the_reply_comes_back() {
    let path = temp_socket_path("basic");
    let listener = Listener::bind(&path).unwrap();
    let mut client = ClientStream::connect(&path).unwrap();
    client
        .write_all(&wire::encode_request(["query", "-N"]))
        .unwrap();

    let (mut conn, args) = accept_and_read(&listener);
    assert_eq!(args, vec!["query", "-N"]);

    let (mut wm, mut registry, mut adapter) = fixture();
    let cmd = command::parse(&args).unwrap();
    let mut ctx = ExecCtx {
        wm: &mut wm,
        registry: &mut registry,
        adapter: &mut adapter,
    };
    let (reply, _events) = exec::execute(&mut ctx, &cmd);
    conn.send_reply(reply).unwrap();
    drop(conn);

    let mut buf = Vec::new();
    client.read_to_end(&mut buf).unwrap();
    assert_eq!(Reply::parse(&buf), Reply::Ok("0x00000001\n".to_string()));
}

#[test]
fn a_failing_command_reaches_the_client_as_a_failure() {
    let path = temp_socket_path("failure");
    let listener = Listener::bind(&path).unwrap();
    let mut client = ClientStream::connect(&path).unwrap();
    client
        .write_all(&wire::encode_request(["config", "not_a_real_setting"]))
        .unwrap();

    let (mut conn, args) = accept_and_read(&listener);

    let (mut wm, mut registry, mut adapter) = fixture();
    let cmd = command::parse(&args).unwrap();
    let mut ctx = ExecCtx {
        wm: &mut wm,
        registry: &mut registry,
        adapter: &mut adapter,
    };
    let (reply, _events) = exec::execute(&mut ctx, &cmd);
    conn.send_reply(reply).unwrap();
    drop(conn);

    let mut buf = Vec::new();
    client.read_to_end(&mut buf).unwrap();
    match Reply::parse(&buf) {
        Reply::Fail(msg) => assert!(msg.contains("Unknown setting")),
        other => panic!("expected Fail, got {other:?}"),
    }
}

#[test]
fn node_close_command_is_forwarded_to_the_adapter() {
    let path = temp_socket_path("close");
    let listener = Listener::bind(&path).unwrap();
    let mut client = ClientStream::connect(&path).unwrap();
    client
        .write_all(&wire::encode_request(["node", "-c"]))
        .unwrap();

    let (mut conn, args) = accept_and_read(&listener);

    let (mut wm, mut registry, mut adapter) = fixture();
    let cmd = command::parse(&args).unwrap();
    {
        let mut ctx = ExecCtx {
            wm: &mut wm,
            registry: &mut registry,
            adapter: &mut adapter,
        };
        let (reply, _events) = exec::execute(&mut ctx, &cmd);
        conn.send_reply(reply).unwrap();
    }
    drop(conn);

    let mut buf = Vec::new();
    client.read_to_end(&mut buf).unwrap();
    assert_eq!(Reply::parse(&buf), Reply::Ok(String::new()));
    assert_eq!(adapter.closed, vec![WindowId(1)]);
}
