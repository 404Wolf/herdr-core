#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::framed::Viewer;
use common::*;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex, mpsc};
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

    /// Nothing more arrives for a moment.
    fn quiet(&self) -> bool {
        self.lines.recv_timeout(Duration::from_millis(800)).is_err()
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

#[test]
fn an_inbox_only_send_is_queued_or_refused_never_typed() {
    let rohan = Session::new();
    let mut s = session();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    let echo = "stty -echo; r state=idle; while read -r l; do echo \"got:$l\"; done";
    let sender = run(&s, &[], echo);
    let agent = run(&s, &[], echo);
    wait_until("reported", || get(&s, &sender)["reported"] == true && get(&s, &agent)["reported"] == true);

    // Nobody follows its inbox: refused, and nothing is typed (not even after the quiet period).
    let (_, err) = s.err(&["send", "--inbox-only", &agent, "review it"]);
    assert_eq!(err["error"]["kind"], "not_following", "{err}");
    std::thread::sleep(Duration::from_millis(800));
    assert!(!text(&s, &agent).contains("got:"), "nothing typed: {}", text(&s, &agent));

    // Following: queued like any send. `--` keeps a text that looks like a flag literal.
    let follower = Follower::start(&s, &agent, &[]);
    let r = hc_in(&s, &sender, &["send", "--inbox-only", &agent, "review it"]);
    assert_eq!(r["via"], "inbox", "{r}");
    assert_eq!(follower.next()["text"], format!("<hc from=\"{sender}\">review it</hc>"));
    hc_in(&s, &sender, &["send", "--inbox-only", "--", &agent, "--help me"]);
    assert_eq!(follower.next()["text"], format!("<hc from=\"{sender}\">--help me</hc>"));
    assert!(!text(&s, &agent).contains("got:"), "nothing typed: {}", text(&s, &agent));

    // Another machine's core may predate `terminal.queue`: it's never forwarded.
    let (_, err) = s.err(&["send", "--inbox-only", "rohan/t1", "hi"]);
    assert_eq!(err["error"]["kind"], "machine_unknown", "{err}");
    // Flags that only make sense for typing don't combine with it.
    assert!(!s.hc(&["send", "--inbox-only", "--force", &agent, "x"]).status.success());
    // The core says it takes it.
    let status = s.json(&["server", "status"]);
    assert!(status["features"].as_array().unwrap().iter().any(|f| f == "queue"), "{status}");
}

/// A core that predates `terminal.queue`, on its own socket, modelled on a
/// real one: it refuses methods it doesn't know, and it ignores fields it
/// doesn't know, so any `terminal.send` it gets is typed into the terminal,
/// which follows no inbox (it answers as an old core does: no `via`).
/// Records each method called.
fn old_core(dir: &Path) -> (PathBuf, Arc<Mutex<Vec<String>>>) {
    let socket = dir.join("old-core.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().map_while(Result::ok) {
            let seen = seen.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(conn.try_clone().unwrap());
                let mut conn = conn;
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let method = req["method"].as_str().unwrap_or_default().to_string();
                    seen.lock().unwrap().push(method.clone());
                    let mut reply = match method.as_str() {
                        "hello" => json!({"result": {
                            "generation": 1, "version": "0.0.0", "core": {"id": "old", "name": "old"},
                            "framed": false, "methods": ["hello", "terminal.send"], "features": ["framed", "status"],
                        }}),
                        "terminal.send" => json!({"result": {"status": "idle", "turn": 1, "started": false}}),
                        _ => json!({"error": {
                            "code": -32601, "message": format!("unknown method {method:?}"),
                            "data": {"kind": "unknown_method", "hint": "see `hc schema`"},
                        }}),
                    };
                    reply["jsonrpc"] = json!("2.0");
                    reply["id"] = req["id"].clone();
                    let _ = writeln!(conn, "{reply}");
                    line.clear();
                }
            });
        }
    });
    (socket, calls)
}

#[test]
fn a_new_hc_never_has_a_core_that_predates_inbox_only_sends_type_them() {
    let s = session();
    let (socket, calls) = old_core(&s.root);
    let out = s.cmd().env("HC_SOCKET", &socket).args(["send", "--inbox-only", "t1", "hi"]).output().unwrap();
    assert!(!out.status.success());
    let err: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "unknown_method", "{err}");
    let calls = calls.lock().unwrap().clone();
    assert!(calls.contains(&"terminal.queue".to_string()), "{calls:?}");
    assert!(!calls.contains(&"terminal.send".to_string()), "it would have been typed: {calls:?}");
}

#[test]
fn an_inbox_only_send_with_a_key_is_queued_once_across_lost_answers_and_restarts() {
    let s = session();
    let echo = "stty -echo; r state=working; while read -r l; do echo \"got:$l\"; done";
    let sender = run(&s, &[], echo);
    let agent = run(&s, &[], echo);
    wait_until("reported", || get(&s, &sender)["reported"] == true && get(&s, &agent)["reported"] == true);
    let mut person = Viewer::attach(&s, &agent, 80, 24, json!({}));
    person.typed(b"human steer\r");
    wait_until("human prompt", || get(&s, &agent)["prompt"]["by"] == "human");
    let follower = Follower::start(&s, &agent, &[]);
    // The core says it keeps keys, and for how long: a sender retries within that.
    let status = s.json(&["server", "status"]);
    assert!(status["features"].as_array().unwrap().iter().any(|f| f == "queue-keyed"), "{status}");
    assert_eq!(status["queue_keys_kept_ms"], 60 * 60 * 1000, "{status}");
    let core = status["core"]["id"].as_str().unwrap().to_string();
    let send = |key: &str, text: &str| {
        hc_in(&s, &sender, &["send", "--inbox-only", "--core", &core, "--key", key, &agent, text])
    };

    let first = send("diffd-k1", "review it");
    assert_eq!((first["via"].as_str(), first.get("repeated")), (Some("inbox"), None), "{first}");
    let message = follower.next();
    assert_eq!(message["text"], format!("<hc from=\"{sender}\">review it</hc>"));
    let prompt = get(&s, &agent)["prompt"].clone();
    assert_eq!(prompt, json!({"by":"agent", "from":sender, "ms":message["ts"]}));
    assert_eq!(get(&s, &agent)["status"]["state"], "working", "enqueue is not inferred consumption");
    // The sender lost that answer and asks again: queued already, not twice.
    let again = send("diffd-k1", "review it");
    assert_eq!((again["via"].as_str(), again["repeated"].as_bool()), (Some("inbox"), Some(true)), "{again}");
    assert_eq!(get(&s, &agent)["prompt"], prompt, "a deduplicated retry is not a new submission");
    assert!(follower.quiet(), "queued once");
    // Another key is another message.
    send("diffd-k2", "and this");
    assert_eq!(follower.next()["text"], format!("<hc from=\"{sender}\">and this</hc>"));
    let prompt = get(&s, &agent)["prompt"].clone();

    // Acknowledged, and the core restarted: the keys are kept with the terminal.
    hc_in(&s, &agent, &["inbox", "ack", "2"]);
    drop(follower);
    s.json(&["server", "restart"]);
    let back = Follower::start(&s, &agent, &[]);
    let after = send("diffd-k1", "review it");
    assert_eq!(after["repeated"].as_bool(), Some(true), "{after}");
    assert_eq!(get(&s, &agent)["prompt"], prompt, "persisted through restart and unchanged by retry");
    assert!(back.quiet(), "not queued again after the restart");
    // Even with nobody following, a key already queued is answered as queued, not refused.
    drop(back);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(send("diffd-k2", "and this")["repeated"].as_bool(), Some(true));
    let (_, err) = s.err(&["send", "--inbox-only", "--key", "diffd-k3", &agent, "new"]);
    assert_eq!(err["error"]["kind"], "not_following", "{err}");
    // A key first sent before the terminal's keys go back may have been
    // queued and forgotten: refused as such, never queued again, never
    // `not_following` (which would say it never was).
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let long_ago = (now - 2 * 60 * 60 * 1000).to_string();
    let (_, err) = s.err(&["send", "--inbox-only", "--key", "diffd-old", "--key-since", &long_ago, &agent, "x"]);
    assert_eq!(err["error"]["kind"], "key_forgotten", "{err}");
    // A key it still has is still a repeat, however long ago it was first sent.
    let r = hc_in(&s, &sender, &["send", "--inbox-only", "--key", "diffd-k1", "--key-since", &long_ago, &agent, "x"]);
    assert_eq!(r["repeated"].as_bool(), Some(true), "{r}");
    // A key belongs to the core that queued it: another refuses.
    let (_, err) = s.err(&["send", "--inbox-only", "--core", "another-core", "--key", "diffd-k1", &agent, "x"]);
    assert_eq!(err["error"]["kind"], "other_core", "{err}");
    // Keys are 1 to 128 bytes; --key needs --inbox-only.
    let (_, err) = s.err(&["send", "--inbox-only", "--key", &"k".repeat(129), &agent, "x"]);
    assert_eq!(err["error"]["kind"], "invalid_params", "{err}");
    assert!(!s.hc(&["send", "--key", "k", &agent, "x"]).status.success());
    assert!(!text(&s, &agent).contains("got:"), "nothing typed: {}", text(&s, &agent));
}
