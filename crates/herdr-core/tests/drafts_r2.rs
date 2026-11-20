//! Round-2 review probes of hm#101 (rev-hm101 r2), verbatim: a paste-start
//! marker inside an open paste, and UTF-8 in a C-locale Readline.

#![cfg(unix)]
mod common;
use common::framed::Viewer;
use common::*;
use serde_json::json;

fn ses() -> Session {
    let mut s = Session::new();
    s.env.push(("HC_QUIET_MS".into(), "80".into()));
    s.env.push(("HC_DRAFT_MAX_MS".into(), "400".into()));
    s.env.push(("HC_DRAFT_IDLE_MS".into(), "600000".into()));
    s.env.push(("HC_DRAFT_LEFT_MS".into(), "60000".into()));
    s
}

fn real_readline_guard(label: &str, frames: &[&[u8]], locale: &str) {
    let s = ses();
    let out = s.root.join("line");
    let script = format!(
        "INPUTRC=/dev/null; export INPUTRC; LC_ALL={locale}; export LC_ALL; bind 'set enable-bracketed-paste on'; IFS= read -r -e -p READY v; printf '%s' \"$v\" > {}; sleep 20",
        out.display()
    );
    let t = run(&s, &["bash", "--noprofile", "--norc", "-c", &script]);
    let mut v = Viewer::attach(&s, &t, 80, 24, json!({}));
    v.shows("READY");
    for frame in frames {
        v.typed(frame);
        v.conn.call("terminal.get", json!({"terminal": t}));
    }
    let before = s.json(&["get", &t]);
    let sent = s.hc(&["send", &t, "AGENT"]);
    if sent.status.success() {
        wait_until("readline submitted", || out.exists());
        let got = std::fs::read(&out).unwrap();
        if got == b"AGENT" {
            eprintln!("{label}: actual editor input was empty; no contamination");
            return;
        }
        panic!("{label}: early-clear accepted send; readline captured bytes {got:02x?} ({:?}); before={before}", String::from_utf8_lossy(&got));
    }
    let err: serde_json::Value = serde_json::from_slice(&sent.stderr).unwrap();
    assert_eq!(err["error"]["kind"], "drafting", "{label}");
    eprintln!("{label}: guarded, before={before}");
}

#[test]
fn nested_paste_start_is_literal_content_in_readline() {
    real_readline_guard("nested paste", &[b"\x1b[200~abc\x1b[200~\x1b[201~\x7f\x7f\x7f"], "C.UTF-8");
}

#[test]
fn nested_paste_start_split_across_frames_stays_guarded() {
    real_readline_guard("split nested paste", &[b"\x1b[200~abc", b"\x1b[200", b"~\x1b[201~", b"\x7f\x7f\x7f"], "C.UTF-8");
}

#[test]
fn utf8_readline_in_c_locale_must_not_clear_before_byte_erase() {
    real_readline_guard("C locale UTF-8", &[b"\xc3\xa9\x7f"], "C");
}

#[test]
fn a_nested_start_makes_the_paste_uncertain() {
    let mut typing = herdr_core::input::Typing::default();
    typing.keys(b"\x1b[200~abc\x1b[200~\x1b[201~\x7f\x7f\x7f");
    assert!(typing.drafting(), "literal nested start remains: {typing:?}");
}
