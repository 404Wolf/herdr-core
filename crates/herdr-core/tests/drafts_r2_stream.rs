//! Round-2 review probes of hm#101 (rev-hm101 r2), verbatim: an Esc a person's
//! frame ended with must not outlive an agent message, and every frame split
//! of a normal stream reads the same.

#![cfg(unix)]
mod common;
use common::framed::Viewer;
use common::*;
use serde_json::json;

#[test]
fn a_submitted_agent_message_must_retire_the_previous_escape_prefix() {
    let mut s = Session::new();
    s.env.push(("HC_QUIET_MS".into(), "80".into()));
    s.env.push(("HC_DRAFT_MAX_MS".into(), "400".into()));
    s.env.push(("HC_DRAFT_IDLE_MS".into(), "600000".into()));
    s.env.push(("HC_DRAFT_LEFT_MS".into(), "60000".into()));
    let out = s.root.join("line");
    let first = s.root.join("first");
    let script = format!(
        "INPUTRC=/dev/null; export INPUTRC; IFS= read -r -e -p READY1 a; printf '%s' \"$a\" > {}; IFS= read -r -e -p READY2 b; printf '%s' \"$b\" > {}; sleep 20",
        first.display(), out.display()
    );
    let t = run(&s, &["bash", "--noprofile", "--norc", "-c", &script]);
    let mut v = Viewer::attach(&s, &t, 80, 24, json!({}));
    v.shows("READY1");
    v.typed(b"\x1b");
    v.conn.call("terminal.get", json!({"terminal": t}));
    let sent = s.hc(&["send", &t, "FIRST"]);
    assert!(sent.status.success(), "first send: {}", String::from_utf8_lossy(&sent.stderr));
    v.shows("READY2");
    v.typed(b"x");
    v.conn.call("terminal.get", json!({"terminal": t}));
    let before = s.json(&["get", &t]);
    let sent = s.hc(&["send", &t, "AGENT"]);
    if sent.status.success() {
        wait_until("second readline submitted", || out.exists());
        let got = std::fs::read(&out).unwrap();
        panic!("stale Escape swallowed a person's x: submitted {:?}; first={:?}; before={before}", String::from_utf8_lossy(&got), std::fs::read(&first).unwrap());
    }
    let err: serde_json::Value = serde_json::from_slice(&sent.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "drafting");
}

#[test]
fn normal_streams_are_invariant_under_every_single_frame_split() {
    let cases: &[&[u8]] = &[
        b"\xc3\xa9\xf0\x9f\x98\x80\x7f\x7f",
        b"ab\x1b[127;1u\x1b[127;1u",
        b"ab\x1b[200~c\x1b[201~\x7f\x7f\x7f",
        b"ab\x1b[<0;3;3M\x7f\x7f",
        b"\x1b[97;1;97:98u\x7f",
        b"ab\x1b[24;80R\x7f\x7f",
        b"\x1b[200~abc\x7f\x1b[201~\x7f\x7f\x7f",
    ];
    for &bytes in cases {
        let mut whole = herdr_core::input::Typing::default();
        whole.keys(bytes);
        for at in 0..=bytes.len() {
            let mut split = herdr_core::input::Typing::default();
            split.keys(&bytes[..at]);
            split.keys(&bytes[at..]);
            assert_eq!(split, whole, "bytes={bytes:02x?}, split={at}");
        }
        let mut bytewise = herdr_core::input::Typing::default();
        for b in bytes { bytewise.keys(&[*b]); }
        assert_eq!(bytewise, whole, "bytewise bytes={bytes:02x?}");
    }
}
