#![cfg(unix)] // Windows: no /dev/full, and tests/windows.rs

mod common;

use common::*;
use std::path::Path;
use std::time::Duration;

/// The disk fills up: the core's registry can't be written, and its own log
/// (on the same disk) can't take a line either.
#[test]
fn a_full_disk_neither_stops_the_registry_writer_nor_fails_a_send() {
    let mut s = Session::new();
    s.env.push(("HC_SAVE_RETRY_MS".into(), "50".into()));
    s.env.push(("HC_QUIET_MS".into(), "100".into()));
    let state = s.root.join("state/herdr-core/test");
    std::fs::create_dir_all(&state).unwrap();
    // Every line the core logs fails with ENOSPC, as on a full disk (`eprintln!` panics then).
    std::os::unix::fs::symlink("/dev/full", state.join("core.log")).unwrap();
    let t = s.json(&["run", "--bg", "sh", "-c", BYTES])["address"].as_str().unwrap().to_string();
    let registry = state.join("terminals.json");
    wait_until("saved", || std::fs::read_to_string(&registry).is_ok_and(|r| r.contains(&t)));

    // A directory where the registry's temporary file goes: every write fails and the file stays as it was.
    std::fs::create_dir(state.join("terminals.tmp")).unwrap();

    // A change the registry can't take: calls that only change it still answer.
    s.json(&["label", &t, "k=v"]);
    // The status says so once, for whoever shows health; no call has to.
    let status = s.json(&["server", "status"]);
    assert!(status["saving"]["since_ms"].as_u64().is_some_and(|ms| ms > 0), "{status}");
    assert!(status["saving"]["reason"].is_string(), "{status}");
    // The send is typed, so it is no error: it says the save failed.
    let sent = s.json(&["send", &t, "x"]);
    assert!(sent["warning"].as_str().is_some_and(|w| w.contains("could not save")), "{sent}");
    wait_until("typed", || text(&s, &t).contains("78"));
    // So do keys.
    assert!(s.json(&["keys", &t, "a"])["warning"].is_string());

    // A restart over a registry that is behind would kill terminals: refused, and taken back.
    let (_, err) = s.err(&["server", "restart"]);
    assert_eq!(err["error"]["kind"], "unavailable", "{err}");
    assert!(err["error"]["message"].as_str().unwrap().contains("not restarting"), "{err}");
    let t2 = s.json(&["run", "--bg", "sh", "-c", BYTES])["address"].as_str().unwrap().to_string();

    // Forced, it says what it kills: t2, live but missing from the registry on disk that the next core loads.
    let out = s.hc(&["server", "restart", "--force-stale-registry"]);
    assert!(out.status.success(), "{out:?}");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("missing from the saved registry") && said.contains(&t2), "{said}");
    wait_until_for("the old terminal is adopted", Duration::from_secs(20), || get(&s, &t)["state"] == "live");
    let (_, gone) = s.err(&["get", &t2]);
    assert_eq!(gone["error"]["kind"], "not_found", "{gone}");

    // The disk has room again: the writer, still alive, saves what changed.
    std::fs::remove_dir(state.join("terminals.tmp")).unwrap();
    s.json(&["label", &t, "k=w"]);
    wait_until("saved again", || std::fs::read_to_string(&registry).is_ok_and(|r| r.contains("\"k\":\"w\"")));
    let sent = s.json(&["send", &t, "y"]);
    assert!(sent.get("warning").is_none(), "{sent}");
    assert!(s.json(&["server", "status"]).get("saving").is_none());
    wait_until_for("typed", Duration::from_secs(10), || text(&s, &t).contains("79"));
}

/// `eprintln!` panics when stderr can't be written, and a panic in the registry writer or a
/// holder thread ends it for good: the core and holder log with `log!`. (The CLI prints for
/// people and may panic on a closed pipe; so may tests.)
#[test]
fn the_core_logs_with_log_not_eprintln() {
    fn scan(dir: &Path, found: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if path.is_dir() {
                if name != "cli" {
                    scan(&path, found);
                }
                continue;
            }
            let skipped = name.ends_with("tests.rs") || ["cli.rs", "main.rs", "log.rs"].contains(&name.as_str());
            if !name.ends_with(".rs") || skipped {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // What follows `#[cfg(test)]` is test code.
            let code = text.split("#[cfg(test)]").next().unwrap();
            for (n, line) in code.lines().enumerate() {
                let line = line.trim_start();
                let prints = ["eprintln!", "eprint!", "println!", "print!"].iter().any(|m| line.contains(m));
                if prints && !line.starts_with("//") {
                    found.push(format!("{}:{}: {line}", path.display(), n + 1));
                }
            }
        }
    }
    let mut found = Vec::new();
    scan(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut found);
    assert!(found.is_empty(), "use crate::log! (it never panics):\n{}", found.join("\n"));
}
