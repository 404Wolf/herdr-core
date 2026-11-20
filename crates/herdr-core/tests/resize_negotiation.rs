#![cfg(unix)]

mod common;

use common::framed::{Framed, Viewer};
use common::*;
use herdr_core::wire;
use serde_json::json;
use std::time::Duration;

#[test]
fn attach_confirms_size_notifications_locally_and_through_two_relays() {
    let owner = Session::new();
    let t = run(&owner, &["sleep", "100"]);
    let _leader = Viewer::attach(&owner, &t, 80, 24, json!({}));
    let mut relay = Session::new();
    relay.env.push(fake_ssh(&relay, &[("owner", &owner)]));
    relay.json(&["machine", "add", "owner"]);
    let mut client = Session::new();
    client.env.push(fake_ssh(&client, &[("relay", &relay)]));
    client.json(&["machine", "add", "relay"]);
    for (session, terminal) in [(&owner, t.clone()), (&client, format!("relay/owner/{t}"))] {
        for (observe, requested) in [(true, true), (true, false), (false, true)] {
            let mut conn = Framed::open(session);
            let reply = conn.call(
                "terminal.attach",
                json!({
                    "terminal": terminal, "cols": 80, "rows": 24,
                    "observe": observe, "resize_notifications": requested
                }),
            );
            let enabled = requested;
            assert_eq!(reply["result"]["resize_notifications"], enabled, "{reply}");
            if enabled {
                let channel = reply["result"]["channel"].as_u64().unwrap() as u32;
                assert!(conn.stash.is_empty(), "reply must precede stream frames");
                assert_eq!(conn.read(Duration::from_secs(5)), Some((channel, wire::RESIZE, vec![80, 0, 24, 0])));
            }
        }
    }
}

#[test]
fn interactive_grid_negotiation_falls_back_if_either_hop_is_pre_fix() {
    let Some(old) = std::env::var_os("HC_PRE_GRID_BIN") else {
        eprintln!("skipped: set HC_PRE_GRID_BIN to the pre-fix core");
        return;
    };
    for old_owner in [true, false] {
        let mut owner = Session::new();
        let mut client = Session::new();
        let legacy = if old_owner { &mut owner } else { &mut client };
        let bin = legacy.root.join("bin/herdr-core");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::copy(&old, &bin).unwrap();
        legacy.bin = bin;
        let t = run(&owner, &["sleep", "100"]);
        client.env.push(fake_ssh(&client, &[("owner", &owner)]));
        client.json(&["machine", "add", "owner"]);
        let mut conn = Framed::open(&client);
        let reply = conn.call(
            "terminal.attach",
            json!({"terminal": format!("owner/{t}"), "cols": 80, "rows": 24, "resize_notifications": true}),
        );
        assert_eq!(reply["result"]["resize_notifications"], false, "legacy owner={old_owner}: {reply}");
    }
}

#[test]
fn initial_two_relay_attach_falls_back_and_maps_data_from_old_owner() {
    let Some(old) = std::env::var_os("HC_PRE_GRID_BIN") else {
        eprintln!("skipped: set HC_PRE_GRID_BIN to a pre-grid artifact");
        return;
    };
    let mut owner = Session::new();
    owner.bin = old.into();
    let t = run(&owner, &["sh", "-c", "echo fallback-stream-data; sleep 100"]);
    wait_until("owner output", || text(&owner, &t).contains("fallback-stream-data"));
    let mut relay = Session::new();
    relay.env.push(fake_ssh(&relay, &[("owner", &owner)]));
    relay.json(&["machine", "add", "owner"]);
    let mut client = Session::new();
    client.env.push(fake_ssh(&client, &[("relay", &relay)]));
    client.json(&["machine", "add", "relay"]);
    let mut conn = Framed::open(&client);
    let reply = conn.call(
        "terminal.attach",
        json!({
            "terminal": format!("relay/owner/{t}"), "cols": 80, "rows": 24,
            "resize_notifications": true, "visibility_updates": true, "visible": false,
            "history": 100
        }),
    );
    let channel = reply["result"]["channel"].as_u64().expect("initial fallback must succeed") as u32;
    assert_eq!(reply["result"]["resize_notifications"], false, "{reply}");
    assert_eq!(reply["result"]["visibility_updates"], false, "{reply}");
    let mut bytes = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !String::from_utf8_lossy(&bytes).contains("fallback-stream-data") {
        assert!(std::time::Instant::now() < deadline, "fallback DATA was not mapped");
        if let Some((ch, kind, data)) = conn.read(Duration::from_millis(200)) {
            assert_eq!(ch, channel);
            if kind == wire::DATA {
                bytes.extend(data);
            }
        }
    }
}
