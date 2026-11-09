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
        sized: true,
        desktop_group: conn.local_desktop.then(|| ("local".into(), "desktop".into())),
        display: Display::default(),
        recovering: false,
    }
}

#[test]
fn desktop_windows_fit_both_axes_without_focus_reflow_and_keep_latest_display() {
    let now = Instant::now();
    let a = desktop_conn();
    let b = desktop_conn();
    let mut vs = vec![viewer(&a, (100, 20), Some(now)), viewer(&b, (80, 30), Some(now))];
    vs[0].display.bg = Some("#ffffff".into());
    vs[0].active_at = Some(now + std::time::Duration::from_secs(1));
    let (leader, size) = viewer_size(&vs, true);
    assert_eq!(size, Some((80, 20)));
    assert_eq!(leader.unwrap().display.bg.as_deref(), Some("#ffffff"));
    vs[1].active_at = Some(now + std::time::Duration::from_secs(2));
    assert_eq!(viewer_size(&vs, true).1, Some((80, 20)));
    assert_eq!(viewer_size(&vs, false).1, Some((80, 30)), "latest policy remains available");
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
    assert_eq!(viewer_size(&vs, true).1, Some((40, 10)), "phone activity still takes over");
    vs[1].observe = true;
    assert_eq!(viewer_size(&vs, true).1, Some((100, 30)));
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

fn drain(conn: &Arc<Conn>, channel: u32, screen: &mut ghostty_vt::Terminal) {
    conn.with(|out| {
        out.resume(channel);
        while let Some(frame) = out.next_frame() {
            if frame[4] == wire::DATA {
                screen.write(&frame[wire::HEADER..]);
            }
        }
    });
}

#[test]
fn larger_desktop_viewers_keep_only_the_shared_scrolling_grid() {
    let (core, id, root) = core_with_screen(20, 4);
    let tall = desktop_conn();
    let short = desktop_conn();
    let request = |size| AttachRequest {
        observe: false,
        resize_notifications: false,
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
