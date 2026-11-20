use super::*;
use crate::events::{Filter, Sink};
use crate::paths::Paths;
use std::sync::atomic::AtomicBool;

#[test]
fn automatic_input_requires_support_at_every_hop() {
    for features in [json!([]), json!(["automatic_input"])] {
        let (frames, _rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": features}));
        for reply in [json!({}), json!({"automatic_input": false}), json!({"automatic_input": true})] {
            assert_eq!(
                automatic_input(&line, &reply),
                !features.as_array().unwrap().is_empty() && reply["automatic_input"] == true
            );
        }
    }
}

#[test]
fn viewer_visibility_requires_confirmation_from_every_hop() {
    for features in [json!([]), json!(["viewer_visibility"])] {
        let (frames, _rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": features}));
        for reply in [json!({}), json!({"visibility_updates": false}), json!({"visibility_updates": true})] {
            assert_eq!(
                visibility_updates(&line, true, &reply),
                line.features.iter().any(|f| f == "viewer_visibility") && reply["visibility_updates"] == true
            );
        }
        assert!(!visibility_updates(&line, false, &json!({"visibility_updates": true})));
    }
}

#[tokio::test]
async fn viewer_visibility_frame_stays_ordered_with_data_and_resize_across_a_relay() {
    let root = std::path::PathBuf::from(format!(
        "/var/tmp/hc-visibility-relay-{}-{}",
        std::process::id(),
        crate::platform::random_hex()
    ));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let conn = Arc::new(Conn::default());
    conn.with(|out| out.open(7));
    let (frames, mut rx) = mpsc::unbounded_channel();
    core.lock().relayed.insert(
        (conn_key(&conn), 7),
        Relayed {
            automatic_input: false,
            visibility_updates: true,
            focus_updates: false,
            used_at: None,
            replies: Default::default(),
            focus_by_use: false,
            line: Line::new(frames, &json!({"features": ["viewer_visibility"]})),
            there: 9,
            conn: conn.clone(),
            machine: "remote".into(),
            params: json!({"visible": true, "visibility_updates": true}),
            attempt: None,
        },
    );

    let mut input = [
        wire::encode(7, wire::DATA, b"before resize"),
        wire::encode(7, wire::RESIZE, &[218, 0, 74, 0]),
        wire::encode(7, wire::VISIBILITY, &[0]),
        wire::encode(7, wire::DATA, b"after hidden"),
    ]
    .concat();
    let mut reader = std::io::Cursor::new(&mut input);
    let mut forwarded = Vec::new();
    while let Some((channel, kind, payload)) = wire::read_frame(&mut reader).await.unwrap() {
        assert!(relay_in(&core, &conn, channel, kind, &payload));
        forwarded.push(rx.recv().await.unwrap());
    }
    assert_eq!(
        forwarded,
        [
            wire::encode(9, wire::DATA, b"before resize"),
            wire::encode(9, wire::RESIZE, &[218, 0, 74, 0]),
            wire::encode(9, wire::VISIBILITY, &[0]),
            wire::encode(9, wire::DATA, b"after hidden"),
        ]
    );
    assert_eq!(core.lock().relayed[&(conn_key(&conn), 7)].params["visible"], false);
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn automatic_input_survives_relays_but_not_a_capability_downgrade() {
    for (feature, confirmed) in [(false, true), (true, false), (true, true)] {
        let root = std::path::PathBuf::from(format!(
            "/var/tmp/hc-automatic-relay-{}-{}",
            std::process::id(),
            crate::platform::random_hex()
        ));
        let paths = Paths {
            socket: root.join("core.sock"),
            holder_socket: root.join("holder.sock"),
            state: root.clone(),
            config: root.join("config.toml"),
        };
        paths.create_dirs().unwrap();
        let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
        let machine = Machine {
            name: "away".into(),
            target: "away".into(),
            via: false,
            temporary: false,
            session: None,
            id: "remote".into(),
            host: "away".into(),
            version: "test".into(),
        };
        let conn = Arc::new(Conn::default());
        conn.with(|out| out.open(7));
        let (old_frames, mut old_rx) = mpsc::unbounded_channel();
        core.lock().relayed.insert(
            (conn_key(&conn), 7),
            Relayed {
                automatic_input: true,
                visibility_updates: false,
                focus_updates: false,
                used_at: None,
                replies: Default::default(),
                focus_by_use: false,
                line: Line::new(old_frames, &json!({"features": ["automatic_input"]})),
                there: 9,
                conn: conn.clone(),
                machine: machine.id.clone(),
                params: json!({"terminal": "t1"}),
                attempt: None,
            },
        );
        assert!(relay_in(&core, &conn, 7, wire::AUTOMATIC_INPUT, b"\x1b"));
        let frame = old_rx.recv().await.unwrap();
        assert_eq!(frame, wire::encode(9, wire::AUTOMATIC_INPUT, b"\x1b"));
        let (frames, mut rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": if feature { vec!["automatic_input"] } else { vec![] }}));
        core.lock().lines.insert(machine.id.clone(), LineState::Connected(line.clone())); // this pass's line is current
        let task = tokio::spawn(reattach(core.clone(), machine, line.clone()));
        let request = rx.recv().await.unwrap();
        let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
        line.take_reply(json!({"id": request["id"], "result": {"channel": 10, "automatic_input": confirmed}}));
        task.await.unwrap();
        if feature && confirmed {
            assert!(core.lock().relayed.contains_key(&(conn_key(&conn), 7)));
            assert!(relay_in(&core, &conn, 7, wire::AUTOMATIC_INPUT, b"[48;24;80t"));
            assert_eq!(rx.recv().await.unwrap(), wire::encode(10, wire::AUTOMATIC_INPUT, b"[48;24;80t"));
        } else {
            assert!(!core.lock().relayed.contains_key(&(conn_key(&conn), 7)));
            assert!(!lock(&line.streams).contains_key(&10));
            assert_eq!(rx.recv().await.unwrap(), wire::encode(10, wire::CLOSE, b""));
            let close = conn.outbox().next_frame().unwrap();
            assert_eq!(close[4], wire::CLOSE);
        }
        drop(core);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn reconnect_rejects_downgrade_before_mapping_fallback_data() {
    for (require_resize, require_visibility) in [(true, false), (false, true)] {
        for stalled in [false, true] {
            // The immediate relay is capable, but its deeper path has downgraded.
            let (frames, mut rx) = mpsc::unbounded_channel();
            let line = Line::new(frames, &json!({"features": ["resize_notifications", "viewer_visibility"]}));
            let conn = Arc::new(Conn::default());
            conn.with(|out| {
                out.open(7);
                if stalled {
                    out.data(7, &vec![120; wire::WINDOW]);
                    while out.next_frame().is_some() {}
                }
            });
            let calling = line.clone();
            let sink = conn.clone();
            let task = tokio::spawn(async move {
                calling
                    .call_into(
                        "relay",
                        "terminal.attach",
                        json!({"observe": true, "resumed": true, "resize_notifications": true}),
                        Some(((sink, 7), require_resize, require_visibility, false, 0)),
                        Duration::from_secs(1),
                    )
                    .await
            });
            let request = rx.recv().await.unwrap();
            let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
            line.take_reply(json!({"id": request["id"], "result": {"channel": 9, "resize_notifications": false, "visibility_updates": false}}));
            // Model buffered DATA consumed before the awakened caller can run.
            if let Some((sink, here)) = lock(&line.streams).get(&9) {
                sink.with(|out| {
                    out.data(*here, b"unsupported fallback paint");
                });
            }
            assert!(task.await.unwrap().is_err(), "downgraded reattach must be rejected");
            conn.with(|out| {
                assert_eq!(out.queued(7), 0, "rejected fallback DATA must not enter the downstream queue");
                out.close(7, b"");
                let frame = out.next_frame().expect("CLOSE must not wait for credit");
                assert_eq!(wire::decode_header(frame[..wire::HEADER].try_into().unwrap()).unwrap().1, wire::CLOSE);
                assert!(out.next_frame().is_none());
            });
            assert!(!lock(&line.streams).contains_key(&9), "rejected stream must never be mapped");
        }
    }
}

#[tokio::test]
async fn failed_reattach_drains_pending_data_before_close() {
    let root = std::path::PathBuf::from(format!(
        "/var/tmp/hc-reattach-drain-{}-{}",
        std::process::id(),
        crate::platform::random_hex()
    ));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine = Machine {
        name: "away".into(),
        target: "away".into(),
        via: false,
        temporary: false,
        session: None,
        id: "remote".into(),
        host: "away".into(),
        version: "test".into(),
    };
    let conn = Arc::new(Conn::default());
    conn.with(|out| {
        out.open(7);
        assert!(out.data(7, &vec![b'x'; wire::WINDOW]));
        while out.next_frame().is_some() {}
        assert!(out.data(7, b"last output before the line dropped"));
        assert!(out.next_frame().is_none(), "pending output must wait for credit");
    });
    let (old_frames, old_rx) = mpsc::unbounded_channel();
    let old_line = Line::new(old_frames, &json!({}));
    core.lock().relayed.insert(
        (conn_key(&conn), 7),
        Relayed {
            automatic_input: true,
            visibility_updates: false,
            focus_updates: false,
            used_at: None,
            replies: Default::default(),
            focus_by_use: false,
            line: old_line,
            there: 9,
            conn: conn.clone(),
            machine: machine.id.clone(),
            params: json!({"terminal": "t1"}),
            attempt: None,
        },
    );
    drop(old_rx); // The viewer survives its old line.
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    core.lock().lines.insert(machine.id.clone(), LineState::Connected(line.clone())); // this pass's line is current
    let task = tokio::spawn(reattach(core.clone(), machine, line.clone()));
    let request = rx.recv().await.unwrap();
    let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
    assert_eq!(request["method"], "terminal.attach");
    line.take_reply(crate::rpc::response(
        request["id"].clone(),
        Err(RpcError::new("finished", "terminal finished", "")),
    ));
    task.await.unwrap();
    assert!(core.lock().relayed.is_empty());
    conn.with(|out| {
        out.credit(7, wire::WINDOW);
        let data = out.next_frame().expect("queued output before CLOSE");
        assert_eq!(wire::decode_header(data[..wire::HEADER].try_into().unwrap()).unwrap().1, wire::DATA);
        assert_eq!(&data[wire::HEADER..], b"last output before the line dropped");
        let close = out.next_frame().expect("CLOSE after queued output");
        assert_eq!(wire::decode_header(close[..wire::HEADER].try_into().unwrap()).unwrap().1, wire::CLOSE);
        assert!(out.next_frame().is_none());
    });
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn remote_misses_and_renames_keep_subscribers_on_the_current_machine() {
    let root = std::path::PathBuf::from(format!(
        "/var/tmp/hc-doc-missed-{}-{}",
        std::process::id(),
        crate::platform::random_hex()
    ));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine = Machine {
        name: "rohan".into(),
        target: "rohan".into(),
        via: false,
        temporary: false,
        session: None,
        id: "remote".into(),
        host: "rohan".into(),
        version: "test".into(),
    };
    let (tx, mut rx) = mpsc::channel(4);
    let (filtered_tx, mut filtered_rx) = mpsc::channel(4);
    {
        let mut st = core.lock();
        st.events.seq = 7;
        st.machines.push(machine.clone());
        core.subscribe(&mut st, Filter::default(), Sink::Plain { tx, behind: Arc::new(AtomicBool::new(false)) });
        core.subscribe(
            &mut st,
            Filter { terminal: Some("t1".into()), ..Default::default() },
            Sink::Plain { tx: filtered_tx, behind: Arc::new(AtomicBool::new(false)) },
        );
    }
    let (frames, _rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    mirror_events(&core, &machine, &line, b"{\"method\":\"missed\",\"params\":{\"seq\":9999,\"terminals\":[]}}\n");
    let msg: Value = serde_json::from_str(&rx.try_recv().expect("local missed notification")).unwrap();
    assert_eq!(msg["method"], "missed");
    assert_eq!(msg["params"]["seq"], 7);
    assert!(filtered_rx.try_recv().is_err());
    mirror_replace(&core, &machine, vec![json!({"address":"t1", "state":"live", "status":{"state":"working"}})]);
    rx.try_recv().unwrap();
    let conn = Arc::new(Conn::default());
    conn.with(|out| {
        out.open(1);
        assert!(out.data_lines(1, &vec![b'\n'; wire::MAX_PENDING]));
    });
    {
        let mut st = core.lock();
        core.subscribe(
            &mut st,
            Filter { terminal: Some("renamed/t1".into()), ..Default::default() },
            Sink::Framed { conn: conn.clone(), channel: 1 },
        );
        st.machines[0].name = "renamed".into();
        rename_mirror(&core, &mut st, &machine.id, "rohan", "renamed");
    }
    let mut bytes = Vec::new();
    conn.with(|out| {
        while let Some(frame) = out.next_frame() {
            bytes.extend_from_slice(&frame[wire::HEADER..]);
        }
    });
    let missed = bytes
        .split(|b| *b == b'\n')
        .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
        .find(|line| line["method"] == "missed")
        .expect("backpressure snapshot");
    assert_eq!(missed["params"]["terminals"][0]["address"], "renamed/t1");
    let removed: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    let created: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(removed["params"]["terminal"], "rohan/t1");
    assert_eq!(removed["params"]["type"], "removed");
    assert_eq!(created["params"]["terminal"], "renamed/t1");
    assert_eq!(created["params"]["data"]["status"]["state"], "working");
    mirror_events(&core, &machine, &line, b"{\"method\":\"event\",\"params\":{\"type\":\"status\",\"terminal\":\"t1\",\"data\":{\"status\":{\"state\":\"blocked\"}}}}\n");
    let status: Value = serde_json::from_str(&rx.try_recv().unwrap()).unwrap();
    assert_eq!(status["params"]["terminal"], "renamed/t1");
    assert_eq!(core.lock().mirrors[&machine.id]["renamed/t1"]["status"]["state"], "blocked");
    core.lock().machines.clear();
    mirror_event(&core, &machine, json!({"type": "doc", "data": {"name": "app", "version": 1}}));
    assert!(rx.try_recv().is_err(), "removed machines must not emit doc events");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn peer_build_hello_is_optional_and_preserves_precise_identity() {
    for (hello, expected) in [
        (json!({}), None),
        (json!({"build": ""}), None),
        (json!({"build": "0.1.18-c645d00-grid/abc123"}), Some("0.1.18-c645d00-grid/abc123")),
    ] {
        let (frames, _rx) = mpsc::unbounded_channel();
        assert_eq!(Line::new(frames, &hello).build.as_deref(), expected);
    }
}

#[test]
fn peer_build_view_flags_live_differences_and_legacy_peers_and_clears_on_disconnect() {
    let machine = Machine {
        name: "peer".into(), target: "peer".into(), via: false, temporary: false,
        session: None, id: "peer-id".into(), host: "peer".into(), version: "0.1.18".into(),
    };
    let mut st = State::default();
    for (build, skew) in [(None, true), (Some(crate::client::peer_build()), false),
        (Some("0.1.18-same-version/different-patches".into()), true)] {
        let (frames, _rx) = mpsc::unbounded_channel();
        st.lines.insert(machine.id.clone(), LineState::Connected(Line::new(frames, &json!({"build": build}))));
        let current = view(&st, &machine);
        assert_eq!(current.build, build);
        assert_eq!(current.build_skew, skew);
        let wire = serde_json::to_value(current).unwrap();
        assert_eq!(wire["build_skew"], skew);
    }
    st.lines.insert(machine.id.clone(), LineState::Connecting);
    let current = view(&st, &machine);
    assert_eq!(current.build, None);
    assert!(!current.build_skew);
}

#[test]
fn viewer_focus_requires_confirmation_from_every_hop_and_is_stripped_before_old_hops() {
    for features in [json!([]), json!(["viewer_focus"])] {
        let (frames, _rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": features}));
        let capable = line.features.iter().any(|f| f == "viewer_focus");
        for reply in [json!({}), json!({"focus_updates": false}), json!({"focus_updates": true})] {
            assert_eq!(focus_updates(&line, true, &reply), capable && reply["focus_updates"] == true);
        }
        assert!(!focus_updates(&line, false, &json!({"focus_updates": true})));
        let mut params = json!({"focus_updates": true, "focused": true});
        viewers::negotiate_focus(&line, &mut params);
        assert_eq!(params["focus_updates"], capable, "never forward an opt-in an old hop cannot confirm");
        assert_eq!(params["focused"], capable);
    }
}

/// Two windows on one machine viewing a terminal on another, through this
/// core: WINDOW_FOCUS goes through per downstream viewer, in order, and only
/// for streams whose whole path negotiated it.
#[tokio::test]
async fn window_focus_is_relayed_per_downstream_viewer() {
    let root = std::path::PathBuf::from(format!(
        "/var/tmp/hc-focus-relay-{}-{}",
        std::process::id(),
        crate::platform::random_hex()
    ));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({"features": ["viewer_visibility", "viewer_focus"]}));
    let hidden_window = Arc::new(Conn::default());
    let focused_window = Arc::new(Conn::default());
    let legacy = Arc::new(Conn::default());
    for (conn, there, focus) in [(&hidden_window, 9, true), (&focused_window, 10, true), (&legacy, 11, false)] {
        conn.with(|out| out.open(7));
        core.lock().relayed.insert(
            (conn_key(conn), 7),
            Relayed {
                automatic_input: false,
                visibility_updates: true,
                focus_updates: focus,
                used_at: None,
                replies: Default::default(),
                focus_by_use: false,
                line: line.clone(),
                there,
                conn: conn.clone(),
                machine: "macro".into(),
                params: json!({"visible": true, "visibility_updates": true, "focus_updates": focus, "focused": focus}),
                attempt: None,
            },
        );
    }
    assert!(relay_in(&core, &hidden_window, 7, wire::WINDOW_FOCUS, &[0]));
    assert!(relay_in(&core, &focused_window, 7, wire::RESIZE, &[218, 0, 187, 0]));
    assert!(relay_in(&core, &focused_window, 7, wire::WINDOW_FOCUS, &[1]));
    assert!(relay_in(&core, &legacy, 7, wire::WINDOW_FOCUS, &[1]), "consumed, not forwarded");
    assert!(relay_in(&core, &focused_window, 7, wire::WINDOW_FOCUS, &[2]), "malformed: dropped");
    let mut forwarded = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        forwarded.push(frame);
    }
    assert_eq!(
        forwarded,
        [
            wire::encode(9, wire::WINDOW_FOCUS, &[0]),
            wire::encode(10, wire::RESIZE, &[218, 0, 187, 0]),
            wire::encode(10, wire::WINDOW_FOCUS, &[1]),
        ]
    );
    let st = core.lock();
    assert_eq!(st.relayed[&(conn_key(&hidden_window), 7)].params["focused"], false, "reattach carries the latest");
    assert_eq!(st.relayed[&(conn_key(&focused_window), 7)].params["focused"], true);
    // Item 71: the reattach says how long ago the viewer was last used, so a
    // dropped line never looks like fresh use at the owner.
    let used = st.relayed[&(conn_key(&focused_window), 7)].used_at.expect("WINDOW_FOCUS(1) is use");
    assert!(viewers::idle_ms(&st.relayed[&(conn_key(&hidden_window), 7)]).is_none(), "unfocused: nothing to say");
    assert!(viewers::idle_ms(&st.relayed[&(conn_key(&legacy), 7)]).is_none(), "no focus path: nothing to say");
    drop(st);
    std::thread::sleep(Duration::from_millis(20));
    // A terminal reply sent as DATA is not use; typing is.
    assert!(relay_in(&core, &focused_window, 7, wire::DATA, b"\x1b[24;80R"));
    let r = core.lock().relayed[&(conn_key(&focused_window), 7)].clone();
    assert_eq!(r.used_at, Some(used), "a reply is not use");
    assert!(viewers::idle_ms(&r).unwrap() >= 20, "the age since the last use");
    assert!(relay_in(&core, &focused_window, 7, wire::DATA, b"x"));
    let r = core.lock().relayed[&(conn_key(&focused_window), 7)].clone();
    assert!(r.used_at > Some(used), "typing is use");
    assert!(viewers::idle_ms(&r).unwrap() < 20);
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn reconnect_rejects_a_focus_downgrade() {
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({"features": ["resize_notifications", "viewer_visibility", "viewer_focus"]}));
    let conn = Arc::new(Conn::default());
    conn.with(|out| out.open(7));
    let calling = line.clone();
    let sink = conn.clone();
    let task = tokio::spawn(async move {
        calling
            .call_into(
                "relay",
                "terminal.attach",
                json!({"resumed": true, "focus_updates": true}),
                Some(((sink, 7), false, false, true, 0)),
                Duration::from_secs(1),
            )
            .await
    });
    let request = rx.recv().await.unwrap();
    let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
    line.take_reply(json!({"id": request["id"], "result": {"channel": 9, "focus_updates": false}}));
    let err = task.await.unwrap().unwrap_err();
    assert_eq!(err.message, FOCUS_LOST);
    assert_eq!(rx.recv().await.unwrap(), wire::encode(9, wire::CLOSE, b""), "the downgraded stream there is closed");
}

#[test]
fn resize_history_is_stripped_before_old_hops_and_needs_the_owners_echo() {
    for features in [json!(["resize_notifications"]), json!(["resize_notifications", "resize_history"])] {
        let (frames, _rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": features}));
        let capable = line.features.iter().any(|f| f == "resize_history");
        let mut params = json!({"resize_notifications": true, "resize_history": 500});
        viewers::negotiate_resize_history(&line, &mut params);
        assert_eq!(params["resize_history"], if capable { 500 } else { 0 }, "never ask a hop that cannot confirm");
        let mut params = json!({"resize_notifications": false, "resize_history": 500});
        viewers::negotiate_resize_history(&line, &mut params);
        assert_eq!(params["resize_history"], 0, "history rides on RESIZE");
        // An old owner behind a new hop never echoes it: 0, not what was asked.
        assert_eq!(viewers::resize_history(&line, 500, &json!({})), 0);
        assert_eq!(viewers::resize_history(&line, 500, &json!({"resize_history": 500})), if capable { 500 } else { 0 });
        assert_eq!(
            viewers::resize_history(&line, 500, &json!({"resize_history": 9000})),
            if capable { 500 } else { 0 }
        );
    }
}

/// A new relay in front of an old owner: the old owner ignores
/// `resize_history` and never echoes it, so the relay must not confirm it
/// (nor ask a hop whose hello lacks it). In front of a new owner it confirms
/// exactly what the owner did.
#[tokio::test]
async fn a_relay_confirms_resize_history_only_when_the_owner_did() {
    let root = std::path::PathBuf::from(format!(
        "/var/tmp/hc-history-relay-{}-{}",
        std::process::id(),
        crate::platform::random_hex()
    ));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine = Machine {
        name: "owner".into(),
        target: "owner".into(),
        via: false,
        temporary: false,
        session: None,
        id: "owner-id".into(),
        host: "owner".into(),
        version: "0.1.18".into(),
    };
    let old = json!(["resize_notifications", "viewer_visibility", "viewer_focus", "automatic_input"]);
    let new = json!(["resize_notifications", "viewer_visibility", "viewer_focus", "automatic_input", "resize_history"]);
    // (owner hello, what the owner's reply says, what the relay must confirm, what it must forward)
    for (features, echo, confirmed, forwarded) in
        [(&old, json!(null), 0, 0), (&new, json!(1000), 1000, 1000), (&new, json!(null), 0, 1000)]
    {
        let (frames, mut rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": features}));
        core.lock().lines.insert(machine.id.clone(), LineState::Connected(line.clone()));
        let conn = Arc::new(Conn::default());
        let (c, k, m) = (core.clone(), conn.clone(), machine.clone());
        let task = tokio::spawn(async move {
            let params =
                json!({"terminal": "t1", "cols": 80, "rows": 24, "resize_notifications": true, "resize_history": 1000});
            viewers::attach(&c, &k, &m, params).await
        });
        let request = rx.recv().await.unwrap();
        let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
        assert_eq!(request["params"]["resize_history"], forwarded, "{features}: {request}");
        let mut result = json!({"channel": 9, "terminal": {"address": "t1"}, "resize_notifications": true,
            "visibility_updates": false, "focus_updates": false, "automatic_input": true});
        if !echo.is_null() {
            result["resize_history"] = echo;
        }
        line.take_reply(json!({"id": request["id"], "result": result}));
        let reply = task.await.unwrap().unwrap();
        assert_eq!(reply["resize_history"], confirmed, "{features} echo {:?}: {reply}", result["resize_history"]);
        let st = core.lock();
        let relayed = st.relayed.values().find(|r| Arc::ptr_eq(&r.conn, &conn)).unwrap();
        assert_eq!(relayed.params["resize_history"], confirmed, "a reattach requires only what was confirmed");
    }
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

/// A relayed stream that was confirmed `resize_history` comes back after a
/// dropped line only with it confirmed again; an owner that lost it (old
/// hello, or no echo) closes the stream instead of resuming screen-only.
#[tokio::test]
async fn reconnect_rejects_a_resize_history_downgrade() {
    for (feature, echo) in [(false, false), (true, false), (true, true)] {
        let root = std::path::PathBuf::from(format!(
            "/var/tmp/hc-history-reattach-{}-{}",
            std::process::id(),
            crate::platform::random_hex()
        ));
        let paths = Paths {
            socket: root.join("core.sock"),
            holder_socket: root.join("holder.sock"),
            state: root.clone(),
            config: root.join("config.toml"),
        };
        paths.create_dirs().unwrap();
        let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
        let machine = Machine {
            name: "away".into(),
            target: "away".into(),
            via: false,
            temporary: false,
            session: None,
            id: "remote".into(),
            host: "away".into(),
            version: "test".into(),
        };
        let conn = Arc::new(Conn::default());
        conn.with(|out| out.open(7));
        let (old_frames, _old_rx) = mpsc::unbounded_channel();
        core.lock().relayed.insert(
            (conn_key(&conn), 7),
            Relayed {
                automatic_input: false,
                visibility_updates: false,
                focus_updates: false,
                used_at: None,
                replies: Default::default(),
                focus_by_use: false,
                line: Line::new(old_frames, &json!({"features": ["resize_notifications", "resize_history"]})),
                there: 9,
                conn: conn.clone(),
                machine: machine.id.clone(),
                params: json!({"terminal": "t1", "resize_notifications": true, "resize_history": 1000}),
                attempt: None,
            },
        );
        let (frames, mut rx) = mpsc::unbounded_channel();
        let features =
            if feature { vec!["resize_notifications", "resize_history"] } else { vec!["resize_notifications"] };
        let line = Line::new(frames, &json!({"features": features}));
        core.lock().lines.insert(machine.id.clone(), LineState::Connected(line.clone())); // this pass's line is current
        let task = tokio::spawn(reattach(core.clone(), machine, line.clone()));
        let request = rx.recv().await.unwrap();
        let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
        assert_eq!(request["params"]["resize_history"], if feature { 1000 } else { 0 }, "{request}");
        let mut result = json!({"channel": 10, "resize_notifications": true});
        if echo {
            result["resize_history"] = 1000.into();
        }
        line.take_reply(json!({"id": request["id"], "result": result}));
        task.await.unwrap();
        if feature && echo {
            assert!(core.lock().relayed.contains_key(&(conn_key(&conn), 7)), "confirmed again: resumed");
        } else {
            assert!(!core.lock().relayed.contains_key(&(conn_key(&conn), 7)));
            assert!(!lock(&line.streams).contains_key(&10));
            assert_eq!(rx.recv().await.unwrap(), wire::encode(10, wire::CLOSE, b""));
            let close = conn.outbox().next_frame().unwrap();
            assert_eq!(close[4], wire::CLOSE, "the viewer attaches again and renegotiates");
        }
        drop(core);
        let _ = std::fs::remove_dir_all(root);
    }
}

/// Review v3 (3), the reviewer's probe: the receiving task may read the reply
/// and the next frames before the reattach task resumes. A downgraded owner's
/// stream must never be connected here at all.
#[tokio::test]
async fn a_resize_history_downgrade_is_refused_before_any_frame_is_forwarded() {
    for (feature, echo) in [(false, false), (true, false), (true, true)] {
        let root = std::path::PathBuf::from(format!(
            "/var/tmp/hc-history-reattach-{}-{}",
            std::process::id(),
            crate::platform::random_hex()
        ));
        let paths = Paths {
            socket: root.join("core.sock"),
            holder_socket: root.join("holder.sock"),
            state: root.clone(),
            config: root.join("config.toml"),
        };
        paths.create_dirs().unwrap();
        let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
        let machine = Machine {
            name: "away".into(),
            target: "away".into(),
            via: false,
            temporary: false,
            session: None,
            id: "remote".into(),
            host: "away".into(),
            version: "test".into(),
        };
        let conn = Arc::new(Conn::default());
        conn.with(|out| out.open(7));
        let (old_frames, _old_rx) = mpsc::unbounded_channel();
        core.lock().relayed.insert(
            (conn_key(&conn), 7),
            Relayed {
                automatic_input: false,
                visibility_updates: false,
                focus_updates: false,
                used_at: None,
                replies: Default::default(),
                focus_by_use: false,
                line: Line::new(old_frames, &json!({"features": ["resize_notifications", "resize_history"]})),
                there: 9,
                conn: conn.clone(),
                machine: machine.id.clone(),
                params: json!({"terminal": "t1", "resize_notifications": true, "resize_history": 1000}),
                attempt: None,
            },
        );
        let (frames, mut rx) = mpsc::unbounded_channel();
        let features =
            if feature { vec!["resize_notifications", "resize_history"] } else { vec!["resize_notifications"] };
        let line = Line::new(frames, &json!({"features": features}));
        core.lock().lines.insert(machine.id.clone(), LineState::Connected(line.clone())); // this pass's line is current
        let task = tokio::spawn(reattach(core.clone(), machine, line.clone()));
        let request = rx.recv().await.unwrap();
        let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
        assert_eq!(request["params"]["resize_history"], if feature { 1000 } else { 0 }, "{request}");
        let mut result = json!({"channel": 10, "resize_notifications": true});
        if echo {
            result["resize_history"] = 1000.into();
        }
        line.take_reply(json!({"id": request["id"], "result": result}));
        // Reproduce the receiving task reading the reply and the next DATA
        // frame before the separate reattach task runs. Current-thread Tokio
        // makes that valid scheduling order deterministic here.
        let mut leaked = Vec::new();
        if let Some((sink, here)) = lock(&line.streams).get(&10) {
            sink.with(|out| {
                out.ordered_resize(*here, &[80, 0, 24, 0]);
                out.data(*here, b"\x18\x1b[H\x1b[2Jold-owner-screen-only");
                while let Some(frame) = out.next_frame() {
                    leaked.push(frame);
                }
            });
        }
        task.await.unwrap();
        if feature && echo {
            assert!(core.lock().relayed.contains_key(&(conn_key(&conn), 7)), "confirmed again: resumed");
        } else {
            assert!(!core.lock().relayed.contains_key(&(conn_key(&conn), 7)));
            assert!(!lock(&line.streams).contains_key(&10));
            assert_eq!(rx.recv().await.unwrap(), wire::encode(10, wire::CLOSE, b""));
            let close = conn.outbox().next_frame().unwrap();
            assert_eq!(close[4], wire::CLOSE, "the viewer attaches again and renegotiates");
        }
        drop(core);
        let _ = std::fs::remove_dir_all(root);
        if !feature || !echo {
            assert!(
                leaked.is_empty(),
                "downgraded history stream delivered a RESIZE and screen-only DATA before closing"
            );
        }
    }
}
// The #88 v3 reviewer's probe: a missing or lower echo is refused before mapping.

#[tokio::test]
async fn reviewer_v3_missing_and_lower_echo_are_rejected_before_mapping() {
    for (feature, echo, accepted) in [
        (true, Value::Null, false),
        (true, json!(0), false),
        (true, json!(999), false),
        (true, json!(1000), true),
        (true, json!(1001), true),
        (true, json!("1000"), false),
        (false, json!(1000), false),
    ] {
        let (frames, mut rx) = mpsc::unbounded_channel();
        let line = Line::new(
            frames,
            &json!({"features":if feature{vec!["resize_notifications","resize_history"]}else{vec!["resize_notifications"]}}),
        );
        let sink = Arc::new(Conn::default());
        sink.with(|out| out.open(7));
        let task = {
            let line = line.clone();
            tokio::spawn(async move {
                line.call_into(
                    "away",
                    "terminal.attach",
                    json!({"resize_history":1000}),
                    Some(((sink, 7), true, false, false, 1000)),
                    Duration::from_secs(2),
                )
                .await
            })
        };
        let request = rx.recv().await.unwrap();
        let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
        line.take_reply(
            json!({"id":request["id"],"result":{"channel":10,"resize_notifications":true,"resize_history":echo}}),
        );
        assert_eq!(lock(&line.streams).contains_key(&10), accepted, "feature={feature} echo={echo}");
        let reply = task.await.unwrap();
        assert_eq!(reply.is_ok(), accepted);
        if !accepted {
            assert_eq!(reply.unwrap_err().message, HISTORY_LOST);
            assert_eq!(rx.recv().await.unwrap(), wire::encode(10, wire::CLOSE, b""));
        }
    }
}

/// INS-2950: when an older core there ignores the reattach credit and the
/// backlog here for a viewer overflows anyway, the stream is closed
/// (`relay_overflow`) and ended there, never continued after a hole.
#[tokio::test]
async fn a_relay_backlog_overflow_closes_the_stream_instead_of_dropping_silently() {
    let root = std::path::PathBuf::from(format!(
        "/var/tmp/hc-relay-overflow-{}-{}",
        std::process::id(),
        crate::platform::random_hex()
    ));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let conn = Arc::new(Conn::default());
    conn.with(|out| out.open(7));
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    core.lock().relayed.insert(
        (conn_key(&conn), 7),
        Relayed {
            automatic_input: false,
            visibility_updates: false,
            focus_updates: false,
            used_at: None,
            replies: Default::default(),
            focus_by_use: false,
            line: line.clone(),
            there: 9,
            conn: conn.clone(),
            machine: "remote".into(),
            params: json!({}),
            attempt: None,
        },
    );
    lock(&line.streams).insert(9, (conn.clone(), 7));
    // The viewer is not reading: an old core there keeps sending.
    let chunk = vec![b'x'; 16 * 1024];
    for _ in 0..(wire::MAX_PENDING / chunk.len()) {
        stream_data(&core, &line, 9, &chunk);
    }
    assert!(core.lock().relayed.contains_key(&(conn_key(&conn), 7)), "4 MiB still fits");
    stream_data(&core, &line, 9, &chunk);
    assert!(!core.lock().relayed.contains_key(&(conn_key(&conn), 7)), "no longer relayed");
    assert!(!lock(&line.streams).contains_key(&9));
    assert_eq!(rx.recv().await.unwrap(), wire::encode(9, wire::CLOSE, b""), "ended there");
    let close = conn.outbox().next_frame().unwrap();
    assert_eq!(close, wire::encode(7, wire::CLOSE, br#"{"reason":"relay_overflow"}"#), "the viewer is told");
    stream_data(&core, &line, 9, b"later");
    assert!(conn.outbox().next_frame().is_none(), "nothing after the hole");
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_reattach_starts_with_the_credit_the_viewer_has_left() {
    let mut out = wire::Outbox::default();
    out.open(7);
    assert_eq!(out.room(7), wire::WINDOW);
    out.data(7, &vec![b'x'; 100]);
    assert_eq!(out.room(7), wire::WINDOW - 100, "queued bytes count");
    while out.next_frame().is_some() {}
    assert_eq!(out.room(7), wire::WINDOW - 100, "sent and not credited back count too");
    out.data(7, &vec![b'x'; wire::WINDOW]);
    assert_eq!(out.room(7), 0);
    out.open(8);
    out.set_credit(8, 10 * wire::WINDOW);
    out.data(8, &vec![b'x'; wire::WINDOW + 1]);
    assert_eq!(out.room(8), 0, "never more than a window");
}

/// INS-2950 review (deterministic): the reattach takes the viewer's room as
/// the new stream's starting credit; CREDIT the viewer returns before the
/// new stream is installed here (also right after the reply maps it, before
/// the reattach resumes) is kept and given to the new stream once, and
/// nothing goes to the dead line. CREDIT after that goes straight there.
#[tokio::test(flavor = "current_thread")]
async fn credit_during_a_reattach_reaches_the_new_stream_exactly_once() {
    let root = std::path::PathBuf::from(format!(
        "/var/tmp/hc-credit-race-{}-{}",
        std::process::id(),
        crate::platform::random_hex()
    ));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine = Machine {
        name: "owner".into(),
        target: "owner".into(),
        via: false,
        temporary: false,
        session: None,
        id: "remote".into(),
        host: "owner".into(),
        version: "test".into(),
    };
    let conn = Arc::new(Conn::default());
    conn.with(|out| {
        out.open(7);
        out.set_credit(7, 0); // the phone holds its whole window
    });
    let (old_frames, mut old_rx) = mpsc::unbounded_channel();
    core.lock().relayed.insert(
        (conn_key(&conn), 7),
        Relayed {
            automatic_input: false,
            visibility_updates: false,
            focus_updates: false,
            used_at: None,
            replies: Default::default(),
            focus_by_use: false,
            line: Line::new(old_frames, &json!({})),
            there: 9,
            conn: conn.clone(),
            machine: machine.id.clone(),
            params: json!({"terminal": "t1"}),
            attempt: None,
        },
    );
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({"features": ["attach_credit"]}));
    core.lock().lines.insert(machine.id.clone(), LineState::Connected(line.clone())); // this pass's line is current
    let task = tokio::spawn(reattach(core.clone(), machine, line.clone()));
    let request = rx.recv().await.unwrap();
    let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
    assert_eq!(request["params"]["credit"], 0);
    let credit = |n: u32| assert!(relay_in(&core, &conn, 7, wire::CREDIT, &n.to_le_bytes()));
    // In flight: the phone consumed and credited everything it held.
    credit(wire::WINDOW as u32);
    assert_eq!(conn.outbox().room(7), wire::WINDOW);
    // The reply maps the stream; before the reattach resumes, more CREDIT.
    line.take_reply(json!({"id": request["id"], "result": {"channel": 10}}));
    credit(100);
    assert!(old_rx.try_recv().is_err(), "nothing to the dead line");
    task.await.unwrap();
    // Installed: later CREDIT goes straight to the new stream.
    credit(5);
    let mut granted = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        assert_eq!(frame[4], wire::CREDIT);
        assert_eq!(frame[..4], 10u32.to_le_bytes(), "to the new stream there");
        granted.push(u32::from_le_bytes(frame[wire::HEADER..].try_into().unwrap()) as usize);
    }
    assert_eq!(granted, [wire::WINDOW + 100, 5], "each credited byte once, to the new stream");
    assert!(old_rx.try_recv().is_err());
    assert!(core.lock().relayed[&(conn_key(&conn), 7)].attempt.is_none());
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

/// A core with one relayed viewer (channel 7 here, 9 on a dead old line)
/// that holds its whole window.
fn held_viewer(name: &str) -> (Arc<Core>, Machine, Arc<Conn>, std::path::PathBuf) {
    let root = std::path::PathBuf::from(format!(
        "/var/tmp/hc-{name}-{}-{}",
        std::process::id(),
        crate::platform::random_hex()
    ));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine = Machine {
        name: "owner".into(),
        target: "owner".into(),
        via: false,
        temporary: false,
        session: None,
        id: "remote".into(),
        host: "owner".into(),
        version: "test".into(),
    };
    let conn = Arc::new(Conn::default());
    conn.with(|out| {
        out.open(7);
        out.set_credit(7, 0);
    });
    let (old_frames, _old_rx) = mpsc::unbounded_channel();
    core.lock().relayed.insert(
        (conn_key(&conn), 7),
        Relayed {
            automatic_input: false,
            visibility_updates: false,
            focus_updates: false,
            used_at: None,
            replies: Default::default(),
            focus_by_use: false,
            line: Line::new(old_frames, &json!({})),
            there: 9,
            conn: conn.clone(),
            machine: machine.id.clone(),
            params: json!({"terminal": "t1"}),
            attempt: None,
        },
    );
    (core, machine, conn, root)
}

/// Polls a future once (the test drives the interleaving itself).
fn poll_once<F: std::future::Future>(f: std::pin::Pin<&mut F>) -> std::task::Poll<F::Output> {
    f.poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
}

/// Frames a line sent, as (kind, channel, payload).
fn sent(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> Vec<(u8, u32, Vec<u8>)> {
    std::iter::from_fn(|| rx.try_recv().ok())
        .map(|f| (f[4], u32::from_le_bytes(f[..4].try_into().unwrap()), f[wire::HEADER..].to_vec()))
        .collect()
}

fn attach_request(rx: &mut mpsc::UnboundedReceiver<Vec<u8>>) -> Value {
    let frame = rx.try_recv().expect("an attach request");
    serde_json::from_slice(&frame[wire::HEADER..]).unwrap()
}

/// INS-2950 v3: attempt A got its reply (mapped there) but its line dropped
/// before A installed; attempt B began on the next line. CREDIT returned
/// during B belongs to B: A, now stale, neither takes it nor installs, and
/// only closes its own stream there.
#[tokio::test(flavor = "current_thread")]
async fn an_older_reattach_finishing_late_leaves_the_newer_one_its_credit() {
    let (core, machine, conn, root) = held_viewer("overlap");
    let (a_frames, mut a_rx) = mpsc::unbounded_channel();
    let a = Line::new(a_frames, &json!({"features": ["attach_credit"]}));
    core.lock().lines.insert(machine.id.clone(), LineState::Connected(a.clone())); // this pass's line is current
    let mut attempt_a = Box::pin(reattach(core.clone(), machine.clone(), a.clone()));
    assert!(poll_once(attempt_a.as_mut()).is_pending());
    let request = attach_request(&mut a_rx);
    assert_eq!(request["params"]["credit"], 0);
    a.take_reply(json!({"id": request["id"], "result": {"channel": 10}}));
    lock(&a.streams).clear(); // A's line drops
    let (b_frames, mut b_rx) = mpsc::unbounded_channel();
    let b = Line::new(b_frames, &json!({"features": ["attach_credit"]}));
    core.lock().lines.insert(machine.id.clone(), LineState::Connected(b.clone())); // this pass's line is current
    let mut attempt_b = Box::pin(reattach(core.clone(), machine, b.clone()));
    assert!(poll_once(attempt_b.as_mut()).is_pending());
    let request = attach_request(&mut b_rx);
    assert_eq!(request["params"]["credit"], 0);
    // The phone consumes and credits all it held while B waits.
    assert!(relay_in(&core, &conn, 7, wire::CREDIT, &(wire::WINDOW as u32).to_le_bytes()));
    assert!(poll_once(attempt_a.as_mut()).is_ready());
    assert_eq!(sent(&mut a_rx), [(wire::CLOSE, 10, vec![])], "stale A only closes its own stream");
    {
        let st = core.lock();
        let entry = &st.relayed[&(conn_key(&conn), 7)];
        assert_eq!(entry.there, 9, "A installed nothing");
        assert_eq!(entry.attempt.as_ref().map(|a| a.kept), Some(wire::WINDOW as u64), "B's bucket untouched");
    }
    b.take_reply(json!({"id": request["id"], "result": {"channel": 11}}));
    assert!(poll_once(attempt_b.as_mut()).is_ready());
    let credit = (wire::WINDOW as u32).to_le_bytes().to_vec();
    assert_eq!(sent(&mut b_rx), [(wire::CREDIT, 11, credit)], "B gets every credited byte, once");
    let st = core.lock();
    let entry = &st.relayed[&(conn_key(&conn), 7)];
    assert!(Arc::ptr_eq(&entry.line, &b) && entry.there == 11 && entry.attempt.is_none());
    drop(st);
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

/// INS-2950 v3: attempt A fails (its line drops before the reply) only after
/// a newer attempt B has installed. Stale A must not remove B's stream nor
/// close the viewer.
#[tokio::test(flavor = "current_thread")]
async fn an_older_reattach_failing_late_does_not_end_the_newer_stream() {
    let (core, machine, conn, root) = held_viewer("stale-fail");
    let (a_frames, mut a_rx) = mpsc::unbounded_channel();
    let a = Line::new(a_frames, &json!({}));
    core.lock().lines.insert(machine.id.clone(), LineState::Connected(a.clone())); // this pass's line is current
    let mut attempt_a = Box::pin(reattach(core.clone(), machine.clone(), a.clone()));
    assert!(poll_once(attempt_a.as_mut()).is_pending());
    attach_request(&mut a_rx);
    let (b_frames, mut b_rx) = mpsc::unbounded_channel();
    let b = Line::new(b_frames, &json!({}));
    core.lock().lines.insert(machine.id.clone(), LineState::Connected(b.clone())); // this pass's line is current
    let mut attempt_b = Box::pin(reattach(core.clone(), machine, b.clone()));
    assert!(poll_once(attempt_b.as_mut()).is_pending());
    let request = attach_request(&mut b_rx);
    b.take_reply(json!({"id": request["id"], "result": {"channel": 11}}));
    assert!(poll_once(attempt_b.as_mut()).is_ready());
    lock(&a.pending).clear(); // A's line drops: its call fails
    assert!(poll_once(attempt_a.as_mut()).is_ready());
    let st = core.lock();
    let entry = st.relayed.get(&(conn_key(&conn), 7)).expect("B's stream stays relayed");
    assert!(Arc::ptr_eq(&entry.line, &b) && entry.there == 11);
    drop(st);
    assert!(lock(&b.streams).contains_key(&11), "B's mapping stays");
    assert!(sent(&mut b_rx).is_empty(), "no CLOSE to B's stream");
    assert!(conn.outbox().next_frame().is_none(), "the viewer is not closed");
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

/// `held_viewer` plus a second relayed viewer (channel 8 here, 19 on the
/// same dead old line), also holding its whole window.
fn two_held_viewers(name: &str) -> (Arc<Core>, Machine, Arc<Conn>, std::path::PathBuf) {
    let (core, machine, conn, root) = held_viewer(name);
    let mut second = core.lock().relayed[&(conn_key(&conn), 7)].clone();
    second.params = json!({"terminal": "t2"});
    second.there = 19;
    conn.with(|out| {
        out.open(8);
        out.set_credit(8, 0);
    });
    core.lock().relayed.insert((conn_key(&conn), 8), second);
    (core, machine, conn, root)
}

/// INS-2950 v4: an older reconnect pass A, paused on its first viewer,
/// resumes only after the newer pass B installed both viewers. A must not
/// begin a fresh attempt for the second viewer on its dead line (it would
/// time out as "current" and end B's live stream).
#[tokio::test(flavor = "current_thread")]
async fn an_older_reconnect_pass_does_not_begin_again_after_a_newer_one() {
    let (core, machine, conn, root) = two_held_viewers("old-pass");
    let (a_frames, mut a_rx) = mpsc::unbounded_channel();
    let a = Line::new(a_frames, &json!({}));
    core.lock().lines.insert(machine.id.clone(), LineState::Connected(a.clone()));
    let mut pass_a = Box::pin(reattach(core.clone(), machine.clone(), a.clone()));
    assert!(poll_once(pass_a.as_mut()).is_pending());
    attach_request(&mut a_rx);
    lock(&a.pending).clear(); // A's line drops; its first attempt fails when A resumes
    let (b_frames, mut b_rx) = mpsc::unbounded_channel();
    let b = Line::new(b_frames, &json!({}));
    core.lock().lines.insert(machine.id.clone(), LineState::Connected(b.clone()));
    let mut pass_b = Box::pin(reattach(core.clone(), machine, b.clone()));
    for there in [30, 31] {
        assert!(poll_once(pass_b.as_mut()).is_pending());
        let request = attach_request(&mut b_rx);
        b.take_reply(json!({"id": request["id"], "result": {"channel": there}}));
    }
    assert!(poll_once(pass_b.as_mut()).is_ready());
    assert!(poll_once(pass_a.as_mut()).is_ready(), "A stops: a newer line took over");
    assert!(a_rx.try_recv().is_err(), "A began nothing more on its dead line");
    let st = core.lock();
    for here in [7, 8] {
        let entry = &st.relayed[&(conn_key(&conn), here)];
        assert!(Arc::ptr_eq(&entry.line, &b) && entry.attempt.is_none(), "viewer {here} stays on B");
    }
    drop(st);
    assert!(conn.outbox().next_frame().is_none(), "no viewer closed");
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

/// INS-2950 v4: what a later viewer in the same pass asks for is read when
/// its attempt begins, not when the pass started: a RESIZE and FACTS it
/// sent meanwhile go into its attach.
#[tokio::test(flavor = "current_thread")]
async fn a_later_viewer_in_a_pass_attaches_with_its_current_state() {
    let (core, machine, conn, root) = two_held_viewers("fresh-begin");
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    core.lock().lines.insert(machine.id.clone(), LineState::Connected(line.clone()));
    let mut pass = Box::pin(reattach(core.clone(), machine, line.clone()));
    assert!(poll_once(pass.as_mut()).is_pending());
    let first = attach_request(&mut rx);
    let later = if first["params"]["terminal"] == "t1" { 8 } else { 7 };
    assert!(relay_in(&core, &conn, later, wire::RESIZE, &[90, 0, 30, 0]));
    assert!(relay_in(&core, &conn, later, wire::FACTS, br##"{"fg":"#112233"}"##));
    line.take_reply(json!({"id": first["id"], "result": {"channel": 40}}));
    assert!(poll_once(pass.as_mut()).is_pending());
    let second = attach_request(&mut rx);
    assert_eq!((second["params"]["cols"].clone(), second["params"]["rows"].clone()), (json!(90), json!(30)));
    assert_eq!(second["params"]["display"]["fg"], "#112233");
    line.take_reply(json!({"id": second["id"], "result": {"channel": 41}}));
    assert!(poll_once(pass.as_mut()).is_ready());
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

/// Item 71 (#94 review): a relay keeps when a viewer that reports focus was
/// last used, focused or not, and only a person's use moves it: an empty
/// frame or a reply cut across frames does not. A reattach then carries
/// that age even for an unfocused viewer, so the owner keeps its history.
#[tokio::test]
async fn a_relay_keeps_the_last_use_of_a_viewer_focused_or_not() {
    let root = std::path::PathBuf::from(format!(
        "/var/tmp/hc-use-relay-{}-{}",
        std::process::id(),
        crate::platform::random_hex()
    ));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let (frames, _rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({"features": ["viewer_visibility", "viewer_focus", "focus_by_use"]}));
    let window = Arc::new(Conn::default());
    window.with(|out| out.open(7));
    core.lock().relayed.insert(
        (conn_key(&window), 7),
        Relayed {
            automatic_input: false,
            visibility_updates: true,
            focus_updates: true,
            used_at: None,
            replies: Default::default(),
            focus_by_use: false,
            line,
            there: 9,
            conn: window.clone(),
            machine: "macro".into(),
            params: json!({"visible": true, "visibility_updates": true, "focus_updates": true, "focused": false}),
            attempt: None,
        },
    );
    let entry = || core.lock().relayed[&(conn_key(&window), 7)].clone();
    assert!(viewers::idle_ms(&entry()).is_none(), "never focused, never used: nothing to say");
    assert!(relay_in(&core, &window, 7, wire::WINDOW_FOCUS, &[1]));
    let used = entry().used_at.expect("WINDOW_FOCUS(1) is use");
    std::thread::sleep(Duration::from_millis(20));
    for piece in [&b""[..], b"\x1b[24;", b"80R", b"\x1b", b"[?62c"] {
        assert!(relay_in(&core, &window, 7, wire::DATA, piece));
    }
    assert_eq!(entry().used_at, Some(used), "empty frames and replies in pieces are not use");
    assert!(relay_in(&core, &window, 7, wire::WINDOW_FOCUS, &[0]));
    let idle = viewers::idle_ms(&entry()).expect("unfocused, but used: its age goes along");
    assert!(idle >= 20, "{idle}");
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

// t218's probes from the #94 review (hm-pr94-artifacts), kept as regression tests.
#[test]
fn reviewer94_relay_empty_data_must_not_refresh_last_use() {
    let (core, _machine, conn, root) = held_viewer("review94-empty");
    let key = (conn_key(&conn), 7);
    let before = std::time::Instant::now() - Duration::from_secs(3600);
    {
        let mut st = core.lock();
        let r = st.relayed.get_mut(&key).unwrap();
        r.focus_updates = true;
        r.params["focused"] = true.into();
        r.used_at = Some(before);
    }
    assert!(relay_in(&core, &conn, 7, wire::DATA, b""));
    let got = core.lock().relayed[&key].used_at;
    drop(core);
    let _ = std::fs::remove_dir_all(root);
    assert_eq!(got, Some(before), "empty DATA must remain idle on reattach as on owner");
}

#[test]
fn reviewer94_relay_unfocused_last_use_must_survive_reattach() {
    let (core, _machine, conn, root) = held_viewer("review94-unfocus");
    let key = (conn_key(&conn), 7);
    {
        let mut st = core.lock();
        let r = st.relayed.get_mut(&key).unwrap();
        r.focus_updates = true;
        r.params["focused"] = false.into();
        r.used_at = Some(std::time::Instant::now() - Duration::from_secs(3600));
    }
    let r = core.lock().relayed[&key].clone();
    let got = viewers::idle_ms(&r);
    drop(core);
    let _ = std::fs::remove_dir_all(root);
    assert!(got.is_some(), "last-focused use is valid selection history while focused=false; reconnect must carry it");
}

/// Item 71 (#94 review, live repros): a relay keeps a viewer's input
/// recency also when it sends no focus (an old phone), for its reattach; a
/// reply cut across frames is not use there; and every frame goes on with
/// its own kind and bytes (sizing judgement only).
#[tokio::test]
async fn a_relay_carries_a_legacy_viewers_input_and_never_rewrites_a_frame() {
    let (core, _machine, conn, root) = held_viewer("use-legacy");
    let key = (conn_key(&conn), 7);
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({"features": ["automatic_input"]}));
    {
        let mut st = core.lock();
        let r = st.relayed.get_mut(&key).unwrap();
        r.automatic_input = true;
        r.focus_updates = false;
        r.used_at = None;
        r.line = line;
        r.there = 9;
    }
    assert!(viewers::idle_ms(&core.lock().relayed[&key]).is_none(), "never typed into: nothing to say");
    assert!(relay_in(&core, &conn, 7, wire::DATA, b"x"));
    let used = core.lock().relayed[&key].used_at.expect("typing is use");
    std::thread::sleep(Duration::from_millis(20));
    // A cursor report cut in two, the line dropping between its halves: the
    // relay's parse carries over, so the tail is not use here either.
    assert!(relay_in(&core, &conn, 7, wire::DATA, b"\x1b[24;"));
    assert!(relay_in(&core, &conn, 7, wire::DATA, b"80R"));
    let r = core.lock().relayed[&key].clone();
    assert_eq!(r.used_at, Some(used), "a reply is not use");
    assert!(viewers::idle_ms(&r).unwrap() >= 20, "the legacy viewer's input age goes along on a reattach");
    // A lone Escape a person pressed goes on as DATA: the kind is never
    // rewritten, so the owner's agent-input gate sees a person.
    assert!(relay_in(&core, &conn, 7, wire::DATA, b"\x1b"));
    let mut forwarded = Vec::new();
    while let Ok(frame) = rx.try_recv() {
        forwarded.push(frame);
    }
    assert_eq!(
        forwarded,
        [
            wire::encode(9, wire::DATA, b"x"),
            wire::encode(9, wire::DATA, b"\x1b[24;"),
            wire::encode(9, wire::DATA, b"80R"),
            wire::encode(9, wire::DATA, b"\x1b"),
        ]
    );
    drop(core);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn a_guard_or_bound_is_refused_for_a_core_that_would_ignore_it() {
    use serde_json::json;
    let (frames, mut sent) = tokio::sync::mpsc::unbounded_channel();
    let features = |list: &[&str]| Line::new(frames.clone(), &json!({"features": list}));
    let old = features(&["framed", "attach"]);
    let new = features(&["send-guarded", "read-bounded"]);
    let guarded = json!({"terminal": "t1", "text": "x", "incarnation": "i", "if_ctx": "c", "within_ms": 5000});
    for (method, params) in [
        ("terminal.send", guarded.clone()),
        ("terminal.send", json!({"terminal": "t1", "text": "x", "incarnation": "i"})),
        ("terminal.send", json!({"terminal": "t1", "text": "x", "within_ms": 5})),
        ("terminal.read", json!({"terminal": "t1", "max_bytes": 1000})),
        ("terminal.read", json!({"terminal": "t1", "stitch": false})),
    ] {
        let e = old.require("far", method, &params).expect_err(&format!("{method} {params}"));
        assert_eq!(e.kind, "needs_update");
        new.require("far", method, &params).expect("a core that has the feature gets it");
    }
    // Calls that ask for nothing new go to any core.
    for (method, params) in [
        ("terminal.send", json!({"terminal": "t1", "text": "x"})),
        ("terminal.send", json!({"terminal": "t1", "text": "x", "incarnation": null})),
        ("terminal.read", json!({"terminal": "t1", "lines": 5})),
        ("terminal.read", json!({"terminal": "t1", "stitch": true})),
        ("terminal.get", json!({"terminal": "t1", "max_bytes": 5})),
    ] {
        old.require("far", method, &params).expect("an old call is not refused");
    }
    // `read-bounded` is not `send-guarded`.
    let only_send = features(&["send-guarded"]);
    assert!(only_send.require("far", "terminal.read", &json!({"max_bytes": 1})).is_err());
    assert!(sent.try_recv().is_err(), "nothing was forwarded");
}
