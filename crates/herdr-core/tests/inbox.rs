#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::process::{Child, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn session() -> Session {
    let mut s = Session::new();
    s.env.push(("HC_QUIET_MS".into(), "300".into()));
    s.env.push(("HC_STARTED_MS".into(), "500".into()));
    s
}

fn run(s: &Session, extra: &[&str], body: &str) -> String {
    let script = script(s, body);
    let mut args = vec!["run", "--bg"];
    args.extend_from_slice(extra);
    args.extend(["sh", "-c", script.as_str()]);
    s.json(&args)["address"].as_str().unwrap().to_string()
}

/// `hc` as if typed inside terminal `t`.
fn hc_in(s: &Session, t: &str, args: &[&str]) -> Value {
    let out = s.cmd().env("HC_TERMINAL_ID", t).args(args).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// `hc inbox --follow` inside `t`: its JSON lines, as they come.
struct Follower {
    child: Child,
    lines: mpsc::Receiver<Value>,
}

impl Follower {
    fn start(s: &Session, t: &str, extra: &[&str]) -> Follower {
        let mut cmd = s.cmd();
        cmd.env("HC_TERMINAL_ID", t).args(["inbox", "--follow"]).args(extra).stdout(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in out.lines().map_while(Result::ok) {
                let _ = tx.send(serde_json::from_str(&line).unwrap());
            }
        });
        // The subscription is in place once the core counts it: a send says so.
        std::thread::sleep(Duration::from_millis(500));
        Follower { child, lines }
    }

    fn next(&self) -> Value {
        self.lines.recv_timeout(Duration::from_secs(10)).expect("a message")
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn a_program_following_its_inbox_gets_messages_out_of_band_in_order_until_it_acknowledges_them() {
    let s = session();
    let echo = "stty -echo; r state=idle; while read -r l; do echo \"got:$l\"; done";
    let sender = run(&s, &[], echo);
    let agent = run(&s, &[], echo);
    wait_until("reported", || get(&s, &sender)["reported"] == true && get(&s, &agent)["reported"] == true);

    // Only the terminal itself follows its inbox.
    let out =
        s.cmd().env("HC_TERMINAL_ID", &sender).args(["inbox", "--follow", "--terminal", &agent]).output().unwrap();
    assert!(!out.status.success());

    let follower = Follower::start(&s, &agent, &[]);
    let r = hc_in(&s, &sender, &["send", &agent, "review it"]);
    assert_eq!((r["via"].as_str(), r["waiting"].as_u64()), (Some("inbox"), Some(1)), "{r}");
    hc_in(&s, &sender, &["send", &agent, "and this"]);
    let (one, two) = (follower.next(), follower.next());
    assert_eq!(
        (one["id"].as_u64(), one["kind"].as_str(), one["from"].as_str()),
        (Some(1), Some("send"), Some(sender.as_str()))
    );
    assert_eq!(one["text"], format!("<hc from=\"{sender}\">review it</hc>"), "tagged as when typed");
    assert_eq!(two["id"], 2);
    assert!(!text(&s, &agent).contains("got:"), "nothing typed: {}", text(&s, &agent));

    // Unacknowledged messages come again to the next follower; acknowledged ones leave.
    drop(follower);
    let again = Follower::start(&s, &agent, &[]);
    assert_eq!((again.next()["id"].as_u64(), again.next()["id"].as_u64()), (Some(1), Some(2)));
    assert_eq!(hc_in(&s, &agent, &["inbox", "ack", "1"])["waiting"], 1);
    let left = s.cmd().args(["inbox", "--terminal", &agent, "--json"]).output().unwrap();
    let left: Vec<Value> = left.stdout.as_slice().lines().map(|l| serde_json::from_str(&l.unwrap()).unwrap()).collect();
    assert_eq!(left.iter().map(|m| m["id"].as_u64().unwrap()).collect::<Vec<_>>(), [2]);

    // News about a child is queued too, as a note.
    let kid = run(&s, &["--parent", &agent], "r state=working; w a; r state=blocked; sleep 100");
    touch(&s, "a");
    let note = again.next();
    assert_eq!((note["id"].as_u64(), note["kind"].as_str()), (Some(3), Some("note")), "{note}");
    assert!(note["text"].as_str().unwrap().contains(&format!("{kid} is blocked")), "{note}");

    // Waiting messages and their ids survive a restart.
    drop(again);
    s.json(&["server", "restart"]);
    let ids = |s: &Session| {
        let out = s.cmd().args(["inbox", "--terminal", &agent, "--json"]).output().unwrap();
        out.stdout
            .as_slice()
            .lines()
            .map(|l| serde_json::from_str::<Value>(&l.unwrap()).unwrap()["id"].as_u64().unwrap())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&s), [2, 3]);
    let back = Follower::start(&s, &agent, &["--after", "3"]);
    hc_in(&s, &sender, &["send", &agent, "after the restart"]);
    assert_eq!(back.next()["id"], 4, "ids are never reused");

    // Nobody following: typed, as before.
    drop(back);
    std::thread::sleep(Duration::from_millis(300));
    let r = hc_in(&s, &sender, &["send", &agent, "typed now"]);
    assert_eq!(r["via"], "typed", "{r}");
    wait_until("typed", || text(&s, &agent).contains("got:<hc"));
}
