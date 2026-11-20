//! A person's unsent draft against real line editors and odd input framing:
//! the probes from the independent review of hm#101, each a case where an
//! early "the input is empty" would let agent text into what a person left.

#![cfg(unix)]
mod common;
use common::framed::Viewer;
use common::*;
use serde_json::json;
use std::time::Duration;

fn ses() -> Session {
    let mut s = Session::new();
    s.env.push(("HC_QUIET_MS".into(), "80".into()));
    s.env.push(("HC_DRAFT_MAX_MS".into(), "400".into()));
    s.env.push(("HC_DRAFT_IDLE_MS".into(), "600000".into()));
    s.env.push(("HC_DRAFT_LEFT_MS".into(), "60000".into()));
    s
}
fn barrier(v: &mut Viewer, t: &str) {
    v.conn.call("terminal.get", json!({"terminal": t}));
}

// Real Readline: Alt-Backspace deletes only words2, leaving two_.
#[test]
fn alt_backspace_must_not_submit_the_remaining_prefix() {
    let s = ses();
    let out = s.root.join("line");
    let script = format!(
        "INPUTRC=/dev/null; export INPUTRC; IFS= read -r -e -p READY v; printf '%s' \"$v\" > {}; sleep 20",
        out.display()
    );
    let t = run(&s, &["bash", "--noprofile", "--norc", "-c", &script]);
    let mut v = Viewer::attach(&s, &t, 80, 24, json!({}));
    v.shows("READY");
    v.typed(b"two_words2\x1b\x7f");
    barrier(&mut v, &t);
    let send = s.hc(&["send", &t, "AGENT"]);
    if send.status.success() {
        wait_until("readline finished", || out.exists());
        let got = std::fs::read_to_string(&out).unwrap();
        panic!("send entered a person's remaining draft: {got:?}; get={}", s.json(&["get", &t]));
    }
    let err: serde_json::Value = serde_json::from_slice(&send.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "drafting");
}

#[test]
fn a_split_utf8_character_and_one_backspace_leaves_no_draft() {
    let s = ses();
    let t = run(&s, &["sh", "-c", BYTES]);
    let mut v = Viewer::attach(&s, &t, 80, 24, json!({}));
    v.typed(&[0xc3]);
    barrier(&mut v, &t);
    v.typed(&[0xa9]);
    barrier(&mut v, &t);
    wait_until("utf8 arrived", || text(&s, &t).contains("a9"));
    v.typed(b"\x7f");
    barrier(&mut v, &t);
    let state = s.json(&["get", &t]);
    let send = s.hc(&["send", &t, "x"]);
    assert!(
        send.status.success(),
        "one Unicode character was erased but send failed: {}; get={state}",
        String::from_utf8_lossy(&send.stderr)
    );
}

#[test]
fn a_split_modified_delete_key_must_not_be_counted_as_printable_tail() {
    let s = ses();
    let t = run(&s, &["sh", "-c", BYTES]);
    let mut v = Viewer::attach(&s, &t, 80, 24, json!({}));
    v.typed(b"x");
    barrier(&mut v, &t);
    wait_until("raw ready", || text(&s, &t).contains("78"));
    v.typed(b"\x1b[127;");
    barrier(&mut v, &t);
    v.typed(b"1u");
    barrier(&mut v, &t);
    let state = s.json(&["get", &t]);
    let send = s.hc(&["send", &t, "y"]);
    assert!(
        send.status.success(),
        "CSI-u erased x but tail was misparsed: {}; get={state}",
        String::from_utf8_lossy(&send.stderr)
    );
}

#[test]
fn bracketed_paste_across_frames_keeps_control_bytes_literal() {
    let mut t = herdr_core::input::Typing::default();
    t.keys(b"\x1b[200~abc");
    t.keys(b"\x7f\x7f\x7f\x1b[201~");
    assert!(t.drafting(), "DEL bytes INSIDE an open bracketed paste must not erase abc");
}

#[test]
fn a_second_viewer_keeps_left_draft_until_the_last_viewer_leaves() {
    let mut s = ses();
    s.env.push(("HC_DRAFT_LEFT_MS".into(), "300".into()));
    let t = run(&s, &["sh", "-c", BYTES]);
    let mut a = Viewer::attach(&s, &t, 80, 24, json!({}));
    let mut b = Viewer::attach(&s, &t, 80, 24, json!({}));
    a.typed(b"a");
    barrier(&mut a, &t);
    b.typed(b"b");
    barrier(&mut b, &t);
    assert_eq!(s.json(&["get", &t])["draft"]["chars"], 2);
    drop(b);
    std::thread::sleep(Duration::from_millis(400));
    assert!(s.json(&["get", &t]).get("draft").is_some());
    drop(a);
    wait_until("no viewers", || s.json(&["viewers", &t])["viewers"].as_array().unwrap().is_empty());
    assert!(s.hc(&["send", &t, "z"]).status.success());
}

#[test]
fn explicitly_automatic_replies_do_not_keep_a_draft_alive() {
    let mut s = ses();
    s.env.push(("HC_DRAFT_IDLE_MS".into(), "300".into()));
    s.env.push(("HC_DRAFT_MAX_MS".into(), "2000".into()));
    let t = run(&s, &["sh", "-c", BYTES]);
    let mut v = Viewer::attach(&s, &t, 80, 24, json!({}));
    v.typed(b"x");
    barrier(&mut v, &t);
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(80));
        v.conn.send(v.channel, herdr_core::wire::AUTOMATIC_INPUT, b"\x1b[24;80R");
        barrier(&mut v, &t);
    }
    assert!(s.json(&["get", &t]).get("draft").is_none());
    assert!(s.hc(&["send", &t, "y"]).status.success());
}

#[test]
fn csi_u_associated_text_must_count_every_inserted_character() {
    let mut t = herdr_core::input::Typing::default();
    // A key event with two associated text codepoints, per kitty's protocol.
    t.keys(b"\x1b[97;1;97:98u");
    t.keys(b"\x7f");
    assert!(t.drafting(), "ab minus b still leaves a in a protocol-aware editor");
}

#[test]
fn shifted_punctuation_must_not_turn_into_a_word_character() {
    let mut t = herdr_core::input::Typing::default();
    t.keys(b"ab\x1b[49:33;2;33ucd"); // ab!cd, not ab1cd
    t.keys(b"\x1b\x7f"); // deletes cd in an alphanumeric-word editor
    assert!(t.drafting(), "ab! remains in the composer");
}

#[test]
fn agent_keys_must_invalidate_the_existing_draft_count() {
    let s = ses();
    let out = s.root.join("line");
    let script = format!(
        "INPUTRC=/dev/null; export INPUTRC; IFS= read -r -e -p READY v; printf '%s' \"$v\" > {}; sleep 20",
        out.display()
    );
    let t = run(&s, &["bash", "--noprofile", "--norc", "-c", &script]);
    let mut v = Viewer::attach(&s, &t, 80, 24, json!({}));
    v.shows("READY");
    v.typed(b"abc");
    barrier(&mut v, &t);
    s.json(&["keys", &t, "left"]);
    v.typed(b"\x7f\x7f\x7f");
    barrier(&mut v, &t);
    let send = s.hc(&["send", &t, "AGENT"]);
    if send.status.success() {
        wait_until("readline finished", || out.exists());
        panic!("agent keys moved the cursor but count stayed exact: {:?}", std::fs::read_to_string(&out).unwrap());
    }
    let err: serde_json::Value = serde_json::from_slice(&send.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "drafting");
}
