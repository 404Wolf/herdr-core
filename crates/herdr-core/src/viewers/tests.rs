use super::*;
use crate::wire;
use std::cell::Cell;

thread_local! {
    pub(super) static PAINTS: Cell<usize> = const { Cell::new(0) };
}

fn desktop_conn() -> Arc<Conn> {
    let mut conn = Conn::default();
    conn.local_desktop = true;
    Arc::new(conn)
}

fn viewer(conn: &Arc<Conn>, size: (u16, u16), active_at: Option<Instant>) -> Viewer {
    Viewer {
        conn: conn.clone(),
        channel: 1,
        observe: false,
        resize_notifications: false,
        repaint_pending: false,
        size,
        active_at,
        visible: true,
        sized: true,
        awaiting_size: false,
        visibility_updates: true,
        origin: if conn.local_desktop { ViewerOrigin::LocalDesktop } else { ViewerOrigin::LocalClient },
        peer: None,
        desktop_group: conn.local_desktop.then(|| ("local".into(), "desktop".into())),
        display: Display::default(),
        recovering: false,
    }
}

#[test]
fn desktop_windows_use_the_selected_leader_size_and_display() {
    let now = Instant::now();
    let a = desktop_conn();
    let b = desktop_conn();
    let mut vs = vec![viewer(&a, (100, 20), Some(now)), viewer(&b, (80, 30), Some(now))];
    vs[0].display.bg = Some("#ffffff".into());
    vs[1].display.bg = Some("#eeeeee".into());
    vs[0].active_at = Some(now + std::time::Duration::from_secs(1));
    let (leader, size) = viewer_size(&vs, true);
    assert_eq!(size, Some((100, 20)));
    assert_eq!(leader.unwrap().display.bg.as_deref(), Some("#ffffff"));
    vs[1].active_at = Some(now + std::time::Duration::from_secs(2));
    assert_eq!(viewer_size(&vs, true).1, Some((80, 30)));
    assert_eq!(viewer_size(&vs, false).1, Some((80, 30)), "latest policy remains available");
    let phone = Arc::new(Conn::default());
    let mut mobile = viewer(&phone, (40, 10), Some(now + std::time::Duration::from_secs(3)));
    mobile.display.bg = Some("#000000".into());
    vs.push(mobile);
    let (leader, size) = viewer_size(&vs, true);
    assert_eq!(size, Some((80, 30)), "phone activity cannot override visible desktop sizing");
    assert_eq!(leader.unwrap().display.bg.as_deref(), Some("#eeeeee"), "display facts follow selected desktop leader");
    vs.pop();
    assert_eq!(viewer_size(&vs, true).1, Some((80, 30)));
    vs.pop();
    assert_eq!(viewer_size(&vs, true).1, Some((100, 20)), "detach releases the constraint");
}

#[test]
fn forwarded_desktops_are_scoped_by_both_line_and_group() {
    let now = Instant::now();
    let conn = Arc::new(Conn::default());
    let mut vs = vec![viewer(&conn, (100, 20), Some(now)), viewer(&conn, (80, 30), Some(now))];
    vs[0].desktop_group = Some(("line-a".into(), "desktop".into()));
    vs[1].desktop_group = Some(("line-b".into(), "desktop".into()));
    assert_eq!(viewer_size(&vs, true).1, Some((80, 30)), "another source line is another group");
    vs[1].desktop_group = Some(("line-a".into(), "another-desktop".into()));
    assert_eq!(viewer_size(&vs, true).1, Some((80, 30)), "one line can forward different desktops");
    vs[1].desktop_group = vs[0].desktop_group.clone();
    assert_eq!(viewer_size(&vs, true).1, Some((80, 30)), "same-group followers do not constrain the selected offer");
}

#[test]
fn selected_peer_channel_offer_is_the_grid_despite_old_smaller_channels() {
    let now = Instant::now();
    let peer = Arc::new(Conn::default());
    let mut vs: Vec<_> = [(218, 78), (218, 81), (218, 168), (246, 186)]
        .into_iter()
        .enumerate()
        .map(|(channel, size)| {
            let mut v = viewer(&peer, size, Some(now - std::time::Duration::from_secs(62)));
            v.channel = channel as u32;
            v.desktop_group = Some(("peer".into(), "old-app".into()));
            v
        })
        .collect();
    vs[3].active_at = Some(now - std::time::Duration::from_secs(3));
    let selection = select_sizing(&vs, true);
    assert_eq!(selection.leader, Some(3));
    assert_eq!(selection.contributors, vec![3]);
    assert_eq!(selection.size, Some((246, 186)));
    let (core, id, root) = core_with_screen(218, 78);
    {
        let mut st = core.lock();
        st.viewers.insert(id, vs);
        core.apply_size(&mut st, id);
    }
    let snapshot = core.viewer_list(id, "t1".into());
    assert_eq!(snapshot.current_grid, GridSize { cols: 246, rows: 186 });
    assert_eq!(snapshot.selected_grid, Some(snapshot.current_grid));
    assert_eq!(snapshot.leader.unwrap().channel, 3);
    assert_eq!(snapshot.viewers.iter().filter(|v| v.contributes_to_grid).count(), 1);
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn phones_observers_resumed_and_placeholder_viewers_do_not_pin_desktop_size() {
    let now = Instant::now();
    let desktop = desktop_conn();
    let phone = Arc::new(Conn::default());
    let mut vs = vec![viewer(&desktop, (100, 30), Some(now)), viewer(&phone, (40, 10), Some(now))];
    vs[0].active_at = Some(now + std::time::Duration::from_secs(1));
    assert_eq!(viewer_size(&vs, true).1, Some((100, 30)));
    vs[1].active_at = Some(now + std::time::Duration::from_secs(2));
    assert_eq!(viewer_size(&vs, true).1, Some((100, 30)), "phone activity cannot displace a visible desktop");
    assert_eq!(viewer_size(&vs, false).1, Some((100, 30)), "latest policy still prefers an eligible desktop");
    vs[1].observe = true;
    assert_eq!(viewer_size(&vs, true).1, Some((100, 30)));
    vs[0].visible = false;
    vs[1].observe = false;
    assert_eq!(viewer_size(&vs, true).1, Some((40, 10)), "hiding the last desktop restores mobile fallback");
    assert_eq!(viewer_size(&vs, false).1, Some((40, 10)));
    vs[0].visible = true;
    vs[1] = viewer(&desktop, (40, 10), None);
    assert_eq!(viewer_size(&vs, true).1, Some((100, 30)), "reconnect is not activity");
    vs[1].active_at = Some(now);
    vs[1].sized = false;
    assert_eq!(viewer_size(&vs, true).1, Some((100, 30)), "hidden attach placeholders do not constrain");
    vs[1].sized = true;
    vs[1].active_at = Some(now + std::time::Duration::from_secs(2));
    assert_eq!(viewer_size(&vs, true).1, Some((40, 10)));
    vs[1].active_at = Some(now - std::time::Duration::from_secs(3600));
    assert_eq!(viewer_size(&vs, true).1, Some((100, 30)), "an idle follower cannot shrink the leader");
    vs[1].active_at = Some(now);
    vs[1].size = (0, 0);
    assert_eq!(viewer_size(&vs, true).1, Some((100, 30)), "zero size does not constrain");
}

#[test]
fn mobile_only_and_desktop_pending_fallbacks_remain_authoritative() {
    let now = Instant::now();
    let phone = Arc::new(Conn::default());
    let mut mobile_only = vec![viewer(&phone, (40, 10), Some(now))];
    assert_eq!(viewer_size(&mobile_only, true).1, Some((40, 10)), "mobile-only sizing remains available");
    assert_eq!(viewer_size(&mobile_only, false).1, Some((40, 10)));

    let desktop = desktop_conn();
    mobile_only.push(viewer(&desktop, (218, 74), Some(now + std::time::Duration::from_secs(1))));
    mobile_only[1].sized = false;
    mobile_only[1].awaiting_size = true;
    assert_eq!(viewer_size(&mobile_only, true).1, Some((40, 10)), "a pending desktop cannot lead before measurement");
    assert_eq!(viewer_size(&mobile_only, false).1, Some((40, 10)));

    mobile_only[1].awaiting_size = false;
    mobile_only[1].visible = false;
    assert_eq!(viewer_size(&mobile_only, true).1, Some((40, 10)), "a hidden desktop cannot lead");
}

#[test]
fn selected_visible_offer_and_visibility_restore_the_grid() {
    let now = Instant::now();
    let portrait = desktop_conn();
    let landscape = desktop_conn();
    let mut vs = vec![viewer(&portrait, (218, 168), Some(now)), viewer(&landscape, (218, 74), Some(now))];

    vs[1].visible = false;
    assert_eq!(viewer_size(&vs, true).1, Some((218, 168)), "off-workspace view is excluded");

    vs[1].visible = true;
    assert_eq!(viewer_size(&vs, true).1, Some((218, 74)), "visible view constrains both axes");

    vs[1].visible = false;
    assert_eq!(viewer_size(&vs, true).1, Some((218, 168)), "hiding restores the larger grid");
}

#[test]
fn idle_visible_followers_do_not_constrain_the_active_offer() {
    let now = Instant::now();
    let large = desktop_conn();
    let small = desktop_conn();
    let mut vs = vec![viewer(&large, (218, 168), Some(now)), viewer(&small, (218, 74), Some(now))];
    vs[1].active_at = Some(now - std::time::Duration::from_secs(3600));
    assert!(vs[1].visible);
    assert_eq!(viewer_size(&vs, true).1, Some((218, 168)), "an idle visible follower cannot shrink the active offer");

    vs[1].active_at = None;
    assert_eq!(viewer_size(&vs, true).1, Some((218, 168)), "a returning viewer without a fresh size cannot constrain");
    vs[1].visible = true;
    vs[1].active_at = Some(now - std::time::Duration::from_secs(3600));
    assert_eq!(
        viewer_size(&vs, true).1,
        Some((218, 168)),
        "an idle follower stays visible without contributing its grid"
    );
}

fn core_with_screen(cols: u16, rows: u16) -> (Arc<Core>, u32, std::path::PathBuf) {
    let root = std::path::PathBuf::from(format!("/var/tmp/hc-resize-{}", crate::platform::random_hex()));
    let paths = crate::paths::Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let id = {
        let mut st = core.lock();
        let id = st
            .reg
            .create(crate::terminals::NewTerminal { argv: vec!["sh".into()], cols, rows, ..Default::default() }, 0)
            .unwrap();
        st.screens.insert(id, ghostty_vt::Terminal::new(cols, rows, 1 << 20));
        id
    };
    (core, id, root)
}

#[test]
fn resumed_viewers_hold_the_inherited_grid_until_a_physical_size_arrives() {
    for notifications in [false, true] {
        let (core, id, root) = core_with_screen(103, 74);
        let narrow = desktop_conn();
        let wide = desktop_conn();
        let placeholder = desktop_conn();
        let observer = desktop_conn();
        let request = |size, resumed, observe| AttachRequest {
            observe,
            resize_notifications: notifications,
            visible: true,
            visibility_updates: true,
            history: 0,
            size,
            display: Display::default(),
            resumed,
            desktop_group: Some(("line".into(), "desktop".into())),
        };
        let grid = || core.lock().screens[&id].size();
        let a = core.attach(&narrow, id, request((103, 74), true, false)).unwrap();
        let b = core.attach(&wide, id, request((246, 91), true, false)).unwrap();
        assert_eq!(grid(), (103, 74), "all resumed streams must retain the inherited grid");
        core.viewer_focus(&wide, b, true);
        core.viewer_input(&wide, b, b"", false);
        assert_eq!(grid(), (103, 74), "focus and input are not physical geometry");
        core.attach(&placeholder, id, request((10, 3), false, false)).unwrap();
        let observed = core.attach(&observer, id, request((20, 5), false, true)).unwrap();
        core.viewer_resize(&observer, observed, 20, 5);
        core.viewer_resize(&wide, b, 0, 0);
        assert_eq!(grid(), (103, 74), "placeholders, observers and zero sizes must not release the hold");
        core.viewer_resize(&wide, b, 246, 91);
        assert_eq!(grid(), (246, 91), "a fresh physical offer releases the provisional hold");
        core.viewer_focus(&narrow, a, true);
        core.viewer_input(&narrow, a, b"", false);
        assert_eq!(grid(), (246, 91), "a still-unmeasured resumed stream must not take sizing back");
        core.viewer_resize(&narrow, a, 120, 50);
        assert_eq!(grid(), (120, 50), "new physical offers again constrain both axes");
        core.viewer_focus(&wide, b, true);
        assert_eq!(grid(), (246, 91), "focus selects that viewer's own grid");
        drop(core);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn visibility_updates_recompute_the_grid_and_resumed_views_wait_for_measurement() {
    let (core, id, root) = core_with_screen(103, 74);
    let portrait = desktop_conn();
    let landscape = desktop_conn();
    let request = |size, resumed, visible| AttachRequest {
        observe: false,
        resize_notifications: true,
        visible,
        visibility_updates: true,
        history: 0,
        size,
        display: Display::default(),
        resumed,
        desktop_group: Some(("line".into(), "desktop".into())),
    };
    let grid = || core.lock().screens[&id].size();

    let p = core.attach(&portrait, id, request((218, 168), false, true)).unwrap();
    core.viewer_resize(&portrait, p, 218, 168);
    assert_eq!(grid(), (218, 168));

    let l = core.attach(&landscape, id, request((218, 74), true, false)).unwrap();
    assert_eq!(grid(), (218, 168), "hidden resumed placeholder neither leads nor blocks");
    core.viewer_visibility(&landscape, l, true);
    assert_eq!(grid(), (218, 168), "showing an unmeasured resumed view holds the inherited grid");
    let pending = core.viewer_list(id, "t1".into());
    let pending = pending
        .viewers
        .iter()
        .find(|viewer| viewer.identity == (ViewerIdentity { connection_id: landscape.id, channel: l }))
        .unwrap();
    assert!(pending.visible);
    assert!(!pending.size_eligible);
    assert!(pending.awaiting_size);
    assert!(!pending.sized);
    assert!(!pending.leader);
    core.viewer_focus(&landscape, l, false);
    core.viewer_input(&landscape, l, b"", false);
    core.viewer_resize(&landscape, l, 0, 0);
    assert_eq!(grid(), (218, 168), "focus, input and zero size cannot release the resume hold");

    core.viewer_resize(&landscape, l, 218, 74);
    assert_eq!(grid(), (218, 74), "first positive physical measurement releases the hold");
    let measured = core.viewer_list(id, "t1".into());
    let measured = measured
        .viewers
        .iter()
        .find(|viewer| viewer.identity == (ViewerIdentity { connection_id: landscape.id, channel: l }))
        .unwrap();
    assert!(measured.size_eligible && measured.leader && measured.contributes_to_grid);
    assert!(!measured.awaiting_size);
    core.viewer_visibility(&landscape, l, false);
    assert_eq!(grid(), (218, 168), "hiding the smaller view restores the larger grid");
    core.viewer_focus(&landscape, l, true);
    core.viewer_focus(&landscape, l, false);
    assert_eq!(grid(), (218, 168), "focus cannot make a hidden view eligible");

    core.viewer_visibility(&landscape, l, true);
    assert_eq!(grid(), (218, 74), "showing a measured view restores its constraint");
    core.viewer_focus(&landscape, l, false);
    assert_eq!(grid(), (218, 74), "focus loss does not hide a visible constraint");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn viewer_diagnostics_track_visibility_leader_and_disconnect() {
    let (core, id, root) = core_with_screen(100, 40);
    let wide = desktop_conn();
    let narrow = desktop_conn();
    let phone = Arc::new(Conn::default());
    let request = |size, desktop_group, visibility_updates| AttachRequest {
        observe: false,
        resize_notifications: true,
        visible: true,
        visibility_updates,
        history: 0,
        size,
        display: Display::default(),
        resumed: false,
        desktop_group,
    };
    let a = core.attach(&wide, id, request((100, 40), Some(("local".into(), "desktop".into())), true)).unwrap();
    core.viewer_resize(&wide, a, 100, 40);
    let b = core.attach(&narrow, id, request((80, 30), Some(("local".into(), "desktop".into())), true)).unwrap();
    core.viewer_resize(&narrow, b, 80, 30);
    let mobile = core.attach(&phone, id, request((40, 10), None, false)).unwrap();
    core.viewer_resize(&phone, mobile, 40, 10);
    let grid = || core.lock().screens[&id].size();
    assert_eq!(grid(), (80, 30));

    let snapshot = core.viewer_list(id, "t1".into());
    assert_eq!(snapshot.current_grid, GridSize { cols: 80, rows: 30 });
    assert_eq!(snapshot.selected_grid, Some(GridSize { cols: 80, rows: 30 }));
    let a_identity = ViewerIdentity { connection_id: wide.id, channel: a };
    let b_identity = ViewerIdentity { connection_id: narrow.id, channel: b };
    let mobile_identity = ViewerIdentity { connection_id: phone.id, channel: mobile };
    let a_info = snapshot.viewers.iter().find(|v| v.identity == a_identity).unwrap();
    let b_info = snapshot.viewers.iter().find(|v| v.identity == b_identity).unwrap();
    let mobile_info = snapshot.viewers.iter().find(|v| v.identity == mobile_identity).unwrap();
    assert!(snapshot.leader == Some(b_info.identity));
    assert!(a_info.size_eligible && !a_info.contributes_to_grid);
    assert!(b_info.size_eligible && b_info.leader && b_info.contributes_to_grid);
    assert!(mobile_info.visible && !mobile_info.size_eligible && !mobile_info.contributes_to_grid);
    assert_eq!(mobile_info.origin, ViewerOrigin::LocalClient);
    assert!(!mobile_info.visibility_updates, "legacy peers do not negotiate visibility");
    assert!(mobile_info.peer_pid.is_none(), "non-Unix or unavailable PID is omitted");
    assert_eq!(
        core.viewer_list(id, "t1".into()).viewers.iter().find(|v| v.identity == mobile_identity).unwrap().identity,
        mobile_info.identity,
        "identity stays stable while the connection lives"
    );

    core.viewer_visibility(&narrow, b, false);
    assert_eq!(grid(), (100, 40), "hidden minimum no longer constrains the visible desktop");
    let hidden = core.viewer_list(id, "t1".into());
    let hidden_b = hidden.viewers.iter().find(|v| v.identity == b_identity).unwrap();
    assert!(!hidden_b.visible && !hidden_b.size_eligible && !hidden_b.contributes_to_grid);
    assert_eq!(hidden.selected_grid, Some(GridSize { cols: 100, rows: 40 }));

    core.viewer_visibility(&wide, a, false);
    assert_eq!(grid(), (40, 10), "with no visible desktop, mobile becomes fallback");
    let no_desktop = core.viewer_list(id, "t1".into());
    let phone_info = no_desktop.viewers.iter().find(|v| v.identity == mobile_identity).unwrap();
    assert!(phone_info.size_eligible && phone_info.leader && phone_info.contributes_to_grid);
    assert!(!no_desktop.viewers.iter().any(|v| v.identity == a_identity && v.leader));

    core.viewer_visibility(&wide, a, true);
    assert_eq!(grid(), (100, 40));
    core.detach(&wide, None);
    assert_eq!(grid(), (40, 10), "EOF detach removes the desktop offer and restores mobile sizing");
    let detached = core.viewer_list(id, "t1".into());
    assert!(!detached.viewers.iter().any(|v| v.identity == a_identity));
    assert!(detached.viewers.iter().find(|v| v.identity == mobile_identity).unwrap().leader);
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_viewer_stays_visible_and_eligible_until_detach() {
    let (core, id, root) = core_with_screen(100, 40);
    let legacy = desktop_conn();
    let channel = core
        .attach(
            &legacy,
            id,
            AttachRequest {
                observe: false,
                resize_notifications: false,
                visible: true,
                visibility_updates: false,
                history: 0,
                size: (100, 40),
                display: Display::default(),
                resumed: false,
                desktop_group: Some(("local".into(), "desktop".into())),
            },
        )
        .unwrap();
    core.viewer_resize(&legacy, channel, 100, 40);
    core.viewer_visibility(&legacy, channel, false);
    core.viewer_focus(&legacy, channel, false);
    let visible = core.viewer_list(id, "t1".into());
    let legacy_view = visible.viewers.first().unwrap();
    assert!(legacy_view.visible && legacy_view.size_eligible && legacy_view.leader);
    assert!(!legacy_view.visibility_updates);
    std::thread::sleep(std::time::Duration::from_millis(2));
    assert!(core.viewer_list(id, "t1".into()).viewers[0].visible, "idle time never expires legacy offers");
    core.detach(&legacy, None);
    assert!(core.viewer_list(id, "t1".into()).viewers.is_empty());
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn relayed_viewer_origin_and_pid_mask_survive_peer_line_cleanup() {
    let (core, id, root) = core_with_screen(80, 24);
    let mut peer_conn = Conn::default();
    peer_conn.peer_pid = Some(4242);
    let peer_conn = Arc::new(peer_conn);
    crate::remote::peer_joined(
        &core,
        &peer_conn,
        &serde_json::json!({"core":{"id":"remote-id","name":"remote"},"calls_you":"test"}),
    );
    let channel = core
        .attach(
            &peer_conn,
            id,
            AttachRequest {
                observe: false,
                resize_notifications: true,
                visible: true,
                visibility_updates: true,
                history: 0,
                size: (80, 24),
                display: Display::default(),
                resumed: false,
                desktop_group: Some(("remote-id".into(), "desktop".into())),
            },
        )
        .unwrap();
    crate::remote::peer_left(&core, &peer_conn);
    let info = core
        .viewer_list(id, "t1".into())
        .viewers
        .into_iter()
        .find(|viewer| viewer.identity == (ViewerIdentity { connection_id: peer_conn.id, channel }))
        .unwrap();
    assert_eq!(info.origin, ViewerOrigin::Relayed);
    assert_eq!(info.peer.as_deref(), Some("remote"));
    assert_eq!(info.peer_pid, None, "a remote PID is never reported, even after peer-line cleanup");
    assert!(info.resize_notifications);
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn fresh_same_size_reoffers_do_not_ping_pong_between_desktop_groups() {
    let (core, id, root) = core_with_screen(100, 40);
    let wide = desktop_conn();
    let narrow = desktop_conn();
    let request = |size, group: &str, resumed| AttachRequest {
        observe: false,
        resize_notifications: true,
        visible: true,
        visibility_updates: true,
        history: 0,
        size,
        display: Display::default(),
        resumed,
        desktop_group: Some(("line".into(), group.into())),
    };
    let a = core.attach(&wide, id, request((100, 40), "wide", false)).unwrap();
    core.viewer_resize(&wide, a, 100, 40);
    let b = core.attach(&narrow, id, request((80, 24), "narrow", false)).unwrap();
    core.viewer_resize(&narrow, b, 80, 24);
    let grid = || core.lock().screens[&id].size();
    assert_eq!(grid(), (80, 24));

    // Drain any wake caused by setup. An identical offer still wakes the
    // writer, but must not refresh activity and steal desktop-group leadership.
    tokio::time::timeout(std::time::Duration::from_secs(1), wide.wake.notified()).await.unwrap();
    core.viewer_resize(&wide, a, 100, 40);
    tokio::time::timeout(std::time::Duration::from_secs(1), wide.wake.notified()).await.unwrap();
    assert_eq!(grid(), (80, 24), "unchanged physical reoffer must not select another desktop group");
    for _ in 0..3 {
        core.viewer_resize(&narrow, b, 80, 24);
        core.viewer_resize(&wide, a, 100, 40);
        assert_eq!(grid(), (80, 24), "identical reoffers must not ping-pong group leadership");
    }

    core.viewer_resize(&wide, a, 110, 45);
    assert_eq!(grid(), (110, 45), "a changed physical offer still selects its group");
    let resumed = desktop_conn();
    let c = core.attach(&resumed, id, request((90, 30), "resumed", true)).unwrap();
    assert_eq!(grid(), (110, 45));
    core.viewer_resize(&resumed, c, 90, 30);
    assert_eq!(grid(), (90, 30), "an unchanged cached offer must still release a resumed hold");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_attach_visibility_defaults_to_eligible() {
    let params: crate::api::AttachParams = serde_json::from_value(serde_json::json!({
        "terminal": "t1",
        "cols": 80,
        "rows": 24
    }))
    .unwrap();
    assert!(params.visible);
    assert!(!params.visibility_updates);
}

fn drain(conn: &Arc<Conn>, channel: u32, screen: &mut ghostty_vt::Terminal) {
    conn.with(|out| {
        out.resume(channel);
        while let Some(frame) = out.next_frame() {
            if frame[4] == wire::DATA {
                screen.write(&frame[wire::HEADER..]);
            } else if frame[4] == wire::RESIZE {
                let size = &frame[wire::HEADER..];
                screen.resize(u16::from_le_bytes([size[0], size[1]]), u16::from_le_bytes([size[2], size[3]]));
            }
        }
    });
}

#[test]
fn wider_desktop_viewer_wraps_inline_redraw_at_the_shared_width() {
    let (core, id, root) = core_with_screen(20, 8);
    let wide = desktop_conn();
    let narrow = desktop_conn();
    let request = |size| AttachRequest {
        observe: false,
        resize_notifications: true,
        visible: true,
        visibility_updates: true,
        history: 0,
        size,
        display: Display::default(),
        resumed: false,
        desktop_group: Some(("forwarded-line".into(), "desktop".into())),
    };
    let a = core.attach(&wide, id, request((40, 8))).unwrap();
    core.viewer_resize(&wide, a, 40, 8);
    let b = core.attach(&narrow, id, request((20, 8))).unwrap();
    core.viewer_resize(&narrow, b, 20, 8);
    wide.with(|out| out.resume(a));
    narrow.with(|out| out.resume(b));
    let mut wide_screen = ghostty_vt::Terminal::new(40, 8, 1 << 20);
    let mut narrow_screen = ghostty_vt::Terminal::new(20, 8, 1 << 20);
    core.prepare_repaints(&wide);
    core.prepare_repaints(&narrow);
    drain(&wide, a, &mut wide_screen);
    drain(&narrow, b, &mut narrow_screen);
    for bytes in [
        b"| markdown table row | result |\r\n\r\n> prompt\r\nINSERT bypass".as_slice(),
        b"\x1b[2A\r\x1b[J> next prompt\r\nINSERT bypass",
    ] {
        core.output(&mut core.lock(), id, bytes);
        core.prepare_repaints(&wide);
        core.prepare_repaints(&narrow);
        drain(&wide, a, &mut wide_screen);
        drain(&narrow, b, &mut narrow_screen);
        let expected = core.lock().screens[&id].screen_rows();
        assert_eq!(narrow_screen.screen_rows(), expected);
        assert_eq!(
            wide_screen.screen_rows().iter().map(|r| r.trim_end()).collect::<Vec<_>>(),
            expected.iter().map(|r| r.trim_end()).collect::<Vec<_>>(),
            "inline cursor-up redraw must use the PTY's wrap width"
        );
    }
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn larger_desktop_viewers_keep_only_the_shared_scrolling_grid() {
    let (core, id, root) = core_with_screen(20, 4);
    let tall = desktop_conn();
    let short = desktop_conn();
    let request = |size| AttachRequest {
        observe: false,
        resize_notifications: false,
        visible: true,
        visibility_updates: true,
        history: 0,
        size,
        display: Display::default(),
        resumed: false,
        desktop_group: Some(("local".into(), "desktop".into())),
    };
    let a = core.attach(&tall, id, request((20, 8))).unwrap();
    core.viewer_resize(&tall, a, 20, 8);
    let b = core.attach(&short, id, request((20, 4))).unwrap();
    core.viewer_resize(&short, b, 20, 4);
    tall.with(|out| out.resume(a));
    short.with(|out| out.resume(b));
    let mut tall_screen = ghostty_vt::Terminal::new(20, 8, 1 << 20);
    tall_screen.write(b"old tall pane\x1b[7;1HSTALE COPY");
    let mut short_screen = ghostty_vt::Terminal::new(20, 4, 1 << 20);
    let scrolling: Vec<u8> = (1..=500).flat_map(|n| format!("line {n}\r\n").into_bytes()).collect();
    for bytes in [
        scrolling.as_slice(),
        b"\x1b[2A\rrewritten inline line which wraps\x1b[K",
        b"\x1b[2;4r\x1b[4;1Hmargin\nscroll\nagain\n",
        b"\x1b[?1049hALT\r\n1\r\n2\r\n3\r\n4\r\n5",
        b"\x1b[?1049l\x1b[r\r\nback on main\r\n",
        b"\x1bcreset\r\n1\r\n2\r\n3\r\n4\r\n5",
    ] {
        core.output(&mut core.lock(), id, bytes);
        core.prepare_repaints(&tall);
        core.prepare_repaints(&short);
        drain(&tall, a, &mut tall_screen);
        drain(&short, b, &mut short_screen);
        let expected = core.lock().screens[&id].screen_rows();
        assert_eq!(short_screen.screen_rows(), expected, "fitting viewer keeps the original stream");
        let actual = tall_screen.screen_rows();
        assert_eq!(
            actual[..4].iter().map(|r| r.trim_end()).collect::<Vec<_>>(),
            expected.iter().map(|r| r.trim_end()).collect::<Vec<_>>(),
            "larger pane must show the core's rows"
        );
        assert!(actual[4..].iter().all(|r| r.trim().is_empty()), "stale rows below the shared grid: {actual:?}");
    }
    tall_screen.resize(20, 12);
    tall_screen.write(b"\x1b[11;1HSTALE AFTER RESIZE");
    core.viewer_resize(&tall, a, 20, 12); // This activity now selects the tall viewer.
    core.prepare_repaints(&tall);
    drain(&tall, a, &mut tall_screen);
    assert_eq!(core.lock().screens[&id].size(), (20, 12));
    assert_eq!(tall_screen.screen_rows(), core.lock().screens[&id].screen_rows());
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn resize_drags_serialize_nothing_without_observers_and_only_latest_when_stalled() {
    let (core, id, root) = core_with_screen(80, 24);
    let leader = Arc::new(Conn::default());
    let request = |observe, resize_notifications| AttachRequest {
        observe,
        resize_notifications,
        visible: true,
        visibility_updates: false,
        history: 0,
        size: (80, 24),
        display: Display::default(),
        resumed: false,
        desktop_group: None,
    };
    let channel = core.attach(&leader, id, request(false, false)).unwrap();
    PAINTS.set(0);
    for cols in 81..100 {
        core.viewer_resize(&leader, channel, cols, 24);
    }
    assert_eq!(PAINTS.get(), 0, "desktop-only drags must not serialize paints");
    let observer = Arc::new(Conn::default());
    let observed = core.attach(&observer, id, request(true, true)).unwrap();
    observer.with(|out| {
        out.resume(observed);
        out.data(observed, &vec![b'x'; wire::WINDOW]);
        while out.next_frame().is_some() {}
    });
    PAINTS.set(0);
    for cols in 100..200 {
        core.viewer_resize(&leader, channel, cols, 24);
        core.prepare_repaints(&observer);
    }
    assert_eq!(PAINTS.get(), 0, "no serialization while credit stalled");
    observer.with(|out| {
        assert_eq!(out.queued(observed), 0, "no obsolete paints accumulated");
        assert!(out.next_frame().is_none());
        out.credit(observed, wire::WINDOW);
    });
    core.prepare_repaints(&observer);
    assert_eq!(PAINTS.get(), 1);
    observer.with(|out| {
        let frame = out.next_frame().unwrap();
        assert_eq!(frame[4], wire::RESIZE);
        assert_eq!(&frame[wire::HEADER..], &[199, 0, 24, 0]);
        let frame = out.next_frame().unwrap();
        assert_eq!(frame[4], wire::DATA);
        assert_eq!(frame[wire::HEADER], 0x18, "cancel a partial old-grid sequence");
    });
    core.prepare_repaints(&observer);
    assert_eq!(PAINTS.get(), 1, "unchanged grid is not repainted twice");
    // Exit before the writer next wakes must still deliver the final screen.
    core.viewer_resize(&leader, channel, 200, 24);
    core.record_finish(&mut core.lock(), id, proto::Exit { code: Some(0), signal: None }, "exited");
    observer.with(|out| {
        let size = out.next_frame().expect("final resize");
        assert_eq!(size[4], wire::RESIZE);
        assert_eq!(&size[wire::HEADER..], &[200, 0, 24, 0]);
        assert_eq!(out.next_frame().unwrap()[4], wire::DATA);
        while let Some(frame) = out.next_frame() {
            if frame[4] == wire::CLOSE {
                break;
            }
        }
    });
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}
