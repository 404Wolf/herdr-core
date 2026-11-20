//! Round-3 review probes of hm#101 (rev-hm101 r3), verbatim: keys a person
//! presses while an agent's input goes in (held, delivered after it) make an
//! uncertain draft, and an Esc before the agent's input can't swallow them
//! (`carried_escape_before_send_must_not_swallow_a_held_human_character` was
//! `xAGENT`; `held_plain_text_is_uncertain_after_an_agent_send` is the plain
//! held text variant). Left out: the reviewer's exploratory
//! `delayed_escape_then_backspace_encoding_does_not_erase_real_readline_text`,
//! which sends a CSI u backspace to a Readline that never asked for CSI u keys;
//! the review states it is not a finding.

// `Duration` served the left-out probe; the rest is as the reviewer wrote it.
#![allow(unused_imports)]
#![cfg(unix)]
mod common;
use common::framed::Viewer;
use common::*;
use herdr_core::input::Typing;
use serde_json::json;
use std::process::Stdio;
use std::time::Duration;

fn ses() -> Session {
    let mut s = Session::new();
    for (k,v) in [("HC_QUIET_MS","80"),("HC_DRAFT_MAX_MS","400"),
                  ("HC_DRAFT_IDLE_MS","600000"),("HC_DRAFT_LEFT_MS","60000")] {
        s.env.push((k.into(),v.into()));
    }
    s
}
fn barrier(v: &mut Viewer, t: &str) { v.conn.call("terminal.get",json!({"terminal":t})); }

fn held_prefix(prefix: &[u8], held: &[u8]) {
    let s=ses();
    let out=s.root.join("line");
    let first=s.root.join("first");
    let script=s.root.join("readline.sh");
    std::fs::write(&script,format!(
        "INPUTRC=/dev/null; export INPUTRC; LC_ALL=C.UTF-8; export LC_ALL; IFS= read -r -e -p READY1 a; printf '%s' \"$a\" > {}; IFS= read -r -e -p READY2 b; printf '%s' \"$b\" > {}; sleep 20",
        first.display(),out.display())).unwrap();
    let td=s.root.join("config/herdr-core/templates");
    std::fs::create_dir_all(&td).unwrap();
    std::fs::write(td.join("reader.md"),format!(
        "---\npaste_gap_ms: 1500\ncommand: bash --noprofile --norc {}\n---\n",script.display())).unwrap();
    let t=run(&s,&["reader"]);
    let mut v=Viewer::attach(&s,&t,80,24,json!({}));
    v.shows("READY1");
    v.typed(prefix); barrier(&mut v,&t);
    let send1=s.cmd().args(["send",&t,"FIRST"]).stdin(Stdio::null())
        .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    v.shows("FIRST");
    // The paste is visibly in Readline, and its 1500 ms gap has not ended.
    assert!(!first.exists(),"missed the busy window");
    v.typed(held); barrier(&mut v,&t);
    let during=s.json(&["get",&t]);
    assert!(!first.exists(),"held input was not sent within the busy window");
    let sent1=send1.wait_with_output().unwrap();
    assert!(sent1.status.success(),"{}",String::from_utf8_lossy(&sent1.stderr));
    v.shows("READY2");
    let before=s.json(&["get",&t]);
    let sent=s.hc(&["send",&t,"AGENT"]);
    if sent.status.success() {
        wait_until("second Readline submitted",||out.exists());
        let got=std::fs::read(&out).unwrap();
        assert_eq!(got,b"AGENT", "agent boundary prefix={prefix:02x?}, held={held:02x?}: captured {:?}; first={:?}; during={during}; before={before}",String::from_utf8_lossy(&got),std::fs::read(&first).unwrap());
    } else {
        let err:serde_json::Value=serde_json::from_slice(&sent.stderr).unwrap();
        assert_eq!(err["error"]["kind"],"drafting","{err}");
        eprintln!("held prefix guarded: prefix={prefix:02x?}, held={held:02x?}; before={before}");
    }
}

#[test]
fn carried_escape_before_send_must_not_swallow_a_held_human_character() {
    held_prefix(b"\x1b",b"x");
}

#[test]
fn carried_csi_before_send_must_not_swallow_held_text() {
    held_prefix(b"\x1b[",b"97u");
}

#[test]
fn a_new_escape_held_during_send_cannot_absorb_the_next_character() {
    held_prefix(b"",b"\x1b");
}

#[test]
fn held_plain_text_is_uncertain_after_an_agent_send() {
    held_prefix(b"",b"xy\x7f");
}

#[test]
fn every_non_whitelisted_token_keeps_an_active_draft_for_all_frame_splits() {
    let tokens:&[&[u8]]=&[
        b"\x1b[D",b"\x1b[A",b"\x1b[H",b"\x1b[3~",b"\x1bOH",b"\x1bOD",
        b"\x1b\x7f",b"\x1bb",b"\x17",b"\x09",b"\x04",b"\x00",b"\n",
        b"\x1b[<0;2;2M",b"\x1b[<64;2;2M",b"\x1b[<35;2;2M",b"\x1b[<0;2;2m",
        b"\x1b[M ##",b"\x1b[M`##",b"\x1b[24;80R",b"\x1b[?1;2c",b"\x1b[?1u",
        b"\x1b[I",b"\x1b[O",b"\x1b]11;?\x07",b"\x1bPq\x1b\\",b"\x1b_q\x1b\\",
        b"\x1b[200~\x1b[201~",b"\x1b[200~ab\x1b[200~\x1b[201~",
        b"\x1b[200~\r\x03\x15\x1b[201~",b"\x1b[97;5u",b"\x1b[127;3u",
        b"\x1b[13;2u",b"\x1b[27u",b"\x1b[57352u",
    ];
    let mut variants=0;
    for token in tokens {
        for at in 0..=token.len() {
            let mut t=Typing::default();t.keys(b"ab");
            t.keys(&token[..at]);t.keys(&token[at..]);t.keys(&[0x7f;32]);
            assert!(t.drafting(),"token={token:02x?},split={at}; {t:?}");
            assert_eq!(t.chars(),None,"token={token:02x?},split={at}; {t:?}");
            variants+=1;
        }
        let mut t=Typing::default();t.keys(b"ab");
        for b in *token {t.keys(&[*b]);}t.keys(&[0x7f;32]);
        assert!(t.drafting(),"bytewise token={token:02x?}; {t:?}");
        variants+=1;
    }
    eprintln!("{variants} non-whitelist framing variants remained uncertain");
}

#[test]
fn allowed_text_release_and_modifier_streams_preserve_counts_at_all_splits() {
    let cases:&[(&[u8],usize)]=&[
        (b"abc",3),("é😀".as_bytes(),2),(b"\x1b[97u",1),(b"\x1b[97;1:1u",1),
        (b"\x1b[97;1:2u",1),(b"\x1b[97;1:3u",0),(b"\x1b[127;1:3u",0),
        (b"\x1b[57441;2u",0),(b"\x1b[57442;5u",0),(b"\x1b[57452u",0),
        (b"\x1b[97;1;97:98u",2),(b"\x1b[97;1:3;97u",0),
        (b"\x1b[104:72;130u",1),(b"\x1b[104;2u",1),
        (b"\x1b[0;;229u",1),(b"\x1b[0;;128512:97u",2),
    ];
    for (token,count) in cases {
        for at in 0..=token.len() {
            let mut t=Typing::default();t.keys(b"xy");
            t.keys(&token[..at]);t.keys(&token[at..]);
            assert_eq!(t.chars(),Some(2+count),"token={token:02x?},split={at};{t:?}");
            t.keys(&vec![0x7f;2+count]);
            assert!(!t.drafting(),"token={token:02x?},split={at};{t:?}");
        }
    }
}

#[test]
fn two_viewers_share_delivery_order_even_inside_sequences() {
    let s=ses();let t=run(&s,&["sh","-c",BYTES]);
    let mut a=Viewer::attach(&s,&t,80,24,json!({}));
    let mut b=Viewer::attach(&s,&t,80,24,json!({}));
    a.typed(b"x");barrier(&mut a,&t);
    wait_until("raw receiver ready",||text(&s,&t).contains("78"));
    a.typed(b"\x1b[127;");barrier(&mut a,&t);
    b.typed(b"1u");barrier(&mut b,&t);
    assert!(s.json(&["get",&t]).get("draft").is_none());
    a.typed(b"\xc3");barrier(&mut a,&t);
    b.typed(b"\xa9z");barrier(&mut b,&t);
    assert_eq!(s.json(&["get",&t])["draft"]["chars"],2);
    a.typed(b"\x7f");barrier(&mut a,&t);
    b.typed(b"\x7f");barrier(&mut b,&t);
    assert!(s.json(&["get",&t]).get("draft").is_none());
    a.typed(b"ab\x1b[200");barrier(&mut a,&t);
    b.typed(b"~c\x1b[20");barrier(&mut b,&t);
    a.typed(b"1~\x7f\x7f\x7f");barrier(&mut a,&t);
    assert!(s.json(&["get",&t])["draft"]["chars"].is_null());
    assert_eq!(s.err(&["send",&t,"AGENT"]).1["error"]["kind"],"drafting");
}
