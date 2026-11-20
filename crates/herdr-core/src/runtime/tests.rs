use super::*;
use serde_json::json;

#[tokio::test(start_paused = true)]
async fn keys_pressed_during_a_paste_follow_its_submit() {
    use crate::viewers::{AttachRequest, Display};
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    let (core, root) = test_core();
    let (writer, frames) = mpsc::channel();
    let (bye, _) = mpsc::channel();
    let id = {
        let mut st = core.lock();
        let id =
            st.reg.create(crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() }, 0).unwrap();
        st.reg.terminals.get_mut(&id).unwrap().holder = Some("fake".into());
        st.screens.insert(id, Screen::new(80, 24, 1 << 20));
        st.holders.push(Link { writer, pid: 0, instance: "fake".into(), current: true, bye, bye_rx: None });
        id
    };
    let person = Arc::new(crate::wire::Conn::default());
    let channel = core
        .attach(
            &person,
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
                size: (80, 24),
                display: Display::default(),
                resumed: false,
                desktop_group: None,
            },
        )
        .unwrap();
    // Ignore attach's resize frames. From here the fake holder sees only input.
    while frames.try_recv().is_ok() {}
    {
        let gone = crate::input::Gone::default();
        let sending = core.send(id, "abc", false, None, &gone);
        tokio::pin!(sending);
        poll_fn(|cx| {
            assert!(sending.as_mut().poll(cx).is_pending(), "the paste must pause before submit");
            Poll::Ready(())
        })
        .await;
        assert_eq!(frames.try_recv().unwrap(), proto::frame(proto::INPUT, id, b"abc"));
        assert!(frames.try_recv().is_err());
        core.viewer_input(&person, channel, b"Z", false);
        assert!(frames.try_recv().is_err(), "the person's keys must stay held");
        tokio::time::advance(crate::input::PASTE_GAP).await;
        sending.await.unwrap();
        assert_eq!(frames.try_recv().unwrap(), proto::frame(proto::INPUT, id, b"\r"));
        assert_eq!(frames.try_recv().unwrap(), proto::frame(proto::INPUT, id, b"Z"));
    }
    drop(core);
    remove_root(&root);
}

#[tokio::test]
async fn server_exit_flushes_even_when_background_tasks_still_own_core() {
    let (mut core, root) = test_core();
    core.flush_saved().await.unwrap();
    let (release, blocked) = mpsc::channel();
    Arc::get_mut(&mut core).unwrap().registry_writer = RegistryWriter::start(move |_| {
        blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(())
    })
    .unwrap();
    let background_owner = core.clone();
    core.save(&core.lock());
    let finish = crate::server::finish(&core);
    tokio::pin!(finish);
    let waited = tokio::time::timeout(Duration::from_millis(10), &mut finish).await.is_err();
    release.send(()).unwrap();
    if waited {
        finish.await.unwrap();
    }
    drop(background_owner);
    remove_root(&root);
    assert!(waited, "serve returned before its queued snapshot finished");
}

#[tokio::test]
async fn server_stop_succeeds_even_when_the_registry_flush_times_out() {
    let (mut core, root) = test_core();
    core.flush_saved().await.unwrap();
    let (release, blocked) = mpsc::channel();
    let mut first = true;
    Arc::get_mut(&mut core).unwrap().registry_writer = RegistryWriter::start(move |_| {
        if first {
            first = false;
            blocked.recv_timeout(Duration::from_secs(15)).unwrap();
        }
        Ok(())
    })
    .unwrap();
    let call = crate::api::Call { core: &core, conn: None, plain: None, gone: Default::default() };
    let started = Instant::now();
    let stop = crate::api::dispatch(&call, "server.stop", json!({})).await;
    release.send(()).unwrap();
    core.flush_saved().await.unwrap();
    drop(core);
    remove_root(&root);
    assert!(started.elapsed() >= Duration::from_secs(10), "stuck disks must still time out");
    assert_eq!(stop.unwrap()["stopped"], true);
}

#[tokio::test]
async fn a_restart_saves_first_starts_nothing_meanwhile_and_is_taken_back_when_it_cannot() {
    let (mut core, root) = test_core();
    core.flush_saved().await.unwrap();
    let (release, blocked) = mpsc::channel();
    let mut first = true;
    Arc::get_mut(&mut core).unwrap().registry_writer = RegistryWriter::start(move |_| {
        if first {
            first = false;
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        Err(io::Error::other("disk full"))
    })
    .unwrap();
    let call = crate::api::Call { core: &core, conn: None, plain: None, gone: Default::default() };
    let err = {
        let restarting = crate::api::dispatch(&call, "server.restart", json!({}));
        tokio::pin!(restarting);
        // It waits for its save, and nothing can start meanwhile: a terminal created now
        // would be missing from what the next core loads, and it kills what that doesn't list.
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut restarting).await.is_err());
        let refused = crate::api::not_stopping(&core, &core.lock()).unwrap_err();
        assert!(refused.message.contains("restarting"), "{}", refused.message);
        release.send(()).unwrap();
        // The save failed: refused before anything was acknowledged, and taken back.
        restarting.await.unwrap_err()
    };
    assert_eq!(err.kind, "unavailable");
    assert!(err.message.contains("not restarting") && err.message.contains("disk full"), "{}", err.message);
    assert!(!core.restart.load(std::sync::atomic::Ordering::SeqCst), "the restart was taken back");
    assert!(!root.join("state/restart-env.json").exists(), "no environment left behind");
    crate::api::not_stopping(&core, &core.lock()).unwrap();

    // Forced: it goes ahead and names what the next core will kill.
    let lost = {
        let mut st = core.lock();
        let id = st
            .reg
            .create(crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() }, now_ms())
            .unwrap();
        crate::terminals::address(id)
    };
    let forced = crate::api::dispatch(&call, "server.restart", json!({"force_stale_registry": true})).await.unwrap();
    let warning = forced["warning"].as_str().expect("it says what it kills");
    assert!(warning.contains(&lost) && warning.contains("disk full"), "{warning}");
    assert!(core.restart.load(std::sync::atomic::Ordering::SeqCst));
    drop(core);
    remove_root(&root);
}

/// A restart forced over a failing save names what the next core kills: the live terminals
/// missing from the registry on disk, whenever saving began to fail. (The terminal that
/// triggers the first failed write is created before that failure is reported.)
#[tokio::test]
async fn a_forced_restart_names_every_live_terminal_missing_from_the_saved_registry() {
    let (mut core, root) = test_core();
    core.flush_saved().await.unwrap(); // the empty registry is on disk
    let (began, started) = mpsc::channel();
    let (release, blocked) = mpsc::channel();
    let mut first = true;
    Arc::get_mut(&mut core).unwrap().registry_writer = RegistryWriter::start(move |_| {
        if first {
            first = false;
            began.send(()).unwrap();
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        Err(io::Error::other("disk full"))
    })
    .unwrap();
    let new_terminal = |core: &Core| {
        let mut st = core.lock();
        let id = st
            .reg
            .create(crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() }, now_ms())
            .unwrap();
        core.save(&st);
        crate::terminals::address(id)
    };
    let first_lost = new_terminal(&core);
    started.recv_timeout(Duration::from_secs(5)).unwrap();
    // The first failure is reported well after the terminal was created.
    tokio::time::sleep(Duration::from_millis(20)).await;
    release.send(()).unwrap();
    assert!(core.flush_warning().await.is_some());
    assert!(core.saving_failure().unwrap().0 > 0);
    let call = crate::api::Call { core: &core, conn: None, plain: None, gone: Default::default() };
    let answer = crate::api::dispatch(&call, "server.restart", json!({"force_stale_registry": true})).await.unwrap();
    let warning = answer["warning"].as_str().unwrap();
    assert!(warning.contains(&first_lost) && warning.contains("disk full"), "{warning}");

    // What is on disk is not lost, whenever it was written; the file is what counts.
    let saved = serde_json::to_vec(&core.lock().reg).unwrap();
    write_atomic(&core.paths.state.join("terminals.json"), &saved).unwrap();
    assert_eq!(crate::api::unsaved(&core).unwrap(), Vec::<String>::new());
    let second_lost = new_terminal(&core);
    assert_eq!(crate::api::unsaved(&core).unwrap(), [second_lost]);
    // A registry that can't be read says so, instead of promising anything.
    std::fs::write(core.paths.state.join("terminals.json"), b"{ not json").unwrap();
    assert!(crate::api::unsaved(&core).is_err());
    drop(core);
    remove_root(&root);
}

fn test_core() -> (Arc<Core>, PathBuf) {
    let root = std::env::temp_dir().join(format!("hc-persistence-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    (core, root)
}

#[tokio::test]
async fn label_and_kill_reply_with_the_record_observed_before_flushing() {
    for method in ["terminal.kill", "terminal.label"] {
        let (mut core, root) = test_core();
        core.flush_saved().await.unwrap();
        let (release, blocked) = mpsc::channel();
        Arc::get_mut(&mut core).unwrap().registry_writer = RegistryWriter::start(move |_| {
            blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok(())
        })
        .unwrap();
        let id = {
            let mut st = core.lock();
            let id = st
                .reg
                .create(crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() }, now_ms())
                .unwrap();
            st.reg.finish(id, Some(0), None, "exited", now_ms());
            core.save(&st);
            id
        };
        let reply = {
            let call = crate::api::Call { core: &core, conn: None, plain: None, gone: Default::default() };
            let request =
                crate::api::dispatch(&call, method, json!({"terminal": crate::terminals::address(id), "labels": {}}));
            tokio::pin!(request);
            assert!(tokio::time::timeout(Duration::from_millis(10), &mut request).await.is_err());
            core.lock().reg.terminals.get_mut(&id).unwrap().turn = 1;
            // Label queued a second snapshot; kill reused the existing one.
            release.send(()).unwrap();
            let _ = release.send(());
            request.await.unwrap()
        };
        let captured = reply["turn"].clone();
        drop(core);
        remove_root(&root);
        assert_eq!(captured, 0, "{method} returned a turn observed after its barrier: {reply}");
    }
}

#[cfg(unix)]
#[test]
fn a_blocked_registry_write_does_not_hold_the_core_lock() {
    use std::os::unix::ffi::OsStrExt;
    let root = PathBuf::from(format!("/var/tmp/hc-save-lock-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !core.paths.state.join("terminals.json").exists() {
        assert!(Instant::now() < deadline, "startup registry was not written");
        std::thread::sleep(Duration::from_millis(1));
    }
    let tmp = core.paths.state.join("terminals.tmp");
    let name = std::ffi::CString::new(tmp.as_os_str().as_bytes()).unwrap();
    // Opening this temporary file for writing waits for a reader, like a stalled disk.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let (saved, done) = mpsc::channel();
    let saving = core.clone();
    let worker = std::thread::spawn(move || {
        saving.save(&saving.lock());
        saved.send(()).unwrap();
    });
    let returned = done.recv_timeout(Duration::from_millis(250)).is_ok();
    let unlocked = core.state.try_lock().is_ok();
    // Always release the blocked write, even when checking the old synchronous implementation.
    let reader = std::fs::File::open(&tmp).unwrap();
    worker.join().unwrap();
    drop(core);
    drop(reader);
    remove_root(&root);
    assert!(returned && unlocked, "saving to a stalled disk must leave output and input free to take the core lock");
}

#[tokio::test]
async fn a_prepared_restart_survives_sigterm_and_rejects_stop() {
    let root = std::env::temp_dir().join(format!("hc-restart-stop-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let call = crate::api::Call { core: &core, conn: None, plain: None, gone: Default::default() };
    crate::api::dispatch(&call, "server.restart", serde_json::json!({})).await.unwrap();
    crate::restart::on_sigterm(&core);
    assert!(core.paths.state.join("restart-env.json").exists(), "SIGTERM keeps the explicit restart's note");
    let error = crate::api::dispatch(&call, "server.stop", serde_json::json!({})).await.unwrap_err();
    assert_eq!(error.kind, "unavailable");
    assert!(!core.lock().stopping, "a prepared handoff keeps its holders");
    crate::restart::discard(&core.paths);
    core.restart.store(false, std::sync::atomic::Ordering::SeqCst);
    crate::api::dispatch(&call, "server.stop", serde_json::json!({})).await.unwrap();
    assert!(crate::restart::prepare(&core).is_err(), "a stop already in progress wins");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn host_shutdown_is_lost_without_overriding_explicit_actions() {
    for (killed, stopping, shutdown, expected) in [
        (false, false, false, "exited"),
        (false, false, true, "host_shutdown"),
        (false, true, false, "server_stop"),
        (false, true, true, "server_stop"),
        (true, false, false, "killed"),
        (true, false, true, "killed"),
        (true, true, false, "killed"),
        (true, true, true, "killed"),
    ] {
        assert_eq!(finish_reason(killed, stopping, shutdown), expected);
    }
}

#[cfg(unix)]
#[test]
fn a_new_holder_of_another_build_reports_both_identities() {
    let root = PathBuf::from(format!("/var/tmp/hc-holder-build-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let (stream, mut holder) = UnixStream::pair().unwrap();
    let fake = std::thread::spawn(move || {
        assert_eq!(proto::read(&mut holder).unwrap().unwrap().0, proto::HELLO);
        let welcome = proto::Welcome {
            version: proto::VERSION,
            instance: "other".into(),
            pid: 0,
            build: "other-build".into(),
            previous: "none".into(),
            terminals: vec![],
        };
        io::Write::write_all(&mut holder, &proto::json_frame(proto::WELCOME, 0, &welcome)).unwrap();
        assert_eq!(proto::read(&mut holder).unwrap().unwrap().0, proto::DRAIN);
    });
    let err = core.take_over(stream, true, &root.join("run/core.holder.x.sock"), None).unwrap_err().to_string();
    assert!(err.contains("other-build"), "{err}");
    assert!(err.contains(&crate::client::build_id()), "{err}");
    assert!(core.lock().holders.is_empty());
    fake.join().unwrap();
    drop(core);
    remove_root(&root);
}

/// A frame no core can read: the reader of a link that gets it fails.
#[cfg(unix)]
const GARBAGE: [u8; 9] = [proto::OUTPUT, 1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];

/// A core with one live terminal on a fake holder. Each connection's hello is
/// answered as told ("welcome", "garbage", or both), and the fake reports
/// what it saw: connection number, then "hello", "no hello" or "closed".
#[cfg(unix)]
fn on_fake_holder(root: &std::path::Path, answers: &[&str]) -> (Arc<Core>, u32, mpsc::Receiver<String>) {
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let id = {
        let mut st = core.lock();
        let new = crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() };
        let id = st.reg.create(new, now_ms()).unwrap();
        st.reg.terminals.get_mut(&id).unwrap().holder = Some("fake".into());
        id
    };
    let held = proto::HeldTerminal { id, pid: 0, cols: 80, rows: 24, exit: None };
    let welcome = proto::Welcome {
        version: proto::VERSION,
        instance: "fake".into(),
        pid: 0,
        build: crate::client::build_id(),
        previous: "crash".into(),
        terminals: vec![held],
    };
    let mut welcome = proto::json_frame(proto::WELCOME, 0, &welcome);
    welcome.extend(proto::frame(proto::TAIL, id, b""));
    let socket = root.join("run/core.holder.fake.sock");
    let listener = platform::listen(&socket).unwrap();
    let answers: Vec<Vec<u8>> = answers
        .iter()
        .map(|a| {
            let mut frames = if a.contains("welcome") { welcome.clone() } else { vec![] };
            if a.contains("garbage") {
                frames.extend(GARBAGE);
            }
            frames
        })
        .collect();
    let (heard, reports) = mpsc::channel();
    std::thread::spawn(move || {
        for (n, (stream, answer)) in listener.incoming().zip(answers).enumerate() {
            let (mut stream, heard) = (stream.unwrap(), heard.clone());
            std::thread::spawn(move || {
                let Ok(Some(_)) = proto::read(&mut stream) else {
                    return heard.send(format!("{n} no hello")).unwrap();
                };
                heard.send(format!("{n} hello")).unwrap();
                std::thread::sleep(Duration::from_millis(50)); // time for the test to act, well within RELINK_WAIT
                let _ = io::Write::write_all(&mut stream, &answer);
                while let Ok(Some(_)) = proto::read(&mut stream) {} // the core's frames, until it closes
                let _ = heard.send(format!("{n} closed"));
            });
        }
    });
    core.take_over(platform::connect(&socket).unwrap(), false, &socket, None).unwrap();
    (core, id, reports)
}

#[cfg(unix)]
fn reported(reports: &mpsc::Receiver<String>, n: usize) -> Vec<String> {
    let mut seen: Vec<String> = (0..n).map_while(|_| reports.recv_timeout(Duration::from_secs(5)).ok()).collect();
    seen.sort();
    seen
}

/// Live and not ended by losing its holder: left for whoever adopts it next.
#[cfg(unix)]
fn still_live(core: &Core, id: u32) -> bool {
    core.lock().reg.terminals.get(&id).is_some_and(|t| t.is_live() && t.reason.is_none())
}

#[cfg(unix)]
#[test]
fn a_link_whose_reading_fails_reconnects_but_not_twice_in_a_row() {
    let root = PathBuf::from(format!("/var/tmp/hc-holder-relink-{}", platform::random_hex()));
    let (core, id, reports) = on_fake_holder(&root, &["welcome garbage", "welcome garbage", ""]);
    assert_eq!(
        reported(&reports, 5),
        ["0 closed", "0 hello", "1 closed", "1 hello", "2 no hello"],
        "each failed link closes; the first failure reconnects, the second soon after does not"
    );
    assert!(core.lock().holders.is_empty());
    assert!(still_live(&core, id));
    drop(core);
    remove_root(&root);
}

#[cfg(unix)]
#[test]
fn a_reconnect_the_holder_never_answers_gives_up_and_frees_holder_start() {
    let root = PathBuf::from(format!("/var/tmp/hc-holder-relink-{}", platform::random_hex()));
    let (core, id, reports) = on_fake_holder(&root, &["welcome garbage", ""]);
    assert_eq!(reported(&reports, 4), ["0 closed", "0 hello", "1 closed", "1 hello"]);
    let start = Instant::now();
    while core.holder_start.try_lock().is_err() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(core.holder_start.try_lock().is_ok(), "runs and stops can start holders again");
    assert!(still_live(&core, id));
    drop(core);
    remove_root(&root);
}

#[cfg(unix)]
#[test]
fn a_reconnect_whose_handshake_is_done_is_not_cut_however_slow_the_rest() {
    let root = PathBuf::from(format!("/var/tmp/hc-holder-relink-{}", platform::random_hex()));
    let (core, id, reports) = on_fake_holder(&root, &["welcome garbage", "welcome"]);
    while reports.recv_timeout(Duration::from_secs(5)).unwrap() != "1 hello" {}
    // Adopting waits for this lock: slow, but after the welcome came in time.
    let st = core.lock();
    std::thread::sleep(RELINK_WAIT * 3);
    drop(st);
    let start = Instant::now();
    while core.lock().holders.is_empty() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
    std::thread::sleep(RELINK_WAIT * 2);
    assert_eq!(core.lock().holders.len(), 1, "the new link stays");
    assert!(reports.try_iter().all(|r| r != "1 closed"), "and is not cut");
    assert!(still_live(&core, id));
    drop(core);
    remove_root(&root);
}

#[cfg(unix)]
#[test]
fn a_reconnect_that_fails_leaves_the_holders_terminals_live() {
    let root = PathBuf::from(format!("/var/tmp/hc-holder-relink-{}", platform::random_hex()));
    let (core, id, reports) = on_fake_holder(&root, &["welcome garbage", "garbage"]);
    assert_eq!(reported(&reports, 4), ["0 closed", "0 hello", "1 closed", "1 hello"]);
    assert!(core.lock().holders.is_empty());
    assert!(still_live(&core, id), "its holder still answers: nothing ends");
    drop(core);
    remove_root(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn waiting_for_holder_start_does_not_block_the_core_executor() {
    let root = PathBuf::from(format!("/var/tmp/hc-holder-start-{}", platform::random_hex()));
    let paths = Paths {
        state: root.join("state"),
        config: root.join("config.toml"),
        socket: root.join("run/core.sock"),
        holder_socket: root.join("run/core.holder.sock"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
    let (ready, entered) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let c = core.clone();
    let blocker = std::thread::spawn(move || {
        let _starting = c.holder_start.lock().unwrap();
        ready.send(()).unwrap();
        let _ = wait.recv_timeout(Duration::from_secs(1));
        // Prevent any real holder from starting, including if the regression stalls the executor.
        c.lock().stopping = true;
    });
    entered.recv().unwrap();
    let c = core.clone();
    let start = tokio::spawn(async move { c.ensure_holder().await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let answered_while_starting = !core.lock().stopping;
    let _ = release.send(());
    blocker.join().unwrap();
    assert!(start.await.unwrap().is_err());
    drop(core);
    remove_root(&root);
    assert!(answered_while_starting, "holder startup blocked the core executor");
}

/// Holder threads can keep the core, and so its registry writer, alive a
/// moment after the test drops it: a last snapshot may still land.
fn remove_root(root: impl AsRef<std::path::Path>) {
    let start = Instant::now();
    loop {
        match std::fs::remove_dir_all(root.as_ref()) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) if start.elapsed() < Duration::from_secs(5) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("removing {}: {e}", root.as_ref().display()),
        }
    }
}

// ---- guarded send, incarnation, ctx, bounded read ----

mod guarded {
    use super::*;
    use crate::input::{Gone, Guard};
    use crate::viewers::{AttachRequest, Display};
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    const INCARNATION: &str = "0123456789abcdef";

    /// A core whose registry writer is instant: these tests run on a paused clock, where
    /// the 10 s flush timeout would otherwise fire before the writer thread answers.
    fn test_core() -> (Arc<Core>, PathBuf) {
        let (mut core, root) = super::test_core();
        Arc::get_mut(&mut core).unwrap().registry_writer = RegistryWriter::start(|_| Ok(())).unwrap();
        (core, root)
    }

    /// A live terminal on a fake holder: what the holder is sent comes out of the receiver.
    fn terminal(core: &Arc<Core>) -> (u32, mpsc::Receiver<Vec<u8>>) {
        let (writer, frames) = mpsc::channel();
        let (bye, _) = mpsc::channel();
        let mut st = core.lock();
        let id =
            st.reg.create(crate::terminals::NewTerminal { argv: vec!["sh".into()], ..Default::default() }, 0).unwrap();
        let instance = format!("fake{id}");
        let t = st.reg.terminals.get_mut(&id).unwrap();
        t.holder = Some(instance.clone());
        t.pid = Some(42);
        t.incarnation = Some(INCARNATION.into());
        st.screens.insert(id, Screen::new(80, 24, 1 << 20));
        st.holders.push(Link { writer, pid: 0, instance, current: true, bye, bye_rx: None });
        (id, frames)
    }

    fn request() -> AttachRequest {
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
            size: (80, 24),
            display: Display::default(),
            resumed: false,
            desktop_group: None,
        }
    }

    fn ctx(core: &Core, id: u32) -> String {
        core.lock().reg.terminals[&id].ctx.clone()
    }

    fn guard(core: &Core, id: u32) -> Guard {
        Guard { incarnation: INCARNATION.into(), if_ctx: ctx(core, id), within: Duration::from_secs(10) }
    }

    fn drain(frames: &mpsc::Receiver<Vec<u8>>) -> Vec<Vec<u8>> {
        // Not the empty frames that probe a link (`Link::alive`): they write nothing.
        frames.try_iter().filter(|f| !f.is_empty()).collect()
    }

    fn input(id: u32, bytes: &[u8]) -> Vec<u8> {
        proto::frame(proto::INPUT, id, bytes)
    }

    fn report(state: &'static str, message: &str, progress: Option<u8>) -> ghostty_vt::StatusReport {
        ghostty_vt::StatusReport {
            state,
            kind: None,
            progress,
            id: String::new(),
            app: String::new(),
            title: String::new(),
            message: message.into(),
        }
    }

    /// Polls `send` once: it must have typed its paste and be pausing before the submit key.
    async fn pause_after_paste<F: Future>(send: &mut std::pin::Pin<&mut F>) {
        poll_fn(|cx| {
            assert!(send.as_mut().poll(cx).is_pending(), "the paste must pause before submit");
            Poll::Ready(())
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_guarded_send_types_a_paste_then_its_submit_and_moves_ctx() {
        let (core, root) = test_core();
        let gone = Gone::default();
        let (id, frames) = terminal(&core);
        let before = guard(&core, id);
        let sent = core.send_guarded(id, "abc", None, before, &gone);
        tokio::pin!(sent);
        pause_after_paste(&mut sent).await;
        assert_eq!(drain(&frames), vec![input(id, b"abc")]);
        tokio::time::advance(crate::input::PASTE_GAP).await;
        sent.await.unwrap();
        assert_eq!(drain(&frames), vec![input(id, b"\r")]);
        remove_root(&root);
    }

    #[tokio::test(start_paused = true)]
    async fn a_guarded_send_moves_ctx_so_the_same_ctx_cannot_send_twice() {
        let (core, root) = test_core();
        let gone = Gone::default();
        let (id, frames) = terminal(&core);
        let first = guard(&core, id);
        let again = Guard { incarnation: first.incarnation.clone(), if_ctx: first.if_ctx.clone(), within: first.within };
        let sending = core.send_guarded(id, "abc", None, first, &gone);
        tokio::pin!(sending);
        pause_after_paste(&mut sending).await;
        tokio::time::advance(crate::input::PASTE_GAP).await;
        sending.await.unwrap();
        drain(&frames);
        let e = core.send_guarded(id, "abc", None, again, &gone).await.err().unwrap();
        assert_eq!(e.kind, "context_changed");
        assert!(drain(&frames).is_empty());
        remove_root(&root);
    }

    #[tokio::test(start_paused = true)]
    async fn refusals_type_nothing() {
        let (core, root) = test_core();
        let (id, frames) = terminal(&core);
        let gone = Gone::default();
        // Another program.
        let mut g = guard(&core, id);
        g.incarnation = "ffffffffffffffff".into();
        assert_eq!(core.send_guarded(id, "x", None, g, &gone).await.err().unwrap().kind, "replaced");
        // Another context.
        let mut g = guard(&core, id);
        g.if_ctx = "nonce-0".into();
        assert_eq!(core.send_guarded(id, "x", None, g, &gone).await.err().unwrap().kind, "context_changed");
        // Blocked on a dialog.
        {
            let mut st = core.lock();
            let t = st.reg.terminals.get_mut(&id).unwrap();
            crate::status::apply(t, &report("blocked", "Apply?", None));
        }
        let g = guard(&core, id);
        assert_eq!(core.send_guarded(id, "x", None, g, &gone).await.err().unwrap().kind, "blocked");
        // A status mismatch, as for any send.
        {
            let mut st = core.lock();
            let t = st.reg.terminals.get_mut(&id).unwrap();
            crate::status::apply(t, &report("idle", "", None));
        }
        let g = guard(&core, id);
        assert_eq!(core.send_guarded(id, "x", Some("working"), g, &gone).await.err().unwrap().kind, "status_mismatch");
        // Its caller gone before it types.
        let (ended, _) = tokio::sync::watch::channel(true);
        let g = guard(&core, id);
        assert_eq!(core.send_guarded(id, "x", None, g, &Gone::watch(&ended)).await.err().unwrap().kind, "cancelled");
        // A finished terminal.
        core.lock().reg.terminals.get_mut(&id).unwrap().state = "finished".into();
        let g = guard(&core, id);
        assert_eq!(core.send_guarded(id, "x", None, g, &gone).await.err().unwrap().kind, "finished");
        assert!(drain(&frames).is_empty(), "no refusal typed anything");
        remove_root(&root);
    }

    #[tokio::test(start_paused = true)]
    async fn viewer_input_before_the_fence_refuses_and_after_it_waits_for_the_submit() {
        let (core, root) = test_core();
        let gone = Gone::default();
        let (id, frames) = terminal(&core);
        let person = Arc::new(crate::wire::Conn::default());
        let channel = core.attach(&person, id, request()).unwrap();
        drain(&frames);
        // First: the person's key moved ctx before the send read it, and the send holds the old one.
        let stale = guard(&core, id);
        core.viewer_input(&person, channel, b"Z", false);
        assert_eq!(drain(&frames), vec![input(id, b"Z")], "no send in progress: straight through");
        let e = core.send_guarded(id, "abc", None, stale, &gone).await.err().unwrap();
        assert_eq!(e.kind, "context_changed");
        assert!(drain(&frames).is_empty());
        // Second, on a terminal nobody has typed on: the key comes in the pause. It is held, the
        // message goes whole, the key follows.
        let (id, frames) = terminal(&core);
        let channel = core.attach(&person, id, request()).unwrap();
        drain(&frames);
        let fresh = guard(&core, id);
        let sending = core.send_guarded(id, "abc", None, fresh, &gone);
        tokio::pin!(sending);
        pause_after_paste(&mut sending).await;
        assert_eq!(drain(&frames), vec![input(id, b"abc")]);
        let during = ctx(&core, id);
        core.viewer_input(&person, channel, b"Q", false);
        assert_ne!(ctx(&core, id), during, "ctx moves at once, so the next decision on the old one is stale");
        assert!(drain(&frames).is_empty(), "held");
        tokio::time::advance(crate::input::PASTE_GAP).await;
        sending.await.expect("a person's held keys do not fail the send");
        assert_eq!(drain(&frames), vec![input(id, b"\r"), input(id, b"Q")]);
        remove_root(&root);
    }

    #[tokio::test(start_paused = true)]
    async fn a_terminals_own_replies_do_not_move_ctx_but_a_person_does() {
        let (core, root) = test_core();
        let gone = Gone::default();
        let (id, frames) = terminal(&core);
        let person = Arc::new(crate::wire::Conn::default());
        let channel = core.attach(&person, id, request()).unwrap();
        drain(&frames);
        let before = ctx(&core, id);
        for _ in 0..50 {
            // A program asking for the cursor position in a loop, and the viewer's emulator answering,
            // as DATA (an older path) and as AUTOMATIC input.
            core.output(&mut core.lock(), id, b"\x1b[6n");
            core.viewer_input(&person, channel, b"\x1b[24;80R", false);
            core.viewer_input(&person, channel, b"\x1b[?62;22c", true);
            core.viewer_input(&person, channel, b"\x1b[I", true);
            core.viewer_input(&person, channel, b"", false);
        }
        assert_eq!(ctx(&core, id), before);
        let sent = core
            .send_guarded(id, "go", None, guard(&core, id), &gone);
        tokio::pin!(sent);
        pause_after_paste(&mut sent).await;
        tokio::time::advance(crate::input::PASTE_GAP).await;
        sent.await.expect("a guarded send goes through while a program queries the cursor in a loop");
        core.viewer_input(&person, channel, b"x", false);
        assert_ne!(ctx(&core, id), before);
        let after = ctx(&core, id);
        core.viewer_input(&person, channel, b"\x1b[24;80R", false);
        assert_eq!(ctx(&core, id), after);
        remove_root(&root);
    }

    #[tokio::test(start_paused = true)]
    async fn the_status_hooks_report_moves_ctx_and_the_terminal_title_does_not() {
        let (core, root) = test_core();
        let (id, _frames) = terminal(&core);
        let bytes = |body: &str| format!("\x1b]7501;{body}\x1b\\").into_bytes();
        let b64 = |s: &str| {
            use std::fmt::Write;
            // Base64 of a short ASCII string, the way the status hook sends `msg`.
            const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for c in s.as_bytes().chunks(3) {
                let n = c.iter().enumerate().fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
                for i in 0..4 {
                    write!(out, "{}", if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' }).unwrap();
                }
            }
            out
        };
        let feed = |body: String| core.output(&mut core.lock(), id, &bytes(&body));
        let osc_title = |title: &str| core.output(&mut core.lock(), id, format!("\x1b]0;{title}\x07").as_bytes());
        // The OSC title: Claude animates it while it works.
        let c0 = ctx(&core, id);
        for n in 0..20 {
            osc_title(&format!("⠋ working {n}"));
        }
        assert_eq!(ctx(&core, id), c0, "the terminal title does not move ctx");
        assert!(core.lock().reg.terminals[&id].title.contains("working"), "but it was seen");
        feed("state=idle".into());
        let c1 = ctx(&core, id);
        assert_ne!(c1, c0, "the first report");
        feed("state=idle".into());
        assert_eq!(ctx(&core, id), c1, "the same report again");
        feed("state=idle:progress=10".into());
        assert_eq!(ctx(&core, id), c1, "progress alone");
        feed(format!("state=idle:msg={}", b64("Ready")));
        let c2 = ctx(&core, id);
        assert_ne!(c2, c1, "the same state word with another message (E22)");
        feed(format!("state=idle:msg={}:title={}", b64("Ready"), b64("Fix the build")));
        let c3 = ctx(&core, id);
        assert_ne!(c3, c2, "the report's own title");
        feed("state=blocked:kind=question".into());
        let c4 = ctx(&core, id);
        assert_ne!(c4, c3, "state and kind");
        remove_root(&root);
    }

    #[tokio::test(start_paused = true)]
    async fn a_status_change_before_the_submit_key_ends_in_unknown_with_no_submit() {
        let (core, root) = test_core();
        let gone = Gone::default();
        let (id, frames) = terminal(&core);
        let sending = core.send_guarded(id, "abc", None, guard(&core, id), &gone);
        tokio::pin!(sending);
        pause_after_paste(&mut sending).await;
        assert_eq!(drain(&frames), vec![input(id, b"abc")]);
        {
            let mut st = core.lock();
            let t = st.reg.terminals.get_mut(&id).unwrap();
            crate::status::apply(t, &report("blocked", "Apply these changes?", None));
        }
        tokio::time::advance(crate::input::PASTE_GAP).await;
        let e = sending.await.err().expect("the pre-submit check fails");
        assert_eq!(e.kind, "unknown");
        assert!(e.hint.contains("input box"), "{}", e.hint);
        assert!(drain(&frames).is_empty(), "no submit key, and nothing typed again");
        remove_root(&root);
    }

    #[tokio::test(start_paused = true)]
    async fn losing_the_holder_or_the_program_mid_paste_gives_unknown() {
        for how in ["holder gone", "replaced", "link down before the paste", "link down after the paste"] {
            let (core, root) = test_core();
        let gone = Gone::default();
            let (id, frames) = terminal(&core);
            let (spare_id, spare) = {
                // A second terminal and holder that must never receive what was for the first.
                let (writer, frames) = mpsc::channel();
                let (bye, _) = mpsc::channel();
                let mut st = core.lock();
                st.holders.push(Link { writer, pid: 0, instance: "other".into(), current: false, bye, bye_rx: None });
                (0, frames)
            };
            let _ = spare_id;
            let mut frames = Some(frames);
            if how == "link down before the paste" {
                drop(frames.take());
            }
            if how == "link down before the paste" {
                let e = core.send_guarded(id, "abc", None, guard(&core, id), &gone).await.err().unwrap();
                assert_eq!(e.kind, "unknown", "{how}");
                remove_root(&root);
                continue;
            }
            let sending = core.send_guarded(id, "abc", None, guard(&core, id), &gone);
            tokio::pin!(sending);
            pause_after_paste(&mut sending).await;
            assert_eq!(drain(frames.as_ref().unwrap()), vec![input(id, b"abc")], "{how}");
            match how {
                "holder gone" => core.lock().holders.retain(|l| l.instance != format!("fake{id}")),
                "replaced" => {
                    // The program was replaced by another one on the same holder and address.
                    let mut st = core.lock();
                    let t = st.reg.terminals.get_mut(&id).unwrap();
                    t.pid = Some(43);
                    t.incarnation = Some("aaaaaaaaaaaaaaaa".into());
                }
                _ => drop(frames.take()),
            }
            tokio::time::advance(crate::input::PASTE_GAP).await;
            let e = sending.await.err().unwrap_or_else(|| panic!("{how}: must not report success"));
            assert_eq!(e.kind, "unknown", "{how}");
            assert!(drain(&spare).is_empty(), "{how}: nothing went to another holder");
            if let Some(frames) = &frames {
                assert!(drain(frames).is_empty(), "{how}: no submit key");
            }
            remove_root(&root);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn within_ms_gives_busy_with_nothing_typed() {
        let (core, root) = test_core();
        let gone = Gone::default();
        let (id, frames) = terminal(&core);
        let person = Arc::new(crate::wire::Conn::default());
        let channel = core.attach(&person, id, request()).unwrap();
        // A person typed just now: the send waits for them to pause, but not past its 1 s.
        core.viewer_input(&person, channel, b"hel", false);
        drain(&frames);
        let mut g = guard(&core, id);
        g.within = Duration::from_millis(1000);
        let e = core.send_guarded(id, "abc", None, g, &gone).await.err().unwrap();
        assert_eq!(e.kind, "busy");
        assert!(drain(&frames).is_empty());
        // Another send holding the terminal's input also makes it busy.
        let (id2, frames2) = terminal(&core);
        let first = core.send_guarded(id2, "one", None, guard(&core, id2), &gone);
        tokio::pin!(first);
        pause_after_paste(&mut first).await;
        let mut g = guard(&core, id2);
        g.within = Duration::from_millis(50);
        let second = core.send_guarded(id2, "two", None, g, &gone).await.err().unwrap();
        assert_eq!(second.kind, "busy");
        first.await.unwrap();
        assert_eq!(drain(&frames2), vec![input(id2, b"one"), input(id2, b"\r")]);
        remove_root(&root);
    }

    #[tokio::test(start_paused = true)]
    async fn the_api_checks_the_guard_params_and_old_sends_are_unchanged() {
        let (core, root) = test_core();
        let (id, frames) = terminal(&core);
        let call = crate::api::Call { core: &core, conn: None, plain: None, gone: Default::default() };
        let t = crate::terminals::address(id);
        let ctx = ctx(&core, id);
        for params in [
            json!({"terminal": t, "text": "x", "incarnation": INCARNATION}),
            json!({"terminal": t, "text": "x", "if_ctx": ctx}),
            json!({"terminal": t, "text": "x", "within_ms": 100}),
            json!({"terminal": t, "text": "x", "incarnation": INCARNATION, "if_ctx": ctx, "within_ms": 15001}),
            json!({"terminal": t, "text": "x", "incarnation": INCARNATION, "if_ctx": ctx, "force": true}),
        ] {
            let e = crate::api::dispatch(&call, "terminal.send", params.clone()).await.err().expect("refused");
            assert_eq!(e.kind, "invalid_params", "{params}");
        }
        assert!(drain(&frames).is_empty());
        // No guard: as today, a paste, the gap, Enter.
        let old = crate::api::dispatch(&call, "terminal.send", json!({"terminal": t, "text": "hi"}));
        tokio::pin!(old);
        poll_fn(|cx| {
            assert!(old.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(drain(&frames), vec![input(id, b"hi")]);
        tokio::time::advance(crate::input::PASTE_GAP).await;
        let sent = old.await.unwrap();
        assert_eq!(sent["via"], "typed");
        assert_eq!(drain(&frames), vec![input(id, b"\r")]);
        assert_ne!(super::guarded::ctx(&core, id), ctx, "an agent's send moves ctx");
        // The guard through the API, in all three params, and the defaults.
        let guarded = json!({"terminal": t, "text": "yo", "incarnation": INCARNATION, "if_ctx": super::guarded::ctx(&core, id)});
        let call2 = crate::api::dispatch(&call, "terminal.send", guarded);
        tokio::pin!(call2);
        poll_fn(|cx| {
            assert!(call2.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(drain(&frames), vec![input(id, b"yo")]);
        tokio::time::advance(crate::input::PASTE_GAP).await;
        call2.await.unwrap();
        // The features are advertised.
        for f in ["incarnation", "send-guarded", "read-bounded"] {
            assert!(crate::api::FEATURES.contains(&f), "{f}");
        }
        remove_root(&root);
    }

    #[tokio::test(start_paused = true)]
    async fn incarnation_is_new_on_each_spawn_and_ctx_moves_with_it() {
        let (core, root) = test_core();
        let (id, _frames) = terminal(&core);
        let mut st = core.lock();
        let t = &st.reg.terminals[&id];
        let (old, c0) = (t.incarnation.clone().unwrap(), t.ctx.clone());
        // A restart (kept recovery) and a plain spawn both come through `spawn_result`.
        for reason in [Some("exited"), None] {
            st.reg.terminals.get_mut(&id).unwrap().reason = reason.map(Into::into);
            let was = st.reg.terminals[&id].incarnation.clone();
            core.spawn_result(&mut st, &format!("fake{id}"), id, Ok(77));
            let t = &st.reg.terminals[&id];
            let now = t.incarnation.clone().unwrap();
            assert_eq!(now.len(), 16);
            assert!(now.bytes().all(|b| b.is_ascii_hexdigit()));
            assert_ne!(Some(now), was);
        }
        let t = &st.reg.terminals[&id];
        assert_ne!(t.incarnation.as_deref(), Some(old.as_str()));
        assert_ne!(t.ctx, c0);
        drop(st);
        remove_root(&root);
    }

    #[tokio::test(start_paused = true)]
    async fn list_get_attach_and_events_carry_incarnation_and_ctx() {
        let (core, root) = test_core();
        let (id, _frames) = terminal(&core);
        let person = Arc::new(crate::wire::Conn::default());
        let (_, record) = core.attach_record(&person, id, request()).unwrap();
        let (inc, c) = (record.incarnation.clone().unwrap(), record.ctx.clone());
        assert_eq!(inc, INCARNATION);
        assert_eq!(c, ctx(&core, id), "taken with the first paint, in one critical section");
        let call = crate::api::Call { core: &core, conn: None, plain: None, gone: Default::default() };
        let t = crate::terminals::address(id);
        let got = crate::api::dispatch(&call, "terminal.get", json!({"terminal": t})).await.unwrap();
        assert_eq!((got["incarnation"].as_str(), got["ctx"].as_str()), (Some(inc.as_str()), Some(c.as_str())));
        let list = crate::api::dispatch(&call, "terminal.list", json!({})).await.unwrap();
        assert_eq!(list["terminals"][0]["incarnation"], inc.as_str());
        assert_eq!(list["terminals"][0]["ctx"], c.as_str());
        let snapshot = core.snapshot(&core.lock(), &Default::default());
        assert_eq!(snapshot["terminals"][0]["ctx"], c.as_str());
        assert_eq!(snapshot["terminals"][0]["incarnation"], inc.as_str());
        // The status event says the ctx the report made.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        core.subscribe(&mut core.lock(), Default::default(), crate::events::Sink::Waiter(tx));
        core.output(&mut core.lock(), id, b"\x1b]7501;state=working\x1b\\");
        let mut seen = None;
        while let Ok(ev) = rx.try_recv() {
            if ev["type"] == "status" {
                seen = Some(ev);
            }
        }
        let ev = seen.expect("a status event");
        assert_eq!(ev["data"]["ctx"], ctx(&core, id).as_str(), "{ev}");
        remove_root(&root);
    }

    #[tokio::test]
    async fn live_terminals_without_an_incarnation_get_one_on_first_load_and_keep_it_after() {
        let root = std::env::temp_dir().join(format!("hc-backfill-{}", platform::random_hex()));
        let paths = || Paths {
            state: root.join("state"),
            config: root.join("config.toml"),
            socket: root.join("run/core.sock"),
            holder_socket: root.join("run/core.holder.sock"),
        };
        paths().create_dirs().unwrap();
        // A registry as an older core saved it: no `incarnation`, no `ctx`.
        let mut reg = crate::terminals::Registry::default();
        let new = || crate::terminals::NewTerminal { argv: vec!["sh".into()], cwd: "/".into(), ..Default::default() };
        let now = crate::runtime::now_ms();
        let live = reg.create(new(), now).unwrap();
        let done = reg.create(new(), now).unwrap();
        reg.finish(done, Some(0), None, "exited", now);
        let mut value = serde_json::to_value(&reg).unwrap();
        for t in value["terminals"].as_object_mut().unwrap().values_mut() {
            let t = t.as_object_mut().unwrap();
            t.remove("incarnation");
            t.remove("ctx");
        }
        std::fs::write(paths().state.join("terminals.json"), value.to_string()).unwrap();
        let core = Core::start(paths(), CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
        let first = {
            let st = core.lock();
            // `attach_all` finds no holder for the live record: it ended with its holder. The backfill
            // is what is checked, before that, so read what the load itself did through the registry.
            st.reg.terminals.get(&live).map(|t| t.incarnation.clone())
        };
        // (A live record with no holder is finished by `attach_all`; its incarnation is still the one
        // the load gave it.)
        let first = first.expect("the record is still there");
        let inc = first.expect("a live record got an incarnation on load");
        assert_eq!(inc.len(), 16);
        assert!(inc.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(core.lock().reg.terminals[&done].incarnation, None, "a finished record gets none");
        assert!(core.lock().reg.terminals[&live].ctx.contains('-'));
        core.flush_saved().await.unwrap();
        let saved = std::fs::read_to_string(paths().state.join("terminals.json")).unwrap();
        assert!(saved.contains(&inc), "saved with the record");
        drop(core);
        // A second load keeps what was saved: backfill only fills a gap.
        let again = Core::start(paths(), CoreInfo { id: "test".into(), name: "test".into() }).unwrap();
        assert_eq!(again.lock().reg.terminals[&live].incarnation.as_deref(), Some(inc.as_str()));
        drop(again);
        remove_root(&root);
    }

    // ---- K3 ----

    fn read(max_bytes: Option<usize>, lines: usize, stitch: bool) -> crate::api::ReadParams {
        crate::api::ReadParams { terminal: "t1".into(), caller: None, lines, max_bytes, stitch }
    }

    fn result(text: &str) -> crate::api::ReadResult {
        crate::api::ReadResult {
            text: text.into(),
            complete: true,
            how: "history".into(),
            why: None,
            truncated: false,
            incarnation: None,
        }
    }

    #[test]
    fn max_bytes_bounds_the_escaped_text_and_drops_the_oldest_lines() {
        let bound = crate::api::bound;
        // No bound, or one that fits: untouched.
        let r = bound(result("a\nb\nc"), None);
        assert_eq!((r.text.as_str(), r.truncated), ("a\nb\nc", false));
        let r = bound(result("a\nb\nc"), Some(7)); // a\nb\nc escapes to 7 bytes
        assert_eq!((r.text.as_str(), r.truncated), ("a\nb\nc", false));
        // One byte less: the oldest line goes.
        let r = bound(result("a\nb\nc"), Some(6));
        assert_eq!((r.text.as_str(), r.truncated, r.complete), ("b\nc", true, false));
        // The bound counts escapes: a quote is two bytes, a newline two, a control character six.
        let r = bound(result("\"\"\n\"\""), Some(5));
        assert_eq!(r.text, "\"\"");
        assert!(r.truncated);
        let r = bound(result("x\u{1}"), Some(7)); // x + \u0001
        assert_eq!((r.text.as_str(), r.truncated), ("x\u{1}", false));
        // Whole lines only, newest kept: a last line too long on its own is dropped too.
        let r = bound(result("short\nthis one is far too long"), Some(10));
        assert_eq!((r.text.as_str(), r.truncated), ("", true));
        let r = bound(result("old\nmiddle\nnew"), Some(9));
        assert_eq!(r.text, "new");
        for max in 0..40 {
            let text = "alpha\n\"beta\"\ngamma\n\u{e9}t\u{e9}\ndelta";
            let r = bound(result(text), Some(max));
            assert!(serde_json::to_string(&r.text).unwrap().len() - 2 <= max.max(0), "{max}: {:?}", r.text);
            assert!(text.ends_with(&r.text), "a tail of the lines: {:?}", r.text);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_bounded_read_formats_only_the_rows_asked_for_and_reads_incarnation_with_them() {
        let (core, root) = test_core();
        let (id, _frames) = terminal(&core);
        assert_eq!(crate::terminals::address(id), "t1");
        {
            let mut st = core.lock();
            let lines: String = (1..=500).map(|n| format!("line {n}\r\n")).collect();
            core.output(&mut st, id, lines.as_bytes());
        }
        let call = crate::api::Call { core: &core, conn: None, plain: None, gone: Default::default() };
        let ask = |p: crate::api::ReadParams| {
            let params = serde_json::to_value(&p).unwrap();
            let call = &call;
            async move { crate::api::dispatch(call, "terminal.read", params).await.unwrap() }
        };
        // An old call: all history asked for comes back, nothing new in the answer but the incarnation.
        let all = ask(read(None, 1000, true)).await;
        assert!(all["text"].as_str().unwrap().contains("line 1\n"));
        assert!(all.get("truncated").is_none());
        assert_eq!(all["incarnation"], INCARNATION);
        // A bounded one: only 10 rows of history above the screen.
        let some = ask(read(Some(1 << 20), 10, true)).await;
        let text = some["text"].as_str().unwrap();
        let rows = text.lines().count();
        assert!(rows <= 24 + 10, "{rows}");
        assert!(text.contains("line 500"), "the newest row is there");
        assert!(!text.contains("line 400\n"), "rows beyond `lines` were never formatted");
        assert_eq!(some["incarnation"], INCARNATION);
        // And cut to bytes, oldest first.
        let cut = ask(read(Some(40), 10, true)).await;
        let text = cut["text"].as_str().unwrap();
        assert!(serde_json::to_string(text).unwrap().len() - 2 <= 40, "{text:?}");
        assert!(text.ends_with("line 500") || text.ends_with("line 500\n") || text.contains("line 500"));
        assert_eq!(cut["truncated"], true);
        assert_eq!(cut["complete"], false);
        // `stitch: false` on a full-screen program reads its screen as it is, never scrolls it.
        core.output(&mut core.lock(), id, b"\x1b[?1049h\x1b[Hfull screen app");
        let screen = ask(read(Some(1 << 20), 200, false)).await;
        assert_eq!(screen["how"], "screen");
        assert!(screen["text"].as_str().unwrap().contains("full screen app"));
        assert!(screen.get("why").is_none());
        assert!(!core.lock().stitching.contains(&id));
        remove_root(&root);
    }
}
