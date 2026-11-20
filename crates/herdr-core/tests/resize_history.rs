#![cfg(target_os = "linux")]
//! `resize_history` end to end, here and through a relaying core: the attach
//! reply confirms the N the owner applies, and the repaint after a RESIZE
//! clears the viewer's history (ESC[3J) and carries the owner's.

mod common;

use common::framed::{Framed, Viewer};
use common::*;
use herdr_core::wire;
use serde_json::json;
use std::time::Duration;

fn has(raw: &[u8], text: &[u8]) -> bool {
    raw.windows(text.len()).any(|w| w == text)
}

#[test]
fn resize_history_is_confirmed_here_and_through_a_relay_and_repaints_carry_history() {
    let owner = Session::new();
    let t = run(&owner, &["sh", "-c", "seq 1 40; exec sleep 100"]);
    wait_until("output", || owner.json(&["read", &t])["text"].as_str().is_some_and(|s| s.contains("40")));
    let mut pickle = Session::new();
    pickle.env.push(fake_ssh(&pickle, &[("macro", &owner)]));
    pickle.json(&["machine", "add", "macro"]);
    let hello = Framed::open(&pickle).call("hello", json!({"framed": true}));
    assert!(hello["result"]["features"].as_array().unwrap().iter().any(|f| f == "resize_history"), "{hello}");
    let remote = format!("macro/{t}");

    // Without resize_notifications nothing is applied, here or there.
    for (session, terminal) in [(&owner, t.as_str()), (&pickle, remote.as_str())] {
        let reply = Framed::open(session).call(
            "terminal.attach",
            json!({"terminal": terminal, "cols": 30, "rows": 10, "observe": true, "resize_history": 5}),
        );
        assert_eq!(reply["result"]["resize_history"], 0, "{reply}");
        let reply = Framed::open(session).call(
            "terminal.attach",
            json!({"terminal": terminal, "cols": 30, "rows": 10, "resize_notifications": true, "resize_history": 5}),
        );
        assert_eq!(reply["result"]["resize_history"], 5, "confirmed: {reply}");
    }

    let extra = json!({"resize_notifications": true, "resize_history": 5});
    let mut phone = Viewer::attach_on(Framed::open(&pickle), &remote, 30, 10, extra);
    phone.conn.send(phone.channel, wire::RESIZE, &[30, 0, 10, 0]);
    phone.until("first paint", |v| v.screen.plain_text().contains("40"));
    phone.raw.clear();
    phone.conn.send(phone.channel, wire::RESIZE, &[35, 0, 10, 0]);
    phone.until("history repaint", |v| has(&v.raw, b"\x1b[3J") && has(&v.raw, b"40"));
    let at = phone.raw.windows(4).position(|w| w == b"\x1b[3J").unwrap();
    let after = &phone.raw[at..];
    // 32..40 and the cursor row are the screen: 27..31 are the last 5 history rows.
    assert!(after.starts_with(b"\x1b[3J27\r\n28\r\n29\r\n30\r\n31"), "{:?}", String::from_utf8_lossy(after));
    assert!(!has(after, b"26"), "only the last 5 history rows");
}

/// Heavy rows (`R0000`..), each with many color changes: about 3 KB of VT
/// per history row, so 300 rows are more than one 256 KiB credit window but
/// within 2 MiB, and 1000 rows are over 2 MiB.
fn heavy_rows(rows: usize) -> Vec<u8> {
    let mut text = Vec::new();
    for n in 0..rows {
        text.extend(format!("R{n:04}").as_bytes());
        for c in 0..97u32 {
            let c = c as usize;
            let (a, b) = ((n + c) % 251, (n * 3 + c) % 251);
            text.extend(format!("\x1b[38;2;{a};{b};{};48;2;{b};{};{a}mxy", c * 2, c + 50).as_bytes());
        }
        text.extend(b"\x1b[0m\r\n");
    }
    text
}

/// An owner terminal that prints `rows` heavy rows, then on a line `alt`
/// enters the alternate screen, `main` leaves it, `flood` prints 5 MiB of
/// SGR-only output (no text, no history change).
fn owner_terminal(owner: &Session, rows: usize) -> String {
    std::fs::write(owner.root.join("rows.vt"), heavy_rows(rows)).unwrap();
    std::fs::write(owner.root.join("sgr.vt"), b"\x1b[0m".repeat(5 << 18)).unwrap();
    let r = owner.root.display();
    let script = format!(
        "cat {r}/rows.vt; while read l; do case $l in alt) printf '\\033[?1049hALT';; \
         main) printf '\\033[?1049l';; flood) cat {r}/sgr.vt;; esac; done"
    );
    let t = run(owner, &["sh", "-c", &script]);
    let last = format!("R{:04}", rows - 1);
    wait_until("rows", || owner.json(&["read", &t])["text"].as_str().is_some_and(|s| s.contains(&last)));
    t
}

/// Ends the relay's line to `owner` for real: kills that line's own
/// `herdr-core connect` bridge (found by its HOME, the owner's root), with
/// redials blocked until `restore_line`.
fn cut_line(relay: &Session, owner: &Session) {
    std::fs::write(relay.root.join("down"), "").unwrap();
    let home = format!("HOME={}", owner.root.display());
    let mut killed = 0;
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else { continue };
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
        let is_bridge = cmdline.split(|b| *b == 0).any(|a| a == b"connect")
            && environ.split(|b| *b == 0).any(|e| e == home.as_bytes());
        if is_bridge {
            let _ = std::process::Command::new("/bin/sh").args(["-c", &format!("kill -9 {pid}")]).status();
            killed += 1;
        }
    }
    assert!(killed > 0, "no line bridge to {} found", owner.root.display());
    wait_until("line down", || relay.json(&["machine", "ls"])["machines"][0]["state"] == "unreachable");
}

fn restore_line(relay: &Session) {
    std::fs::remove_file(relay.root.join("down")).unwrap();
}

/// A phone-like viewer through `relay`, with a big scrollback of its own:
/// attached on 200x10 (which sizes the grid), then resized to 201x10 (a
/// resize paint), not crediting yet.
fn slow_phone(relay: &Session, terminal: &str, resize_history: u64) -> Viewer {
    let extra = json!({"resize_notifications": true, "resize_history": resize_history});
    let mut phone = Viewer::attach_on(Framed::open(relay), terminal, 200, 10, extra);
    phone.screen = ghostty_vt::Terminal::new(200, 10, 256 << 20);
    phone.credit = false;
    phone.conn.send(phone.channel, wire::RESIZE, &[200, 0, 10, 0]);
    phone.drain(Duration::from_millis(300));
    phone.conn.send(phone.channel, wire::RESIZE, &[201, 0, 10, 0]);
    phone
}

/// Credits again like a well-behaved viewer: one window now, then as bytes
/// arrive (so the relay never holds more than a window for it).
fn credit(phone: &mut Viewer) {
    phone.credit = true;
    phone.conn.send(phone.channel, wire::CREDIT, &(wire::WINDOW as u32).to_le_bytes());
}

fn relay_to(owner: &Session) -> Session {
    let mut relay = Session::new();
    relay.env.push(fake_ssh(&relay, &[("owner", owner)]));
    relay.env.push(("HC_RECONNECT_MS".into(), "200".into()));
    relay.json(&["machine", "add", "owner"]);
    relay
}

/// Review v4 (a), the real relay path: a history replacement is cut by the
/// line dropping (after its ESC[3J), the program is on the alternate screen
/// when the relay reattaches, and comes back to the main screen later. The
/// reconnect is the loss: the main-screen repair is owed until it is out.
#[test]
fn a_line_dropped_mid_replacement_is_repaired_after_an_alternate_screen() {
    let owner = Session::new();
    let t = owner_terminal(&owner, 400);
    let relay = relay_to(&owner);
    let remote = format!("owner/{t}");
    let mut phone = slow_phone(&relay, &remote, 300);
    phone.until("a replacement cut by the credit window", |v| has(&v.raw, b"\x1b[3J") && v.received > 200_000);
    phone.drain(Duration::from_millis(300));
    assert!(!phone.screen.plain_text().contains("R0385"), "the replacement's newest rows did not arrive");
    cut_line(&relay, &owner);
    owner.json(&["send", &t, "alt"]);
    wait_until("alternate screen", || owner.json(&["read", &t])["text"].as_str().is_some_and(|s| s.contains("ALT")));
    restore_line(&relay);
    credit(&mut phone);
    phone.until("reattached on the alternate screen", |v| v.screen.alt_screen());
    phone.drain(Duration::from_millis(500));
    phone.raw.clear();
    owner.json(&["send", &t, "main"]);
    phone.until("back on the main screen with history", |v| {
        !v.screen.alt_screen() && v.screen.plain_text().contains("R0385")
    });
    assert!(has(&phone.raw, b"\x1b[3J"), "the repair is a whole replacement");
    let text = phone.screen.plain_text();
    assert!(text.contains("R0110") && text.contains("R0380"), "the 300 history rows are back");
}

/// Review v4 (b), the real relay path: the repair a reattach owes (over
/// 2 MiB, N = 1000) is not out yet when more than 4 MiB of output overflows
/// the backlog behind it. It must still arrive whole, never screen-only.
#[test]
fn an_oversized_reconnect_repair_overflowed_by_output_still_arrives_whole() {
    let owner = Session::new();
    let t = owner_terminal(&owner, 1100);
    let relay = relay_to(&owner);
    let remote = format!("owner/{t}");
    let mut phone = slow_phone(&relay, &remote, 1000);
    phone.credit = true;
    phone.until("the screen", |v| v.screen.plain_text().contains("R1099"));
    phone.drain(Duration::from_millis(300));
    assert!(!has(&phone.raw, b"\x1b[3J"), "over the budget: resize paints are screen-only");
    phone.credit = false;
    cut_line(&relay, &owner);
    phone.drain(Duration::from_millis(300));
    phone.raw.clear();
    phone.received = 0;
    restore_line(&relay);
    // The repair starts out (one credit window at most), then output overflows behind it.
    phone.until("the repair starts", |v| has(&v.raw, b"\x1b[3J"));
    owner.json(&["send", &t, "flood"]);
    std::thread::sleep(Duration::from_millis(1500));
    credit(&mut phone);
    // What the owner itself still keeps (its scrollback budget may have let the oldest go).
    let kept = owner.json(&["read", &t, "--lines", "2000"])["text"].as_str().unwrap().to_string();
    let oldest = kept.lines().find(|l| l.starts_with('R')).unwrap()[..5].to_string();
    phone.until("the whole history", |v| {
        let text = v.screen.plain_text();
        text.contains(&oldest) && text.contains("R1080")
    });
    assert!(phone.received > 2 << 20, "an oversized repair: {} bytes since the reattach", phone.received);
}

/// The relayed viewer's connection at the owner, if one is attached.
fn relayed_at_owner(owner: &Session, t: &str) -> Option<u64> {
    let viewers = owner.json(&["viewers", t]);
    let relayed = viewers["viewers"].as_array()?.iter().find(|v| v["origin"] == "relayed")?.clone();
    relayed["identity"]["connection_id"].as_u64()
}

/// INS-2950, the real relay path: a phone stops returning credit (its
/// parser is busy) while the line to the owner drops and comes back 20
/// times. Each reattach used to start a fresh 256 KiB window there, so the
/// relay piled up windows for the phone until its 4 MiB backlog overflowed
/// and was dropped silently, the stream then going on mid-paint. Now a
/// reattach starts with only the credit the phone has left: nothing piles
/// up, nothing is dropped, and once the phone reads again every paint it
/// gets is whole.
#[test]
fn twenty_reattaches_under_a_held_phone_credit_drop_nothing() {
    let owner = Session::new();
    let t = owner_terminal(&owner, 400);
    let relay = relay_to(&owner);
    let remote = format!("owner/{t}");
    let mut phone = slow_phone(&relay, &remote, 200);
    // The phone takes its first window, then holds its credit.
    phone.drain(Duration::from_millis(800));
    assert!(phone.received > 0 && phone.received <= wire::WINDOW, "{}", phone.received);
    let mut previous = relayed_at_owner(&owner, &t);
    for round in 0..20 {
        cut_line(&relay, &owner);
        restore_line(&relay);
        wait_until(&format!("reattach {round}"), || {
            let now = relayed_at_owner(&owner, &t);
            now.is_some() && now != previous
        });
        previous = relayed_at_owner(&owner, &t);
        phone.drain(Duration::from_millis(100));
    }
    assert!(phone.received <= wire::WINDOW, "the held credit was respected: {}", phone.received);
    assert!(phone.closed.is_none(), "not closed while held");
    phone.raw.clear();
    credit(&mut phone);
    phone.until("the history back", |v| v.screen.plain_text().contains("R0300"));
    phone.drain(Duration::from_millis(500));
    assert!(phone.closed.is_none(), "{:?}", phone.closed);
    // What arrived after the hold: line notices, then whole paints (each a
    // CAN, then the same replacement), never the rest of a cut one.
    let notice = b"\r\n[line to owner lost; reconnecting...]\r\n";
    let mut after = phone.raw.clone();
    while let Some(at) = after.windows(notice.len()).position(|w| w == notice) {
        after.drain(at..at + notice.len());
    }
    let head = String::from_utf8_lossy(&after[..after.len().min(200)]).to_string();
    assert_eq!(after.first(), Some(&0x18), "it goes on with a whole paint: {head:?}");
    let paints: Vec<&[u8]> = after.split(|b| *b == 0x18).skip(1).collect();
    let last = paints.last().unwrap();
    assert!(paints.iter().all(|p| last.starts_with(p)), "every paint is a prefix of the whole one, none spliced");
    assert!(has(last, b"\x1b[3J"), "a whole replacement");
}

/// INS-2950 review: CREDIT the phone returns while a reattach is in flight
/// (after the relay took its starting credit, before the new stream there
/// is connected here) must reach the new stream. With a scheduling barrier
/// at the owner (a debug-build test gate) holding the attach reply, the phone
/// credits every byte it held; after the reply the repair must flow, not
/// stall at a starting credit of 0.
#[test]
fn credit_returned_while_a_reattach_is_in_flight_reaches_the_new_stream() {
    let mut owner = Session::new();
    let gate = owner.root.join("attach-gate");
    std::fs::create_dir(&gate).unwrap();
    owner.env.push(("HC_TEST_ATTACH_GATE".into(), gate.display().to_string()));
    let t = owner_terminal(&owner, 400);
    let relay = relay_to(&owner);
    let mut phone = slow_phone(&relay, &format!("owner/{t}"), 300);
    phone.drain(Duration::from_millis(500));
    assert_eq!(phone.received, wire::WINDOW, "the phone holds a whole window");
    cut_line(&relay, &owner);
    restore_line(&relay);
    wait_until("the reattach waits at the gate", || gate.join("ready").exists());
    let request: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(gate.join("ready")).unwrap()).unwrap();
    assert_eq!(request["credit"], 0, "nothing left to take when it started: {request}");
    // Every held byte consumed and credited before the new stream is connected here.
    phone.conn.send(phone.channel, wire::CREDIT, &(phone.received as u32).to_le_bytes());
    phone.credit = true;
    phone.drain(Duration::from_millis(300));
    phone.raw.clear();
    phone.received = 0;
    std::fs::write(gate.join("release"), "").unwrap();
    phone.until("the repair flows", |v| v.screen.plain_text().contains("R0385"));
    assert!(phone.closed.is_none());
    assert!(has(&phone.raw, b"\x1b[3J"), "the whole replacement");
}

/// A relay to `owner` whose reattaches wait at `HC_TEST_REATTACH_GATE`
/// after their reply (debug builds); returns it and the gate folder.
fn gated_relay_to(owner: &Session) -> (Session, std::path::PathBuf) {
    let mut relay = Session::new();
    let gate = relay.root.join("reattach-gate");
    std::fs::create_dir(&gate).unwrap();
    relay.env.push(("HC_TEST_REATTACH_GATE".into(), gate.display().to_string()));
    relay.env.push(fake_ssh(&relay, &[("owner", owner)]));
    relay.env.push(("HC_RECONNECT_MS".into(), "200".into()));
    relay.json(&["machine", "add", "owner"]);
    (relay, gate)
}

fn gated_owner() -> (Session, std::path::PathBuf) {
    let mut owner = Session::new();
    let gate = owner.root.join("attach-gate");
    std::fs::create_dir(&gate).unwrap();
    owner.env.push(("HC_TEST_ATTACH_GATE".into(), gate.display().to_string()));
    (owner, gate)
}

/// INS-2950 v3, the real relay path with scheduling barriers: reattach A
/// gets its reply, then waits (relay gate) while its line drops again;
/// reattach B starts at credit 0 and its owner reply is held (owner gate)
/// while the phone credits everything it held. A, finishing late, is stale:
/// B must get that credit and its repair must flow.
#[test]
fn an_older_reattach_finishing_late_does_not_starve_the_newer_one() {
    let (owner, owner_gate) = gated_owner();
    std::fs::write(owner_gate.join("release"), "").unwrap(); // A's owner reply passes
    let t = owner_terminal(&owner, 400);
    let (relay, gate) = gated_relay_to(&owner);
    let mut phone = slow_phone(&relay, &format!("owner/{t}"), 300);
    phone.drain(Duration::from_millis(500));
    assert_eq!(phone.received, wire::WINDOW);
    cut_line(&relay, &owner);
    restore_line(&relay);
    wait_until("A has its reply", || gate.join("ready-1").exists());
    std::fs::write(gate.join("release-2"), "").unwrap(); // B installs as soon as its reply is in
    std::fs::remove_file(owner_gate.join("release")).unwrap();
    std::fs::remove_file(owner_gate.join("ready")).unwrap();
    cut_line(&relay, &owner);
    restore_line(&relay);
    wait_until("B's owner reply is held", || owner_gate.join("ready").exists());
    let request: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(owner_gate.join("ready")).unwrap()).unwrap();
    assert_eq!(request["credit"], 0);
    phone.conn.send(phone.channel, wire::CREDIT, &(phone.received as u32).to_le_bytes());
    phone.credit = true;
    phone.drain(Duration::from_millis(200));
    std::fs::write(gate.join("release-1"), "").unwrap(); // A finishes late
    std::thread::sleep(Duration::from_millis(300));
    phone.raw.clear();
    phone.received = 0;
    std::fs::write(owner_gate.join("release"), "").unwrap();
    phone.until("B's repair flows", |v| v.screen.plain_text().contains("R0385"));
    assert!(phone.closed.is_none());
}

/// INS-2950 v3, real relay path: reattach A's line drops before its reply
/// (owner gate holds it), A's failure waits (relay gate); reattach B
/// succeeds and history flows. A failing late must not close B's stream.
#[test]
fn an_older_reattach_failing_late_does_not_close_the_newer_stream() {
    let (owner, owner_gate) = gated_owner();
    let t = owner_terminal(&owner, 400);
    let (relay, gate) = gated_relay_to(&owner);
    let mut phone = slow_phone(&relay, &format!("owner/{t}"), 300);
    phone.drain(Duration::from_millis(500));
    cut_line(&relay, &owner);
    restore_line(&relay);
    wait_until("A's owner reply is held", || owner_gate.join("ready").exists());
    phone.conn.send(phone.channel, wire::CREDIT, &(phone.received as u32).to_le_bytes());
    phone.credit = true;
    cut_line(&relay, &owner);
    wait_until("A failed, held at the relay gate", || gate.join("ready-1").exists());
    assert_eq!(std::fs::read_to_string(gate.join("ready-1")).unwrap(), r#"{"ok":false}"#);
    std::fs::write(gate.join("release-2"), "").unwrap();
    std::fs::write(owner_gate.join("release"), "").unwrap();
    restore_line(&relay);
    phone.until("B's history flows", |v| v.screen.plain_text().contains("R0385"));
    std::fs::write(gate.join("release-1"), "").unwrap();
    phone.drain(Duration::from_millis(500));
    assert!(phone.closed.is_none(), "A's late failure left B alone: {:?}", phone.closed);
    let before = phone.received;
    owner.json(&["send", &t, "flood"]);
    phone.until("B's stream still carries output", |v| v.received > before);
}

/// INS-2950 v4, the real relay path with two viewers: reconnect pass A is
/// held (relay gate) after its first viewer's reply while its line drops
/// again; pass B reattaches both viewers and output reaches both. A, let go,
/// must not begin anything on its dead line: no attempt times out there and
/// closes a live viewer.
#[test]
fn an_older_reconnect_pass_leaves_both_newer_viewers_alone() {
    let owner = Session::new();
    let t = owner_terminal(&owner, 400);
    let (relay, gate) = gated_relay_to(&owner);
    for n in [2, 3, 4] {
        std::fs::write(gate.join(format!("release-{n}")), "").unwrap();
    }
    let config = relay.root.join("config/herdr-core");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.toml"), "machine_timeout = 2.0\n").unwrap();
    let mut one = slow_phone(&relay, &format!("owner/{t}"), 300);
    let mut two = slow_phone(&relay, &format!("owner/{t}"), 300);
    for phone in [&mut one, &mut two] {
        phone.credit = true;
        phone.conn.send(phone.channel, wire::CREDIT, &(phone.received as u32).to_le_bytes());
        phone.drain(Duration::from_millis(200));
    }
    cut_line(&relay, &owner);
    restore_line(&relay);
    wait_until("A has its first viewer's reply", || gate.join("ready-1").exists());
    cut_line(&relay, &owner);
    restore_line(&relay);
    wait_until("B reattached both viewers", || gate.join("ready-3").exists());
    for phone in [&mut one, &mut two] {
        phone.drain(Duration::from_millis(300));
    }
    let (before_one, before_two) = (one.received, two.received);
    owner.json(&["send", &t, "main"]);
    one.until("output on B (one)", |v| v.received > before_one);
    two.until("output on B (two)", |v| v.received > before_two);
    std::fs::write(gate.join("release-1"), "").unwrap();
    // Longer than the 2 s attach deadline a dead-line attempt would wait out.
    for phone in [&mut one, &mut two] {
        phone.drain(Duration::from_millis(1700));
    }
    assert!(!gate.join("ready-4").exists(), "A began no attempt on its dead line");
    assert!(one.closed.is_none() && two.closed.is_none(), "{:?} {:?}", one.closed, two.closed);
}
