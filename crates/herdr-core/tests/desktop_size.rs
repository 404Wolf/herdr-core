#![cfg(target_os = "linux")]

mod common;

use common::framed::{Framed, Viewer, desktop};
use common::*;
use herdr_core::wire;
use serde_json::json;

#[test]
fn focused_desktop_owns_the_real_pty_while_other_windows_and_phone_follow() {
    let s = Session::new();
    let t = run(&s, &["sh", "-c", "while read l; do stty size; done"]);
    let (mut app_a, mut a) = desktop(&s);
    let (mut app_b, mut b) = desktop(&s);
    let attach = |conn: &mut Framed, cols, rows, focused| {
        let reply = conn.call(
            "terminal.attach",
            json!({
                "terminal": t, "cols": cols, "rows": rows, "focus_updates": true,
                "focused": focused, "visibility_updates": true,
            }),
        );
        assert_eq!(reply["result"]["focus_updates"], true);
        reply["result"]["channel"].as_u64().unwrap() as u32
    };
    let ca = attach(&mut a, 100, 20, true);
    a.send(ca, wire::RESIZE, &[100, 0, 20, 0]);
    a.call("terminal.get", json!({"terminal": t}));
    let cb = attach(&mut b, 120, 40, false);
    let size = || {
        let r = s.json(&["get", &t]);
        (r["cols"].as_u64().unwrap(), r["rows"].as_u64().unwrap())
    };
    b.send(cb, wire::RESIZE, &[80, 0, 30, 0]);
    b.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (100, 20), "unfocused physical offers do not shrink the focused window");
    b.send(cb, wire::FOCUS, &[1, 0]);
    b.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (100, 20), "malformed focus does not transfer ownership");
    b.send(cb, wire::FOCUS, &[1]);
    b.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (80, 30));
    a.send(ca, wire::FOCUS, &[1]);
    a.send(ca, wire::DATA, b"probe\r");
    a.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (100, 20));
    wait_until("PTY sees the focused viewer's full offer", || text(&s, &t).contains("20 100"));

    let mut phone = Viewer::attach(&s, &t, 40, 10, json!({}));
    phone.typed(b"phone\r");
    phone.conn.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (100, 20), "mobile input does not change desktop focus ownership");
    drop(phone);
    a.send(ca, wire::FOCUS, &[0]);
    b.send(cb, wire::FOCUS, &[0]);
    b.send(cb, wire::RESIZE, &[60, 0, 15, 0]);
    b.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (100, 20), "last-focused grid remains sticky on blur");
    drop(a);
    app_a.wait().unwrap();
    wait_until("leader detach leaves the grid held", || {
        let snapshot = s.json(&["viewers", &t]);
        snapshot["leader"].is_null() && snapshot["viewers"].as_array().unwrap().len() == 1
    });
    assert_eq!(size(), (100, 20), "unfocused survivor cannot inherit leadership");
    b.send(cb, wire::FOCUS, &[1]);
    b.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (60, 15));
    let config = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "size_policy = 'latest'\n").unwrap();
    b.send(cb, wire::FOCUS, &[1]);
    b.call("terminal.get", json!({"terminal": t}));
    assert_eq!(size(), (60, 15), "both policy names preserve focused ownership");
    drop(b);
    app_b.wait().unwrap();
}
