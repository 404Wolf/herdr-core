#![cfg(target_os = "linux")]
//! Focused-window-wins, end to end through a relaying core: two app windows
//! on one machine view a terminal owned by another machine's core.

mod common;

use common::framed::{Framed, desktop};
use common::*;
use herdr_core::wire;
use serde_json::{Value, json};

fn attach(conn: &mut Framed, terminal: &str, cols: u16, rows: u16, focus: bool, focused: bool) -> (u32, Value) {
    let reply = conn.call(
        "terminal.attach",
        json!({
            "terminal": terminal, "cols": cols, "rows": rows,
            "resize_notifications": true, "visibility_updates": true, "visible": true,
            "focus_updates": focus, "focused": focused
        }),
    );
    let channel = reply["result"]["channel"].as_u64().unwrap_or_else(|| panic!("attach failed: {reply}")) as u32;
    let size = [cols.to_le_bytes(), rows.to_le_bytes()].concat();
    conn.send(channel, wire::RESIZE, &size);
    (channel, reply)
}

fn grid(owner: &Session, t: &str) -> (u64, u64) {
    let r = owner.json(&["get", t]);
    (r["cols"].as_u64().unwrap(), r["rows"].as_u64().unwrap())
}

/// 2026-11-13, macro/t1b6 from small-pickle: hidden 2158x1510 window (218x74,
/// previously focused) and the focused 2158x3821 window (218x187), both via
/// the small-pickle core. The owner must size to the focused window.
#[test]
fn incident_2026_11_13_focused_window_wins_through_a_relay() {
    let owner = Session::new();
    let t = run(&owner, &["sleep", "100"]);
    let mut pickle = Session::new();
    pickle.env.push(fake_ssh(&pickle, &[("macro", &owner)]));
    pickle.json(&["machine", "add", "macro"]);
    let hello = Framed::open(&pickle).call("hello", json!({"framed": true}));
    assert!(hello["result"]["features"].as_array().unwrap().iter().any(|f| f == "viewer_focus"), "{hello}");
    let remote = format!("macro/{t}");
    let (mut app_hidden, mut hidden) = desktop(&pickle);
    let (mut app_focused, mut focused) = desktop(&pickle);

    let (h, reply) = attach(&mut hidden, &remote, 218, 74, true, true);
    assert_eq!(reply["result"]["focus_updates"], true, "every hop confirms: {reply}");
    wait_until("hidden window sizes first", || grid(&owner, &t) == (218, 74));
    // Wolf moves to the portrait window; the old one loses focus but its
    // special workspace never reports it hidden.
    hidden.send(h, wire::WINDOW_FOCUS, &[0]);
    let (f, _) = attach(&mut focused, &remote, 218, 187, true, true);
    wait_until("grid follows the focused window", || grid(&owner, &t) == (218, 187));
    // The unfocused window's activity and offers are ignored.
    hidden.send(h, wire::FOCUS, &[1]);
    hidden.send(h, wire::RESIZE, &[218, 0, 70, 0]);
    hidden.send(h, wire::DATA, b"");
    hidden.call("terminal.get", json!({"terminal": remote}));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(grid(&owner, &t), (218, 187), "an unfocused window never shrinks the grid");

    let viewers = owner.json(&["viewers", &t]);
    assert_eq!(viewers["focus_authority"], true, "{viewers}");
    let list = viewers["viewers"].as_array().unwrap();
    assert_eq!(list.iter().filter(|v| v["focus_updates"] == true).count(), 2, "{viewers}");
    assert_eq!(list.iter().filter(|v| v["focused"] == true && v["leader"] == true).count(), 1, "{viewers}");

    // Nothing focused: the last focused window keeps the grid.
    focused.send(f, wire::WINDOW_FOCUS, &[0]);
    focused.call("terminal.get", json!({"terminal": remote}));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(grid(&owner, &t), (218, 187));
    // Focusing the other window hands it the grid, through the relay.
    hidden.send(h, wire::WINDOW_FOCUS, &[1]);
    wait_until("refocused window decides", || grid(&owner, &t) == (218, 70));
    drop(hidden);
    drop(focused);
    app_hidden.wait().unwrap();
    app_focused.wait().unwrap();
}

/// A viewer that cannot report focus (old app or client) only receives the grid.
#[test]
fn non_reporting_viewer_through_a_relay_never_shrinks_the_focused_window() {
    let owner = Session::new();
    let t = run(&owner, &["sleep", "100"]);
    let mut pickle = Session::new();
    pickle.env.push(fake_ssh(&pickle, &[("macro", &owner)]));
    pickle.json(&["machine", "add", "macro"]);
    let remote = format!("macro/{t}");
    let (mut app, mut window) = desktop(&pickle);
    let (mut app_old, mut old) = desktop(&pickle);
    attach(&mut window, &remote, 200, 60, true, true);
    wait_until("focused window sizes", || grid(&owner, &t) == (200, 60));
    let (o, reply) = attach(&mut old, &remote, 80, 24, false, false);
    assert_eq!(reply["result"]["focus_updates"], false, "{reply}");
    old.send(o, wire::WINDOW_FOCUS, &[1]); // not negotiated: the relay drops it
    old.send(o, wire::FOCUS, &[1]);
    old.send(o, wire::DATA, b"x");
    old.call("terminal.get", json!({"terminal": remote}));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(grid(&owner, &t), (200, 60), "old viewer only receives the grid");
    let viewers = owner.json(&["viewers", &t]);
    assert!(viewers["viewers"].as_array().unwrap().iter().any(|v| v["focus_updates"] == false), "{viewers}");
    drop(window);
    drop(old);
    app.wait().unwrap();
    app_old.wait().unwrap();
}

/// 2026-11-13, laptop/t6k: a new app on small-pickle views a laptop terminal
/// through the small-pickle core. At the owner the relayed viewer must be
/// focus-reporting and visible; and a hidden owner-local window with focus
/// history must not lock out a visible measured window that arrives without focus.
#[test]
fn incident_2026_11_13_relay_viewer_reports_focus_and_is_not_locked_out() {
    let owner = Session::new();
    let t = run(&owner, &["sleep", "100"]);
    let mut pickle = Session::new();
    pickle.env.push(fake_ssh(&pickle, &[("framework", &owner)]));
    pickle.json(&["machine", "add", "framework"]);
    let remote = format!("framework/{t}");
    // The laptop's own window: focused once, then hidden on a special workspace.
    let (mut app_local, mut local) = desktop(&owner);
    let (l, _) = attach(&mut local, &t, 120, 40, true, true);
    wait_until("local window sizes", || grid(&owner, &t) == (120, 40));
    local.send(l, wire::WINDOW_FOCUS, &[0]);
    local.send(l, wire::VISIBILITY, &[0]);
    local.call("terminal.get", json!({"terminal": t}));

    // A new app through the new relay: focus-reporting and visible at the owner.
    let (mut app_new, mut new) = desktop(&pickle);
    let (n, reply) = attach(&mut new, &remote, 263, 197, true, true);
    assert_eq!(reply["result"]["focus_updates"], true, "{reply}");
    wait_until("the relayed focused window decides", || grid(&owner, &t) == (263, 197));
    let viewers = owner.json(&["viewers", &t]);
    let relayed: Vec<&Value> =
        viewers["viewers"].as_array().unwrap().iter().filter(|v| v["origin"] == "relayed").collect();
    assert_eq!(relayed.len(), 1, "{viewers}");
    assert_eq!(relayed[0]["focus_updates"], true, "{viewers}");
    assert_eq!(relayed[0]["focused"], true, "{viewers}");
    assert_eq!(relayed[0]["visible"], true, "{viewers}");
    assert_eq!(relayed[0]["leader"], true, "{viewers}");
    drop(new);
    app_new.wait().unwrap();

    // An old app through the same relay (no focus): with the only reporter
    // hidden, its visible measured window takes the grid instead of waiting.
    let (mut app_old, mut old) = desktop(&pickle);
    let (o, reply) = attach(&mut old, &remote, 263, 150, false, false);
    assert_eq!(reply["result"]["focus_updates"], false, "{reply}");
    wait_until("visible non-reporter is not locked out", || grid(&owner, &t) == (263, 150));
    // The laptop window is shown again: it holds focus history and decides.
    local.send(l, wire::RESIZE, &[120, 0, 40, 0]);
    local.send(l, wire::VISIBILITY, &[1]);
    wait_until("shown reporter decides again", || grid(&owner, &t) == (120, 40));
    old.send(o, wire::DATA, b"");
    old.call("terminal.get", json!({"terminal": remote}));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(grid(&owner, &t), (120, 40));
    drop(old);
    drop(local);
    app_old.wait().unwrap();
    app_local.wait().unwrap();
}
