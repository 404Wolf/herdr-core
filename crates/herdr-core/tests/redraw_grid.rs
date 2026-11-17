#![cfg(target_os = "linux")]

mod common;

use common::framed::{Framed, Viewer, desktop};
use common::*;
use herdr_core::wire;
use serde_json::json;
use std::time::Duration;

#[test]
fn same_version_grid_upgrade_keeps_the_holder_child_id_and_backlog() {
    let Some(old) = std::env::var_os("HC_PRE_GRID_BIN") else {
        eprintln!("skipped: set HC_PRE_GRID_BIN to the installed pre-fix core");
        return;
    };
    let mut s = Session::new();
    let bin = s.root.join("bin/herdr-core");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::copy(old, &bin).unwrap();
    s.bin = bin.clone();
    let t = run(&s, &["sh", "-c", "echo preserved-backlog; while read l; do echo received-$l; done"]);
    wait_until("old PTY is ready", || text(&s, &t).contains("preserved-backlog"));
    let before = s.json(&["server", "status"]);
    let child = get(&s, &t)["pid"].clone();
    let attach = |s: &Session| {
        Framed::open(s).call(
            "terminal.attach",
            json!({"terminal": t, "cols": 80, "rows": 24, "resize_notifications": true}),
        )["result"]["resize_notifications"]
            .clone()
    };
    assert_eq!(attach(&s), false, "the running pre-fix binary cannot negotiate an interactive grid");
    let new = std::env::var_os("HC_GRID_BIN").map(std::path::PathBuf::from).unwrap_or_else(|| BIN.into());
    std::fs::copy(new, bin.with_extension("new")).unwrap();
    std::fs::rename(bin.with_extension("new"), &bin).unwrap();
    s.json(&["server", "restart"]);
    let after = s.json(&["server", "status"]);
    assert_eq!(before["version"], after["version"], "the wire version stays 0.1.18");
    assert_ne!(before["pid"], after["pid"], "an explicit restart runs the new same-version binary");
    assert_eq!(attach(&s), true, "the new behavior is active, not merely a different process");
    assert!(
        after["holders"].as_array().unwrap().iter().any(|h| {
            h["pid"] == before["holder"]["pid"]
                && h["instance"] == before["holder"]["instance"]
                && h["current"] == false
        }),
        "the old holder stays alive and drains while its build differs from the new core"
    );
    assert_eq!(get(&s, &t)["pid"], child);
    assert_eq!(get(&s, &t)["state"], "live");
    s.json(&["send", &t, "after-upgrade"]);
    wait_until("same child accepts input", || text(&s, &t).contains("received-after-upgrade"));
    assert!(text(&s, &t).contains("preserved-backlog"));
}

#[test]
fn remote_inline_redraw_uses_one_grid_across_resize_and_holder_reconnect() {
    let far = Session::new();
    let program = format!(
        "stty -echo; i=0; while [ $i -lt 160 ]; do echo retained-backlog-$i; i=$((i+1)); done; while read frame; do \
         stty size >{}/pty-size; \
         printf '\\033[H\\033[2J| markdown table row | result | expected | duplicated wrap |\\r\\n\\r\\n> prompt\\r\\nINSERT bypass'; \
         printf '\\033['; sleep 0.02; \
         printf '2A\\r\\033[J> updated prompt with wrapping words for the shared grid\\r\\nINSERT frame-%s' \"$frame\"; \
         done",
        far.root.display()
    );
    let t = run(&far, &["sh", "-c", &program]);
    let pid = get(&far, &t)["pid"].clone();
    let holder = far.json(&["server", "status"])["holder"].clone();
    let mut near = Session::new();
    near.env.push(fake_ssh(&near, &[("far", &far)]));
    near.env.push(("HC_RECONNECT_MS".into(), "100".into()));
    near.json(&["machine", "add", "far"]);
    let address = format!("far/{t}");
    let (mut app_a, conn_a) = desktop(&near);
    let (mut app_b, conn_b) = desktop(&near);
    let mut a = Viewer::attach_on(
        conn_a,
        &address,
        80,
        16,
        json!({"resize_notifications": true, "visibility_updates": true, "focus_updates": true, "focused": false}),
    );
    let mut b = Viewer::attach_on(
        conn_b,
        &address,
        40,
        12,
        json!({"resize_notifications": true, "visibility_updates": true, "focus_updates": true, "focused": true}),
    );
    let resize = |v: &mut Viewer, cols: u16, rows: u16| {
        v.conn.send(v.channel, wire::RESIZE, &[cols.to_le_bytes(), rows.to_le_bytes()].concat());
        v.conn.call("terminal.get", json!({"terminal": address}));
    };
    resize(&mut a, 80, 16);
    resize(&mut b, 40, 12);

    let check = |a: &mut Viewer, b: &mut Viewer, frame: &str, size: (u16, u16)| {
        b.typed(format!("{frame}\n").as_bytes());
        a.shows(&format!("frame-{frame}"));
        b.shows(&format!("frame-{frame}"));
        let mut owner =
            Viewer::attach(&far, &t, size.0, size.1, json!({"observe": true, "resize_notifications": true}));
        owner.shows(&format!("frame-{frame}"));
        assert_eq!(a.screen.size(), size, "wide viewer renders the PTY grid");
        assert_eq!(b.screen.size(), size, "narrow viewer renders the PTY grid");
        assert_eq!(a.screen.screen_rows(), owner.screen.screen_rows());
        assert_eq!(b.screen.screen_rows(), owner.screen.screen_rows());
        assert_eq!(get(&far, &t)["pid"], pid, "the same PTY child survives");
        assert_eq!(
            std::fs::read_to_string(far.root.join("pty-size")).unwrap().trim(),
            format!("{} {}", size.1, size.0)
        );
    };
    check(&mut a, &mut b, "one", (40, 12));
    resize(&mut b, 50, 10); // Both rows and columns change.
    check(&mut a, &mut b, "two", (50, 10));
    // Focus transfers the single grid owner without changing either physical offer.
    for _ in 0..4 {
        b.conn.send(b.channel, wire::FOCUS, &[0]);
        a.conn.send(a.channel, wire::FOCUS, &[1]);
        a.conn.send(a.channel, wire::FOCUS, &[0]);
        b.conn.send(b.channel, wire::FOCUS, &[1]);
    }
    b.conn.call("terminal.get", json!({"terminal": address}));
    assert_eq!((get(&far, &t)["cols"].as_u64(), get(&far, &t)["rows"].as_u64()), (Some(50), Some(10)));

    // Only this test's isolated core restarts; its holder and PTY child live on.
    far.json(&["server", "restart"]);
    let holders = far.json(&["server", "status"])["holders"].clone();
    assert!(
        holders.as_array().unwrap().iter().any(|h| h["pid"] == holder["pid"] && h["instance"] == holder["instance"]),
        "the same holder process owns the same child after reconnect"
    );
    wait_until("bridge reconnects to isolated owner", || {
        near.json(&["machine", "ls"])["machines"][0]["state"] == "connected"
    });
    a.drain(Duration::from_millis(100));
    b.drain(Duration::from_millis(100));
    assert!(a.closed.is_none() && b.closed.is_none(), "capable reattach keeps the channels");
    // Returning viewers re-establish their pane offers through the same channels.
    resize(&mut a, 80, 16);
    resize(&mut b, 50, 10);
    check(&mut a, &mut b, "three", (50, 10));
    assert!(text(&far, &t).contains("retained-backlog"), "the original backlog survives the restart");
    drop(a);
    drop(b);
    app_a.wait().unwrap();
    app_b.wait().unwrap();
}
