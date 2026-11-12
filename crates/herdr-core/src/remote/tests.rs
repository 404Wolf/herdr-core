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
            focus_updates: true,
            line: Line::new(frames, &json!({"features": ["viewer_visibility", "viewer_focus"]})),
            there: 9,
            conn: conn.clone(),
            machine: "remote".into(),
            params: json!({"visible": true, "visibility_updates": true, "focus_updates": true, "focused": false}),
        },
    );

    let mut input = [
        wire::encode(7, wire::DATA, b"before resize"),
        wire::encode(7, wire::RESIZE, &[218, 0, 74, 0]),
        wire::encode(7, wire::FOCUS, &[1]),
        wire::encode(7, wire::FOCUS, &[0]),
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
            wire::encode(9, wire::FOCUS, &[1]),
            wire::encode(9, wire::FOCUS, &[0]),
            wire::encode(9, wire::VISIBILITY, &[0]),
            wire::encode(9, wire::DATA, b"after hidden"),
        ]
    );
    assert_eq!(core.lock().relayed[&(conn_key(&conn), 7)].params["visible"], false);
    assert_eq!(core.lock().relayed[&(conn_key(&conn), 7)].params["focused"], false);
    assert!(relay_in(&core, &conn, 7, wire::FOCUS, &[2]));
    assert!(rx.try_recv().is_err(), "invalid focus never reaches the remote core");
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
                line: Line::new(old_frames, &json!({"features": ["automatic_input"]})),
                there: 9,
                conn: conn.clone(),
                machine: machine.id.clone(),
                params: json!({"terminal": "t1"}),
            },
        );
        assert!(relay_in(&core, &conn, 7, wire::AUTOMATIC_INPUT, b"\x1b"));
        let frame = old_rx.recv().await.unwrap();
        assert_eq!(frame, wire::encode(9, wire::AUTOMATIC_INPUT, b"\x1b"));
        let (frames, mut rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": if feature { vec!["automatic_input"] } else { vec![] }}));
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
    for stalled in [false, true] {
        // The immediate relay is capable, but its deeper path has downgraded.
        let (frames, mut rx) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": ["resize_notifications"]}));
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
                    Some(((sink, 7), true, false, false)),
                    Duration::from_secs(1),
                )
                .await
        });
        let request = rx.recv().await.unwrap();
        let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
        line.take_reply(json!({"id": request["id"], "result": {"channel": 9, "resize_notifications": false}}));
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
            line: old_line,
            there: 9,
            conn: conn.clone(),
            machine: machine.id.clone(),
            params: json!({"terminal": "t1"}),
        },
    );
    drop(old_rx); // The viewer survives its old line.
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
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
fn viewer_focus_requires_both_hop_capability_and_attach_confirmation() {
    for supported in [false, true] {
        let (frames, _) = mpsc::unbounded_channel();
        let line = Line::new(frames, &json!({"features": if supported { vec!["viewer_focus"] } else { vec![] }}));
        for requested in [false, true] {
            let mut params = json!({"focus_updates": requested, "focused": true});
            viewers::negotiate_focus(&line, &mut params);
            assert_eq!(params["focus_updates"], requested && supported);
            assert_eq!(params["focused"], requested && supported);
            for reply in [json!({}), json!({"focus_updates": false}), json!({"focus_updates": true})] {
                assert_eq!(
                    viewers::focus_updates(&line, requested, &reply),
                    requested && supported && reply["focus_updates"] == true
                );
            }
        }
    }
}

#[tokio::test]
async fn focus_downgrade_is_rejected_before_any_fallback_paint_is_mapped() {
    for supported in [false, true] {
        for confirmed in [false, true] {
            let (frames, mut rx) = mpsc::unbounded_channel();
            let line = Line::new(frames, &json!({"features": if supported { vec!["viewer_focus"] } else { vec![] }}));
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
                        Some(((sink, 7), false, false, true)),
                        Duration::from_secs(1),
                    )
                    .await
            });
            let request = rx.recv().await.unwrap();
            let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
            line.take_reply(json!({"id": request["id"], "result": {"channel": 9, "focus_updates": confirmed}}));
            let result = task.await.unwrap();
            assert_eq!(result.is_ok(), supported && confirmed);
            assert_eq!(lock(&line.streams).contains_key(&9), supported && confirmed);
            if !(supported && confirmed) {
                assert_eq!(rx.recv().await.unwrap(), wire::encode(9, wire::CLOSE, b""));
                assert_eq!(conn.outbox().queued(7), 0, "downgrade never maps or queues a legacy paint");
            }
        }
    }
}

#[tokio::test]
async fn reattach_keeps_focus_changes_received_while_the_attach_reply_is_pending() {
    let root = std::path::PathBuf::from(format!("/var/tmp/hc-focus-reattach-{}", crate::platform::random_hex()));
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
    let (old_frames, _old_rx) = mpsc::unbounded_channel();
    core.lock().relayed.insert((conn_key(&conn), 7), Relayed {
        automatic_input: false, visibility_updates: true, focus_updates: true,
        line: Line::new(old_frames, &json!({"features":["viewer_focus","viewer_visibility"]})),
        there: 9, conn: conn.clone(), machine: machine.id.clone(),
        params: json!({"terminal":"t1","focus_updates":true,"focused":true,"visibility_updates":true,"visible":true}),
    });
    let (frames, mut rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({"features":["viewer_focus","viewer_visibility"]}));
    let task = tokio::spawn(reattach(core.clone(), machine, line.clone()));
    let request = rx.recv().await.unwrap();
    let request: Value = serde_json::from_slice(&request[wire::HEADER..]).unwrap();
    assert_eq!(request["params"]["focused"], true);
    relay_in(&core, &conn, 7, wire::FOCUS, &[0]);
    relay_in(&core, &conn, 7, wire::VISIBILITY, &[0]);
    line.take_reply(json!({"id":request["id"],"result":{"channel":10,"focus_updates":true,"visibility_updates":true}}));
    task.await.unwrap();
    let params = core.lock().relayed[&(conn_key(&conn), 7)].params.clone();
    assert_eq!(params["focused"], false);
    assert_eq!(params["visible"], false);
    assert_eq!(rx.recv().await.unwrap(), wire::encode(10, wire::VISIBILITY, &[0]));
    assert_eq!(rx.recv().await.unwrap(), wire::encode(10, wire::FOCUS, &[0]));
    assert!(rx.try_recv().is_err(), "cached geometry is never replayed as a fresh measurement");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}
