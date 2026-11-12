use super::tests::{core_with_screen, desktop_conn};
use super::*;

fn attach(core: &Core, id: u32, conn: &Arc<Conn>, size: (u16, u16), focused: bool) -> u32 {
    let channel = core
        .attach(
            conn,
            id,
            AttachRequest {
                observe: false,
                resize_notifications: true,
                history: 0,
                size,
                display: Display::default(),
                resumed: false,
                visible: true,
                visibility_updates: true,
                focus_updates: true,
                focused,
                desktop_group: Some(("peer".into(), "desktop".into())),
            },
        )
        .unwrap();
    core.viewer_resize(conn, channel, size.0, size.1);
    channel
}

fn check(core: &Core, id: u32, grid: (u16, u16), leader: Option<(&Arc<Conn>, u32)>) {
    let snapshot = core.viewer_list(id, "t1".into());
    assert_eq!(snapshot.current_grid, GridSize { cols: grid.0, rows: grid.1 });
    assert_eq!(snapshot.leader, leader.map(|(conn, channel)| ViewerIdentity { connection_id: conn.id, channel }));
    assert_eq!(snapshot.selected_grid, leader.map(|_| snapshot.current_grid));
    assert_eq!(snapshot.viewers.iter().filter(|v| v.contributes_to_grid).count(), usize::from(leader.is_some()));
}

#[test]
fn focus_switches_one_grid_input_reclaims_and_blur_is_sticky() {
    let (core, id, root) = core_with_screen(282, 101);
    let peer = desktop_conn();
    let large = attach(&core, id, &peer, (282, 101), true);
    let old = attach(&core, id, &peer, (112, 42), false);
    check(&core, id, (282, 101), Some((&peer, large)));
    core.viewer_focus(&peer, old, true);
    check(&core, id, (112, 42), Some((&peer, old)));
    core.viewer_resize(&peer, old, 120, 50);
    check(&core, id, (120, 50), Some((&peer, old)));
    core.viewer_input(&peer, large, b"typed", false);
    check(&core, id, (282, 101), Some((&peer, large)));
    core.viewer_input(&peer, old, b"reply", true);
    check(&core, id, (282, 101), Some((&peer, large)));
    core.viewer_focus(&peer, large, false);
    core.viewer_focus(&peer, old, false);
    core.viewer_resize(&peer, old, 40, 10);
    core.viewer_resize(&peer, large, 70, 20);
    check(&core, id, (282, 101), Some((&peer, large)));
    core.viewer_visibility(&peer, large, false);
    core.viewer_resize(&peer, large, 60, 15);
    check(&core, id, (282, 101), Some((&peer, large)));
    core.viewer_focus(&peer, old, true);
    check(&core, id, (40, 10), Some((&peer, old)));
    core.detach(&peer, Some(old));
    check(&core, id, (40, 10), None);
    core.viewer_visibility(&peer, large, true);
    core.viewer_resize(&peer, large, 282, 101);
    check(&core, id, (40, 10), None);
    core.viewer_focus(&peer, large, true);
    check(&core, id, (282, 101), Some((&peer, large)));
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn no_focus_holds_and_legacy_mobile_and_unfocused_relays_cannot_take_over() {
    let (core, id, root) = core_with_screen(282, 101);
    let app = desktop_conn();
    let main = attach(&core, id, &app, (282, 101), false);
    check(&core, id, (282, 101), None);
    let legacy = desktop_conn();
    let phone = Arc::new(Conn::default());
    let relay = Arc::new(Conn::default());
    let other = attach(&core, id, &relay, (112, 42), false);
    let legacy_channel = attach(&core, id, &legacy, (80, 24), false);
    {
        let mut st = core.lock();
        let (_, viewer) = Core::viewer(&mut st, &legacy, legacy_channel).unwrap();
        viewer.focus_updates = false;
    }
    let mobile = attach(&core, id, &phone, (40, 10), false);
    {
        let mut st = core.lock();
        let (_, viewer) = Core::viewer(&mut st, &phone, mobile).unwrap();
        viewer.desktop_group = None;
        viewer.focus_updates = false;
    }
    core.viewer_focus(&legacy, legacy_channel, true);
    core.viewer_input(&phone, mobile, b"phone", false);
    check(&core, id, (282, 101), None);
    core.viewer_focus(&app, main, true);
    core.viewer_resize(&legacy, legacy_channel, 50, 20);
    core.viewer_resize(&relay, other, 100, 30);
    core.viewer_input(&phone, mobile, b"phone", false);
    check(&core, id, (282, 101), Some((&app, main)));
    core.viewer_focus(&relay, other, true);
    check(&core, id, (100, 30), Some((&relay, other)));
    core.viewer_focus(&relay, other, false);
    check(&core, id, (100, 30), Some((&relay, other)));
    core.detach(&relay, None);
    check(&core, id, (100, 30), None);
    core.detach(&app, None);
    core.viewer_focus(&legacy, legacy_channel, true);
    check(&core, id, (50, 20), Some((&legacy, legacy_channel)));
    core.detach(&legacy, None);
    check(&core, id, (40, 10), Some((&phone, mobile)));
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn resumed_focus_waits_for_positive_measurement_and_observers_cannot_claim() {
    let (core, id, root) = core_with_screen(103, 74);
    let conn = desktop_conn();
    let channel = core
        .attach(
            &conn,
            id,
            AttachRequest {
                observe: false,
                resize_notifications: true,
                history: 0,
                size: (246, 186),
                display: Display::default(),
                resumed: true,
                visible: true,
                visibility_updates: true,
                focus_updates: true,
                focused: true,
                desktop_group: Some(("peer".into(), "desktop".into())),
            },
        )
        .unwrap();
    check(&core, id, (103, 74), None);
    core.viewer_resize(&conn, channel, 0, 0);
    check(&core, id, (103, 74), None);
    core.viewer_input(&conn, channel, b"typed", false);
    check(&core, id, (103, 74), None);
    core.viewer_resize(&conn, channel, 246, 186);
    check(&core, id, (246, 186), Some((&conn, channel)));
    let observer = attach(&core, id, &conn, (40, 10), false);
    {
        let mut st = core.lock();
        Core::viewer(&mut st, &conn, observer).unwrap().1.observe = true;
    }
    core.viewer_focus(&conn, observer, true);
    core.viewer_input(&conn, observer, b"ignored", false);
    check(&core, id, (246, 186), Some((&conn, channel)));
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn attach_confirms_focus_only_for_local_or_trusted_relay_desktops() {
    let (core, id, root) = core_with_screen(103, 74);
    let phone = Arc::new(Conn::default());
    let app = desktop_conn();
    let relay = Arc::new(Conn::default());
    crate::remote::peer_joined(
        &core,
        &relay,
        &serde_json::json!({"core":{"id":"remote-id","name":"remote"},"calls_you":"test"}),
    );
    for (conn, expected, size) in [(&phone, false, (40, 10)), (&app, true, (282, 101)), (&relay, true, (246, 186))] {
        let result = crate::api::dispatch(
            &crate::api::Call { core: &core, conn: Some(conn), plain: None },
            "terminal.attach",
            serde_json::json!({"terminal":crate::terminals::address(id),"cols":size.0,"rows":size.1,
                "focus_updates":true,"focused":true,"visibility_updates":true,"desktop_group":"reported"}),
        )
        .await
        .unwrap();
        assert_eq!(result["focus_updates"], expected);
        let channel = result["channel"].as_u64().unwrap() as u32;
        core.viewer_resize(conn, channel, size.0, size.1);
        let snapshot = core.viewer_list(id, "t1".into());
        let info = snapshot
            .viewers
            .iter()
            .find(|v| v.identity == (ViewerIdentity { connection_id: conn.id, channel }))
            .unwrap();
        assert_eq!(info.focus_updates, expected);
        assert_eq!(info.focused, expected);
        if expected {
            check(&core, id, size, Some((conn, channel)));
        } else {
            assert!(info.desktop_group.is_none(), "ordinary clients cannot claim a desktop");
        }
    }
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}
