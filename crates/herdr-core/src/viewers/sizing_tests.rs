use super::tests::{core_with_screen, desktop_conn, viewer};
use super::*;

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
