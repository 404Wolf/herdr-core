//! An update never breaks anything: terminals started by the previous release
//! keep running, stay readable and typeable after the swap, and machines on
//! different releases keep talking. Needs the previous release's binary in
//! HC_OLD_BIN (scripts/upgrade-test.sh builds it); skipped without one.

#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use serde_json::Value;

#[test]
fn an_old_retained_holder_restores_the_new_cores_environment_after_raw_restart() {
    let Some(old) = std::env::var_os("HC_OLD_BIN").map(std::path::PathBuf::from) else {
        eprintln!("skipped: no HC_OLD_BIN (run scripts/upgrade-test.sh <previous release>)");
        return;
    };
    let mut s = Session::new();
    s.env.push(("HC_RESTART_VALUE".into(), "old-holder".into()));
    s.env.push(("HC_HOLDER_GRACE_MS".into(), "100".into()));
    install(&mut s, &old);
    let terminal = start(&s, &["sh", "-c", "echo ready; while read line; do echo got-$line; done"]);
    says(&s, &terminal, "ready");
    install(&mut s, std::path::Path::new(BIN));
    s.env.retain(|(key, _)| key != "HC_RESTART_VALUE");
    s.env.push(("HC_RESTART_VALUE".into(), "saved-core".into()));
    let pid = s.json(&["server", "restart"])["pid"].clone();
    let reply = s.connect().call(1, "server.restart", serde_json::json!({}));
    assert!(reply.get("error").is_none(), "{reply}");
    // No autostarting CLI: only the released holder can launch the successor.
    wait_until("old holder launches the new executable", || {
        let Ok(stream) = herdr_core::platform::connect(&s.socket()) else { return false };
        herdr_core::client::call(&stream, "server.status", serde_json::json!({}))
            .is_ok_and(|status| status["pid"] != pid)
    });
    let out = s.hc(&["exec", "printf '%s' \"$HC_RESTART_VALUE\""]);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "saved-core");
    s.json(&["send", &terminal, "after"]);
    says(&s, &terminal, "got-after");
}

/// Puts `bin` in place as this session's `herdr-core` (a rename, as an update does).
fn install(s: &mut Session, bin: &std::path::Path) {
    let dir = s.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let tmp = dir.join("herdr-core.new");
    std::fs::copy(bin, &tmp).unwrap();
    std::fs::rename(&tmp, dir.join("herdr-core")).unwrap();
    s.bin = dir.join("herdr-core");
}

/// `hc run` as the old release starts things: it returned at once (no --bg then).
fn start(s: &Session, args: &[&str]) -> String {
    let bg = String::from_utf8(s.hc(&["run", "--help"]).stdout).unwrap().contains("--bg");
    let run: &[&str] = if bg { &["run", "--bg"] } else { &["run"] };
    s.json(&[run, args].concat())["address"].as_str().unwrap().to_string()
}

fn says(s: &Session, t: &str, what: &str) {
    wait_until(&format!("{t} shows {what}"), || text(s, t).contains(what));
}

#[cfg(target_os = "linux")]
#[test]
fn a_materialized_or_replaced_executable_can_start_its_first_holder() {
    for retain_old in [true, false] {
        let mut s = Session::new();
        s.env.push(("HC_HOLDER_GRACE_MS".into(), "100".into()));
        install(&mut s, std::path::Path::new(BIN));
        let pid = s.json(&["server", "status"])["pid"].clone();
        if retain_old {
            std::fs::rename(&s.bin, s.root.join("retired-core")).unwrap();
        }
        install(&mut s, std::path::Path::new(BIN));
        let t = start(&s, &["sh", "-c", "echo first-holder; sleep 100"]);
        says(&s, &t, "first-holder");
        let status = s.json(&["server", "status"]);
        assert_eq!(status["pid"], pid, "the running core did not need a restart");
        assert_eq!(status["holder"]["current"], true);

        unsafe { libc::kill(pid.as_i64().unwrap() as i32, libc::SIGKILL) };
        wait_until("the isolated core exited", || !pid_alive(pid.as_i64().unwrap()));
        // Only the holder may restart it: no CLI autostart during this wait.
        wait_until("holder recovery from the installed executable", || s.connect_ok());
        let recovered = s.json(&["server", "status"]);
        let recovered_pid = recovered["pid"].as_u64().unwrap();
        assert_ne!(recovered["pid"], pid);
        assert_eq!(
            std::fs::read_link(format!("/proc/{recovered_pid}/exe")).unwrap(),
            s.bin,
            "recovery runs the installed executable, never the retired build"
        );
        assert_eq!(get(&s, &t)["state"], "live");
    }
}

#[test]
fn an_update_keeps_terminals_and_machines_running() {
    let Some(old) = std::env::var_os("HC_OLD_BIN").map(std::path::PathBuf::from) else {
        eprintln!("skipped: no HC_OLD_BIN (run scripts/upgrade-test.sh <previous release>)");
        return;
    };
    let new = std::path::PathBuf::from(BIN);
    let mut rohan = Session::new();
    let mut s = Session::new();
    install(&mut rohan, &old);
    install(&mut s, &old);
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    let help = String::from_utf8(s.hc(&["--help"]).stdout).unwrap();
    let machines = help.contains("machine add");
    let old_has_put = help.contains("put <t>");

    let echo = "echo before; while read l; do echo got-$l; done";
    let live = start(&s, &["--name", "echo", "sh", "-c", echo]);
    let done = start(&s, &["sh", "-c", "echo finished-early; exit 5"]);
    says(&s, &live, "before");
    wait_until("the early one finished", || get(&s, &done)["state"] == "finished");
    let afar = machines.then(|| {
        s.json(&["machine", "add", "rohan"]);
        let t = start(&s, &["--on", "rohan", "sh", "-c", echo]);
        says(&s, &t, "before");
        t
    });

    // This machine updates; rohan stays on the old release.
    let old_pid = s.json(&["server", "status"])["pid"].clone();
    install(&mut s, &new);
    s.hc(&["server", "restart"]);
    assert_ne!(s.json(&["server", "status"])["pid"], old_pid, "a new core runs");
    let t = get(&s, &live);
    assert_eq!((t["state"].as_str(), t["name"].as_str()), (Some("live"), Some("echo")), "{t}");
    says(&s, &live, "before");
    s.json(&["send", &live, "after"]);
    says(&s, &live, "got-after");
    let d = get(&s, &done);
    assert_eq!((d["state"].as_str(), d["exit_code"].as_i64()), (Some("finished"), Some(5)), "{d}");
    assert!(text(&s, &done).contains("finished-early"));

    if let Some(t) = &afar {
        wait_until("the line to the old rohan", || s.json(&["machine", "ls"])["machines"][0]["state"] == "connected");
        s.json(&["send", t, "mixed"]);
        says(&s, t, "got-mixed");
        if !String::from_utf8(rohan.hc(&["run", "--help"]).stdout).unwrap().contains("--bg") {
            let out = String::from_utf8(s.hc(&["run", "--on", "rohan", "echo there"]).stdout).unwrap();
            assert!(
                out.contains("not waited on") && out.contains("hc machine update rohan"),
                "an older side's run is named: {out}"
            );
        }
        if !old_has_put {
            let small = s.root.join("small.txt");
            std::fs::write(&small, "hi").unwrap();
            let (_, e) = s.err(&["--json", "put", t, small.to_str().unwrap()]);
            assert_eq!(e["error"]["kind"], "needs_update", "what the older side lacks is named: {e}");
        }
        // Then rohan updates too, under its running terminal.
        install(&mut rohan, &new);
        rohan.hc(&["server", "restart"]);
        wait_until("the line back", || {
            s.json(&["machine", "ls"])["machines"][0]["state"] == "connected"
                && s.hc(&["--json", "send", t, "both"]).status.success()
        });
        says(&s, t, "got-both");
    }
    let list: Value = s.json(&["ls"]);
    assert!(list["terminals"].as_array().unwrap().iter().any(|t| t["address"] == live.as_str()), "{list}");
}

/// Unlike `install` above, this keeps the holder's original immutable path.
/// Models a service manager delaying the selected successor beyond holder grace.
#[cfg(target_os = "linux")]
#[test]
fn an_immutable_old_holder_can_win_a_gapped_successor_launch() {
    let (Some(old), Some(new)) = (std::env::var_os("HC_IMMUTABLE_OLD_BIN"), std::env::var_os("HC_SUCCESSOR_BIN"))
    else {
        eprintln!("skipped: set HC_IMMUTABLE_OLD_BIN and HC_SUCCESSOR_BIN to exact binaries");
        return;
    };
    let old = std::fs::canonicalize(old).unwrap();
    let new = std::fs::canonicalize(new).unwrap();
    assert_ne!(old, new, "the immutable incumbent and successor must have distinct paths");
    let mut s = Session::new();
    s.bin = old.clone();
    s.env.push(("HC_HOLDER_GRACE_MS".into(), "3000".into()));
    s.env.push(("HC_UPGRADE_FIXTURE".into(), "retained-environment".into()));
    let marker = s.root.join("output-during-handover");
    let terminal = start(
        &s,
        &[
            "sh",
            "-c",
            "echo ready-$HC_UPGRADE_FIXTURE; while ! test -e \"$1\"; do sleep 0.05; done; echo during-gap; while read line; do echo got-$line-$HC_UPGRADE_FIXTURE; done",
            "fixture",
            marker.to_str().unwrap(),
        ],
    );
    says(&s, &terminal, "ready-retained-environment");
    let before = s.json(&["server", "status"]);
    let child = get(&s, &terminal)["pid"].clone();
    let old_pid = before["pid"].as_i64().unwrap();
    let child_stat = || {
        let stat = std::fs::read_to_string(format!("/proc/{child}/stat")).unwrap();
        let fields: Vec<_> = stat.rsplit_once(") ").unwrap().1.split_whitespace().collect();
        (fields[3].to_owned(), fields[19].to_owned()) // session and start ticks
    };
    let identity = child_stat();
    let reply = s.connect().call(1, "server.restart", serde_json::json!({}));
    assert!(reply.get("error").is_none(), "{reply}");
    wait_until("incumbent exits after planned handover", || !pid_alive(old_pid));
    std::fs::write(&marker, "").unwrap();
    // No autostarting CLI during the gap: the immutable holder alone competes.
    std::thread::sleep(std::time::Duration::from_millis(3500));
    wait_until("the old holder starts its cached executable", || s.connect_ok());
    let after = s.json(&["server", "status"]);
    let winner = after["pid"].as_u64().unwrap();
    assert_ne!(after["pid"], before["pid"]);
    assert_eq!(std::fs::read_link(format!("/proc/{winner}/exe")).unwrap(), old);
    // One delayed successor launch cannot take an already-owned core.lock.
    let attempt = s.program(&new).arg("serve").output().unwrap();
    assert!(attempt.status.success(), "{attempt:?}");
    assert_eq!(s.json(&["server", "status"])["pid"], after["pid"]);
    assert_eq!(after["holder"]["pid"], before["holder"]["pid"]);
    assert_eq!(after["holder"]["instance"], before["holder"]["instance"]);
    assert_eq!(get(&s, &terminal)["pid"], child);
    assert_eq!(get(&s, &terminal)["address"], terminal);
    assert_eq!(child_stat(), identity);
    says(&s, &terminal, "during-gap");
    s.json(&["send", &terminal, "after"]);
    says(&s, &terminal, "got-after-retained-environment");
    eprintln!(
        "immutable restart race reproduced; old={old:?}, attempted successor={new:?}, same holder/child retained"
    );
}
