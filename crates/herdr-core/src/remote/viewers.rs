//! Viewer streams relayed to other cores, including reattachment.

use super::*;

/// A viewer here on a terminal there. It outlives a dropped line: when the
/// line is back, it is attached again on the same channel here.
#[derive(Clone)]
pub struct Relayed {
    pub automatic_input: bool,
    pub visibility_updates: bool,
    /// The whole path confirmed WINDOW_FOCUS: forwarded, and required on reattach.
    pub focus_updates: bool,
    /// With `focus_updates`: when the viewer was last used (WINDOW_FOCUS(1)
    /// or DATA a person sent). A reattach tells the owner how long ago
    /// (`idle_ms`), so a dropped line never looks like fresh use.
    pub used_at: Option<std::time::Instant>,
    /// A terminal reply cut across DATA frames is still a reply.
    pub replies: crate::input::Replies,
    /// The path confirmed the owner sizes by use (`focus_by_use`); required
    /// again on reattach, like `focus_updates`.
    pub focus_by_use: bool,
    pub line: Arc<Line>,
    pub there: u32,
    pub conn: Arc<Conn>,
    pub machine: String,
    pub params: Value,
    /// The reattach in flight, if any (see [`Attempt`]). None: live, CREDIT
    /// goes straight to `line`/`there`.
    pub attempt: Option<Attempt>,
}

/// One reattach of a relayed viewer, after its line dropped. Per viewer
/// there is at most one current attempt, kept on its [`Relayed`] and only
/// changed under the core lock:
///
/// - live (`attempt: None`): `line`/`there` is the stream; CREDIT goes there.
/// - reattaching (`Some(Attempt { id, kept })`): started by `reattach`, which
///   takes the viewer's room as the new stream's starting credit; CREDIT is
///   counted here (`kept`), not sent to the dead line. A new attempt
///   replaces an older one (its room already counts that one's `kept`).
/// - install (only the current `id`): `line`/`there` become the new stream,
///   `kept` is sent to it once, back to live.
/// - fail (only the current `id`): the viewer's stream ends here.
/// - stale (the `id` is no longer current, or the viewer is gone): the
///   attempt touches nothing of the viewer; it only closes the stream it
///   opened there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub id: u64,
    pub kept: u64,
}

static NEXT_ATTEMPT: AtomicU64 = AtomicU64::new(1);

/// Why an opted-in stream was refused: a hop on the way lost resize support.
pub(super) const RESIZE_LOST: &str = "resize notifications lost";
/// Why an opted-in stream was refused: a hop on the way lost visibility support.
pub(super) const VISIBILITY_LOST: &str = "viewer visibility updates lost";
/// Why an opted-in stream was refused: a hop on the way lost window focus support.
pub(super) const FOCUS_LOST: &str = "viewer focus updates lost";
/// Why an opted-in stream was refused: the path no longer confirms its `resize_history`.
pub(super) const HISTORY_LOST: &str = "resize history lost";

/// Viewers left waiting by a dropped line to `m` get their terminal back.
pub(super) async fn reattach(core: Arc<Core>, m: Machine, line: Arc<Line>) {
    let waiting: Vec<(usize, u32)> = core
        .lock()
        .relayed
        .iter()
        .filter(|(_, r)| r.machine == m.id && !Arc::ptr_eq(&r.line, &line))
        .map(|(k, _)| *k)
        .collect();
    for (key, here) in waiting {
        // Begin this attempt (see `Attempt`). The viewer's credit here is still
        // partly spent on what is queued for it: the new stream there starts
        // with only what it can still take, not a fresh window each time
        // (repeated drops would pile windows up here until the backlog
        // overflows). Taken under the lock `relay_in` counts CREDIT under, so
        // each credited byte is in this starting credit or in `kept`, once.
        // A pass only acts on viewers whose current stream is still on the
        // line it is repairing, using fresh entry state: none at all once a
        // newer line took over (that line's own pass repairs them), and not a
        // viewer already moved on. Its params are read now, not when the pass
        // started.
        let id = NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed);
        let (r, room) = {
            let mut st = core.lock();
            if !matches!(st.lines.get(&m.id), Some(LineState::Connected(l)) if Arc::ptr_eq(l, &line)) {
                break;
            }
            let Some(entry) = st.relayed.get_mut(&(key, here)) else { continue };
            if entry.machine != m.id || Arc::ptr_eq(&entry.line, &line) {
                continue;
            }
            entry.attempt = Some(Attempt { id, kept: 0 });
            (entry.clone(), entry.conn.outbox().room(here))
        };
        // The screen is painted again; the viewer already has its history.
        let mut params = r.params.clone();
        params["history"] = 0.into();
        params["resumed"] = true.into();
        params["credit"] = room.into();
        if let Some(idle) = idle_ms(&r) {
            params["idle_ms"] = idle.into();
        }
        negotiate_resize(&line, &mut params);
        negotiate_visibility(&line, &mut params);
        negotiate_focus(&line, &mut params);
        negotiate_resize_history(&line, &mut params);
        let wanted_history = r.params["resize_history"].as_u64().unwrap_or(0);
        let deadline = deadline_for(&core, "terminal.attach", &params);
        // Checked in `take_reply`, before the stream there is connected here.
        let sink = (
            (r.conn.clone(), here),
            r.params["resize_notifications"] == true,
            r.visibility_updates,
            r.focus_updates,
            wanted_history,
        );
        let reply = line.call_into(&m.name, "terminal.attach", params, Some(sink), deadline).await;
        reattach_gate(reply.is_ok()).await;
        let lost_resize = matches!(&reply, Err(e) if e.message == RESIZE_LOST);
        let lost_visibility =
            matches!(&reply, Err(e) if [VISIBILITY_LOST, FOCUS_LOST, HISTORY_LOST].contains(&e.message.as_str()));
        let mut st = core.lock();
        let reply = reply.ok();
        // Stale: a newer attempt took over, or the viewer is gone (detached,
        // or closed for overflow). Only the stream this attempt opened there ends.
        let current = st.relayed.get(&(key, here)).and_then(|e| e.attempt.as_ref()).is_some_and(|a| a.id == id);
        if !current {
            if let Some(there) = reply.as_ref().and_then(|reply| reply["channel"].as_u64()) {
                lock(&line.streams).remove(&(there as u32));
                let _ = line.frames.send(wire::encode(there as u32, wire::CLOSE, b""));
            }
            continue;
        }
        let lost_by_use = r.focus_by_use && reply.as_ref().is_some_and(|reply| !focus_by_use(&line, reply));
        if lost_by_use || r.automatic_input && reply.as_ref().is_some_and(|reply| !automatic_input(&line, reply)) {
            st.relayed.remove(&(key, here));
            if let Some(there) = reply.as_ref().and_then(|reply| reply["channel"].as_u64()) {
                lock(&line.streams).remove(&(there as u32));
                let _ = line.frames.send(wire::encode(there as u32, wire::CLOSE, b""));
            }
            r.conn.with(|out| out.close(here, b""));
            continue;
        }
        match reply.and_then(|reply| reply["channel"].as_u64()) {
            Some(there) => {
                // Install: the new stream gets the CREDIT kept meanwhile, once;
                // later CREDIT goes straight there.
                let there = there as u32;
                let entry = st.relayed.get_mut(&(key, here)).expect("current attempt");
                let kept = entry.attempt.take().map_or(0, |a| a.kept);
                entry.line = line.clone();
                entry.there = there;
                send_credit(&line, there, kept);
            }
            None => {
                st.relayed.remove(&(key, here));
                // A downgraded path must not deliver paints sized for the old
                // contract; any other failure still hands over what arrived.
                if lost_resize || lost_visibility {
                    r.conn.with(|out| out.close(here, b""));
                } else {
                    r.conn.with(|out| out.finish(here, b""));
                }
            }
        }
    }
}

/// Tests only (debug builds): with `HC_TEST_REATTACH_GATE` set to a folder,
/// the n-th reattach in this process (1, 2, ...) writes `ready-n` there
/// (`{"ok": ..}`, whether its reply came) once its reply is in, and waits for
/// `release-n` before it installs or fails: a scheduling barrier for
/// overlapping reattaches.
async fn reattach_gate(ok: bool) {
    static SEEN: AtomicU64 = AtomicU64::new(0);
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(gate) = std::env::var_os("HC_TEST_REATTACH_GATE").map(std::path::PathBuf::from) else { return };
    let n = SEEN.fetch_add(1, Ordering::Relaxed) + 1;
    let _ = std::fs::write(gate.join(format!("ready-{n}")), json!({ "ok": ok }).to_string());
    while !gate.join(format!("release-{n}")).exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// What a reattach tells the owner about a viewer: how long ago it was last
/// used here, focused or not, reporting focus or not (at least 1 ms: never
/// "just now", the line dropped meanwhile). None: nothing to say.
pub(super) fn idle_ms(r: &Relayed) -> Option<u64> {
    if !r.focus_updates {
        // A viewer that sends no focus: how long since it was typed into.
        return r.used_at.map(|used| (used.elapsed().as_millis() as u64).max(1));
    }
    match r.used_at {
        Some(used) => Some((used.elapsed().as_millis() as u64).max(1)),
        // Focused but never seen used here: as old as anything can be.
        None => (r.params["focused"] == true).then_some(u64::MAX),
    }
}

/// The owner sizes by use, as the path there confirms.
pub(super) fn focus_by_use(line: &Line, reply: &Value) -> bool {
    line.features.iter().any(|f| f == "focus_by_use") && reply["focus_by_use"] == true
}

pub(super) fn automatic_input(line: &Line, reply: &Value) -> bool {
    line.features.iter().any(|f| f == "automatic_input") && reply["automatic_input"] == true
}

pub(super) fn negotiate_resize(line: &Line, params: &mut Value) {
    if params["resize_notifications"] == true && !line.features.iter().any(|f| f == "resize_notifications") {
        params["resize_notifications"] = false.into();
    }
}

pub(super) fn negotiate_visibility(line: &Line, params: &mut Value) {
    if params["visibility_updates"] == true && !line.features.iter().any(|f| f == "viewer_visibility") {
        params["visibility_updates"] = false.into();
    }
}

/// Never forward a focus opt-in to a hop that would not confirm it.
pub(super) fn negotiate_focus(line: &Line, params: &mut Value) {
    if params["focus_updates"] == true && !line.features.iter().any(|f| f == "viewer_focus") {
        params["focus_updates"] = false.into();
        params["focused"] = false.into();
    }
}

/// Never ask a hop without `resize_history` for it: it would paint screen-only.
pub(super) fn negotiate_resize_history(line: &Line, params: &mut Value) {
    if params["resize_notifications"] != true || !line.features.iter().any(|f| f == "resize_history") {
        params["resize_history"] = 0.into();
    }
}

/// What the path there confirmed: the owner's echo, and only through a hop
/// that has the feature (an old core never echoes it).
pub(super) fn resize_history(line: &Line, requested: u64, reply: &Value) -> u64 {
    if !line.features.iter().any(|f| f == "resize_history") {
        return 0;
    }
    reply["resize_history"].as_u64().unwrap_or(0).min(requested)
}

pub(super) fn focus_updates(line: &Line, requested: bool, reply: &Value) -> bool {
    requested && line.features.iter().any(|f| f == "viewer_focus") && reply["focus_updates"] == true
}

pub(super) fn visibility_updates(line: &Line, requested: bool, reply: &Value) -> bool {
    requested && line.features.iter().any(|f| f == "viewer_visibility") && reply["visibility_updates"] == true
}

/// Opens a viewer here on a terminal there: the stream channel there is
/// relayed to a new channel on this connection, both ways.
pub async fn attach(core: &Core, conn: &Arc<Conn>, m: &Machine, mut params: Value) -> Result<Value, RpcError> {
    // Never launder an ordinary client's claimed desktop through our trusted line.
    params["desktop_group"] =
        core.desktop_group(conn, params["desktop_group"].as_str()).map_or(Value::Null, |(_, group)| group.into());
    let visibility_requested = params["visibility_updates"] == true;
    if !visibility_requested {
        params["visible"] = true.into();
    }
    let focus_requested = params["focus_updates"] == true;
    if !focus_requested {
        params["focused"] = false.into();
    }
    let line = line(core, m).await?;
    let mut forwarded = params.clone();
    negotiate_resize(&line, &mut forwarded);
    negotiate_visibility(&line, &mut forwarded);
    negotiate_focus(&line, &mut forwarded);
    negotiate_resize_history(&line, &mut forwarded);
    let here = conn.new_channel();
    conn.with(|out| out.open_paused(here));
    let deadline = deadline_for(core, "terminal.attach", &forwarded);
    match line
        .call_into(&m.name, "terminal.attach", forwarded.clone(), Some(((conn.clone(), here), false, false, false, 0)), deadline)
        .await
    {
        Ok(mut r) => {
            let there = r["channel"].as_u64().unwrap_or_default() as u32;
            let resize_enabled = forwarded["resize_notifications"] == true && r["resize_notifications"] == true;
            let visibility_enabled = visibility_updates(&line, visibility_requested, &r);
            r["resize_notifications"] = resize_enabled.into();
            r["visibility_updates"] = visibility_enabled.into();
            let focus_enabled = focus_updates(&line, focus_requested, &r);
            r["focus_updates"] = focus_enabled.into();
            params["focus_updates"] = focus_enabled.into();
            if !focus_enabled {
                params["focused"] = false.into();
            }
            params["resize_notifications"] = resize_enabled.into();
            params["visibility_updates"] = visibility_enabled.into();
            let requested = forwarded["resize_history"].as_u64().unwrap_or(0);
            let history = if resize_enabled { resize_history(&line, requested, &r) } else { 0 };
            r["resize_history"] = history.into();
            // A reattach asks for what was confirmed, and requires it again.
            params["resize_history"] = history.into();
            let automatic_input = automatic_input(&line, &r);
            r["automatic_input"] = automatic_input.into();
            // Only the owner's word, through a hop that knows the rule.
            let by_use = focus_enabled && focus_by_use(&line, &r);
            r["focus_by_use"] = by_use.into();
            let used_at = match params["idle_ms"].as_u64() {
                Some(ms) => crate::viewers::used_ago(Some(ms)),
                None => (focus_enabled && params["focused"] == true).then(std::time::Instant::now),
            };
            let viewer = Relayed {
                automatic_input,
                visibility_updates: visibility_enabled,
                focus_updates: focus_enabled,
                used_at,
                replies: Default::default(),
                focus_by_use: by_use,
                line,
                there,
                conn: conn.clone(),
                machine: m.id.clone(),
                params,
                attempt: None,
            };
            core.lock().relayed.insert((conn_key(conn), here), viewer);
            r["channel"] = here.into();
            Ok(localize(core, &m.name, &[], r))
        }
        Err(e) => {
            conn.with(|out| out.close(here, b""));
            Err(e)
        }
    }
}

/// A viewer's frame for a relayed stream goes there. False: not relayed.
pub fn relay_in(core: &Core, conn: &Arc<Conn>, here: u32, kind: u8, payload: &[u8]) -> bool {
    let r = {
        let mut st = core.lock();
        let Some(r) = st.relayed.get_mut(&(conn_key(conn), here)) else { return false };
        if kind == wire::VISIBILITY {
            if !r.visibility_updates || payload.len() != 1 || payload[0] > 1 {
                return true;
            }
            r.params["visible"] = (payload[0] == 1).into();
        }
        if kind == wire::WINDOW_FOCUS {
            // Per downstream viewer, end to end; a reattach carries the latest state.
            if !r.focus_updates || payload.len() != 1 || payload[0] > 1 {
                return true;
            }
            r.params["focused"] = (payload[0] == 1).into();
            if payload[0] == 1 {
                r.used_at = Some(std::time::Instant::now());
            }
        }
        // Use, for any viewer (one that sends no focus leads by input alone).
        // Judged for sizing only: the frame goes on as it came, kind and bytes
        // (the owner's agent-input gate must see a person's DATA as DATA).
        if kind == wire::DATA && !payload.is_empty() && !r.replies.only_replies(payload) {
            r.used_at = Some(std::time::Instant::now());
        }
        // What a reattach after a dropped line brings back.
        if kind == wire::RESIZE && payload.len() == 4 {
            r.params["cols"] = u16::from_le_bytes([payload[0], payload[1]]).into();
            r.params["rows"] = u16::from_le_bytes([payload[2], payload[3]]).into();
        } else if kind == wire::FACTS
            && let Ok(display) = serde_json::from_slice::<Value>(payload)
        {
            r.params["display"] = display;
        }
        // Read here: our stream to the viewer may send more, and so may the
        // one there to us. Counted under the lock (see `reattach`); while a
        // reattach is in flight it is kept for the new stream there.
        if kind == wire::CREDIT && payload.len() == 4 {
            let n = u32::from_le_bytes(payload.try_into().unwrap());
            conn.with(|out| out.credit(here, n as usize));
            if let Some(attempt) = &mut r.attempt {
                attempt.kept += u64::from(n);
                return true;
            }
        }
        r.clone()
    };
    // While the line is down this goes nowhere: typing into a gap is lost, not queued.
    let _ = r.line.frames.send(wire::encode(r.there, kind, payload));
    true
}

/// CREDIT for a stream there, in frames of at most u32::MAX bytes.
fn send_credit(line: &Line, there: u32, mut bytes: u64) {
    while bytes > 0 {
        let n = bytes.min(u64::from(u32::MAX)) as u32;
        let _ = line.frames.send(wire::encode(there, wire::CREDIT, &n.to_le_bytes()));
        bytes -= u64::from(n);
    }
}

/// A viewer's connection closed: its relayed streams there end.
pub fn conn_gone(core: &Core, conn: &Arc<Conn>) {
    let key = conn_key(conn);
    let mut st = core.lock();
    let gone: Vec<(u32, Arc<Line>, u32)> = st
        .relayed
        .iter()
        .filter(|((k, _), _)| *k == key)
        .map(|((_, here), r)| (*here, r.line.clone(), r.there))
        .collect();
    for (here, line, there) in gone {
        st.relayed.remove(&(key, here));
        lock(&line.streams).remove(&there);
        let _ = line.frames.send(wire::encode(there, wire::CLOSE, b""));
    }
}
