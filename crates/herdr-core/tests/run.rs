//! `hc run` works like a bash tool: it waits until the program is not
//! working (it exits, or says it is idle, done, blocked or error) and hands
//! back the output, the exit code as its own. A timeout leaves it running.

#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;

fn hc(s: &Session, args: &[&str]) -> (i32, String) {
    let out = s.hc(args);
    (out.status.code().unwrap_or(-1), String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn initial_size_is_optional_and_applies_before_the_program_starts() {
    use serde_json::{Value, json};
    let s = Session::new();
    s.json(&["server", "start"]);
    let mut conn = s.connect();
    for (id, size, expected) in [(1, Value::Null, "120 200"), (2, json!({"cols": 73, "rows": 19}), "19 73")] {
        let mut params = json!({"argv": ["sh", "-c", "stty size"], "wait": true});
        if !size.is_null() {
            params["size"] = size;
        }
        let reply = conn.call(id, "terminal.run", params);
        assert_eq!(reply["result"]["output"].as_str().unwrap().trim(), expected, "{reply}");
    }
    for size in [json!({"cols": 0, "rows": 19}), json!({"cols": 73, "rows": 0}), json!({"cols": 73})] {
        let reply = conn.call(3, "terminal.run", json!({"argv": ["sh", "-c", "stty size"], "size": size}));
        assert_eq!(reply["error"]["data"]["kind"], "invalid_params", "{reply}");
    }
}

#[test]
fn initial_display_answers_the_programs_first_color_query() {
    use serde_json::json;
    let s = Session::new();
    s.json(&["server", "start"]);
    let mut conn = s.connect();
    // Asks for the background at once, like a TUI at startup, before any viewer attaches.
    let probe = "stty raw -echo min 0 time 10; printf '\\033]11;?\\033\\\\'; r=$(dd bs=256 count=1 2>/dev/null); \
                 stty sane; printf '%s' \"$r\" | od -An -c | tr -d ' \\n'";
    let params = json!({"argv": ["sh", "-c", probe], "wait": true, "display": {"bg": "#fbfcfb"}});
    let reply = conn.call(1, "terminal.run", params);
    assert!(reply["result"]["output"].as_str().unwrap().contains("fbfb/fcfc/fbfb"), "{reply}");
}

#[test]
fn a_quick_command_returns_its_output_and_exit_code_and_leaves_no_note() {
    let mut s = Session::new();
    s.env.push(("HC_QUIET_MS".into(), "0".into()));
    let parent = run(&s, &["sh", "-c", &script(&s, "r state=idle; cat")]);
    let out = s.cmd().env("HC_TERMINAL_ID", &parent).args(["run", "sh", "-c", "echo hi; exit 3"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3), "the program's exit code");
    assert!(String::from_utf8_lossy(&out.stdout).contains("hi"));
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(!text(&s, &parent).contains("<hc>"), "it saw the exit itself: no note\n{}", text(&s, &parent));

    let (code, out) = hc(&s, &["run", "echo a && echo b | tr b c"]);
    assert_eq!(code, 0);
    assert!(out.contains("a\nc"), "one quoted string is a shell line: {out:?}");
}

#[test]
fn pagers_never_hold_a_run_and_huge_output_keeps_its_tail() {
    let s = Session::new();
    let repo = s.root.join("repo");
    std::fs::create_dir(&repo).unwrap();
    let r = repo.display().to_string();
    let setup = format!(
        "cd {r} && git init -q && for i in $(seq 1 150); do git -c user.name=t -c user.email=t@t commit -q --allow-empty -m c$i; done"
    );
    assert!(std::process::Command::new("sh").args(["-c", &setup]).status().unwrap().success());
    // Longer than the 120-row screen: git would open less and wait.
    let (code, out) = hc(&s, &["run", "--timeout", "10", "--cwd", &r, "git", "log", "--oneline"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(" c1\n") || out.trim_end().ends_with(" c1"), "all of it, no pager");

    let (code, out) = hc(&s, &["run", "awk 'BEGIN{for(i=1;i<=1000;i++) printf \"%0180d\\n\", i}'"]);
    assert_eq!(code, 0);
    assert!(out.len() < 60_000, "capped: {} bytes", out.len());
    assert!(out.starts_with("[hc] earlier output cut: hc read t"), "{}", &out[..100]);
    assert!(out.trim_end().ends_with(&format!("{:0180}", 1000)), "the tail is kept");
}

#[test]
fn a_program_that_says_it_stopped_working_returns_and_a_slow_one_keeps_running() {
    let s = Session::new();
    let (code, out) = hc(
        &s,
        &["run", "sh", "-c", &script(&s, "r state=working; echo busy; sleep 0.4; echo ready; r state=idle; cat")],
    );
    assert_eq!(code, 0);
    assert!(out.contains("ready") && out.contains("idle"), "back once idle, not at working: {out:?}");

    let (code, out) = hc(&s, &["run", "--timeout", "0.5", "sh", "-c", "echo started; sleep 100"]);
    assert_eq!(code, 124, "timed out, like timeout(1)");
    assert!(out.contains("started") && out.contains("still running as t2"), "{out:?}");
    assert_eq!(get(&s, "t2")["state"], "live");

    let started = s.json(&["run", "--bg", "sleep", "100"]);
    assert_eq!((started["address"].as_str(), started["state"].as_str()), (Some("t3"), Some("live")), "--bg: at once");
    let full = s.json(&["run", "sh", "-c", "echo json"]);
    assert_eq!(
        (full["wait"]["exit_code"].as_i64(), full["output"].as_str().map(str::trim)),
        (Some(0), Some("json")),
        "{full}"
    );
}

#[test]
fn runs_inherit_only_the_effective_local_parents_space_unless_explicit_or_unparented() {
    use serde_json::json;
    let s = Session::new();
    let parent = s.json(&["run", "--bg", "--label", "herdr.space=project", "--label", "role=pm", "sleep", "100"]);
    let caller = parent["address"].as_str().unwrap();
    let plain = run(&s, &["sleep", "100"]);
    s.json(&["run", "--bg", "--name", "selected", "--label", "herdr.space=selected", "sleep", "100"]);
    let mut conn = s.connect();
    for (i, extra, expected) in [
        (1, json!({}), json!({"herdr.space": "project"})),
        (
            2,
            json!({"labels": {"herdr.space": "other", "role": "worker"}}),
            json!({"herdr.space": "other", "role": "worker"}),
        ),
        (3, json!({"no_parent": true}), json!({})),
        (4, json!({"caller": "t99999"}), json!({})),
        (5, json!({"caller": null}), json!({})),
        (6, json!({"parent": plain}), json!({})),
        (7, json!({"outlive_parent": true}), json!({"herdr.space": "project"})),
        (8, json!({"parent": "selected"}), json!({"herdr.space": "selected"})),
        (9, json!({"caller": "remote/t1"}), json!({})),
        (10, json!({"labels": {"herdr.space": ""}}), json!({})),
    ] {
        let mut params = json!({"argv": ["sleep", "100"], "caller": caller});
        params.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let reply = conn.call(i, "terminal.run", params);
        assert_eq!(reply["result"]["labels"], expected, "{reply}");
    }
    let identity = s.json(&["server", "status"]);
    for on in [vec![], vec!["--on", identity["core"]["id"].as_str().unwrap()]] {
        let out = s
            .cmd()
            .env("HC_TERMINAL_ID", caller)
            .args(["--json", "run", "--bg"])
            .args(&on)
            .args(["sleep", "100"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let child: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(get(&s, child["address"].as_str().unwrap())["labels"], json!({"herdr.space": "project"}));
    }
    let reply = conn.call(11, "terminal.run", json!({"argv": ["sleep", "100"], "caller": plain}));
    assert_eq!(reply["result"]["labels"], json!({}));
}

#[test]
fn remote_runs_inherit_only_an_explicit_destination_parents_space() {
    use serde_json::json;
    let remote = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("remote", &remote)]));
    let parent = s.json(&["run", "--bg", "--label", "herdr.space=source", "sleep", "100"]);
    let destination = remote.json(&["run", "--bg", "--label", "herdr.space=destination", "sleep", "100"]);
    s.json(&["machine", "add", "remote"]);
    for (flags, expected) in [
        (vec![], json!({})),
        (vec!["--label", "herdr.space=explicit"], json!({"herdr.space": "explicit"})),
        (vec!["--parent", destination["address"].as_str().unwrap()], json!({"herdr.space": "destination"})),
        (vec!["--no-parent"], json!({})),
    ] {
        let out = s
            .cmd()
            .env("HC_TERMINAL_ID", parent["address"].as_str().unwrap())
            .args(["--json", "run", "--bg", "--on", "remote"])
            .args(&flags)
            .args(["sleep", "100"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let child: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(get(&s, child["address"].as_str().unwrap())["labels"], expected);
    }
}
