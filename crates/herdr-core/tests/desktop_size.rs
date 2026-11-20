#![cfg(target_os = "linux")]

mod common;

use common::framed::{Framed, Viewer, desktop};
use common::*;
use herdr_core::wire;
use serde_json::json;

#[test]
fn local_windows_share_a_real_pty_size_and_remote_input_never_shrinks_it() {
    let s = Session::new();
    let t = run(&s, &["sh", "-c", "while read l; do stty size; done"]);
    let (mut app_a, mut a) = desktop(&s);
    let (mut app_b, mut b) = desktop(&s);
    let attach = |conn: &mut Framed, cols, rows| {
        conn.call("terminal.attach", json!({"terminal": t, "cols": cols, "rows": rows}))["result"]["channel"]
            .as_u64()
            .unwrap() as u32
    };
    let ca = attach(&mut a, 100, 20);
    a.send(ca, wire::RESIZE, &[100, 0, 20, 0]);
    a.call("terminal.get", json!({"terminal": t}));
    let cb = attach(&mut b, 120, 40); // hidden attach placeholder
    let size = || {
        let r = s.json(&["get", &t]);
        (r["cols"].as_u64().unwrap(), r["rows"].as_u64().unwrap())
    };
    assert_eq!(size(), (100, 20));
    b.send(cb, wire::RESIZE, &[80, 0, 30, 0]);
    b.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (80, 20));
    a.send(ca, wire::FOCUS, &[1]);
    a.send(ca, wire::DATA, b"probe\r");
    a.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (80, 20));
    wait_until("PTY sees the shared size", || text(&s, &t).contains("20 80"));

    // Desktop first: a visible, measured desktop window keeps the grid; a
    // phone attaching or typing never shrinks it.
    let mut phone = Viewer::attach(&s, &t, 40, 10, json!({}));
    phone.conn.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (80, 20), "a phone attaching does not shrink the desktop grid");
    phone.typed(b"phone\r");
    phone.conn.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (80, 20), "remote input does not take size control");
    wait_until("PTY keeps the desktop size after remote input", || text(&s, &t).matches("20 80").count() >= 2);
    a.send(ca, wire::FOCUS, &[1]);
    a.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (80, 20));
    drop(phone);
    a.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (80, 20), "the desktop grid is unchanged after the phone detaches");

    let config = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "size_policy = 'latest'\n").unwrap();
    a.send(ca, wire::FOCUS, &[1]);
    a.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (100, 20), "legacy policy can be selected without a restart");
    drop(a);
    drop(b);
    app_a.wait().unwrap();
    app_b.wait().unwrap();
}
