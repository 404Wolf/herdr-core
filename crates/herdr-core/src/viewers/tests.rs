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
        resize_history: 0,
        repaint_pending: false,
        size,
        active_at,
        visible: true,
        sized: true,
        awaiting_size: false,
        visibility_updates: true,
        focus_updates: false,
        focused: false,
        focused_at: None,
        input_at: None,
        replies: Default::default(),
        origin: if conn.local_desktop { ViewerOrigin::LocalDesktop } else { ViewerOrigin::LocalClient },
        peer: None,
        desktop_group: conn.local_desktop.then(|| ("local".into(), "desktop".into())),
        display: Display::default(),
        recovering: false,
    }
}

#[test]
fn desktop_windows_fit_both_axes_and_use_the_selected_leader_display() {
    let now = Instant::now();
    let a = desktop_conn();
    let b = desktop_conn();
    let mut vs = vec![viewer(&a, (100, 20), Some(now)), viewer(&b, (80, 30), Some(now))];
    vs[0].display.bg = Some("#ffffff".into());
    vs[1].display.bg = Some("#eeeeee".into());
    vs[0].active_at = Some(now + std::time::Duration::from_secs(1));
    let (leader, size) = viewer_size(&vs, true);
    assert_eq!(size, Some((80, 20)));
    assert_eq!(leader.unwrap().display.bg.as_deref(), Some("#ffffff"));
    vs[1].active_at = Some(now + std::time::Duration::from_secs(2));
    assert_eq!(viewer_size(&vs, true).1, Some((80, 20)));
    assert_eq!(viewer_size(&vs, false).1, Some((80, 30)), "latest policy remains available");
    let phone = Arc::new(Conn::default());
    let mut mobile = viewer(&phone, (40, 10), Some(now + std::time::Duration::from_secs(3)));
    mobile.display.bg = Some("#000000".into());
    vs.push(mobile);
    let (leader, size) = viewer_size(&vs, true);
    assert_eq!(size, Some((80, 20)), "phone activity cannot override visible desktop sizing");
    assert_eq!(leader.unwrap().display.bg.as_deref(), Some("#eeeeee"), "display facts follow selected desktop leader");
    vs.pop();
    assert_eq!(viewer_size(&vs, true).1, Some((80, 20)));
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
    assert_eq!(viewer_size(&vs, true).1, Some((80, 20)), "only an identical pair shares the grid");
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
    assert_eq!(viewer_size(&vs, true).1, Some((40, 10)));
    vs[1].active_at = Some(now - std::time::Duration::from_secs(3600));
    assert_eq!(viewer_size(&vs, true).1, Some((40, 10)), "an idle visible window must still fit");
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
fn grouped_size_uses_visible_min_and_visibility_restores_the_grid() {
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
fn focus_and_idle_do_not_change_visibility_eligibility() {
    let now = Instant::now();
    let large = desktop_conn();
    let small = desktop_conn();
    let mut vs = vec![viewer(&large, (218, 168), Some(now)), viewer(&small, (218, 74), Some(now))];
    vs[1].active_at = Some(now - std::time::Duration::from_secs(3600));
    assert!(vs[1].visible);
    assert_eq!(viewer_size(&vs, true).1, Some((218, 74)), "an idle visible view still constrains");

    vs[1].active_at = None;
    assert_eq!(viewer_size(&vs, true).1, Some((218, 168)), "a returning viewer without a fresh size cannot constrain");
    vs[1].visible = true;
    vs[1].active_at = Some(now - std::time::Duration::from_secs(3600));
    assert_eq!(viewer_size(&vs, true).1, Some((218, 74)), "visibility, not recency, admits the measured size");
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
            focus_updates: false,
            focused: false,
            idle_ms: None,
            history: 0,
            resize_history: 0,
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
        assert_eq!(grid(), (120, 50), "focus must not cause a resize tug-of-war");
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
        focus_updates: false,
        focused: false,
        idle_ms: None,
        history: 0,
        resize_history: 0,
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
fn viewer_diagnostics_track_visibility_group_min_and_disconnect() {
    let (core, id, root) = core_with_screen(100, 40);
    let wide = desktop_conn();
    let narrow = desktop_conn();
    let phone = Arc::new(Conn::default());
    let request = |size, desktop_group, visibility_updates| AttachRequest {
        observe: false,
        resize_notifications: true,
        visible: true,
        visibility_updates,
        focus_updates: false,
        focused: false,
        idle_ms: None,
        history: 0,
        resize_history: 0,
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
    assert!(a_info.size_eligible && a_info.contributes_to_grid);
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
                focus_updates: false,
                focused: false,
                idle_ms: None,
                history: 0,
                resize_history: 0,
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
                focus_updates: false,
                focused: false,
                idle_ms: None,
                history: 0,
                resize_history: 0,
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
        focus_updates: false,
        focused: false,
        idle_ms: None,
        history: 0,
        resize_history: 0,
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
        focus_updates: false,
        focused: false,
        idle_ms: None,
        history: 0,
        resize_history: 0,
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
        focus_updates: false,
        focused: false,
        idle_ms: None,
        history: 0,
        resize_history: 0,
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
    core.viewer_resize(&tall, a, 20, 12); // The shared PTY stays 20x4.
    core.prepare_repaints(&tall);
    drain(&tall, a, &mut tall_screen);
    assert!(tall_screen.screen_rows()[4..].iter().all(|r| r.trim().is_empty()));
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
        focus_updates: false,
        focused: false,
        idle_ms: None,
        history: 0,
        resize_history: 0,
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

fn focus_request(size: (u16, u16), group: Option<(&str, &str)>, focus_updates: bool, focused: bool) -> AttachRequest {
    AttachRequest {
        observe: false,
        resize_notifications: true,
        visible: true,
        visibility_updates: true,
        focus_updates,
        focused,
        idle_ms: None,
        history: 0,
        resize_history: 0,
        size,
        display: Display::default(),
        resumed: false,
        desktop_group: group.map(|(line, group)| (line.into(), group.into())),
    }
}

/// 2026-11-13: macro/t1b6 viewed from small-pickle through its core (one
/// line, one desktop group). A hidden 2158x1510 window (218x74, previously
/// focused, still claiming visible: special workspaces report nothing) and the
/// focused 2158x3821 window (218x187). The grid was the group min, 218x74; it
/// must follow the focused window, whatever the hidden one says.
#[test]
fn incident_2026_11_13_relayed_hidden_window_cannot_shrink_the_focused_one() {
    for hidden_reports in [false, true] {
        let (core, id, root) = core_with_screen(218, 74);
        let line = Arc::new(Conn::default()); // the small-pickle core's line into macro
        let group = Some(("small-pickle", "small-pickle"));
        let grid = || core.lock().screens[&id].size();
        let hidden = core.attach(&line, id, focus_request((218, 74), group, true, true)).unwrap();
        core.viewer_resize(&line, hidden, 218, 74);
        assert_eq!(grid(), (218, 74));
        // Wolf switches to special:15: the hidden window loses focus, the
        // portrait window attaches focused through the same relay.
        core.viewer_window_focus(&line, hidden, false);
        if hidden_reports {
            core.viewer_visibility(&line, hidden, false);
        }
        let focused = core.attach(&line, id, focus_request((218, 187), group, true, true)).unwrap();
        core.viewer_resize(&line, focused, 218, 187);
        assert_eq!(grid(), (218, 187), "the focused window decides (hidden reports={hidden_reports})");
        // Activity and fresh offers from the hidden window change nothing.
        core.viewer_focus(&line, hidden, true);
        core.viewer_input(&line, hidden, b"", false);
        core.viewer_resize(&line, hidden, 218, 73);
        assert_eq!(grid(), (218, 187), "an unfocused window never shrinks the grid");
        let list = core.viewer_list(id, "t1b6".into());
        assert!(list.focus_authority);
        assert_eq!(list.leader.map(|l| l.channel), Some(focused));
        assert!(list.viewers.iter().any(|v| v.identity.channel == focused && v.focused && v.leader));
        assert!(list.viewers.iter().any(|v| v.identity.channel == hidden && !v.focused && !v.contributes_to_grid));
        // Nothing focused (Wolf in a browser): the last focused window keeps it.
        core.viewer_window_focus(&line, focused, false);
        assert_eq!(grid(), (218, 187), "the last focused viewer keeps the grid");
        // Back to the other window: it decides again.
        core.viewer_visibility(&line, hidden, true);
        core.viewer_resize(&line, hidden, 218, 74);
        core.viewer_window_focus(&line, hidden, true);
        assert_eq!(grid(), (218, 74), "focusing the other window hands it the grid");
        drop(core);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn old_desktops_never_win_over_focus_and_a_phone_wins_by_use() {
    let (core, id, root) = core_with_screen(100, 40);
    let app = desktop_conn();
    let old_app = desktop_conn();
    let phone = Arc::new(Conn::default());
    let grid = || core.lock().screens[&id].size();
    let local = Some(("test", "test"));
    let a = core.attach(&app, id, focus_request((120, 50), local, true, true)).unwrap();
    core.viewer_resize(&app, a, 120, 50);
    assert_eq!(grid(), (120, 50));
    let o = core.attach(&old_app, id, focus_request((80, 24), local, false, false)).unwrap();
    core.viewer_resize(&old_app, o, 80, 24);
    core.viewer_focus(&old_app, o, true);
    // A legacy client cannot claim focus through the new frame.
    core.viewer_window_focus(&old_app, o, true);
    assert_eq!(grid(), (120, 50), "an old desktop viewer only receives the grid");
    // Item 71: a phone (no group, no focus reports) used after the desktop
    // leads; the desktop takes it back on its next input.
    let p = core.attach(&phone, id, focus_request((40, 10), None, false, false)).unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    core.viewer_input(&phone, p, b"x", false);
    assert_eq!(grid(), (40, 10), "the phone used last leads over an idle focused desktop");
    core.viewer_input(&app, a, b"x", false);
    assert_eq!(grid(), (120, 50), "the desktop's next input takes it back");
    // A legacy desktop (a group, no focus) never leads while a reporter decides.
    core.viewer_input(&old_app, o, b"x", false);
    assert_eq!(grid(), (120, 50), "an old desktop viewer only receives the grid, even typing");
    core.detach(&phone, None);
    // The focused window goes away for good: previous rules resume.
    core.detach(&app, None);
    assert_ne!(grid(), (120, 50), "without focus history the legacy selection applies again");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn hidden_or_unmeasured_focused_window_holds_the_grid() {
    let (core, id, root) = core_with_screen(100, 40);
    let line = Arc::new(Conn::default());
    let group = Some(("pickle", "pickle"));
    let grid = || core.lock().screens[&id].size();
    let a = core.attach(&line, id, focus_request((100, 40), group, true, true)).unwrap();
    core.viewer_resize(&line, a, 100, 40);
    let b = core.attach(&line, id, focus_request((90, 30), group, true, false)).unwrap();
    core.viewer_resize(&line, b, 90, 30);
    assert_eq!(grid(), (100, 40), "a never-focused reporter does not constrain");
    core.viewer_window_focus(&line, a, false);
    core.viewer_visibility(&line, a, false);
    assert_eq!(grid(), (100, 40), "the hidden last-focused window holds; nobody else takes over");
    core.viewer_window_focus(&line, b, true);
    assert_eq!(grid(), (90, 30));
    // Focused but its window went hidden (workspace switch without a focus
    // event yet): hold rather than fall back to an older window.
    core.viewer_visibility(&line, a, true);
    core.viewer_visibility(&line, b, false);
    assert_eq!(grid(), (90, 30), "hidden focused window holds");
    // A phone used while the focused window is hidden leads; once that window
    // is shown and measured again, it decides again.
    let phone = Arc::new(Conn::default());
    let p = core.attach(&phone, id, focus_request((40, 10), None, false, false)).unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    assert_eq!(grid(), (90, 30), "attached and sized is not use (item 71)");
    core.viewer_input(&phone, p, b"x", false);
    assert_eq!(grid(), (40, 10), "the phone leads while the focused window is hidden");
    core.viewer_visibility(&line, b, true);
    assert_eq!(grid(), (40, 10), "shown again is not use: the phone was used last (item 71)");
    core.viewer_window_focus(&line, b, true);
    assert_eq!(grid(), (90, 30), "the shown window takes the grid back once focused (or typed into)");
    core.detach(&phone, None);
    // Two panes of the same focused window share the smaller grid.
    core.viewer_window_focus(&line, a, true);
    assert_eq!(grid(), (90, 30), "same focused window, same group: min");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// 2026-11-13, laptop/t6k: the laptop's own app window (focus history,
/// hidden, unmeasured 120x40) and Wolf's focused small-pickle window arriving
/// through the small-pickle core without focus (263x197). The hidden reporter
/// must not lock the visible measured window out for good.
#[test]
fn incident_2026_11_13_hidden_reporter_does_not_lock_out_a_visible_desktop() {
    let (core, id, root) = core_with_screen(263, 96);
    let app = desktop_conn();
    let line = Arc::new(Conn::default()); // the small-pickle core's line
    let phone = Arc::new(Conn::default());
    let grid = || core.lock().screens[&id].size();
    let local = Some(("test", "test"));
    let a = core.attach(&app, id, focus_request((263, 96), local, true, true)).unwrap();
    core.viewer_resize(&app, a, 263, 96);
    assert_eq!(grid(), (263, 96));
    let p = core.attach(&phone, id, focus_request((40, 10), None, false, false)).unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    core.viewer_input(&phone, p, b"x", false);
    // The laptop window is hidden and the phone was used since it was last
    // focused: the phone leads (it sends no focus).
    core.viewer_window_focus(&app, a, false);
    core.viewer_visibility(&app, a, false);
    assert_eq!(grid(), (40, 10), "a phone used after the hidden window's focus leads");
    let list = core.viewer_list(id, "t6k".into());
    assert!(list.focus_authority);
    assert_eq!(list.leader.map(|l| l.channel), Some(p));
    // A reporting window that was never focused stays out, also as a contributor.
    let other = desktop_conn();
    let u = core.attach(&other, id, focus_request((90, 30), local, true, false)).unwrap();
    core.viewer_resize(&other, u, 90, 30);
    assert_eq!(grid(), (40, 10), "an unfocused reporter never takes over");
    // The visible small-pickle window (non-reporting) leads, also over the phone.
    let r = core.attach(&line, id, focus_request((263, 197), Some(("pickle", "pickle")), false, false)).unwrap();
    core.viewer_resize(&line, r, 263, 197);
    assert_eq!(grid(), (263, 197), "a visible measured desktop is not locked out by a hidden reporter");
    let list = core.viewer_list(id, "t6k".into());
    assert!(!list.focus_authority);
    assert_eq!(list.leader.map(|l| l.channel), Some(r));
    // The reporter is shown again: focus history rules again (it keeps the grid
    // as last focused) and the non-reporter only receives it.
    core.viewer_visibility(&app, a, true);
    core.viewer_resize(&app, a, 263, 96);
    assert!(core.viewer_list(id, "t6k".into()).focus_authority, "focus authority is back");
    assert_eq!(grid(), (40, 10), "the phone was used after the reporter's last focus (item 71)");
    core.viewer_window_focus(&app, a, true);
    assert_eq!(grid(), (263, 96), "a shown reporter with focus history decides again once used");
    assert!(core.viewer_list(id, "t6k".into()).focus_authority);
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// Item 71, Wolf's t16b at 14:07 (`hc viewers t16b`): the desktop window is
/// visible and focused but idle for 1 h 41 min, offering 58x132, while the
/// phone is in use (active 98 ms ago), offering 160x72. The phone used last
/// leads with its own size; the desktop's next input takes the grid back.
#[test]
fn wolfs_t16b_the_phone_in_use_leads_over_an_idle_focused_desktop() {
    let base = Instant::now();
    // The listing's ages, oldest first: an instant that many ms before "now".
    let at = |ms_ago: u64| Some(base + Duration::from_millis(6_124_505 - ms_ago));
    let relayed = Arc::new(Conn::default());
    let mut air = viewer(&relayed, (116, 76), None);
    air.channel = 21;
    air.visibility_updates = false;
    air.sized = false;
    air.awaiting_size = true;
    let mut desktop = viewer(&relayed, (58, 132), at(6_100_726));
    desktop.channel = 24;
    desktop.resize_notifications = true;
    desktop.desktop_group = Some(("820feff3".into(), "820feff3".into()));
    desktop.focus_updates = true;
    desktop.focused = true;
    desktop.focused_at = at(6_100_407);
    let mut tab = viewer(&relayed, (0, 0), at(6_124_505));
    tab.channel = 82;
    tab.resize_notifications = true;
    tab.desktop_group = desktop.desktop_group.clone();
    tab.focus_updates = true;
    tab.visible = false;
    tab.sized = false;
    let mut pickle = viewer(&relayed, (91, 108), at(6_048_787));
    pickle.channel = 6;
    pickle.resize_notifications = true;
    pickle.desktop_group = Some(("a5453d9c".into(), "a5453d9c".into()));
    pickle.focus_updates = true;
    pickle.focused = true;
    pickle.focused_at = at(157_683);
    pickle.visible = false;
    pickle.sized = false;
    let phone_conn = Arc::new(Conn::default());
    let mut phone = viewer(&phone_conn, (160, 72), at(98));
    phone.input_at = at(98);
    phone.channel = 2;
    phone.resize_notifications = true;
    let mut vs = vec![air, desktop, tab, pickle, phone];
    for grouped in [true, false] {
        let selection = select_sizing(&vs, grouped);
        assert!(selection.focus_authority);
        assert_eq!(selection.leader, Some(4), "the phone in use leads (grouped={grouped})");
        assert_eq!(selection.contributors, vec![4]);
        assert_eq!(selection.size, Some((160, 72)), "the phone's own size");
    }
    // What the core did then, without the phone: the small-pickle window was
    // focused later but is hidden, so the shown focused desktop leads.
    let without_phone = &vs[..4];
    assert_eq!(select_sizing(without_phone, true).leader, Some(1));
    assert_eq!(select_sizing(without_phone, true).size, Some((58, 132)));
    // Wolf types on the desktop: it takes the grid back.
    vs[1].input_at = Some(base + Duration::from_millis(6_124_506));
    for grouped in [true, false] {
        let selection = select_sizing(&vs, grouped);
        assert_eq!(selection.leader, Some(1), "the desktop's input wins (grouped={grouped})");
        assert_eq!(selection.size, Some((58, 132)));
    }
}

/// Item 71: whichever device was used last leads, and only use moves it: a
/// person's input, WINDOW_FOCUS(1) (a window gaining focus, a phone touched
/// or resumed), or an attach a person caused.
#[test]
fn the_device_used_last_leads_and_the_other_takes_it_back_by_use() {
    let (core, id, root) = core_with_screen(100, 40);
    let app = desktop_conn();
    let phone = Arc::new(Conn::default());
    let grid = || core.lock().screens[&id].size();
    let a = core.attach(&app, id, focus_request((120, 50), Some(("test", "test")), true, true)).unwrap();
    core.viewer_resize(&app, a, 120, 50);
    assert_eq!(grid(), (120, 50));
    // A phone that reports focus, attached without it (prefetched, hidden).
    let p = core.attach(&phone, id, focus_request((40, 10), None, true, false)).unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    assert_eq!(grid(), (120, 50), "a reporter never focused does not decide");
    core.viewer_window_focus(&phone, p, true);
    assert_eq!(grid(), (40, 10), "the phone is touched: it leads with its own size");
    core.viewer_input(&app, a, b"x", false);
    assert_eq!(grid(), (120, 50), "typing on the desktop takes it back");
    core.viewer_window_focus(&phone, p, true);
    assert_eq!(grid(), (40, 10), "a repeated WINDOW_FOCUS(1) is the phone used again");
    core.viewer_window_focus(&app, a, false);
    core.viewer_window_focus(&app, a, true);
    assert_eq!(grid(), (120, 50), "the desktop window gaining focus is use");
    core.viewer_input(&phone, p, b"y", false);
    assert_eq!(grid(), (40, 10), "typing on the phone");
    let list = core.viewer_list(id, "t1".into());
    assert!(list.focus_authority);
    // Channels are per connection: name a viewer by both.
    let of = |conn: &Arc<Conn>, channel| ViewerIdentity { connection_id: conn.id, channel };
    assert_eq!(list.leader, Some(of(&phone, p)));
    assert!(list.viewers.iter().any(|v| v.identity == of(&phone, p) && v.input_ms_ago.is_some()));
    assert!(list.viewers.iter().any(|v| v.identity == of(&app, a) && v.input_ms_ago.is_some()));
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// Item 71, no flapping: an idle desktop whose program keeps asking the
/// terminal things never takes the grid back from the phone in use. The
/// core answers DSR itself (viewers never see the query); a reply a viewer
/// sends as AUTOMATIC_INPUT, or as DATA on a path without it, is not use;
/// neither are identical size offers, visibility flips or program focus out.
#[test]
fn an_idle_desktop_whose_program_asks_dsr_never_takes_the_grid_back() {
    let (core, id, root) = core_with_screen(100, 40);
    let app = desktop_conn();
    let phone = Arc::new(Conn::default());
    let grid = || core.lock().screens[&id].size();
    let a = core.attach(&app, id, focus_request((120, 50), Some(("test", "test")), true, true)).unwrap();
    core.viewer_resize(&app, a, 120, 50);
    // Today's phone: no focus reports; its attach, size and typing are use.
    let p = core.attach(&phone, id, focus_request((40, 10), None, false, false)).unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    core.viewer_input(&phone, p, b"ls\r", false);
    assert_eq!(grid(), (40, 10));
    for cycle in 0..20 {
        {
            // Claude asks for the cursor every 2 s.
            let mut st = core.lock();
            core.output(&mut st, id, b"\x1b[6n");
        }
        core.viewer_input(&app, a, b"\x1b[24;80R", true);
        core.viewer_input(&app, a, b"\x1b[24;80R", false);
        core.viewer_input(&app, a, b"\x1b[?62;22c\x1b]11;rgb:0000/0000/0000\x1b\\", false);
        core.viewer_resize(&app, a, 120, 50);
        core.viewer_visibility(&app, a, false);
        core.viewer_visibility(&app, a, true);
        core.viewer_resize(&app, a, 120, 50);
        core.viewer_focus(&app, a, false);
        assert_eq!(grid(), (40, 10), "cycle {cycle}: the idle desktop never takes it back");
    }
    core.viewer_input(&app, a, b"x", false);
    assert_eq!(grid(), (120, 50), "a key on the desktop does");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// Item 71: a phone in a pocket (VISIBILITY 0) never leads, whatever it
/// sends; the shown focused desktop leads at once. Out of the pocket and
/// touched, the phone leads again.
#[test]
fn a_pocketed_phone_never_leads_and_the_shown_desktop_leads_at_once() {
    let (core, id, root) = core_with_screen(100, 40);
    let app = desktop_conn();
    let phone = Arc::new(Conn::default());
    let grid = || core.lock().screens[&id].size();
    let a = core.attach(&app, id, focus_request((120, 50), Some(("test", "test")), true, true)).unwrap();
    core.viewer_resize(&app, a, 120, 50);
    let p = core.attach(&phone, id, focus_request((40, 10), None, true, true)).unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    assert_eq!(grid(), (40, 10), "opening the pane on the phone is use");
    core.viewer_window_focus(&phone, p, false);
    core.viewer_visibility(&phone, p, false);
    assert_eq!(grid(), (120, 50), "pocketed: the shown focused desktop leads at once");
    core.viewer_input(&phone, p, b"x", false);
    core.viewer_window_focus(&phone, p, true);
    core.viewer_resize(&phone, p, 40, 11);
    assert_eq!(grid(), (120, 50), "a hidden phone never leads, whatever it sends");
    core.viewer_visibility(&phone, p, true);
    core.viewer_window_focus(&phone, p, true);
    assert_eq!(grid(), (40, 11), "shown and touched, it leads with its new size");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// Item 71 with two desktops: the latest used leads; when it is hidden, the
/// shown focused one leads; when neither is shown and focused, the grid is
/// held.
#[test]
fn two_desktops_the_latest_used_leads_and_a_hidden_one_hands_over_to_the_shown_one() {
    let (core, id, root) = core_with_screen(100, 40);
    let laptop = desktop_conn();
    let line = Arc::new(Conn::default()); // another machine's window, through its core
    let grid = || core.lock().screens[&id].size();
    let a = core.attach(&laptop, id, focus_request((120, 50), Some(("test", "test")), true, true)).unwrap();
    core.viewer_resize(&laptop, a, 120, 50);
    let b = core.attach(&line, id, focus_request((90, 30), Some(("pickle", "pickle")), true, true)).unwrap();
    core.viewer_resize(&line, b, 90, 30);
    assert_eq!(grid(), (90, 30), "the window attached focused last");
    core.viewer_input(&laptop, a, b"x", false);
    assert_eq!(grid(), (120, 50), "input on the laptop");
    core.viewer_input(&line, b, b"x", false);
    assert_eq!(grid(), (90, 30), "input on the other desktop");
    core.viewer_visibility(&line, b, false);
    assert_eq!(grid(), (120, 50), "the latest used is hidden: the shown focused one leads");
    core.viewer_window_focus(&laptop, a, false);
    assert_eq!(grid(), (120, 50), "neither shown and focused: held");
    core.viewer_visibility(&line, b, true);
    assert_eq!(grid(), (90, 30), "the latest used, shown again, leads");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// Item 71: observers never lead or contribute, whatever they send.
#[test]
fn observers_never_lead_whatever_they_send() {
    let (core, id, root) = core_with_screen(100, 40);
    let app = desktop_conn();
    let watcher = Arc::new(Conn::default());
    let grid = || core.lock().screens[&id].size();
    let a = core.attach(&app, id, focus_request((120, 50), Some(("test", "test")), true, true)).unwrap();
    core.viewer_resize(&app, a, 120, 50);
    let o = core
        .attach(&watcher, id, AttachRequest { observe: true, ..focus_request((40, 10), None, true, true) })
        .unwrap();
    core.viewer_resize(&watcher, o, 40, 10);
    core.viewer_input(&watcher, o, b"x", false);
    core.viewer_window_focus(&watcher, o, true);
    core.viewer_focus(&watcher, o, true);
    core.viewer_visibility(&watcher, o, true);
    assert_eq!(grid(), (120, 50));
    let list = core.viewer_list(id, "t1".into());
    let seen =
        list.viewers.iter().find(|v| v.identity == ViewerIdentity { connection_id: watcher.id, channel: o }).unwrap();
    assert!(!seen.leader && !seen.contributes_to_grid && !seen.size_eligible);
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// Item 71: an attach nobody caused just now (a reconnect, a reattach) says
/// how long ago the view was last used (`idle_ms`): older than the desktop's
/// last input, it takes nothing over. An attach a person caused is use.
#[test]
fn an_automatic_reattach_with_its_idle_age_takes_nothing_over() {
    let (core, id, root) = core_with_screen(100, 40);
    let app = desktop_conn();
    let phone = Arc::new(Conn::default());
    let grid = || core.lock().screens[&id].size();
    let a = core.attach(&app, id, focus_request((120, 50), Some(("test", "test")), true, true)).unwrap();
    core.viewer_resize(&app, a, 120, 50);
    std::thread::sleep(Duration::from_millis(5));
    core.viewer_input(&app, a, b"x", false);
    let p = core
        .attach(&phone, id, AttachRequest { idle_ms: Some(60_000), ..focus_request((40, 10), None, true, true) })
        .unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    assert_eq!(grid(), (120, 50), "a reconnect idle for a minute does not take over");
    core.detach(&phone, None);
    let p = core.attach(&phone, id, focus_request((40, 10), None, true, true)).unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    assert_eq!(grid(), (40, 10), "an attach a person caused is use");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// A phone-like viewer: interactive, no desktop group, gets RESIZE frames.
fn history_request(resize_history: usize) -> AttachRequest {
    AttachRequest {
        observe: false,
        resize_notifications: true,
        visible: true,
        visibility_updates: false,
        focus_updates: false,
        focused: false,
        idle_ms: None,
        history: 0,
        resize_history,
        size: (20, 4),
        display: Display::default(),
        resumed: false,
        desktop_group: None,
    }
}

/// 12 lines on a 20x4 grid: `row00`..`row08` in history, the rest on screen.
fn core_with_history() -> (Arc<Core>, u32, std::path::PathBuf, Arc<Conn>, u32) {
    let (core, id, root) = core_with_screen(20, 4);
    let text: String = (0..12).map(|n| format!("row{n:02}\r\n")).collect();
    core.lock().screens.get_mut(&id).unwrap().write(text.as_bytes());
    let desktop = desktop_conn();
    let mut req = focus_request((20, 4), Some(("line", "desktop")), false, false);
    req.resize_notifications = false;
    let channel = core.attach(&desktop, id, req).unwrap();
    core.viewer_resize(&desktop, channel, 20, 4);
    (core, id, root, desktop, channel)
}

/// Every frame queued on `channel` since the last call: (kind, payload).
fn frames(conn: &Arc<Conn>, channel: u32) -> Vec<(u8, Vec<u8>)> {
    let mut got = Vec::new();
    conn.with(|out| {
        out.resume(channel);
        got.extend(std::iter::from_fn(|| out.next_frame()).map(|f| (f[4], f[wire::HEADER..].to_vec())));
    });
    got
}

/// The RESIZE then the one paint a resize sent this viewer.
fn resize_and_paint(conn: &Arc<Conn>, channel: u32) -> Vec<u8> {
    let got = frames(conn, channel);
    assert_eq!(got.len(), 2, "RESIZE then one paint: {:?}", got.iter().map(|f| f.0).collect::<Vec<_>>());
    assert_eq!(got[0], (wire::RESIZE, vec![30, 0, 4, 0]));
    assert_eq!(got[1].0, wire::DATA);
    got[1].1.clone()
}

fn has(paint: &[u8], text: &str) -> bool {
    paint.windows(text.len()).any(|w| w == text.as_bytes())
}

#[test]
fn a_resize_history_viewer_gets_its_last_n_history_rows_after_clearing_its_own() {
    let (core, id, root, desktop, leader) = core_with_history();
    let phone = Arc::new(Conn::default());
    let channel = core.attach(&phone, id, history_request(3)).unwrap();
    frames(&phone, channel); // RESIZE + first paint at attach
    core.viewer_resize(&desktop, leader, 30, 4);
    core.queue_resize_paints(&mut core.lock(), id, None);
    let paint = resize_and_paint(&phone, channel);
    let mut start = vec![0x18];
    start.extend(ghostty_vt::PAINT_RESET);
    start.extend(b"\x1b[3J");
    assert!(paint.starts_with(&start), "CAN, the screen reset, then ESC[3J: {:?}", String::from_utf8_lossy(&paint));
    assert!(has(&paint, "row06") && has(&paint, "row07") && has(&paint, "row08"), "the last 3 history rows");
    assert!(!has(&paint, "row05"), "only the last 3");
    assert!(has(&paint, "row11"), "and the screen");
    // Replayed on a viewer with history of its own, the history is the core's.
    let mut screen = ghostty_vt::Terminal::new(30, 4, 1 << 20);
    screen.write(b"old1\r\nold2\r\nold3\r\nold4\r\nold5\r\nold6\r\n");
    screen.write(&paint);
    let all = screen.plain_text();
    assert!(!all.contains("old"), "ESC[3J dropped the viewer's own history: {all:?}");
    assert!(all.starts_with("row06\nrow07\nrow08\n") && all.ends_with("row09\nrow10\nrow11"), "{all:?}");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_viewer_without_resize_history_gets_the_screen_only_paint_byte_for_byte() {
    let params: crate::api::AttachParams =
        serde_json::from_value(serde_json::json!({"terminal": "t1", "cols": 20, "rows": 4})).unwrap();
    assert_eq!(params.resize_history, 0, "absent means 0");
    let (core, id, root, desktop, leader) = core_with_history();
    let phone = Arc::new(Conn::default());
    let channel = core.attach(&phone, id, history_request(params.resize_history)).unwrap();
    frames(&phone, channel);
    core.viewer_resize(&desktop, leader, 30, 4);
    core.queue_resize_paints(&mut core.lock(), id, None);
    let paint = resize_and_paint(&phone, channel);
    assert_eq!(paint, first_paint(&core.lock(), id, 0), "today's screen-only paint");
    assert!(!has(&paint, "\x1b[3J") && !has(&paint, "row08"));
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn viewers_with_different_resize_history_each_get_their_own_paint_built_once() {
    let (core, id, root, desktop, leader) = core_with_history();
    let views: Vec<(usize, Arc<Conn>, u32)> = [0, 0, 2, 5, 5]
        .into_iter()
        .map(|n| {
            let conn = Arc::new(Conn::default());
            let channel = core.attach(&conn, id, history_request(n)).unwrap();
            frames(&conn, channel);
            (n, conn, channel)
        })
        .collect();
    core.viewer_resize(&desktop, leader, 30, 4);
    PAINTS.set(0);
    core.queue_resize_paints(&mut core.lock(), id, None);
    assert_eq!(PAINTS.get(), 3, "one paint per distinct resize_history (0, 2, 5)");
    let paints: Vec<(usize, Vec<u8>)> =
        views.iter().map(|(n, conn, channel)| (*n, resize_and_paint(conn, *channel))).collect();
    assert_eq!(paints[0].1, paints[1].1);
    assert_eq!(paints[3].1, paints[4].1);
    let st = core.lock();
    assert_eq!(paints[0].1, first_paint(&st, id, 0));
    assert_eq!(paints[2].1, resize_paint(&st, id, 2));
    assert_eq!(paints[3].1, resize_paint(&st, id, 5));
    let p2 = &paints[2].1;
    assert!(has(p2, "\x1b[3J") && has(p2, "row07") && has(p2, "row08") && !has(p2, "row06"));
    let p5 = &paints[3].1;
    assert!(has(p5, "\x1b[3J") && has(p5, "row04") && has(p5, "row08") && !has(p5, "row03"));
    drop(st);
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// `wire` frames of one stream, as a viewer reads them: RESIZE, then the
/// paint's bytes in order however they were split.
fn read_resize_and_paint(conn: &Arc<Conn>, channel: u32) -> (Vec<u8>, Vec<u8>, usize) {
    let mut got = frames(conn, channel).into_iter();
    let (kind, size) = got.next().expect("a RESIZE");
    assert_eq!(kind, wire::RESIZE);
    let mut paint = Vec::new();
    let mut count = 0;
    for (kind, payload) in got {
        assert_eq!(kind, wire::DATA, "nothing but the paint after the RESIZE");
        paint.extend(payload);
        count += 1;
    }
    (size, paint, count)
}

/// Every cell changes color: thousands of rows format to megabytes.
fn core_with_heavy_history(rows: usize) -> (Arc<Core>, u32, std::path::PathBuf, Arc<Conn>, u32) {
    let (core, id, root) = core_with_screen(200, 4);
    let mut screen = ghostty_vt::Terminal::new(200, 4, 256 << 20);
    for n in 0..rows {
        let cells: String = (0..196)
            .map(|c| {
                let (r, g) = ((n * 7 + c) % 256, (n * 13 + c * 3) % 256);
                format!("\x1b[38;2;{r};{g};{c}m\x1b[48;2;{g};{c};{r}mx")
            })
            .collect();
        screen.write(format!("{n:04}{cells}\r\n").as_bytes());
    }
    core.lock().screens.insert(id, screen);
    let desktop = desktop_conn();
    let mut req = focus_request((200, 4), Some(("line", "desktop")), false, false);
    req.resize_notifications = false;
    let leader = core.attach(&desktop, id, req).unwrap();
    core.viewer_resize(&desktop, leader, 200, 4);
    (core, id, root, desktop, leader)
}

fn phone_on(core: &Arc<Core>, id: u32, resize_history: usize) -> (Arc<Conn>, u32) {
    let phone = Arc::new(Conn::default());
    let mut req = history_request(resize_history);
    req.size = (200, 4);
    let channel = core.attach(&phone, id, req).unwrap();
    // Plenty of credit: the whole paint comes out now, in 16 KiB frames.
    phone.with(|out| out.credit(channel, 64 << 20));
    frames(&phone, channel);
    (phone, channel)
}

/// Review v3 (4): when the whole requested history does not fit the budget,
/// the paint is the screen only, without ESC[3J: never fewer rows after a
/// clear, which would delete the viewer's older rows.
#[test]
fn a_history_paint_over_the_budget_is_the_screen_only_paint_without_clearing() {
    let (core, id, root, desktop, leader) = core_with_heavy_history(4000);
    let (phone, channel) = phone_on(&core, id, 1000);
    core.viewer_resize(&desktop, leader, 210, 4);
    {
        let st = core.lock();
        let full = finish_paint(&st, id, st.screens[&id].paint(1000)).len();
        assert!(full > RESIZE_PAINT_BUDGET, "the test needs a paint over the budget: {full}");
    }
    core.queue_resize_paints(&mut core.lock(), id, None);
    let (size, paint, _) = read_resize_and_paint(&phone, channel);
    assert_eq!(size, vec![210, 0, 4, 0]);
    assert_eq!(paint, first_paint(&core.lock(), id, 0), "the screen-only paint");
    assert!(!has(&paint, "\x1b[3J") && !has(&paint, "3996"));
    // Within the budget, the same pane gets its whole history replaced.
    let (phone, channel) = phone_on(&core, id, 200);
    core.viewer_resize(&desktop, leader, 200, 4);
    core.queue_resize_paints(&mut core.lock(), id, None);
    let (_, paint, _) = read_resize_and_paint(&phone, channel);
    assert!(paint.len() <= RESIZE_PAINT_BUDGET);
    assert!(has(&paint, "\x1b[3J") && has(&paint, "3797") && has(&paint, "3996") && !has(&paint, "3796"));
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// Review v3 (1): live output overflows the backlog while a history
/// replacement is only partly out (its ESC[3J already delivered). The viewer
/// must get a complete replacement again, never the screen-only recovery.
#[test]
fn a_history_replacement_cut_by_live_output_is_sent_again_whole() {
    let (core, id, root, desktop, leader) = core_with_heavy_history(4000);
    let phone = Arc::new(Conn::default());
    let mut req = history_request(200);
    req.size = (200, 4);
    let channel = core.attach(&phone, id, req).unwrap();
    frames(&phone, channel);
    // No credit left, like a slow phone.
    phone.with(|out| {
        assert!(out.data(channel, &vec![b'x'; wire::WINDOW * 2]));
        while out.next_frame().is_some() {}
    });
    core.viewer_resize(&desktop, leader, 210, 4);
    phone.with(|out| out.credit(channel, 64 * 1024));
    core.prepare_repaints(&phone);
    let full = resize_paint(&core.lock(), id, 200);
    let prefix: Vec<u8> = frames(&phone, channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    assert!(has(&prefix, "\x1b[3J") && prefix.len() < full.len(), "a partial history replacement");
    let mut viewer = ghostty_vt::Terminal::new(210, 4, 64 << 20);
    viewer.write(b"old1\r\nold2\r\nold3\r\nold4\r\nold5\r\n");
    viewer.write(&prefix);
    // SGR-only output changes no text, but overflows the backlog behind the paint (over 4 MiB).
    let sgr = b"\x1b[0m".repeat(8 * 1024);
    for _ in 0..140 {
        core.output(&mut core.lock(), id, &sgr);
    }
    phone.with(|out| out.credit(channel, 64 << 20));
    core.prepare_repaints(&phone);
    let rest: Vec<u8> = frames(&phone, channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    assert!(rest.starts_with(&[0x18]) && has(&rest, "\x1b[3J"), "a complete replacement again");
    viewer.write(&rest);
    let got = viewer.plain_text();
    assert!(got.contains("3797") && got.contains("3996"), "every replaced row is back");
    // Delivered whole: the next resize is the normal policy again.
    assert!(!phone.outbox().history_lost(channel));
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// A pane whose every cell has its own colors: `printed` lines of `cols`.
fn dense_core(cols: u16, rows: u16, printed: u32) -> (Arc<Core>, u32, std::path::PathBuf, Arc<Conn>, u32) {
    let (core, id, root) = core_with_screen(cols, rows);
    let mut screen = ghostty_vt::Terminal::new(cols, rows, 64 << 20);
    let mut text = Vec::new();
    for row in 0..printed {
        for col in 0..u32::from(cols) {
            let x = row * u32::from(cols) + col;
            let (a, b, c, d) = (x % 251, (x / 251) % 251, (x / 17) % 251, (x / 79) % 251);
            text.extend(format!("\x1b[38;2;{a};{b};123;48;2;45;{c};{d}mX").as_bytes());
        }
        text.extend(b"\r\n");
    }
    screen.write(&text);
    core.lock().screens.insert(id, screen);
    let desktop = desktop_conn();
    let mut req = focus_request((cols, rows), Some(("line", "desktop")), false, false);
    req.resize_notifications = false;
    let leader = core.attach(&desktop, id, req).unwrap();
    core.viewer_resize(&desktop, leader, cols, rows);
    (core, id, root, desktop, leader)
}

/// Review v3 (2): a screen bigger than the whole backlog limit is still
/// delivered after its RESIZE, and so is a phone's large first paint.
#[test]
fn paints_over_the_backlog_limit_are_delivered_not_dropped() {
    let (core, id, root, desktop, leader) = dense_core(200, 600, 610);
    let phone = Arc::new(Conn::default());
    let mut req = history_request(1000);
    req.size = (200, 600);
    let channel = core.attach(&phone, id, req).unwrap();
    phone.with(|out| out.credit(channel, 64 << 20));
    frames(&phone, channel);
    core.viewer_resize(&desktop, leader, 201, 600);
    let screen = first_paint(&core.lock(), id, 0);
    assert!(screen.len() > wire::MAX_PENDING, "the test needs a screen over the limit: {}", screen.len());
    core.prepare_repaints(&phone);
    let (size, paint, _) = read_resize_and_paint(&phone, channel);
    assert_eq!(size, vec![201, 0, 88, 2]);
    assert_eq!(paint, screen, "the whole screen-only paint (no history fits)");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();

    let (core, id, root, _, _) = dense_core(200, 4, 1100);
    let first = first_paint(&core.lock(), id, 1000);
    assert!(first.len() > wire::MAX_PENDING, "{}", first.len());
    let phone = Arc::new(Conn::default());
    let mut req = history_request(1000);
    req.history = 1000;
    let channel = core.attach(&phone, id, req).unwrap();
    phone.with(|out| out.credit(channel, 64 << 20));
    let got: Vec<u8> = frames(&phone, channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    assert_eq!(got, first, "the phone's first paint with 1000 history rows arrives whole");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// The phone relies on ESC[3J coming after the reset's ED 2, however the
/// paint is cut into frames: CAN, PAINT_RESET (ending ESC[H ESC[2J), ESC[3J,
/// the history, then the screen, and nothing else between.
#[test]
fn a_split_history_paint_keeps_can_reset_clear_history_screen_order_on_the_wire() {
    let (core, id, root, desktop, leader) = core_with_heavy_history(300);
    let (phone, channel) = phone_on(&core, id, 50);
    core.viewer_resize(&desktop, leader, 210, 4);
    core.queue_resize_paints(&mut core.lock(), id, None);
    let (_, paint, count) = read_resize_and_paint(&phone, channel);
    assert!(count > 1, "the test needs a paint split across frames: {} bytes", paint.len());
    assert_eq!(paint, resize_paint(&core.lock(), id, 50), "frames reassemble to the paint, in order");
    let at = |text: &[u8], from: usize| {
        from + paint[from..].windows(text.len()).position(|w| w == text).unwrap_or_else(|| panic!("{text:?}"))
    };
    assert_eq!(paint[0], 0x18, "CAN first");
    assert_eq!(&paint[1..1 + ghostty_vt::PAINT_RESET.len()], ghostty_vt::PAINT_RESET);
    assert!(ghostty_vt::PAINT_RESET.ends_with(b"\x1b[H\x1b[2J"), "the reset ends with its ED 2");
    let clear = 1 + ghostty_vt::PAINT_RESET.len();
    assert_eq!(&paint[clear..clear + 4], b"\x1b[3J", "ESC[3J right after the reset's ED 2");
    assert_eq!(paint.windows(4).filter(|w| w == b"\x1b[3J").count(), 1);
    let first = at(b"0247", clear);
    // Then the oldest of the 50 history rows, with nothing before it but its style (SGR).
    let mut rest = &paint[clear + 4..first];
    while let Some(sgr) = rest.strip_prefix(b"\x1b[") {
        let end = sgr.iter().position(|b| !b.is_ascii_digit() && *b != b';').unwrap();
        assert_eq!(sgr[end], b'm', "only SGR before the first history row");
        rest = &sgr[end + 1..];
    }
    assert!(rest.is_empty(), "only SGR before the first history row: {rest:?}");
    assert!(!has(&paint[..first], "0246"));
    let newest = at(b"0296", first);
    let screen = at(b"0297", newest);
    assert!(at(b"\x1b[H\x1b[2J", newest) < screen, "history pushed up, screen painted after");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn resize_history_is_capped_and_a_huge_count_never_wraps_to_none() {
    let (core, id, root, desktop, leader) = core_with_history();
    for asked in [MAX_RESIZE_HISTORY + 1, 1 << 32, usize::MAX] {
        let (phone, channel) = {
            let phone = Arc::new(Conn::default());
            let channel = core.attach(&phone, id, history_request(asked)).unwrap();
            frames(&phone, channel);
            (phone, channel)
        };
        let st = core.lock();
        let v = st.viewers[&id].iter().find(|v| Arc::ptr_eq(&v.conn, &phone)).unwrap();
        assert_eq!(v.resize_history, MAX_RESIZE_HISTORY, "clamped at attach");
        drop(st);
        let paint = resize_paint(&core.lock(), id, asked);
        assert!(has(&paint, "\x1b[3J") && has(&paint, "row00") && has(&paint, "row08"), "all history, not none");
        drop((phone, channel));
    }
    // ghostty itself saturates instead of wrapping (the attach `history` path).
    let paint = core.lock().screens[&id].paint(1 << 32);
    assert!(has(&paint, "row00"));
    drop((desktop, leader));
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn resize_history_needs_resize_notifications() {
    let (core, id, root, desktop, leader) = core_with_history();
    let viewer = Arc::new(Conn::default());
    let mut req = history_request(3);
    req.resize_notifications = false;
    let channel = core.attach(&viewer, id, req).unwrap();
    let st = core.lock();
    let v = st.viewers[&id].iter().find(|v| v.channel == channel && Arc::ptr_eq(&v.conn, &viewer)).unwrap();
    assert_eq!(v.resize_history, 0, "no RESIZE frames, so no history repaints");
    drop(st);
    drop((desktop, leader));
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}
// The #88 reviewers' probes (v2 and v3 reviews), kept as regression tests.

fn reviewer_dense(cols: u16, rows: u16, printed: u32) -> (Arc<Core>, u32, std::path::PathBuf, Arc<Conn>, u32) {
    let (core, id, root) = core_with_screen(cols, rows);
    let mut screen = ghostty_vt::Terminal::new(cols, rows, 64 << 20);
    let mut text = Vec::new();
    for row in 0..printed {
        for col in 0..u32::from(cols) {
            let x = row * u32::from(cols) + col;
            text.extend(
                format!(
                    "\x1b[38;2;{};{};{};48;2;{};{};{}mX",
                    x % 251,
                    (x / 251) % 251,
                    123,
                    45,
                    (x / 17) % 251,
                    (x / 79) % 251
                )
                .as_bytes(),
            );
        }
        text.extend(b"\r\n");
    }
    screen.write(&text);
    core.lock().screens.insert(id, screen);
    let desktop = desktop_conn();
    let mut req = focus_request((cols, rows), Some(("line", "desktop")), false, false);
    req.resize_notifications = false;
    let leader = core.attach(&desktop, id, req).unwrap();
    core.viewer_resize(&desktop, leader, cols, rows);
    (core, id, root, desktop, leader)
}

#[test]
fn reviewer_eight_mb_history_is_capped_and_ordered_across_frames() {
    let (core, id, root, desktop, leader) = reviewer_dense(200, 4, 1100);
    let phone = Arc::new(Conn::default());
    let channel = core.attach(&phone, id, history_request(1000)).unwrap();
    phone.with(|out| out.credit(channel, 64 << 20));
    frames(&phone, channel);
    core.viewer_resize(&desktop, leader, 201, 4);
    let full = first_paint(&core.lock(), id, 1000);
    let expected = resize_paint(&core.lock(), id, 1000);
    eprintln!("8MB fixture: full={} capped={} budget={}", full.len(), expected.len(), RESIZE_PAINT_BUDGET);
    assert!(full.len() > wire::MAX_PENDING);
    assert!(expected.len() <= RESIZE_PAINT_BUDGET);
    assert_eq!(expected, first_paint(&core.lock(), id, 0));
    core.prepare_repaints(&phone);
    core.output(&mut core.lock(), id, b"after-paint");
    let got = frames(&phone, channel);
    assert_eq!(got[0], (wire::RESIZE, vec![201, 0, 4, 0]));
    assert!(got.len() > 2);
    assert!(got[1..].iter().all(|(k, b)| *k == wire::DATA && b.len() <= 16 * 1024));
    let bytes: Vec<u8> = got[1..].iter().flat_map(|(_, b)| b.clone()).collect();
    assert_eq!(&bytes[..expected.len()], expected);
    assert_eq!(&bytes[expected.len()..], b"after-paint");
    assert_eq!(bytes[0], 0x18);
    assert_eq!(&bytes[1..1 + ghostty_vt::PAINT_RESET.len()], ghostty_vt::PAINT_RESET);
    assert!(!has(&bytes, "\x1b[3J"));

    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn reviewer_large_screen_fallback_must_deliver_or_remain_pending() {
    let (core, id, root, desktop, leader) = reviewer_dense(200, 600, 610);
    let phone = Arc::new(Conn::default());
    let mut req = history_request(1000);
    req.size = (200, 600);
    let channel = core.attach(&phone, id, req).unwrap();
    phone.with(|out| out.credit(channel, 64 << 20));
    frames(&phone, channel);
    core.viewer_resize(&desktop, leader, 201, 600);
    let painted = resize_paint(&core.lock(), id, 1000).len();
    let screen = first_paint(&core.lock(), id, 0).len();
    core.prepare_repaints(&phone);
    let queued = phone.outbox().queued(channel);
    let pending = core.lock().viewers[&id].iter().find(|v| Arc::ptr_eq(&v.conn, &phone)).unwrap().repaint_pending;
    eprintln!("large screen: resize={painted} screen_fallback={screen} queued={queued} repaint_pending={pending}");
    let got = frames(&phone, channel);
    eprintln!("large screen frames before later output={}", got.len());
    core.output(&mut core.lock(), id, b"unrelated-live-output");
    let late = frames(&phone, channel);
    eprintln!("large screen frames after later output={late:?}");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
    assert!(queued > 0 || pending, "both initial resize paint and fallback disappeared");
}

#[test]
fn reviewer_eight_mb_attach_history_must_not_disappear() {
    let (core, id, root, _, _) = reviewer_dense(200, 4, 1100);
    let phone = Arc::new(Conn::default());
    let mut req = history_request(1000);
    req.history = 1000;
    let len = first_paint(&core.lock(), id, 1000).len();
    let channel = core.attach(&phone, id, req).unwrap();
    let queued = phone.outbox().queued(channel);
    let pending = core.lock().viewers[&id].iter().find(|v| Arc::ptr_eq(&v.conn, &phone)).unwrap().repaint_pending;
    eprintln!("8MB initial attach: paint={len} queued={queued} repaint_pending={pending}");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
    assert!(queued > 0 || pending, "initial phone attach still ignores oversized history paint refusal");
}

#[test]
fn reviewer_v3_cut_paint_then_same_capability_reconnect_must_restore_history() {
    let (core, id, root, desktop, leader) = core_with_heavy_history(4000);
    let phone = Arc::new(Conn::default());
    let mut req = history_request(200);
    req.size = (200, 4);
    let channel = core.attach(&phone, id, req).unwrap();
    frames(&phone, channel);
    phone.with(|out| {
        assert!(out.data(channel, &vec![b'x'; wire::WINDOW * 2]));
        while out.next_frame().is_some() {}
    });
    core.viewer_resize(&desktop, leader, 210, 4);
    phone.with(|out| out.credit(channel, 64 * 1024));
    core.prepare_repaints(&phone);
    let full = resize_paint(&core.lock(), id, 200);
    let prefix: Vec<u8> = frames(&phone, channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    assert!(has(&prefix, "\x1b[3J") && prefix.len() < full.len());
    let mut viewer = ghostty_vt::Terminal::new(210, 4, 64 << 20);
    viewer.write(&prefix);
    // The owner sees its old core-line connection go away. The relaying core
    // keeps the viewer open, and reattach requests history=0,resumed=true.
    core.detach(&phone, Some(channel));
    let returned = Arc::new(Conn::default());
    let mut req = history_request(200);
    req.size = (200, 4);
    req.resumed = true;
    req.history = 0;
    let new_channel = core.attach(&returned, id, req).unwrap();
    returned.with(|out| out.credit(new_channel, 64 << 20));
    let rest: Vec<u8> =
        frames(&returned, new_channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    viewer.write(&rest);
    let got = viewer.plain_text();
    eprintln!(
        "same-capability reconnect: full={} prefix={} resumed={} resumed_ED3={} core_has_3996={} viewer_has_3996={}",
        full.len(),
        prefix.len(),
        rest.len(),
        has(&rest, "\x1b[3J"),
        core.lock().screens[&id].plain_text().contains("3996"),
        got.contains("3996")
    );
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        got.contains("3996"),
        "a confirmed history replacement was cut at line loss; same-capability reattach only repaints the screen"
    );
}

#[test]
fn reviewer_v3_forced_recovery_over_budget_keeps_all_rows_and_latest_resize() {
    let (core, id, root, desktop, leader) = core_with_heavy_history(100);
    let phone = Arc::new(Conn::default());
    let mut req = history_request(1000);
    req.size = (200, 4);
    let channel = core.attach(&phone, id, req).unwrap();
    frames(&phone, channel);
    phone.with(|out| {
        assert!(out.data(channel, &vec![b'x'; wire::WINDOW * 2]));
        while out.next_frame().is_some() {}
    });
    core.viewer_resize(&desktop, leader, 201, 4);
    phone.with(|out| out.credit(channel, 64 * 1024));
    core.prepare_repaints(&phone);
    let prefix: Vec<u8> = frames(&phone, channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    assert!(has(&prefix, "\x1b[3J"));
    let mut text = Vec::new();
    for n in 100..650 {
        text.extend(format!("{n:04}").as_bytes());
        for c in 0..196 {
            text.extend(
                format!(
                    "\x1b[38;2;{};{};{};48;2;{};{};{}mx",
                    n % 251,
                    c % 251,
                    (n + c) % 251,
                    c % 251,
                    (n * 7 + c) % 251,
                    123
                )
                .as_bytes(),
            );
        }
        text.extend(b"\r\n");
    }
    for chunk in text.chunks(32 * 1024) {
        core.output(&mut core.lock(), id, chunk);
    }
    core.viewer_resize(&desktop, leader, 202, 4);
    core.viewer_resize(&desktop, leader, 203, 4);
    assert!(phone.outbox().history_lost(channel));
    let (expected, clears) = resize_paint_for(&core.lock(), id, 1000, true);
    assert!(clears && expected.len() > RESIZE_PAINT_BUDGET);
    let fallback = resize_paint(&core.lock(), id, 1000);
    assert!(!has(&fallback, "\x1b[3J"));
    phone.with(|out| out.credit(channel, 64 << 20));
    core.prepare_repaints(&phone);
    let (size, actual, _) = read_resize_and_paint(&phone, channel);
    assert_eq!(size, vec![203, 0, 4, 0]);
    assert_eq!(actual, expected);
    assert!(!phone.outbox().history_lost(channel));
    PAINTS.set(0);
    core.prepare_repaints(&phone);
    assert_eq!(PAINTS.get(), 0, "completed repair must not repeat");
    core.viewer_resize(&desktop, leader, 204, 4);
    core.prepare_repaints(&phone);
    let (_, fresh, _) = read_resize_and_paint(&phone, channel);
    assert!(!has(&fresh, "\x1b[3J"), "fresh oversized resize returns to screen-only policy");
    eprintln!("forced whole recovery: {} bytes, fallback {} bytes, one latest RESIZE", actual.len(), fallback.len());
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn reviewer_v3_initial_history_paint_survives_live_output_before_writer() {
    let (core, id, root, _, _) = reviewer_dense(200, 4, 1100);
    let phone = Arc::new(Conn::default());
    let mut req = history_request(1000);
    req.history = 1000;
    let expected = first_paint(&core.lock(), id, 1000);
    let channel = core.attach(&phone, id, req).unwrap();
    // Output can run before the separate connection writer sends the first
    // paint. A tiny write should fit the bounded live backlog behind it.
    core.output(&mut core.lock(), id, b"\x1b[0m");
    phone.with(|out| out.resume(channel));
    core.prepare_repaints(&phone);
    phone.with(|out| out.credit(channel, 64 << 20));
    let got: Vec<u8> = frames(&phone, channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    eprintln!(
        "initial whole paint plus live output: expected={} got={} ED3={}",
        expected.len(),
        got.len(),
        has(&got, "\x1b[3J")
    );
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        got.starts_with(&expected),
        "ordinary live output discarded the whole attach history paint before the writer could send it"
    );
}

#[test]
fn reviewer_v3_cut_history_then_alt_screen_must_recover_on_main_screen_return() {
    let (core, id, root, desktop, leader) = core_with_heavy_history(4000);
    let phone = Arc::new(Conn::default());
    let mut req = history_request(200);
    req.size = (200, 4);
    let channel = core.attach(&phone, id, req).unwrap();
    frames(&phone, channel);
    phone.with(|out| {
        assert!(out.data(channel, &vec![b'x'; wire::WINDOW * 2]));
        while out.next_frame().is_some() {}
    });
    core.viewer_resize(&desktop, leader, 210, 4);
    phone.with(|out| out.credit(channel, 64 * 1024));
    core.prepare_repaints(&phone);
    let prefix: Vec<u8> = frames(&phone, channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    assert!(has(&prefix, "\x1b[3J"));
    let mut viewer = ghostty_vt::Terminal::new(210, 4, 64 << 20);
    viewer.write(&prefix);
    let sgr = b"\x1b[0m".repeat(8 * 1024);
    for _ in 0..140 {
        core.output(&mut core.lock(), id, &sgr);
    }
    core.output(&mut core.lock(), id, b"\x1b[?1049halternate");
    phone.with(|out| out.credit(channel, 64 << 20));
    core.prepare_repaints(&phone);
    let alt: Vec<u8> = frames(&phone, channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    viewer.write(&alt);
    assert!(viewer.alt_screen() && phone.outbox().history_lost(channel));
    core.output(&mut core.lock(), id, b"\x1b[?1049l");
    core.prepare_repaints(&phone);
    let back: Vec<u8> = frames(&phone, channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    viewer.write(&back);
    assert!(!viewer.alt_screen());
    let got = viewer.plain_text();
    let pending = core.lock().viewers[&id].iter().find(|v| Arc::ptr_eq(&v.conn, &phone)).unwrap().repaint_pending;
    eprintln!(
        "alt-screen recovery: alt={} back={} history_lost={} pending={pending} core_has_3996={} viewer_has_3996={}",
        alt.len(),
        back.len(),
        phone.outbox().history_lost(channel),
        core.lock().screens[&id].plain_text().contains("3996"),
        got.contains("3996")
    );
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
    assert!(
        got.contains("3996"),
        "alternate-screen retry consumed repaint_pending without restoring the cut main-screen history on return"
    );
}

#[test]
fn reviewer_v3_n1000_live_backpressure_uses_screen_only_without_clearing() {
    let (core, id, root, desktop, leader) = core_with_heavy_history(4000);
    let phone = Arc::new(Conn::default());
    let mut req = history_request(1000);
    req.size = (200, 4);
    let channel = core.attach(&phone, id, req).unwrap();
    frames(&phone, channel);
    phone.with(|out| {
        assert!(out.data(channel, &vec![b'x'; wire::WINDOW * 2]));
        while out.next_frame().is_some() {}
    });
    core.viewer_resize(&desktop, leader, 210, 4);
    phone.with(|out| out.credit(channel, 64 * 1024));
    core.prepare_repaints(&phone);
    let prefix: Vec<u8> = frames(&phone, channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    assert_eq!(prefix, first_paint(&core.lock(), id, 0));
    assert!(!has(&prefix, "\x1b[3J"));
    let sgr = b"\x1b[0m".repeat(8 * 1024);
    for _ in 0..140 {
        core.output(&mut core.lock(), id, &sgr);
    }
    phone.with(|out| out.credit(channel, 64 << 20));
    core.prepare_repaints(&phone);
    let after: Vec<u8> = frames(&phone, channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    assert_eq!(after, first_paint(&core.lock(), id, 0));
    assert!(!has(&after, "\x1b[3J"));
    eprintln!(
        "N1000 oversized history: first {} bytes, recovery {} bytes, both screen-only without ED3",
        prefix.len(),
        after.len()
    );
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn reviewer_live_backpressure_must_not_replace_partial_history_with_screen_only() {
    let (core, id, root, desktop, leader) = core_with_heavy_history(4000);
    let phone = Arc::new(Conn::default());
    let mut req = history_request(200);
    req.size = (200, 4);
    let channel = core.attach(&phone, id, req).unwrap();
    frames(&phone, channel);
    // Leave no credit, like a slow phone; the next resize drops this filler.
    phone.with(|out| {
        assert!(out.data(channel, &vec![b'x'; wire::WINDOW * 2]));
        while out.next_frame().is_some() {}
    });
    core.viewer_resize(&desktop, leader, 210, 4);
    phone.with(|out| out.credit(channel, 64 * 1024));
    core.prepare_repaints(&phone);
    let full = resize_paint(&core.lock(), id, 200);
    let partial = frames(&phone, channel);
    let prefix: Vec<u8> = partial.iter().filter(|(k, _)| *k == wire::DATA).flat_map(|(_, b)| b.clone()).collect();
    assert!(has(&prefix, "\x1b[3J"));
    assert!(prefix.len() < full.len());
    let mut viewer = ghostty_vt::Terminal::new(210, 4, 64 << 20);
    viewer.write(b"old1\r\nold2\r\nold3\r\nold4\r\nold5\r\n");
    viewer.write(&prefix);
    // Ordinary SGR output changes no text/history, but fills the unsent
    // backlog in bounded PTY-sized writes while the phone is out of credit.
    let sgr = b"\x1b[0m".repeat(8 * 1024);
    for _ in 0..100 {
        core.output(&mut core.lock(), id, &sgr);
    }
    phone.with(|out| out.credit(channel, 64 << 20));
    core.prepare_repaints(&phone);
    let later = frames(&phone, channel);
    let suffix: Vec<u8> = later.iter().filter(|(k, _)| *k == wire::DATA).flat_map(|(_, b)| b.clone()).collect();
    viewer.write(&suffix);
    let got = viewer.plain_text();
    let retained = core.lock().screens[&id].plain_text().contains("3996");
    eprintln!(
        "pressure: full={} prefix={} remaining={} replacement_has_ED3={} core_retains_3996={retained} viewer_retains_3996={}",
        full.len(),
        prefix.len(),
        suffix.len(),
        has(&suffix, "\x1b[3J"),
        got.contains("3996")
    );
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
    assert!(retained);
    assert!(
        got.contains("3996"),
        "live-output overflow superseded the unsent history paint with a screen-only paint after ED3 had already cleared the phone"
    );
}

/// Review v4 (b), at the owner: a stream resumed after a dropped line owes
/// a whole replacement (here over 2 MiB) from the start. If output
/// overflows the backlog behind it before any of it went out (the stream is
/// still paused for the attach reply), the repair is still owed, and what
/// arrives is the whole replacement, never the screen-only paint.
#[test]
fn a_resumed_repair_dropped_before_it_went_out_is_still_owed() {
    let (core, id, root, _, _) = reviewer_dense(200, 4, 1100);
    let phone = Arc::new(Conn::default());
    let mut req = history_request(1000);
    req.resumed = true;
    let channel = core.attach(&phone, id, req).unwrap();
    assert!(phone.outbox().history_lost(channel), "the dropped line is the loss");
    let sgr = b"\x1b[0m".repeat(8 * 1024);
    for _ in 0..140 {
        core.output(&mut core.lock(), id, &sgr);
    }
    let (expected, clears) = resize_paint_for(&core.lock(), id, 1000, true);
    assert!(clears && expected.len() > RESIZE_PAINT_BUDGET);
    phone.with(|out| {
        out.resume(channel);
        out.credit(channel, 64 << 20);
    });
    core.prepare_repaints(&phone);
    let got: Vec<u8> = frames(&phone, channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    assert!(got.starts_with(&expected), "the whole replacement: {} of {} bytes", got.len(), expected.len());
    assert!(!phone.outbox().history_lost(channel), "repaid once it is out whole");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// Review v4 (a), at the owner: resumed on the alternate screen, the repair
/// stays owed and goes out when the program is back on the main screen.
#[test]
fn a_resumed_stream_on_the_alternate_screen_is_repaired_on_return() {
    let (core, id, root, _, _) = core_with_heavy_history(400);
    core.output(&mut core.lock(), id, b"\x1b[?1049halt");
    let phone = Arc::new(Conn::default());
    let mut req = history_request(200);
    req.size = (200, 4);
    req.resumed = true;
    let channel = core.attach(&phone, id, req).unwrap();
    phone.with(|out| out.credit(channel, 64 << 20));
    let alt: Vec<u8> = frames(&phone, channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    assert!(!has(&alt, "\x1b[3J"), "no history on the alternate screen");
    assert!(phone.outbox().repair_owed(channel));
    core.output(&mut core.lock(), id, b"\x1b[?1049l");
    core.prepare_repaints(&phone);
    let back: Vec<u8> = frames(&phone, channel).into_iter().filter(|f| f.0 == wire::DATA).flat_map(|f| f.1).collect();
    assert_eq!(back, resize_paint_for(&core.lock(), id, 200, true).0, "the whole replacement");
    assert!(!phone.outbox().history_lost(channel));
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// Item 71 (t1jf's decision on #94's review): a phone that sends no focus
/// leads only by real input. After the desktop's input, none of these takes
/// the grid: a changed size, being shown again, an empty frame, a fresh
/// reconnect, a resumed size. Typing on it does.
#[test]
fn a_legacy_phone_takes_the_grid_only_by_real_input() {
    let (core, id, root) = core_with_screen(100, 40);
    let app = desktop_conn();
    let phone = Arc::new(Conn::default());
    let grid = || core.lock().screens[&id].size();
    let a = core.attach(&app, id, focus_request((120, 50), Some(("test", "test")), true, true)).unwrap();
    core.viewer_resize(&app, a, 120, 50);
    let p = core.attach(&phone, id, focus_request((40, 10), None, false, false)).unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    core.viewer_input(&phone, p, b"x", false);
    assert_eq!(grid(), (40, 10), "typed on: the phone leads");
    core.viewer_input(&app, a, b"x", false);
    assert_eq!(grid(), (120, 50), "the desktop's input takes it back");
    core.viewer_resize(&phone, p, 41, 10);
    assert_eq!(grid(), (120, 50), "a changed size is not use");
    core.viewer_visibility(&phone, p, false);
    core.viewer_visibility(&phone, p, true);
    assert_eq!(grid(), (120, 50), "shown again is not use");
    core.viewer_input(&phone, p, b"", false);
    assert_eq!(grid(), (120, 50), "an empty frame is not use");
    core.detach(&phone, None);
    let p = core.attach(&phone, id, focus_request((40, 10), None, false, false)).unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    assert_eq!(grid(), (120, 50), "a fresh reconnect is not use");
    core.detach(&phone, None);
    let p = core
        .attach(&phone, id, AttachRequest { resumed: true, ..focus_request((40, 10), None, false, false) })
        .unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    assert_eq!(grid(), (120, 50), "a resumed size is not use");
    // (A resumed stream's first keys are judged as a possible reply tail
    // until a byte decides: Enter does, within the frame.)
    core.viewer_input(&phone, p, b"y\r", false);
    assert_eq!(grid(), (40, 10), "typing on it is");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// Item 71: a terminal reply cut across DATA frames (a path split the
/// write) is still a reply, not use; a key that follows is use.
#[test]
fn a_reply_cut_across_frames_is_still_not_use() {
    let (core, id, root) = core_with_screen(100, 40);
    let app = desktop_conn();
    let phone = Arc::new(Conn::default());
    let grid = || core.lock().screens[&id].size();
    let a = core.attach(&app, id, focus_request((120, 50), Some(("test", "test")), true, true)).unwrap();
    core.viewer_resize(&app, a, 120, 50);
    let p = core.attach(&phone, id, focus_request((40, 10), None, false, false)).unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    core.viewer_input(&phone, p, b"x", false);
    assert_eq!(grid(), (40, 10));
    for cut in
        [&[&b"\x1b[24;"[..], b"80R"][..], &[b"\x1b", b"[?62;22c"], &[b"\x1b]11;rgb:0000/", b"0000/0000\x1b", b"\\"]]
    {
        for piece in cut {
            core.viewer_input(&app, a, piece, false);
        }
        assert_eq!(grid(), (40, 10), "a reply in pieces is not use: {cut:?}");
    }
    core.viewer_input(&app, a, b"\x1b", false);
    assert_eq!(grid(), (40, 10), "a lone ESC waits: a reply may follow it");
    core.viewer_input(&app, a, b"x", false);
    assert_eq!(grid(), (120, 50), "the next key makes it use");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// Item 71 (#94 review): `idle_ms` at or above a year, such as the phone's
/// i64::MAX "never used" sentinel or a relay's u64::MAX, is no focus history
/// at all, also on Linux where `Instant::checked_sub` of it does not fail.
/// Such a phone gains no focus authority: an old legacy desktop keeps
/// leading, and the phone leads only once used.
#[test]
fn a_never_used_idle_age_is_no_focus_history() {
    for never in [NEVER_USED_MS, i64::MAX as u64, u64::MAX] {
        assert_eq!(used_ago(Some(never)), None, "{never}");
    }
    let (core, id, root) = core_with_screen(100, 40);
    let old = desktop_conn();
    let phone = Arc::new(Conn::default());
    let grid = || core.lock().screens[&id].size();
    // An old desktop app: a group, no focus reports.
    let d = core.attach(&old, id, focus_request((120, 50), Some(("test", "test")), false, false)).unwrap();
    core.viewer_resize(&old, d, 120, 50);
    assert_eq!(grid(), (120, 50));
    for never in [i64::MAX as u64, u64::MAX] {
        let p = core
            .attach(&phone, id, AttachRequest { idle_ms: Some(never), ..focus_request((40, 10), None, true, true) })
            .unwrap();
        core.viewer_resize(&phone, p, 40, 10);
        let list = core.viewer_list(id, "t1".into());
        assert!(!list.focus_authority, "a never-used phone has no focus history ({never})");
        assert!(list.viewers.iter().all(|v| v.focused_ms_ago.is_none()));
        assert_eq!(grid(), (120, 50), "the old desktop keeps leading");
        core.detach(&phone, None);
    }
    let p = core
        .attach(&phone, id, AttachRequest { idle_ms: Some(1_000), ..focus_request((40, 10), None, true, true) })
        .unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    assert!(core.viewer_list(id, "t1".into()).focus_authority, "used a second ago: history");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

// t218's probes from the #94 review (hm-pr94-artifacts), kept as regression tests.

#[test]
fn reviewer94_exact_case_and_alternating_real_use() {
    let (core, id, root) = core_with_screen(200, 120);
    let app = desktop_conn();
    let phone = Arc::new(Conn::default());
    let a = core.attach(&app, id, focus_request((58, 132), Some(("desktop", "main")), true, true)).unwrap();
    core.viewer_resize(&app, a, 58, 132);
    {
        let mut st = core.lock();
        let v = &mut st.viewers.get_mut(&id).unwrap()[0];
        v.focused_at = Instant::now().checked_sub(Duration::from_secs(6060));
    }
    let p = core.attach(&phone, id, focus_request((160, 72), None, false, false)).unwrap();
    core.viewer_resize(&phone, p, 160, 72);
    core.viewer_input(&phone, p, b"x", false);
    assert_eq!(core.lock().screens[&id].size(), (160, 72));
    for _ in 0..50 {
        core.viewer_input(&app, a, b"\x1b[<0;12;3M", false);
        assert_eq!(core.lock().screens[&id].size(), (58, 132));
        core.viewer_input(&phone, p, b"hello\r", false);
        assert_eq!(core.lock().screens[&id].size(), (160, 72));
    }
    core.viewer_visibility(&phone, p, false);
    assert_eq!(core.lock().screens[&id].size(), (58, 132));
    core.viewer_visibility(&app, a, false);
    assert!(core.viewer_list(id, "t1".into()).selected_grid.is_none());
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn reviewer94_reporter_output_and_automatic_noise_do_not_flap() {
    let (core, id, root) = core_with_screen(200, 120);
    let app = desktop_conn();
    let phone = Arc::new(Conn::default());
    let a = core.attach(&app, id, focus_request((58, 132), Some(("desktop", "main")), true, true)).unwrap();
    core.viewer_resize(&app, a, 58, 132);
    let p = core.attach(&phone, id, focus_request((160, 72), None, true, true)).unwrap();
    core.viewer_resize(&phone, p, 160, 72);
    for n in 0..100 {
        {
            let mut st = core.lock();
            core.output(&mut st, id, b"busy output\r\n\x1b[6n\x1b[c\x1b]11;?\x07");
        }
        core.viewer_input(&app, a, b"\x1b[2;4R", false);
        core.viewer_input(&app, a, b"\x1b[?1;2c\x1b]11;rgb:ffff/0000/0000\x07", false);
        core.viewer_input(&app, a, b"\x1b[2;4R", true);
        core.viewer_input(&app, a, b"", false);
        core.viewer_resize(&app, a, 58 + (n % 2), 132);
        core.viewer_visibility(&app, a, false);
        core.viewer_visibility(&app, a, true);
        assert_eq!(core.lock().screens[&id].size(), (160, 72), "cycle {n}");
    }
    core.viewer_input(&app, a, b"\r", false);
    assert_eq!(core.lock().screens[&id].size(), (59, 132));
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn reviewer94_legacy_automatic_events_must_not_take_over() {
    let mut stolen = Vec::new();
    for event in ["resize", "show", "empty-data", "automatic-reconnect", "resumed-resize"] {
        let (core, id, root) = core_with_screen(200, 120);
        let app = desktop_conn();
        let phone = Arc::new(Conn::default());
        let a = core.attach(&app, id, focus_request((58, 132), Some(("desktop", "main")), true, true)).unwrap();
        core.viewer_resize(&app, a, 58, 132);
        let mut p = core.attach(&phone, id, focus_request((160, 72), None, false, false)).unwrap();
        core.viewer_resize(&phone, p, 160, 72);
        if event == "show" {
            core.viewer_visibility(&phone, p, false);
        }
        core.viewer_input(&app, a, b"x", false);
        assert_eq!(core.lock().screens[&id].size(), (58, 132));
        match event {
            "resize" => core.viewer_resize(&phone, p, 161, 72),
            "show" => core.viewer_visibility(&phone, p, true),
            "empty-data" => core.viewer_input(&phone, p, b"", false),
            "automatic-reconnect" | "resumed-resize" => {
                core.detach(&phone, None);
                p = core
                    .attach(
                        &phone,
                        id,
                        AttachRequest {
                            resumed: event == "resumed-resize",
                            idle_ms: Some(60_000),
                            ..focus_request((160, 72), None, false, false)
                        },
                    )
                    .unwrap();
                core.viewer_resize(&phone, p, 160, 72);
            }
            _ => unreachable!(),
        }
        let grid = core.lock().screens[&id].size();
        if grid != (58, 132) {
            stolen.push((event, grid));
        }
        drop(core);
        std::fs::remove_dir_all(root).unwrap();
    }
    assert!(stolen.is_empty(), "automatic legacy phone takeovers: {stolen:?}");
}

#[test]
fn reviewer94_fragmented_cpr_must_not_take_over() {
    let (core, id, root) = core_with_screen(200, 120);
    let app = desktop_conn();
    let phone = Arc::new(Conn::default());
    let a = core.attach(&app, id, focus_request((58, 132), Some(("desktop", "main")), true, true)).unwrap();
    core.viewer_resize(&app, a, 58, 132);
    let p = core.attach(&phone, id, focus_request((160, 72), None, true, true)).unwrap();
    core.viewer_resize(&phone, p, 160, 72);
    core.viewer_input(&app, a, b"\x1b[24;80R", false);
    assert_eq!(core.lock().screens[&id].size(), (160, 72));
    core.viewer_input(&app, a, b"\x1b[24;", false);
    core.viewer_input(&app, a, b"80R", false);
    let got = core.lock().screens[&id].size();
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(got, (160, 72), "same CPR divided between DATA frames is still automatic");
}

#[test]
fn reviewer94_never_used_and_multi_day_idle_ordering() {
    let never = used_ago(Some(i64::MAX as u64));
    let oldest = used_ago(Some(u64::MAX));
    let days = used_ago(Some(5 * 24 * 60 * 60 * 1000));
    assert!(never < days && oldest < days, "never={never:?}, u64max={oldest:?}, days={days:?}");
    let conn = Arc::new(Conn::default());
    let mut desktop = viewer(&conn, (58, 132), days);
    desktop.focus_updates = true;
    desktop.focused = true;
    desktop.focused_at = days;
    desktop.desktop_group = Some(("d".into(), "d".into()));
    let mut phone = viewer(&conn, (160, 72), never);
    phone.focus_updates = true;
    phone.focused = true;
    phone.focused_at = never;
    assert_eq!(select_sizing(&[desktop, phone], true).size, Some((58, 132)));
    eprintln!("safe idle sentinel: i64MAX={never:?}, u64MAX={oldest:?}, 5days={days:?}");
}

#[test]
fn reviewer94_modified_f3_ambiguity_is_wider_than_ctrl_only() {
    for modifier in 2..=16 {
        let key = format!("\x1b[1;{modifier}R");
        assert!(crate::input::terminal_reply(key.as_bytes()), "modifier {modifier}");
    }
    assert!(!crate::input::terminal_reply(b"\x1bOR"));
    assert!(!crate::input::terminal_reply(b"\x1b[1;5Rx"));
}

#[test]
fn reviewer94_five_viewers_loaded_from_wolfs_actual_capture() {
    let captured: serde_json::Value = serde_json::from_str(include_str!("wolf-t16b-1407.json")).unwrap();
    let now = Instant::now();
    let at = |value: &serde_json::Value| value.as_u64().and_then(|ms| now.checked_sub(Duration::from_millis(ms)));
    let mut vs: Vec<Viewer> = captured["viewers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|raw| {
            let conn = Arc::new(Conn::default());
            let mut v = viewer(
                &conn,
                (raw["offered"]["cols"].as_u64().unwrap() as u16, raw["offered"]["rows"].as_u64().unwrap() as u16),
                at(&raw["active_ms_ago"]),
            );
            v.channel = raw["identity"]["channel"].as_u64().unwrap() as u32;
            v.focus_updates = raw["focus_updates"] == true;
            v.focused = raw["focused"] == true;
            v.focused_at = at(&raw["focused_ms_ago"]);
            // The capture predates input_ms_ago. The phone was in Wolf's hands,
            // so a viewer that sends no focus counts its activity as input.
            v.input_at = at(&raw["input_ms_ago"]).or(if raw["focus_updates"] == true {
                None
            } else {
                at(&raw["active_ms_ago"])
            });
            v.visible = raw["visible"] == true;
            v.sized = raw["sized"] == true;
            v.awaiting_size = raw["awaiting_size"] == true;
            v.resize_notifications = raw["resize_notifications"] == true;
            v.visibility_updates = raw["visibility_updates"] == true;
            v.desktop_group = raw["desktop_group"].as_object().map(|_| {
                (
                    raw["desktop_group"]["line"].as_str().unwrap().into(),
                    raw["desktop_group"]["group"].as_str().unwrap().into(),
                )
            });
            v
        })
        .collect();
    assert_eq!(vs.len(), 5);
    for grouped in [false, true] {
        let s = select_sizing(&vs, grouped);
        assert_eq!(s.size, Some((160, 72)));
        assert_eq!(s.leader, Some(4));
        assert_eq!(s.contributors, vec![4]);
    }
    vs[1].input_at = Some(Instant::now());
    for grouped in [false, true] {
        let s = select_sizing(&vs, grouped);
        assert_eq!(s.size, Some((58, 132)));
        assert_eq!(s.leader, Some(1));
    }
}

#[test]
fn reviewer94_never_used_sentinel_must_not_create_focus_authority() {
    let (core, id, root) = core_with_screen(200, 120);
    let old = desktop_conn();
    let phone = Arc::new(Conn::default());
    let a = core.attach(&old, id, focus_request((58, 132), Some(("old", "old")), false, false)).unwrap();
    core.viewer_resize(&old, a, 58, 132);
    let p = core
        .attach(
            &phone,
            id,
            AttachRequest { idle_ms: Some(i64::MAX as u64), ..focus_request((160, 72), None, true, true) },
        )
        .unwrap();
    core.viewer_resize(&phone, p, 160, 72);
    let got = core.viewer_list(id, "t1".into());
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
    assert!(!got.focus_authority, "never used must not create focus history and exclude the old desktop");
}

/// Item 71 (#94 review): a focus-reporting phone attached as never used has
/// no history until its next WINDOW_FOCUS(1) or real input; real input alone
/// is enough to decide and lead.
#[test]
fn a_never_used_reporter_decides_once_typed_into() {
    let (core, id, root) = core_with_screen(100, 40);
    let app = desktop_conn();
    let phone = Arc::new(Conn::default());
    let grid = || core.lock().screens[&id].size();
    let a = core.attach(&app, id, focus_request((120, 50), Some(("test", "test")), true, true)).unwrap();
    core.viewer_resize(&app, a, 120, 50);
    let p = core
        .attach(
            &phone,
            id,
            AttachRequest { idle_ms: Some(i64::MAX as u64), ..focus_request((40, 10), None, true, true) },
        )
        .unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    assert_eq!(grid(), (120, 50), "never used: no history");
    core.viewer_input(&phone, p, b"", false);
    core.viewer_input(&phone, p, b"\x1b[24;80R", false);
    assert_eq!(grid(), (120, 50), "an empty frame or a reply is not use");
    core.viewer_input(&phone, p, b"x", false);
    assert_eq!(grid(), (40, 10), "typed into: it decides and leads, no WINDOW_FOCUS needed");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// Item 71 (#94 review, live repro): an old phone (no focus) typed into
/// just before a relay's line dropped keeps that input recency through the
/// reattach (`idle_ms`): it still leads over a desktop idle for an hour.
/// Without an age, a resumed attach is not use.
#[test]
fn a_legacy_viewer_resumed_through_a_relay_keeps_its_input_recency() {
    let (core, id, root) = core_with_screen(100, 40);
    let app = desktop_conn();
    let line = Arc::new(Conn::default());
    let grid = || core.lock().screens[&id].size();
    let a = core
        .attach(
            &app,
            id,
            AttachRequest { idle_ms: Some(3_600_000), ..focus_request((58, 132), Some(("d", "d")), true, true) },
        )
        .unwrap();
    core.viewer_resize(&app, a, 58, 132);
    let resumed = |idle_ms| AttachRequest { resumed: true, idle_ms, ..focus_request((160, 72), None, false, false) };
    let p = core.attach(&line, id, resumed(Some(5_000))).unwrap();
    core.viewer_resize(&line, p, 160, 72);
    assert_eq!(grid(), (160, 72), "typed 5 s ago, before the line dropped: it still leads");
    core.detach(&line, None);
    let p = core.attach(&line, id, resumed(None)).unwrap();
    core.viewer_resize(&line, p, 160, 72);
    assert_eq!(grid(), (58, 132), "no age: a resumed attach is not use");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// Item 71 (#94 review): a reply cut by a reconnect, its start on the old
/// stream and its tail on the resumed one (in one frame or several), is not
/// use; the frames are delivered as they came. A real key there still
/// counts, within the frame that shows it.
#[test]
fn a_reply_tail_first_on_a_resumed_stream_is_not_use() {
    for (tail, then) in [
        (&[&b"80R"[..]][..], &b"x"[..]),
        (&[b"4;8", b"0R"], b"x"),
        (&[b"[?62;22c"], b"x"),
        (&[b"0000/0000\x1b\\"], b"x"),
        // t218: cut at the reconnect and split again after it.
        (&[b"8", b"0R"], b"x"),
        (&[b"62", b";22", b"c"], b"x"),
        (&[b"24;", b"80t"], b"x"),
        (&[b"11;rgb:00", b"00/0000/0000\x1b", b"\\"], b"x"),
    ] {
        let (core, id, root) = core_with_screen(100, 40);
        let line = Arc::new(Conn::default());
        let phone = Arc::new(Conn::default());
        let grid = || core.lock().screens[&id].size();
        let p = core.attach(&phone, id, focus_request((40, 10), None, true, true)).unwrap();
        core.viewer_resize(&phone, p, 40, 10);
        // The desktop, back through a relay after a dropped line, idle for an hour.
        let a = core
            .attach(
                &line,
                id,
                AttachRequest {
                    resumed: true,
                    idle_ms: Some(3_600_000),
                    ..focus_request((58, 132), Some(("d", "d")), true, true)
                },
            )
            .unwrap();
        core.viewer_resize(&line, a, 58, 132);
        assert_eq!(grid(), (40, 10));
        for piece in tail {
            core.viewer_input(&line, a, piece, false);
        }
        assert_eq!(grid(), (40, 10), "a reply's tail after the reconnect is not use: {tail:?}");
        core.viewer_input(&line, a, then, false);
        assert_eq!(grid(), (58, 132), "a key after it is");
        drop(core);
        std::fs::remove_dir_all(root).unwrap();
    }
    // A key as the first frame of a resumed stream counts at once.
    let (core, id, root) = core_with_screen(100, 40);
    let line = Arc::new(Conn::default());
    let phone = Arc::new(Conn::default());
    let p = core.attach(&phone, id, focus_request((40, 10), None, true, true)).unwrap();
    core.viewer_resize(&phone, p, 40, 10);
    let a = core
        .attach(
            &line,
            id,
            AttachRequest {
                resumed: true,
                idle_ms: Some(3_600_000),
                ..focus_request((58, 132), Some(("d", "d")), true, true)
            },
        )
        .unwrap();
    core.viewer_resize(&line, a, 58, 132);
    core.viewer_input(&line, a, b"ls\r", false);
    assert_eq!(core.lock().screens[&id].size(), (58, 132), "typing first after a reconnect is use");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

/// A first paint's history goes exactly into the viewer's scrollback in the
/// rows the viewer renders: the PTY's for one negotiating RESIZE (its 6-row
/// offer does not size a PTY a focused desktop leads), its own for an
/// `hc attach` without it, taller or shorter than the PTY. No history row is
/// lost to the ED 2 after it and no blank row comes between it and the
/// screen; a screen taller than the viewer overflows into its scrollback.
#[test]
fn a_first_paints_history_fits_the_rows_the_viewer_renders() {
    let request = |size, observe, negotiated| AttachRequest {
        observe,
        resize_notifications: negotiated,
        history: 100,
        resize_history: if negotiated { 100 } else { 0 },
        size,
        display: Display::default(),
        resumed: false,
        visible: true,
        visibility_updates: false,
        focus_updates: false,
        focused: false,
        idle_ms: None,
        desktop_group: None,
    };
    // (negotiated, observe, viewer rows) -> the rows it renders.
    for (negotiated, observe, rows, renders) in
        [(true, false, 6, 4), (false, true, 6, 6), (false, false, 6, 6), (false, true, 3, 3), (false, true, 4, 4)]
    {
        let (core, id, root) = core_with_screen(20, 4);
        let desktop = desktop_conn();
        let leader = core
            .attach(
                &desktop,
                id,
                AttachRequest {
                    history: 0,
                    focus_updates: true,
                    focused: true,
                    desktop_group: Some(("local".into(), "desktop".into())),
                    ..request((20, 4), false, true)
                },
            )
            .unwrap();
        core.viewer_resize(&desktop, leader, 20, 4);
        {
            let mut st = core.lock();
            let screen = st.screens.get_mut(&id).unwrap();
            for n in 0..10 {
                screen.write(format!("H{n:02}\r\n").as_bytes());
            }
            screen.write(b"S1\r\nS2\r\nS3\r\nprompt> ");
        }
        let conn = Arc::new(Conn::default());
        let ch = core.attach(&conn, id, request((20, rows), observe, negotiated)).unwrap();
        assert_eq!(core.lock().screens[&id].size(), (20, 4), "the desktop keeps the PTY's size");
        let mut viewer = ghostty_vt::Terminal::new(20, rows, 1 << 20);
        for (kind, bytes) in frames(&conn, ch) {
            if kind == wire::RESIZE {
                viewer.resize(u16::from_le_bytes([bytes[0], bytes[1]]), u16::from_le_bytes([bytes[2], bytes[3]]));
            } else if kind == wire::DATA {
                viewer.write(&bytes);
            }
        }
        let text = viewer.plain_text();
        let mut lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        // The empty screen rows under the prompt of a taller viewer; a blank row
        // between history and screen stays and fails.
        while lines.last() == Some(&"") {
            lines.pop();
        }
        let case = format!("negotiated {negotiated}, observe {observe}, {rows} rows: {lines:?}");
        assert_eq!(viewer.size().1, renders, "{case}");
        let want: Vec<String> =
            (0..10).map(|n| format!("H{n:02}")).chain(["S1", "S2", "S3", "prompt>"].map(String::from)).collect();
        assert_eq!(lines, want, "{case}");
        // Every history row, no blank one; S1 too when the screen does not fit.
        assert_eq!(viewer.history_rows() as usize, 10 + 4usize.saturating_sub(renders as usize), "{case}");
        drop(core);
        std::fs::remove_dir_all(root).unwrap();
    }
}
